//! TMR's window (TODO 7): a session's events and progress in WinUI 3, through windows-reactor. A double-click opens it, or `tmr --gui ...`
//! (crates/tmr-app's `launch.rs`, TODO 29 S6). Start runs the session on its own thread with the parameters in the window's box, the exe's
//! own to begin with, as the CLI runs them (`tmr::session::run`), and the window shows its setup's facts (the info line, the system panel),
//! the large-page restart question as a dialog, and its run. Its log records go to the log file, and the warnings and errors to the
//! window's errors list too (`logger.rs`). One run a process: the engine's run settings are set once (`doc/todo/29-split-design.md`).
//!
//! The engine's events cross to the window on a channel (`GuiFrontend`), which a waiter on the thread pool drains every 500 ms, reading the
//! suite's progress and the log's lines as it does (`window::listen`). `model.rs` turns what it brings into what the window shows, and
//! `window.rs` draws it.
//!
//! Where the window runs (the README's table): Windows build 22000 or later, with the Windows App Runtime 2.x installed.
//! - Below 22000 (Server 2022 is 20348) Reactor's bootstrap call doesn't exist, so tmr.exe never opens the window there: a double-click
//!   runs the CLI, and `--gui` says why and runs nothing (exit 2) (`FIRST_GUI_BUILD` and `Launch::refused` in crates/tmr-app's
//!   `launch.rs`; the delay-load in its `build.rs` is what lets the exe start there at all).
//! - From 22000 to 26099 (Windows 11 21H2 to 23H2) a double-click flashes a console briefly: those builds ignore the manifest's
//!   `consoleAllocationPolicy`, so Windows opens one, which tmr.exe leaves before the window opens.
//! - From 26100 (Windows 11 24H2, Server 2025) the window is fully supported: a double-click opens it with no console.

// As the engine's (src/lib.rs): a `// SAFETY:` on every `unsafe` block, and `expect` for `unwrap()`
#![cfg_attr(not(test), warn(clippy::undocumented_unsafe_blocks, clippy::unwrap_used))]

mod chart;
mod logger;
mod model;
mod palette;
mod sysinfo;
mod window;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender, SyncSender};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tmr::frontend::{Event, Frontend, RestartChoice, RunStatus};
use tmr::session::SessionEnd;

/// What the session's thread hands the window.
enum Feed {
    /// A session's event, as the engine sent it
    Event(Event),
    /// Large pages need Windows restarted: the window asks, and answers on the reply (`GuiFrontend::large_page_restart`)
    Ask(SyncSender<RestartChoice>),
    /// The session ended: how, or the error that stopped it
    Ended(Result<SessionEnd, String>),
}

/// The window's frontend: each event onto the window's feed, as it comes. No ticks: they draw the CLI's ticker, and the window reads the
/// progress on its own clock. WHEA is still polled by the engine's monitor.
struct GuiFrontend {
    feed: Sender<Feed>,
}

/// How often the session's thread, waiting for the restart's answer, looks whether the window has closed
const ANSWER_WAIT: Duration = Duration::from_millis(500);

impl Frontend for GuiFrontend {
    fn event(&self, event: Event) {
        // Fails only when the window has closed, and then nothing shows the run
        let _ = self.feed.send(Feed::Event(event));
    }

    /// The window asks with a dialog, and this thread waits for its answer. Once the window has gone nobody can answer, and the session is
    /// being stopped (`end_session`), so the answer is Exit: the session ends with nothing allocated, as not run (`UNANSWERED`). 4 KiB
    /// pages, the console's answer to an empty line, would allocate the run's memory only for the stop to end the run before its first
    /// cycle.
    fn large_page_restart(&self) -> RestartChoice {
        let (reply, answer) = mpsc::sync_channel(1);
        if self.feed.send(Feed::Ask(reply)).is_ok() {
            loop {
                match answer.recv_timeout(ANSWER_WAIT) {
                    Ok(choice) => return choice,
                    Err(RecvTimeoutError::Timeout) if !WINDOW_CLOSED.load(Ordering::Relaxed) => {}
                    // The window closed with the question open, or dropped it unanswered
                    Err(_) => break,
                }
            }
        }
        UNANSWERED.store(true, Ordering::Relaxed);
        RestartChoice::Exit
    }
}

/// The session's thread, from Start, and its exit status once it ended
static SESSION: Mutex<Option<JoinHandle<()>>> = Mutex::new(None);
static EXIT_CODE: OnceLock<i32> = OnceLock::new();

/// Set when the window has opened, for the wording of its error, and when it has closed, for the restart's question
static WINDOW_OPENED: AtomicBool = AtomicBool::new(false);
static WINDOW_CLOSED: AtomicBool = AtomicBool::new(false);

/// The exe's icon, an .ico file's bytes, from `main`: the title bar's (`window::icon_path`)
static ICON: OnceLock<&'static [u8]> = OnceLock::new();

/// Set when the restart's question went unanswered, the window having gone: the session ended as for a restart, but nobody chose it
static UNANSWERED: AtomicBool = AtomicBool::new(false);

/// How long a session still running when the window closes has to finish its step and save its results, before it ends with the process
const CLOSE_GRACE: Duration = Duration::from_secs(30);

fn window_opened() {
    WINDOW_OPENED.store(true, Ordering::Relaxed);
}

fn session_started(session: JoinHandle<()>) {
    *SESSION.lock().unwrap_or_else(PoisonError::into_inner) = Some(session);
}

fn session_ended(result: &Result<SessionEnd, String>) {
    let _ = EXIT_CODE.set(exit_status(result, UNANSWERED.load(Ordering::Relaxed)).exit_code());
}

/// The exit status of how the session ended, as the CLI gives it (`RunStatus::exit_code`): its run's; passed (0) when the restart was
/// chosen, but not run (2) when the question went `unanswered`; not run when setup or an error stopped it, and for a one-shot command,
/// which the window doesn't run
fn exit_status(result: &Result<SessionEnd, String>, unanswered: bool) -> RunStatus {
    match result {
        Ok(SessionEnd::Ran { status, .. }) => *status,
        Ok(SessionEnd::RestartChosen) if !unanswered => RunStatus::Passed,
        Ok(SessionEnd::RestartChosen | SessionEnd::Stopped(_) | SessionEnd::Command(_)) | Err(_) => RunStatus::NotRun,
    }
}

/// The window has closed. Reactor 0.100.0 can't keep it open, so a session still running is stopped as Stop stops it, and has `CLOSE_GRACE`
/// to end its step and save its results; the cycles it finished are in the results file already. After that it ends with the process.
fn end_session(session: JoinHandle<()>) {
    if !session.is_finished() {
        tmr::request_stop();
    }
    let deadline = Instant::now() + CLOSE_GRACE;
    while !session.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    if session.is_finished() {
        let _ = session.join();
    }
}

/// How the window ended (`main`), for tmr.exe.
#[derive(Debug, PartialEq, Eq)]
pub struct Closed {
    /// The exit status, the CLI's for the same end (`RunStatus::exit_code`)
    pub exit_code: i32,
    /// Why the window couldn't open, or failed once open, for the console and a message box
    pub error: Option<String>,
}

/// Opens the window, to run `args` (the command line, its exe's name first, less `--gui`), and returns when it closes and the session has
/// ended (`end_session`), with the exit status and the window's error, if it had one (`closed`). `icon` is the exe's icon, an .ico file's
/// bytes, for the title bar. The window needs the Windows App Runtime 2.x, which Reactor offers to download when it is missing.
pub fn main(args: Vec<String>, icon: &'static [u8]) -> Closed {
    let _ = ICON.set(icon);
    let shown = windows_reactor::App::run_component::<window::Window>(args);
    WINDOW_CLOSED.store(true, Ordering::Relaxed);
    let session = SESSION.lock().unwrap_or_else(PoisonError::into_inner).take();
    let started = session.is_some();
    if let Some(session) = session {
        end_session(session);
    }
    let opened = WINDOW_OPENED.load(Ordering::Relaxed);
    let closed = closed(shown.err().map(|e| e.to_string()), opened, started, EXIT_CODE.get().copied());
    if let Some(error) = &closed.error {
        // In the run's log too, when Start opened one
        log::error!("{error}");
    }
    closed
}

/// How the window ended, from Reactor's `error`, whether the window had `opened`, whether Start `started` a session, and the exit status
/// that session ended with (`session_ended`). The status is the session's, interrupted (3) when it outlived `CLOSE_GRACE`, and stopped
/// before testing (2) when Start never ran, whether the window failed or not: a failure ends the window, not the run under way, whose
/// status it would hide. The error says whether the window had opened.
fn closed(error: Option<String>, opened: bool, started: bool, exit_code: Option<i32>) -> Closed {
    let exit_code = match exit_code {
        Some(code) => code,
        None if started => RunStatus::Interrupted.exit_code(),
        None => RunStatus::NotRun.exit_code(),
    };
    let error = error.map(|e| if opened { format!("The window failed: {e}") } else { format!("The window couldn't open: {e}") });
    Closed { exit_code, error }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The restart question (TODO 29 S6): the session's thread waits for the window's answer, and once nobody can answer (the question
    /// dropped, no window, or the window closed with it open), the answer is Exit, as not run. The only test that touches the window's
    /// flags: closing is last.
    #[test]
    fn the_restart_question_waits_for_the_window_and_gives_up_once_it_has_gone() {
        let ask = || {
            let (feed, window) = mpsc::channel();
            let asked = std::thread::spawn(move || GuiFrontend { feed }.large_page_restart());
            let Ok(Feed::Ask(reply)) = window.recv() else {
                panic!("the question wasn't asked");
            };
            (asked, reply, window)
        };
        let unanswered = || UNANSWERED.swap(false, Ordering::Relaxed);

        // Answered after a wait or two, the window open: it is still waited for
        let (asked, reply, _window) = ask();
        std::thread::sleep(ANSWER_WAIT * 3 / 2);
        reply.send(RestartChoice::Continue4K).unwrap();
        assert_eq!((asked.join().unwrap(), unanswered()), (RestartChoice::Continue4K, false));
        let (asked, reply, _window) = ask();
        reply.send(RestartChoice::Exit).unwrap();
        assert_eq!((asked.join().unwrap(), unanswered()), (RestartChoice::Exit, false), "the restart chosen");

        let (asked, reply, _window) = ask();
        drop(reply);
        assert_eq!((asked.join().unwrap(), unanswered()), (RestartChoice::Exit, true), "dropped unanswered");
        let (feed, window) = mpsc::channel();
        drop(window);
        assert_eq!((GuiFrontend { feed }.large_page_restart(), unanswered()), (RestartChoice::Exit, true), "no window to ask");

        // The window closes with the question open, the reply still held: the window's state may hold it after Reactor's loop has ended
        let (asked, _reply, _window) = ask();
        std::thread::sleep(ANSWER_WAIT / 2);
        let closed = Instant::now();
        WINDOW_CLOSED.store(true, Ordering::Relaxed);
        assert_eq!((asked.join().unwrap(), unanswered()), (RestartChoice::Exit, true), "closed unanswered");
        assert!(closed.elapsed() < 2 * ANSWER_WAIT, "given up after {:?}", closed.elapsed());
    }

    /// The exit status is the CLI's for the same end, and the restart is 0 only when it was chosen (TODO 29 S6)
    #[test]
    fn a_session_ends_with_the_clis_exit_status() {
        let ran = |status| Ok(SessionEnd::Ran { status, elapsed: Duration::from_secs(60), plan_only: false });
        for status in [RunStatus::Passed, RunStatus::Failed, RunStatus::NotRun, RunStatus::Interrupted] {
            assert_eq!(exit_status(&ran(status), false), status);
        }
        assert_eq!(exit_status(&Ok(SessionEnd::RestartChosen), false), RunStatus::Passed, "the restart chosen");
        assert_eq!(exit_status(&Ok(SessionEnd::RestartChosen), true), RunStatus::NotRun, "the question unanswered");
        let stopped = Ok(SessionEnd::Stopped(tmr::session::Stopped::Invalid("bad value".to_string())));
        assert_eq!(
            (exit_status(&stopped, false), exit_status(&Err("bad value".to_string()), false)),
            (RunStatus::NotRun, RunStatus::NotRun)
        );
    }

    /// The window's end: the session's status whatever became of the window, interrupted past the grace, not run without Start, and the
    /// window's error, worded by whether it had opened (TODO 29 S6)
    #[test]
    fn the_window_closes_with_the_sessions_status_and_its_own_error() {
        let failed = || Some("Unspecified error (0x80004005)".to_string());
        let ended = |exit_code, error: Option<&str>| Closed { exit_code, error: error.map(str::to_string) };
        assert_eq!(closed(None, true, true, Some(1)), ended(1, None), "a run that found errors");
        assert_eq!(closed(None, true, true, None), ended(3, None), "a session that outlived the grace");
        assert_eq!(closed(None, true, false, None), ended(2, None), "closed before Start");
        assert_eq!(closed(failed(), false, false, None), ended(2, Some("The window couldn't open: Unspecified error (0x80004005)")));
        assert_eq!(
            closed(failed(), true, false, None),
            ended(2, Some("The window failed: Unspecified error (0x80004005)")),
            "failed before Start"
        );
        assert_eq!(
            closed(failed(), true, true, Some(0)),
            ended(0, Some("The window failed: Unspecified error (0x80004005)")),
            "failed during a run that passed: the run's status"
        );
        assert_eq!(closed(failed(), true, true, None), ended(3, Some("The window failed: Unspecified error (0x80004005)")));
    }
}
