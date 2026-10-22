//! TMR's window (TODO 7): a session's events and progress in WinUI 3, through windows-reactor. Since
//! S3 of TODO 29 it is a preview, `tmr --gui ...` from a shell: Start runs the session on its own
//! thread with the exe's arguments, as the CLI does (`tmr::session::run`), and the window shows its
//! setup's facts (the info line, the system panel) and its run. Its log records still print to that
//! shell, until S6 gives the window a logger of its own. One run a process: the engine's run
//! settings are set once (`doc/todo/29-split-design.md`).
//!
//! The engine's events cross to the window on a channel (`GuiFrontend`), which a waiter on the
//! thread pool drains every 500 ms, reading the suite's progress as it does (`window::listen`).
//! `model.rs` turns what it brings into what the window shows, and `window.rs` draws it.

mod chart;
mod model;
mod palette;
mod sysinfo;
mod window;

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;

use tmr::frontend::{Event, Frontend, RestartChoice, RunStatus};
use tmr::session::SessionEnd;

/// What the session's thread hands the window.
enum Feed {
    /// A session's event, as the engine sent it
    Event(Event),
    /// Large pages need Windows restarted, and the run continues on 4 KiB pages
    /// (`GuiFrontend::large_page_restart`)
    RestartNeeded,
    /// The session ended: how, or the error that stopped it
    Ended(Result<SessionEnd, String>),
}

/// The window's frontend: each event onto the window's feed, as it comes. No ticks: they draw the
/// CLI's ticker, and the window reads the progress on its own clock. WHEA is still polled by the
/// engine's monitor.
struct GuiFrontend {
    feed: Sender<Feed>,
}

impl Frontend for GuiFrontend {
    fn event(&self, event: Event) {
        // Fails only when the window has closed, and then nothing shows the run
        let _ = self.feed.send(Feed::Event(event));
    }

    /// The window can't ask yet (S6 of TODO 29 gives it a dialog), so it answers as an empty answer
    /// does on the console, on 4 KiB pages, and says so in its errors.
    fn large_page_restart(&self) -> RestartChoice {
        let _ = self.feed.send(Feed::RestartNeeded);
        RestartChoice::Continue4K
    }
}

/// Whether Start ran the session, and its exit status once it ended
static STARTED: AtomicBool = AtomicBool::new(false);
static EXIT_CODE: OnceLock<i32> = OnceLock::new();

fn session_started() {
    STARTED.store(true, Ordering::Relaxed);
}

/// The exit status of how the session ended, as the CLI gives it (`RunStatus::exit_code`): its
/// run's, 0 when the restart was chosen, 2 when setup or an error stopped it, and 2 for a one-shot
/// command, which the window doesn't run
fn session_ended(result: &Result<SessionEnd, String>) {
    let status = match result {
        Ok(SessionEnd::Ran { status, .. }) => *status,
        Ok(SessionEnd::RestartChosen) => RunStatus::Passed,
        Ok(SessionEnd::Stopped(_) | SessionEnd::Command(_)) | Err(_) => RunStatus::NotRun,
    };
    let _ = EXIT_CODE.set(status.exit_code());
}

/// Opens the window, to run `args` (the command line, its exe's name first, less `--gui`), and
/// returns when it closes, with the exit status the CLI would give (`session_ended`), or stopped
/// before testing if it never started a session, or interrupted if it closed on one still running,
/// which ends with the process. The window needs the Windows App Runtime 2.x, which Reactor offers
/// to download when it is missing.
pub fn main(args: Vec<String>) -> i32 {
    if let Err(e) = windows_reactor::App::run_component::<window::Window>(args) {
        eprintln!("The window couldn't open: {e}");
        return RunStatus::NotRun.exit_code();
    }
    match EXIT_CODE.get() {
        Some(&code) => code,
        None if STARTED.load(Ordering::Relaxed) => RunStatus::Interrupted.exit_code(),
        None => RunStatus::NotRun.exit_code(),
    }
}
