//! The window (TODO 7), the owner's layout from the demos, top to bottom: the header (the title, the status badge and the runtime, the
//! run's errors and the time left under them, the info line, and the system panel on the right), the command bar (the parameters box, Load
//! config, the run button, which is Start, Stop and Run again, and the `…` menu), the tabs, and the tab chosen (TODO 109). The Run tab, the
//! one it opens on, has the position with the test's figures beside it and the progress bar, the throughput chart with its error marks,
//! the results, a row opening its report beside them, and the errors; the chart is never between the results and the errors. The Log tab
//! has the log's lines; Plan, System, Threads and Memory the console's startup sections; Compare two runs' results files. The large-page
//! question is a dialog over it (TODO 29 S6, TODO 113), a broad pick asked again in a second. The figures are the console's ticker's,
//! word for word (`model.rs`).
//!
//! It draws `Model`, and re-renders only when a message arrives: a batch from the feed (at least every 500 ms while the session runs), a
//! button, the dialog's answer, a keystroke in the box, a theme change or a resize. Nothing animates.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use tmr::commands::ResultsComparison;
use tmr::frontend::{ColumnAlignment, Event, ProgressSnapshot, ProgressTracker, Run, TableData, ThreadPriority};
use windows_reactor::*;

use crate::dialogs::{self, Open};
use crate::lang::{self, Phrase};
use crate::logger::{self, LogLine, LogLines};
use crate::model::{Bar, DialogButton, Model, Opened, Slot, Status, StepRow};
use crate::palette::{Palette, Palettes, brush};
use crate::placement::{self, Placement};
use crate::report::{Block, Report};
use crate::settings::{Settings, Theme};
use crate::{Feed, GuiFrontend, chart};

const PAGE_MARGIN_X: f64 = 20.0;
const CARD_PADDING_X: f64 = 16.0;
/// egui's look: a tall, fully rounded bar with the percentage inside.
const PROGRESS_HEIGHT: f64 = 22.0;
/// The window's clock: the waiter returns at least this often while the session runs
const TICK: Duration = Duration::from_millis(500);
/// The rows and error lines drawn, the newest: a night's run has thousands, and every redraw builds the ones drawn. Every row drawn opens
/// its report: the model keeps as many.
const SHOWN: usize = crate::model::REPORTS_KEPT;
/// The log lines the Log tab draws, the newest, in their order (TODO 109)
const LOG_SHOWN: usize = 1_000;

/// The title bar's icon (TODO 7), the exe's own (`main`'s, crates/tmr-app/tmr.ico for tmr.exe). WinUI's AppWindow takes an icon only as an
/// .ico file's path, so it is written once to the temp directory; `None` when it can't be, and the window keeps Windows' default.
fn icon_path() -> Option<&'static str> {
    static PATH: OnceLock<Option<String>> = OnceLock::new();
    PATH.get_or_init(|| {
        let folder = if tmr::home::elevated() { tmr::home::dir().join(tmr::settings_file::FOLDER) } else { std::env::temp_dir() };
        let path = folder.join("tmr-icon.ico");
        tmr::atomic_file::write(&path, crate::ICON.get()?).ok()?;
        Some(path.to_string_lossy().into_owned())
    })
    .as_deref()
}

pub(crate) enum Message {
    Feed(Box<Batch>),
    Start,
    Stop,
    /// The parameters box's text, as typed
    Args(String),
    /// The large-page dialog's answer, by the button that closed it (`Model::answer_large_pages`): its primary, its secondary (4 KiB
    /// pages this time), else its close button, Esc or nothing, which quit
    LargePages(ContentDialogResult),
    /// The grant's radio buttons' selection, none when the control has none
    LargePageTarget(Option<usize>),
    /// The second dialog's answer, after a broad pick: its secondary button grants; its primary button (the first, and focused) and Esc,
    /// the default, go back to the first dialog
    LargePageBroad(ContentDialogResult),
    /// A DIMM chosen in the system panel's selector: its menu item
    Dimm(String),
    /// A results row clicked: its report opens beside the list, or closes when it is open (TODO 109)
    Open(Opened),
    /// The report's close button
    Close,
    /// A tab chosen: its label
    Tab(Option<String>),
    /// Load config: the Open dialog, for a config to go into the parameters box (TODO 109)
    LoadConfig,
    /// The config the Open dialog gave, none when it was cancelled
    ConfigChosen(Result<Vec<PathBuf>, String>),
    /// Run again, once the session is over: the window closes, and tmr.exe starts again with the box's parameters (TODO 109)
    RunAgain,
    /// The Compare tab's Choose…: the Open dialog, for one of its files
    Choose(Slot),
    Chosen(Slot, Result<Vec<PathBuf>, String>),
    /// Its Choose both…: the Open dialog, for the two at once
    ChooseBoth,
    ChosenBoth(Result<Vec<PathBuf>, String>),
    /// Its This run: the run's own results file, once saved
    ThisRun(Slot),
    Swap,
    /// Two files compared, and what came of it
    Compared((PathBuf, PathBuf), Result<ResultsComparison, String>),
    /// The chart's ▾: shown or hidden, which `window.json` keeps (TODO 109)
    Chart,
    /// The hamburger: open or close the menu panel, which drops down in the page
    ToggleMenu,
    /// An item of that panel
    MenuPick(crate::lang::Menu),
    /// A file or a folder opened, or why the shell couldn't
    Opened(Result<(), String>),
    /// The ••• menu's Sanitize, or with `true` its Sanitize one file: the results file or the log the Open dialog gave, none when it was
    /// cancelled
    SanitizeChosen(bool, Result<Vec<PathBuf>, String>),
    /// The pair sanitized, or the one file alone, on a pool thread: what it wrote and replaced, or why it wrote nothing
    /// (`tmr::sanitize::sanitize`, `sanitize_single`)
    Sanitized(Result<String, String>),
    SanitizedClosed,
    AboutClosed,
    /// The Settings dialog (TODO 105): closed; its switches and the theme, each saved to window.json as it changes
    SettingsClosed,
    StartOnOpen(bool),
    AllowAdmin(bool),
    AdminAnswer(ContentDialogResult),
    KeepSerials(bool),
    Theme(Option<usize>),
    Scheme(ColorScheme),
    Size(WindowSize),
    /// A common setting written into the parameters box: `plan`, `memory`, `cycles` or `errors`, and the value
    Preset(&'static str, &'static str),
    /// Settings: a sound at the first error and when the run ends
    Notify(bool),
    /// Replace the stored calibration, or keep it: the dialog's button
    SaveAnswer(ContentDialogResult),
    /// `--get-readings` finished: the text it wrote, or why it stopped
    Readings(Result<String, String>),
    /// A timings line sampled off the UI thread, so a closed port does not stall a redraw
    ReadingsLine(String),
    /// The language chosen in Settings
    Language(Option<usize>),
    /// Settings: look up the latest GitHub release
    CheckUpdates,
    /// The lookup finished: the release, or why it couldn't
    UpdateChecked(Result<crate::update::Release, String>),
    /// Open a page: the repository, or the release the check found
    OpenUrl(String),
    /// Open the release the check found
    OpenRelease,
    /// The last-run note was dismissed, or its results file was opened
    UnfinishedClosed(ContentDialogResult),
    /// The heartbeat wrote. Nothing to show.
    Journaled,
}

/// What the window waits on: the session's feed, and the warnings and errors it logs
pub(crate) struct Feeds {
    events: Receiver<Feed>,
    log: LogLines,
}

/// What a wait on the feed brought: everything queued (nothing, at a tick), where the suite stood then and what WHEA reported, the lines
/// logged and how many of them the logger dropped so far, and the feeds, back for the next wait. `closed` when the session's thread is
/// gone.
pub(crate) struct Batch {
    feed: Vec<Feed>,
    snapshot: Option<ProgressSnapshot>,
    whea: Vec<String>,
    log: Vec<LogLine>,
    dropped: u64,
    feeds: Feeds,
    closed: bool,
}

impl Feed {
    /// The suite's progress, when this is the suite's start
    fn progress(&self) -> Option<Arc<ProgressTracker>> {
        match self {
            Feed::Event(Event::Run(Run::SuiteStarted { progress, .. })) => Some(Arc::clone(progress)),
            _ => None,
        }
    }
}

pub(crate) struct Window {
    model: Model,
    /// The logger's window side, between runs (TODO 109): the logger is the process's, and a run's waiter holds this while it runs
    log: Option<LogLines>,
    tab: Tab,
    /// The About dialog is open
    about: bool,
    /// The Settings dialog is open
    settings_open: bool,
    launch_allows_admin: bool,
    admin_answer: Option<mpsc::SyncSender<bool>>,
    /// The ••• menu's Sanitize came back: its dialog is open with what it wrote, or why it wrote nothing, until it closes
    sanitized: Option<Result<String, String>>,
    /// `window.json`'s (TODO 109): the chart shown or hidden, the theme, the parameters, the size and where the window is
    settings: Settings,
    dark: bool,
    width: f64,
    palettes: Palettes,
    /// The priority this thread, the UI's, was last set to (`follow_priority`): normal as it starts
    priority: ThreadPriority,
    /// ••• → Get timings tools is already downloading
    getting_readings: bool,
    /// Timings and a DIMM temperature, sampled off this thread. Empty until a source answers, and then the hint.
    readings_line: String,
    /// When the last heartbeat was written. A crash keeps that note.
    journal_lock: Option<std::fs::File>,
    last_journal: Option<Instant>,
    /// When timings were last sampled. Empty stays on a slow retry: a miss is a connect timeout.
    last_readings: Option<Instant>,
    /// A run that did not finish, shown once at the next open
    unfinished: Option<crate::journal::Note>,
    /// Settings' check for a newer release
    update: crate::update::Status,
    /// The hamburger's panel is open. It sits in the page, under the command bar, so it opens downward and the rest moves down.
    menu_open: bool,
}

/// The window's tabs (TODO 109): Run, the one it opens on, then the log, the console's startup sections, and Compare
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tab {
    Run,
    Log,
    Plan,
    System,
    Threads,
    Memory,
    Compare,
}

impl Tab {
    const ALL: [Tab; 7] = [Tab::Run, Tab::Log, Tab::Plan, Tab::System, Tab::Threads, Tab::Memory, Tab::Compare];

    fn label(self, language: crate::lang::Language) -> &'static str {
        lang::tab(language, self.index())
    }

    fn index(self) -> usize {
        Self::ALL.iter().position(|tab| *tab == self).unwrap_or(0)
    }

    fn from_label(label: &str) -> Option<Self> {
        lang::tab_index(label).and_then(|index| Self::ALL.get(index).copied())
    }
}

/// The window's size as it opens the first time, before `window.json` has one
const WIDTH: f64 = 860.0;
const HEIGHT: f64 = 960.0;

/// The run button's width: it says Start, Stop, Stopping…, Finishing… or Run again, and keeps its place
const RUN_BUTTON: f64 = 112.0;
/// The command bar's buttons beside the parameters box, Load config and the run button, with their spacing, and the hamburger at the right
const COMMANDS_WIDTH: f64 = 128.0 + RUN_BUTTON + 40.0 + 3.0 * 8.0;
/// The parameters box's widest: the parameters a run takes fit, and the buttons stay beside it
const PARAMETERS_MAX: f64 = 640.0;

/// `--start`, which the window takes out of its parameters: it starts the run as it opens (TODO 109), as Run again asks
pub(crate) const START: &str = "--start";

/// Waits on a thread-pool thread for the feed, up to `TICK`, then returns everything queued as one message, with where the suite stands
/// then and the lines logged. A wait that brought nothing returns as well: that is the window's clock, as Reactor 0.100.0 has no timer.
/// Re-armed from `update` after each delivery, so there is one waiter at a time, and the feeds travel with it. Each wait sets its pool
/// thread to the window's `priority`, and leaves it there (TODO 7): with a worker on every logical CPU, a pool thread at Normal waits for
/// Windows' starvation boost before it even starts the task, and the window's figures froze for 6-10 s at a time while its own thread
/// answered in milliseconds. Raised inside the task and put back after, the next task started low again. The pool reuses the thread
/// for the next wait, so it stays ready; once the window is at Normal again, so is the waiter's next thread.
fn listen(feeds: Feeds, progress: Option<Arc<ProgressTracker>>, priority: ThreadPriority, context: &ComponentContext<Window>) {
    context.spawn_background(move |_| {
        let _ = priority.apply();
        let mut feed = Vec::new();
        let closed = match feeds.events.recv_timeout(TICK) {
            Ok(first) => {
                feed.push(first);
                feed.extend(feeds.events.try_iter());
                false
            }
            Err(RecvTimeoutError::Timeout) => false,
            Err(RecvTimeoutError::Disconnected) => true,
        };
        // From this batch's start, when it is in it. The WHEA lines are the window's to show: the CLI's ticker, which takes them otherwise,
        // isn't running.
        let progress = progress.or_else(|| feed.iter().find_map(Feed::progress));
        let (snapshot, whea) = progress.map_or((None, Vec::new()), |p| (p.snapshot(), p.take_whea_descriptions()));
        let (log, dropped) = feeds.log.take();
        Message::Feed(Box::new(Batch { feed, snapshot, whea, log, dropped, feeds, closed }))
    });
}

impl Window {
    fn palette(&self) -> &Palette {
        self.palettes.get(self.dark)
    }

    fn t(&self, phrase: Phrase) -> &'static str {
        lang::text(self.settings.language, phrase)
    }

    /// A note of where the run is, and a fresh timings line, every couple of seconds, flushed. A crash keeps the last one. A run that ends
    /// closes it, and a write still in flight cannot reopen it.
    fn heartbeat(&mut self, context: &ComponentContext<Self>, ended: bool) {
        if ended {
            if self.journal_lock.is_some() {
                crate::journal::close();
            }
            self.unfinished = None;
            self.last_journal = None;
            return;
        }
        let running = matches!(self.model.status, Status::Running | Status::Stopping) || self.model.calibrating();
        if !running {
            return;
        }
        self.sample_if_due(context);
        let due = self.last_journal.is_none_or(|at| at.elapsed() >= Duration::from_secs(2));
        if !due || self.journal_lock.is_none() {
            return;
        }
        self.last_journal = Some(Instant::now());
        let note = self.note();
        let ticket = crate::journal::ticket();
        context.spawn_background(move |_| {
            crate::journal::save_with(ticket, &note);
            Message::Journaled
        });
    }

    /// Timings and a DIMM temperature, while a run is going. A source that answered is sampled with the note; none yet, every 15 s, so a
    /// closed port's timeout is not paid on every note.
    fn sample_if_due(&mut self, context: &ComponentContext<Self>) {
        let every = if self.readings_line.is_empty() { 15 } else { 2 };
        if self.last_readings.is_none_or(|at| at.elapsed() >= Duration::from_secs(every)) {
            self.sample_readings(context);
        }
    }

    fn note(&self) -> crate::journal::Note {
        crate::journal::now(
            self.model.position.clone(),
            self.model.error_count.as_ref().map(|count| count.text.clone()).unwrap_or_default(),
            self.readings_line.clone(),
            self.model.results_path().map(str::to_string),
        )
    }

    /// Runs the parameters box's command line on the session's own thread, its events to the window's feed. The log file opens now, with
    /// the window's logger, so its name pairs with the run's results.
    fn start(&mut self, context: &ComponentContext<Self>) {
        // A run as a new process's first (TODO 109): the last run's settings, stamp and stop cleared, then a log file of its own
        tmr::session::reset_run();
        let log = self.log.take().unwrap_or_else(logger::install);
        let mut args = self.model.command();
        // Only the process flag, the protected Settings switch or this launch's answer supplies consent. The parameter box is saved.
        tmr::session::take_admin_flag(&mut args);
        if self.launch_allows_admin || tmr::home::window_consent() {
            args.push("--allow-admin".into());
        }
        // No log file under record=off in the box (TODO 105), as the console has none
        self.model.log_path = tmr::logfile::wanted(&args).then(|| {
            logger::open_log_file();
            tmr::logfile::path()
        });
        let (tx, rx) = mpsc::channel();
        let fe = Arc::new(GuiFrontend { feed: tx.clone(), transcript: Some(log.transcript()) });
        // Serials kept in the Settings dialog: the run keeps them, as --serials does; off, the default, it leaves them out (TODO 105)
        if self.settings.keep_serials && !args.iter().any(|arg| arg == "--serials") {
            args.push("--serials".to_string());
        }
        let session = std::thread::Builder::new().name("tmr-engine".to_string()).spawn(move || {
            let result = tmr::session::run(args, fe).map_err(|e| {
                // In the log file, as the CLI has it; the window shows it as the session's end
                log::error!(target: tmr::logfile::FILE_ONLY_TARGET, "{e}");
                e.to_string()
            });
            crate::session_ended(&result);
            // The window closed: the run is over all the same
            let _ = tx.send(Feed::Ended(result));
        });
        match session {
            Ok(session) => {
                crate::session_started(session);
                self.model.start();
                self.unfinished = None;
                self.heartbeat(context, false);
                listen(Feeds { events: rx, log }, None, self.priority, context);
            }
            // As calibrate()'s: the logger's side waits for the next run, and the log file is done with
            Err(e) => {
                self.log = Some(log);
                tmr::logfile::close();
                self.model.ended(Err(format!("the engine's thread didn't start: {e}")));
            }
        }
    }

    /// ••• → Get timings tools: download the two official releases into companions\, then open that folder. No driver is installed.
    fn get_readings(&mut self, context: &ComponentContext<Self>) {
        if self.getting_readings {
            return;
        }
        self.getting_readings = true;
        self.model.position = "Downloading ZenTimings and LibreHardwareMonitor…".to_string();
        context.spawn_background(|_| {
            let mut out = Vec::new();
            let command = tmr::commands::Command::GetReadings(vec!["tmr".into(), "--get-readings".into()]);
            let status = tmr::commands::run(&command, &mut out, &mut |_| false, &|_| String::new());
            let text = String::from_utf8_lossy(&out).trim().to_string();
            match status {
                Ok(tmr::frontend::RunStatus::Passed) => Message::Readings(Ok(text)),
                Err(e) => {
                    let detail = if text.is_empty() { e.to_string() } else { format!("{text}\n{e}") };
                    Message::Readings(Err(detail))
                }
                Ok(_) => Message::Readings(Err(if text.is_empty() { "the download failed".into() } else { text })),
            }
        });
    }

    /// One sample of the timings line, off the UI thread. A miss costs the connect timeout, then the hint stays.
    fn sample_readings(&mut self, context: &ComponentContext<Self>) {
        self.last_readings = Some(Instant::now());
        context.spawn_background(|_| Message::ReadingsLine(crate::readings::line()));
    }

    /// The ••• menu's Calibrate (TODO 110): `--calibrate-all` on a thread of its own, the caches then the seal's width (the fastest
    /// here, which `seal-width=auto` takes), as the console runs it, what it writes and logs in the Log tab and a log file of its own; it
    /// saves what it measured (the menu item was the request), and the position line says how it did. Before a run the info line says
    /// the new one once it ends; after one it keeps that run's facts, and Run again reads the new one. Start waits for it.
    fn calibrate(&mut self, context: &ComponentContext<Self>) {
        tmr::session::reset_run();
        let log = self.log.take().unwrap_or_else(logger::install);
        logger::open_log_file();
        self.model.log_path = Some(tmr::logfile::path());
        let (tx, rx) = mpsc::channel();
        let transcript = log.transcript();
        let exe = self.model.command().first().cloned().unwrap_or_else(|| "tmr.exe".to_string());
        let mut line = vec![exe, "--calibrate-all".to_string()];
        // Serials kept in the Settings dialog: in what it shows and logs, as start()'s run has them (TODO 105)
        if self.settings.keep_serials {
            line.push("--serials".to_string());
        }
        let ask = tx.clone();
        let allow_admin = self.launch_allows_admin || tmr::home::window_consent();
        let frontend = GuiFrontend { feed: tx.clone(), transcript: None };
        let calibration = std::thread::Builder::new().name("tmr-calibrate".to_string()).spawn(move || {
            if let Err(e) = tmr::session::authorize_run(allow_admin, &frontend) {
                let _ = tx.send(Feed::Calibrated(Err(e.to_string())));
                return;
            }
            let command = tmr::commands::Command::Calibrate(line);
            let mut out = logger::TranscriptWriter::new(transcript);
            let mut confirm = move |question: &str| crate::ask_save(&ask, question);
            let result = tmr::commands::run(&command, &mut out, &mut confirm, &|_| String::new()).map_err(|e| e.to_string());
            out.finish();
            let _ = tx.send(Feed::Calibrated(result));
        });
        match calibration {
            Ok(handle) => {
                crate::calibrate_started(handle);
                self.model.calibration_started();
                self.unfinished = None;
                self.heartbeat(context, false);
                listen(Feeds { events: rx, log }, None, self.priority, context);
            }
            Err(e) => {
                self.log = Some(log);
                tmr::logfile::close();
                self.model.calibration_ended(Err(format!("its thread didn't start: {e}")));
            }
        }
    }

    fn take(&mut self, batch: Batch, context: &ComponentContext<Self>) {
        let Batch { feed, snapshot, whea, log, dropped, feeds, closed } = batch;
        let mut ended = false;
        for item in feed {
            match item {
                Feed::Event(event) => self.model.apply(event),
                Feed::Ask { question, reply } => self.model.ask_large_pages(question, reply),
                Feed::AskAdmin(reply) => self.admin_answer = Some(reply),
                Feed::Ended(result) => {
                    self.model.ended(result);
                    ended = true;
                }
                Feed::SaveAsk { question, reply } => self.model.ask_save(question, reply),
                Feed::Calibrated(result) => {
                    self.model.calibration_ended(result);
                    self.model.stored_calibration =
                        Some(crate::model::stored_calibration_text(tmr::session::stored_calibration().as_ref()));
                    ended = true;
                }
            }
        }
        let was_alarm = self.model.error_count.as_ref().is_some_and(|count| count.alarm);
        self.model.logged(log, dropped);
        self.model.tick(snapshot, whea);
        let alarm = self.model.error_count.as_ref().is_some_and(|count| count.alarm);
        if self.settings.notify && alarm && !was_alarm {
            sound(true);
        }
        self.heartbeat(context, ended);
        if closed && !ended {
            // A panic ends the thread before it says how it went (a release build aborts). Whose thread: the window's own calibration's
            // feed is the only one read while the box takes parameters (before a run, or after one); a session's is read with it not
            if self.model.takes_parameters() {
                self.model.calibration_ended(Err("its thread ended without a result: the log file has why".to_string()));
            } else {
                self.model.ended(Err("the session's thread ended without a result: the log file has why".to_string()));
            }
            ended = true;
        }
        // Raised at setup's selection, normal again as the session ends: here, as the model changes it, and before the log file closes
        self.follow_priority();
        if ended {
            if self.settings.notify {
                sound(self.model.error_count.as_ref().is_some_and(|count| count.alarm));
            }
            // The lines logged since the waiter took the batch's, the priority's line among them: this run's Log tab's, not the next's
            let (log, dropped) = feeds.log.take();
            self.model.logged(log, dropped);
            // The run's log file is done with, and the logger's side waits for the next run (TODO 109)
            tmr::logfile::close();
            self.log = Some(feeds.log);
        } else {
            listen(feeds, self.model.progress().cloned(), self.priority, context);
        }
    }

    /// This thread, the UI's, at the priority the model says (`Model::window_priority`), when that changed: the workers run at High, by
    /// default, on every CPU a run takes, and with every CPU taken a window at Normal gets time only from Windows' starvation boost. Once a
    /// change, whether it took: a failure goes to the errors list, and is not tried again until the priority changes.
    fn follow_priority(&mut self) {
        let wanted = self.model.window_priority();
        if wanted == self.priority {
            return;
        }
        // While a run is on, its outcome is a line of the run's output, the Log tab's and the log file's, beside the Workers line (TODO 7);
        // back to Normal as the session ends, only the debug log says so
        let line = match wanted.apply() {
            Ok(()) => format!(
                "🪟  Window: {wanted} priority ({}), with its feed's thread (window-priority=, the workers' by default)",
                wanted.win32_name()
            ),
            Err(e) => {
                self.model.errors.push(format!("The window's thread at {wanted} priority: {e}"));
                format!("⚠️  Window: {wanted} priority ({}) not set (window-priority=): {e}", wanted.win32_name())
            }
        };
        if self.model.session_over() {
            log::debug!("{line}");
        } else {
            tmr::logfile::write_text(&format!("{line}\n"));
            self.model.log.push(line);
        }
        self.priority = wanted;
    }

    /// Writes `window.json`, or says in the errors list why it couldn't
    fn save_settings(&mut self) {
        self.keep_placement();
        if let Err(e) = self.settings.save() {
            self.model.errors.push(format!("window.json: {e}"));
        }
    }

    /// Where the window is now, as `placement.rs` followed it, into the settings, for a save; not followed, they keep what they had
    fn keep_placement(&mut self) {
        if let Some(placement) = placement::last() {
            (self.settings.position, self.settings.maximized) = (Some(placement.position), placement.maximized);
        }
    }

    /// The Compare tab's two files, compared on a pool thread once both are chosen, each pair once
    fn compare(&mut self, context: &ComponentContext<Self>) {
        if let Some(pair) = self.model.compare.next() {
            context.spawn_background(move |_| {
                let result = crate::model::compare(pair.clone());
                Message::Compared(pair, result)
            });
        }
    }

    /// An item of the hamburger's panel: a file or a folder to open, on a thread of its own, or About
    fn menu(&mut self, item: crate::lang::Menu, context: &ComponentContext<Self>) {
        let path = match item {
            lang::Menu::Log => self.model.log_path.clone(),
            lang::Menu::Logs => Some(tmr::home::logs()),
            lang::Menu::Results => self.model.results_path().map(PathBuf::from),
            lang::Menu::ResultsFolder => Some(tmr::home::results()),
            lang::Menu::Calibrate if self.model.takes_parameters() && !self.model.calibrating() => {
                self.calibrate(context);
                None
            }
            lang::Menu::Readings if self.model.takes_parameters() && !self.model.calibrating() && !self.getting_readings => {
                self.get_readings(context);
                None
            }
            lang::Menu::Sanitize | lang::Menu::SanitizeSingle => {
                let single = item == lang::Menu::SanitizeSingle;
                let ask = Open {
                    title: if single {
                        "Sanitize one file: a results file or a log, alone, its pair not looked for"
                    } else {
                        "Sanitize: a run's results file or its log, either names the pair"
                    },
                    filter: ("TMR results and logs", "*.json;*.log"),
                    folder: tmr::home::results(),
                    multiple: false,
                };
                let owner = dialogs::owner();
                context.spawn_background(move |_| Message::SanitizeChosen(single, dialogs::open(owner, ask)));
                None
            }
            lang::Menu::Settings => {
                self.settings_open = true;
                None
            }
            lang::Menu::About => {
                self.about = true;
                None
            }
            lang::Menu::Calibrate | lang::Menu::Readings => None,
        };
        if let Some(path) = path {
            context.spawn_background(move |_| Message::Opened(dialogs::shell_open(path)));
        }
    }
}

/// The Open dialog's filter for results files
const RESULTS_FILES: (&str, &str) = ("TMR results", "*.json");

/// The window's size and place, as it closes, into `window.json` (TODO 109): the next window opens as this one was
impl Drop for Window {
    fn drop(&mut self) {
        self.keep_placement();
        let _ = self.settings.save();
        // Closed on a run: the last note is this moment, not the one a couple of seconds ago, and it stays open for the next launch
        if self.journal_lock.is_some() && (matches!(self.model.status, Status::Running | Status::Stopping) || self.model.calibrating()) {
            crate::journal::save(&self.note());
        }
    }
}

impl Component for Window {
    type Input = Vec<String>;
    type Message = Message;

    fn create(args: &Vec<String>, context: &ComponentContext<Self>) -> Self {
        crate::window_opened();
        let (mut settings, palettes, problems) = Settings::load();
        settings.allow_admin = tmr::home::window_consent();
        let start = args.iter().any(|arg| arg == START) || settings.start_on_open;
        let mut args: Vec<String> = args.iter().filter(|arg| *arg != START).cloned().collect();
        let launch_allows_admin = tmr::session::take_admin_flag(&mut args);
        // Given no parameters, the last run's (TODO 109)
        if args.len() <= 1
            && let Some(parameters) = &settings.parameters
        {
            args.extend(crate::model::args_of(parameters).into_iter().filter(|arg| arg != "--allow-admin"));
        }
        let mut model = Model::new(&args);
        // The cache calibration a run finds, from tmr-cfg.json (TODO 110): the info line says it before Start
        model.stored_calibration = Some(crate::model::stored_calibration_text(tmr::session::stored_calibration().as_ref()));
        // Before Start there is no log file to take them, and maybe no console
        model.errors.extend(problems.into_iter().map(|problem| format!("window.json: {problem}")));
        let (width, dark) = (settings.size.map_or(WIDTH, |(width, _)| width), settings.theme == Theme::Dark);
        let priority = ThreadPriority::Normal;
        let journal_lock = crate::journal::claim();
        let unfinished = journal_lock.as_ref().and_then(|_| crate::journal::unfinished());
        let mut window = Self {
            model,
            log: None,
            tab: Tab::Run,
            about: false,
            settings_open: false,
            launch_allows_admin,
            admin_answer: None,
            sanitized: None,
            settings,
            dark,
            width,
            palettes,
            priority,
            getting_readings: false,
            readings_line: String::new(),
            journal_lock,
            last_journal: None,
            last_readings: None,
            unfinished,
            update: crate::update::Status::Idle,
            menu_open: false,
        };
        window.sample_readings(context);
        if start && window.unfinished.is_none() {
            window.start(context);
        }
        window
    }

    fn update(&mut self, message: Message, context: &ComponentContext<Self>) {
        match message {
            Message::Feed(batch) => self.take(*batch, context),
            // One session a process: the engine's run settings are set once (TODO 29), so another run is another process (Run again)
            Message::Start if self.model.status == Status::Ready && !self.model.calibrating() => {
                // The parameters the window opens with next time, when given none
                self.settings.parameters = Some(self.model.saved_parameters());
                self.save_settings();
                self.start(context);
            }
            Message::Start => {}
            // In this window (TODO 109): a new run's model, the Compare tab's files kept, and the engine readied as a new process's
            Message::RunAgain if self.model.session_over() && !self.model.calibrating() => {
                self.model = self.model.again();
                // Read again, not carried over: the run, or the window's Calibrate after it, may have saved a new one
                self.model.stored_calibration = Some(crate::model::stored_calibration_text(tmr::session::stored_calibration().as_ref()));
                self.settings.parameters = Some(self.model.saved_parameters());
                self.save_settings();
                self.start(context);
            }
            Message::RunAgain => {}
            Message::Stop => {
                if self.model.status == Status::Running || self.model.calibrating() {
                    tmr::request_stop();
                    if self.model.status == Status::Running {
                        self.model.stop();
                    } else {
                        self.model.position = "Stopping the calibration…".to_string();
                    }
                }
            }
            Message::Preset(key, value) if self.model.takes_parameters() => self.model.set_setting(key, value),
            Message::Preset(_, _) => {}
            Message::Notify(on) => {
                self.settings.notify = on;
                self.save_settings();
            }
            Message::SaveAnswer(result) => self.model.answer_save(result == ContentDialogResult::Secondary),
            Message::Readings(result) => {
                self.getting_readings = false;
                match result {
                    Ok(text) => {
                        for line in text.lines() {
                            self.model.log.push(line.to_string());
                        }
                        self.model.position = "Timings tools are ready. See Log for setup instructions.".to_string();
                        let folder = tmr::companions_dir();
                        context.spawn_background(move |_| Message::Opened(dialogs::shell_open(folder)));
                    }
                    Err(e) => {
                        self.model.log.push(e.clone());
                        self.model.errors.push(format!("Timings tools: {e}"));
                        self.model.position = "The timings tools were not downloaded".to_string();
                    }
                }
                self.sample_readings(context);
            }
            Message::ReadingsLine(line) => self.readings_line = line,
            Message::Language(Some(index)) => {
                self.settings.language = crate::lang::Language::at(index);
                self.save_settings();
            }
            Message::Language(None) => {}
            Message::CheckUpdates if !matches!(self.update, crate::update::Status::Checking) => {
                self.update = crate::update::Status::Checking;
                context.spawn_background(|_| Message::UpdateChecked(crate::update::fetch()));
            }
            Message::CheckUpdates => {}
            Message::UpdateChecked(result) => {
                self.update = match result {
                    Ok(release) => crate::update::judge(tmr::BUILD.version, &release),
                    Err(why) => crate::update::Status::Failed(why),
                };
            }
            Message::OpenUrl(url) => {
                context.spawn_background(move |_| Message::Opened(dialogs::shell_open(PathBuf::from(url))));
            }
            Message::OpenRelease => {
                if let crate::update::Status::Available { url, .. } = &self.update {
                    let url = url.clone();
                    context.spawn_background(move |_| Message::Opened(dialogs::shell_open(PathBuf::from(url))));
                }
            }
            Message::Journaled => {}
            Message::UnfinishedClosed(result) => {
                // None: a new run hid the dialog. Closing the file here would mark that run finished.
                if let Some(note) = self.unfinished.take() {
                    crate::journal::close();
                    if result == ContentDialogResult::Primary
                        && let Some(path) = note.results
                    {
                        context.spawn_background(move |_| Message::Opened(dialogs::shell_open(PathBuf::from(path))));
                    }
                }
            }
            Message::Args(text) => self.model.args = text,
            Message::LargePages(result) => self.model.answer_large_pages(match result {
                ContentDialogResult::Primary => DialogButton::Primary,
                ContentDialogResult::Secondary => DialogButton::Secondary,
                _ => DialogButton::Close,
            }),
            Message::LargePageTarget(chosen) => self.model.choose_target(chosen),
            Message::LargePageBroad(result) => self.model.confirm_broad(result == ContentDialogResult::Secondary),
            Message::Dimm(choice) => self.model.choose_dimm(&choice),
            Message::Open(which) => self.model.open(which),
            Message::Close => self.model.opened = None,
            // Only while the parameters box takes one
            Message::LoadConfig if self.model.takes_parameters() => {
                let ask = Open {
                    title: self.t(Phrase::LoadConfigTitle),
                    filter: ("TMR and TM5 configs", "*.cfg;*.json"),
                    folder: tmr::home::configs(),
                    multiple: false,
                };
                let owner = dialogs::owner();
                context.spawn_background(move |_| Message::ConfigChosen(dialogs::open(owner, ask)));
            }
            Message::LoadConfig => {}
            Message::ConfigChosen(Ok(paths)) => {
                if let Some(path) = paths.first()
                    && self.model.takes_parameters()
                {
                    self.model.set_config(path);
                }
            }
            Message::ConfigChosen(Err(e)) => self.model.errors.push(format!("Load config: the Open dialog failed: {e}")),
            Message::Choose(slot) => {
                let title = match slot {
                    Slot::Baseline => self.t(Phrase::CompareBaseline),
                    Slot::Compared => self.t(Phrase::CompareOther),
                };
                let ask = Open { title, filter: RESULTS_FILES, folder: tmr::home::results(), multiple: false };
                let owner = dialogs::owner();
                context.spawn_background(move |_| Message::Chosen(slot, dialogs::open(owner, ask)));
            }
            Message::Chosen(slot, Ok(paths)) => {
                if let Some(path) = paths.into_iter().next() {
                    self.model.compare.set(slot, path);
                    self.compare(context);
                }
            }
            Message::ChooseBoth => {
                let ask =
                    Open { title: self.t(Phrase::CompareBothTitle), filter: RESULTS_FILES, folder: tmr::home::results(), multiple: true };
                let owner = dialogs::owner();
                context.spawn_background(move |_| Message::ChosenBoth(dialogs::open(owner, ask)));
            }
            Message::ChosenBoth(Ok(paths)) => match self.model.compare.set_both(paths) {
                Ok(()) => self.compare(context),
                Err(e) => self.model.errors.push(format!("Compare: {e}")),
            },
            Message::Chosen(_, Err(e)) | Message::ChosenBoth(Err(e)) => {
                self.model.errors.push(format!("Compare: the Open dialog failed: {e}"))
            }
            Message::ThisRun(slot) => {
                if let Some(path) = self.model.results_path().map(PathBuf::from) {
                    self.model.compare.set(slot, path);
                    self.compare(context);
                }
            }
            Message::Swap => {
                self.model.compare.swap();
                self.compare(context);
            }
            Message::Compared(pair, result) => self.model.compare.finished(pair, result),
            Message::Chart => {
                self.settings.chart = !self.settings.chart;
                self.save_settings();
            }
            Message::ToggleMenu => self.menu_open = !self.menu_open,
            Message::MenuPick(item) => {
                self.menu_open = false;
                self.menu(item, context);
            }
            Message::Opened(Ok(())) => {}
            Message::Opened(Err(e)) => self.model.errors.push(e),
            // Copies of the pair the file names, or of the file alone, beside it, on a pool thread: reading and scrubbing a long run's
            // files takes a moment
            Message::SanitizeChosen(single, Ok(paths)) => {
                if let Some(path) = paths.into_iter().next() {
                    let file = path.display().to_string();
                    let sanitize = if single { tmr::sanitize::sanitize_single } else { tmr::sanitize::sanitize };
                    context.spawn_background(move |_| Message::Sanitized(sanitize(&file).map(|done| done.text())));
                }
            }
            Message::SanitizeChosen(_, Err(e)) => self.model.errors.push(format!("Sanitize: the Open dialog failed: {e}")),
            Message::Sanitized(outcome) => self.sanitized = Some(outcome),
            Message::SanitizedClosed => self.sanitized = None,
            Message::AboutClosed => self.about = false,
            Message::SettingsClosed => self.settings_open = false,
            Message::AdminAnswer(result) => {
                if let Some(reply) = self.admin_answer.take() {
                    let yes = result == ContentDialogResult::Secondary;
                    self.launch_allows_admin |= yes;
                    let _ = reply.send(yes);
                    if !yes {
                        crate::session_ended(&Err("Administrator run declined".into()));
                        let _ = context.window().request_close();
                    }
                }
            }
            Message::AllowAdmin(on) => {
                if tmr::home::elevated() {
                    if let Err(e) = tmr::home::set_window_consent(on) {
                        self.model.errors.push(e);
                    }
                    self.settings.allow_admin = tmr::home::window_consent();
                }
            }
            Message::StartOnOpen(on) => {
                self.settings.start_on_open = on;
                self.save_settings();
            }
            Message::KeepSerials(on) => {
                self.settings.keep_serials = on;
                self.save_settings();
            }
            Message::Theme(Some(index)) => {
                self.settings.theme = crate::settings::Theme::at(index);
                self.save_settings();
            }
            Message::Theme(None) => {}
            Message::Tab(label) => {
                if let Some(tab) = label.as_deref().and_then(Tab::from_label) {
                    self.tab = tab;
                }
            }
            Message::Scheme(scheme) => {
                self.dark = match self.settings.theme {
                    Theme::System => scheme == ColorScheme::Dark,
                    Theme::Light => false,
                    Theme::Dark => true,
                }
            }
            // Saved as the window closes (`Drop`): maximized or minimized, the size it had before, which it restores to and opens at
            Message::Size(size) => {
                self.width = size.width;
                // The first: the window is there now, so it goes where it last closed, and is followed from then on (`placement.rs`)
                let (position, maximized) = (self.settings.position, self.settings.maximized);
                placement::attach(position.map(|position| Placement { position, maximized }));
                if placement::restored() {
                    self.settings.size = Some((size.width, size.height));
                }
            }
        }
    }

    fn view(&self, _args: &Vec<String>, context: &mut ViewContext<Self>) -> View {
        context.window_title(format!("Test Memory R (TMR) v{}", env!("CARGO_PKG_VERSION")));
        let (width, height) = self.settings.size.unwrap_or((WIDTH, HEIGHT));
        let theme = match self.settings.theme {
            Theme::System => WindowTheme::System,
            Theme::Light => WindowTheme::Light,
            Theme::Dark => WindowTheme::Dark,
        };
        let visuals = WindowVisuals::new()
            .backdrop(WindowBackdrop::Mica)
            .theme(theme)
            .client_size(width, height)
            .constraints(WindowConstraints { min_width: Some(720.0), min_height: Some(680.0), ..Default::default() });
        context.window_visuals(match icon_path() {
            Some(path) => visuals.icon(path),
            None => visuals,
        });
        context.on_color_scheme(context.callback(Message::Scheme));
        context.on_window_size(context.callback(Message::Size));

        Grid::new()
            .margin(Thickness::new(PAGE_MARGIN_X, 14.0, PAGE_MARGIN_X, 16.0))
            .row_spacing(12.0)
            .rows([
                GridLength::Auto, // header: title, badge, timer, system info; info line
                GridLength::Auto, // the command bar: parameters, Load config, the run button, the menu
                GridLength::Auto, // tabs
                GridLength::STAR, // the tab chosen
            ])
            .children((
                self.header(0, context),
                self.command_bar(1, context),
                self.tab_bar(2, context),
                self.tab_view(3, context),
                self.large_page_dialog(context),
                self.broad_dialog(context),
                self.admin_dialog(context),
                self.about_dialog(context),
                self.settings_dialog(context),
                self.save_dialog(context),
                self.sanitized_dialog(context),
                self.unfinished_dialog(context),
            ))
    }
}

// ---- view pieces -------------------------------------------------------------------------

impl Window {
    /// The badge's fill. Green is a pass only: a running suite with errors is not a pass, and a clean run in progress is the accent, not
    /// the success colour. The count itself sits beside the badge, so it stays in sight on every tab.
    fn status_fill(&self) -> Color {
        let p = self.palette();
        if self.model.calibrating() {
            return p.accent;
        }
        match self.model.status {
            Status::Passed => p.success,
            Status::Failed => p.critical,
            Status::Running if self.model.error_count.as_ref().is_some_and(|count| count.alarm) => p.critical,
            Status::Running => p.accent,
            Status::Ready | Status::Stopping | Status::Interrupted | Status::NotRun => p.neutral,
        }
    }

    /// Three rows on the left: the title, the badge and the runtime; the run's errors, and under the runtime the time its limit leaves;
    /// the info line. The system panel is on the right, right-justified like the runtime.
    fn header(&self, row: i32, context: &ViewContext<Self>) -> View {
        let p = self.palette();
        let info = self.model.info_line();
        let status = if self.model.calibrating() {
            self.t(Phrase::Calibrating)
        } else {
            lang::status(self.settings.language, self.model.status as usize)
        };
        // The badge and the clock are one group: the clock's digits share the status word's centre, not the title's. A tooltip wraps its
        // target, so the clock's alignment is on the stack, not on the text the tooltip wraps.
        let badge = Border::new()
            .vertical_alignment(VerticalAlignment::Center)
            .corner_radius(12.0)
            .padding(Thickness::new(10.0, 4.0, 10.0, 4.0))
            .background(brush(self.status_fill()))
            .content(
                TextBlock::new()
                    .text(status)
                    .font_size(12.0)
                    .font_weight(FontWeight::SEMI_BOLD)
                    .foreground(brush(p.on_status))
                    .vertical_alignment(VerticalAlignment::Center),
            );
        let clock = TextBlock::new()
            .text(self.model.runtime_line())
            .font_size(16.0)
            .font_weight(FontWeight::SEMI_BOLD)
            .foreground(brush(p.text_primary))
            .vertical_alignment(VerticalAlignment::Center)
            .tooltip(self.t(Phrase::RuntimeTip));
        let status_line = StackPanel::new()
            .orientation(Orientation::Horizontal)
            .spacing(8.0)
            .vertical_alignment(VerticalAlignment::Center)
            .grid_column(1)
            .children((badge, self.error_badge(), clock));
        let top = Grid::new().grid_row(0).column_spacing(12.0).columns([GridLength::Auto, GridLength::Auto, GridLength::STAR]).children((
            TextBlock::new()
                .text("Test Memory R")
                .font_size(28.0)
                .font_weight(FontWeight::SEMI_BOLD)
                .foreground(brush(p.text_primary))
                .vertical_alignment(VerticalAlignment::Center)
                .grid_column(0),
            status_line,
        ));
        // The time left on the right, under the runtime, from the suite's start. The run's errors are with the other figures that change as
        // it runs, beside the cycle (`position`, TODO 109).
        let mut counts: Vec<(u64, View)> = Vec::new();
        if let Some(time_left) = &self.model.time_left {
            let text = TextBlock::new()
                .text(time_left.clone())
                .foreground(brush(p.text_secondary))
                .text_trimming(TextTrimming::CharacterEllipsis)
                .horizontal_alignment(HorizontalAlignment::Right);
            let about = format!("{time_left}\nThe suite's time limit: the cycle running when it is reached finishes");
            counts.push((1, text.grid_column(1).tooltip(about)));
        }
        let left = Grid::new().grid_column(0).vertical_alignment(VerticalAlignment::Center).row_spacing(2.0).rows([GridLength::Auto; 3]);
        let left = left.children((
            top,
            Grid::new().grid_row(1).column_spacing(12.0).columns([GridLength::Auto, GridLength::STAR]).keyed_children(counts),
            TextBlock::new()
                .text(info.clone())
                .foreground(brush(p.text_secondary))
                .text_trimming(TextTrimming::CharacterEllipsis)
                .grid_row(2)
                .tooltip(info),
        ));
        Grid::new()
            .grid_row(row)
            .column_spacing(14.0)
            .columns([
                GridLength::STAR, // title, errors, info line
                GridLength::Auto, // divider
                GridLength::Auto, // system info
            ])
            .children((
                left,
                Border::new().grid_column(1).width(1.0).margin(Thickness::xy(0.0, 4.0)).background(brush(p.chart_grid)),
                Border::new().grid_column(2).vertical_alignment(VerticalAlignment::Center).content(self.system_info(context)),
            ))
    }

    /// Four lines, every one right-justified: the CPU, the caches, the memory's total and its DIMMs, and the DIMM selector with the chosen
    /// DIMM's details. Empty until the session's setup has detected them.
    fn system_info(&self, context: &ViewContext<Self>) -> View {
        let p = self.palette();
        let (Some(s), Some(memory)) = (&self.model.system, self.model.memory_line()) else {
            return TextBlock::new()
                .text(self.t(Phrase::SystemInfo))
                .foreground(brush(p.text_secondary))
                .horizontal_alignment(HorizontalAlignment::Right)
                .into();
        };
        let secondary =
            |text: String| TextBlock::new().text(text).foreground(brush(p.text_secondary)).vertical_alignment(VerticalAlignment::Center);
        let line = || StackPanel::new().orientation(Orientation::Horizontal).horizontal_alignment(HorizontalAlignment::Right).spacing(6.0);
        let mut lines: Vec<(u64, View)> = vec![
            (
                0,
                TextBlock::new()
                    .text(s.cpu.clone())
                    .font_size(13.0)
                    .font_weight(FontWeight::SEMI_BOLD)
                    .foreground(brush(p.text_primary))
                    .max_width(380.0)
                    .text_trimming(TextTrimming::CharacterEllipsis)
                    .horizontal_alignment(HorizontalAlignment::Right)
                    .tooltip(s.cpu_detail.clone()),
            ),
            (1, secondary(s.cache_line.clone()).horizontal_alignment(HorizontalAlignment::Right).tooltip(s.cache_detail.clone())),
        ];
        let count = |dimms: String| secondary(dimms).tooltip("The memory modules SMBIOS lists");
        let memory_line = match memory.total {
            Some(total) => line().children((secondary(total).tooltip(memory.about), secondary("·".to_string()), count(memory.dimms))),
            None => line().children((count(memory.dimms),)),
        };
        lines.push((2, memory_line));
        if let Some(dimm) = s.chosen() {
            // Both cut short past a width, so a long locator or part number leaves the header's left side its room; the tooltip has it all
            let name = TextBlock::new()
                .text(dimm.name.clone())
                .foreground(brush(p.text_primary))
                .max_width(120.0)
                .text_trimming(TextTrimming::CharacterEllipsis)
                .vertical_alignment(VerticalAlignment::Center);
            // A menu of the DIMMs, each with its details, on a subtle Button with a ▾: DropDownButton takes a menu too, but 0.100.0
            // gives it no subtle style, and its border would make the line taller than text. The negative margin cancels the button's
            // padding for the same reason, and keeps the line's spacing to the details. With one DIMM there is nothing to choose: its name
            // alone. The menu gives back the item chosen by its label, which the DIMMs' names keep apart (`sysinfo::named_apart`).
            let selector: View = if s.choosable() {
                let choices = s.dimms.iter().enumerate().map(|(i, dimm)| MenuItem::item(i, dimm.choice()));
                let chevron = TextBlock::new().text("▾").foreground(brush(p.text_primary)).vertical_alignment(VerticalAlignment::Center);
                Button::new()
                    .style(ButtonStyle::Subtle)
                    .margin(Thickness::new(0.0, -5.0, -11.0, -6.0))
                    .vertical_alignment(VerticalAlignment::Center)
                    .content(StackPanel::new().orientation(Orientation::Horizontal).spacing(4.0).children((name, chevron)))
                    .menu(Menu::new(choices, context.callback(Message::Dimm)))
            } else {
                name.into()
            };
            let details =
                secondary(dimm.details.clone()).max_width(280.0).text_trimming(TextTrimming::CharacterEllipsis).tooltip(dimm.choice());
            lines.push((3, line().children((selector, details))));
        }
        StackPanel::new().spacing(2.0).horizontal_alignment(HorizontalAlignment::Right).keyed_children(lines)
    }

    /// Where the run is, as the console's ticker has it, its two lines together (TODO 109): the cycle, the run's errors beside it, in the
    /// error colour once there are any; under it the test, its stage and figures beside it. Then the progress bar. A line's left part keeps
    /// its width beside its right, which gives way when the window is narrow; alone, it takes the line, and is cut short past it, its
    /// tooltip having it all.
    fn position(&self, row: i32) -> View {
        let p = self.palette();
        let paused = matches!(self.model.status, Status::Stopping | Status::Interrupted);
        let line = |left: TextBlock, about: String, right: Option<View>| -> View {
            let left = left.text_trimming(TextTrimming::CharacterEllipsis).vertical_alignment(VerticalAlignment::Center);
            let mut parts: Vec<(u64, View)> = vec![(0, left.grid_column(0).tooltip(about))];
            let columns = match right {
                Some(right) => {
                    parts.push((1, right));
                    vec![GridLength::Auto, GridLength::STAR]
                }
                None => vec![GridLength::STAR],
            };
            Grid::new().column_spacing(16.0).columns(columns).keyed_children(parts)
        };
        let right = |text: TextBlock, about: String| -> View {
            let text = text.text_trimming(TextTrimming::CharacterEllipsis).horizontal_alignment(HorizontalAlignment::Right);
            text.vertical_alignment(VerticalAlignment::Center).grid_column(1).tooltip(about)
        };
        let position = &self.model.position;
        let errors = self.model.error_count.as_ref().map(|count| {
            let (weight, colour) = if count.alarm { (FontWeight::SEMI_BOLD, p.error) } else { (FontWeight::NORMAL, p.text_secondary) };
            let about = format!(
                "{}\nThe run's errors, the running test's so far included: the tests' bad words, the seal's, and the hardware errors WHEA \
                 reported, C corrected and UC uncorrected. Any of them fails the run.",
                count.text
            );
            right(TextBlock::new().text(count.text.clone()).font_weight(weight).foreground(brush(colour)), about)
        });
        let cycle =
            TextBlock::new().text(position.clone()).font_size(16.0).font_weight(FontWeight::SEMI_BOLD).foreground(brush(p.text_primary));
        let mut lines: Vec<(u64, View)> = vec![(0, line(cycle, position.clone(), errors))];
        if let Some(test) = &self.model.test_position {
            let figures = self
                .model
                .live
                .as_ref()
                .map(|live| right(TextBlock::new().text(live.clone()).foreground(brush(p.text_secondary)), live.clone()));
            lines.push((1, line(TextBlock::new().text(test.clone()).foreground(brush(p.text_primary)), test.clone(), figures)));
        }
        lines.push((2, progress_bar(&self.model.bar, paused, self.width - 2.0 * PAGE_MARGIN_X, p)));
        StackPanel::new().grid_row(row).spacing(6.0).keyed_children(lines)
    }

    /// The command bar (TODO 109), every control in one place, at the top: the parameters box, as wide as a run's parameters want and no
    /// wider, Load config and the run button beside it, and at the right the `…` menu (the log, the results, their folders, About). The
    /// box and Load config take parameters before Start, and once the session is over, for Run again.
    fn command_bar(&self, row: i32, context: &ViewContext<Self>) -> View {
        let takes = self.model.takes_parameters();
        let box_width = (self.width - 2.0 * PAGE_MARGIN_X - COMMANDS_WIDTH).clamp(200.0, PARAMETERS_MAX);
        let parameters = TextBox::new()
            .text(self.model.args.clone())
            .placeholder_text(self.t(Phrase::Parameters))
            .is_enabled(takes)
            .width(box_width)
            .on_text_changed(context.callback(Message::Args))
            .grid_column(0)
            .automation_name(self.t(Phrase::Parameters))
            .tooltip(self.t(Phrase::ParametersTip));
        // A config's path into the box, as `config=`, from the home's configs\ (TODO 109)
        let load = Button::new()
            .is_enabled(takes)
            .grid_column(1)
            .on_click(context.callback(|()| Message::LoadConfig))
            .content(self.t(Phrase::LoadConfig));
        // A hamburger, not a flyout: the panel opens downward in the page, and the rest moves down with it. A flyout at the top of the
        // window opens upward when the screen has no room below.
        let glyph = if self.menu_open { Symbol::Cancel } else { Symbol::GlobalNavigationButton };
        let more = Button::new()
            .grid_column(4)
            .style(ButtonStyle::Subtle)
            .width(40.0)
            .vertical_alignment(VerticalAlignment::Center)
            .on_click(context.callback(|()| Message::ToggleMenu))
            .automation_name(self.t(Phrase::MenuButton))
            .content(SymbolIcon::new().symbol(glyph))
            .tooltip(self.t(Phrase::MenuButton));
        let bar = Grid::new()
            .column_spacing(8.0)
            .columns([GridLength::Auto, GridLength::Auto, GridLength::Auto, GridLength::STAR, GridLength::Auto])
            .children((parameters, load, self.run_button(2, context), more));
        let mut parts: Vec<(u64, View)> = vec![(0, bar)];
        if self.menu_open {
            parts.push((1, self.menu_panel(context)));
        }
        parts.push((2, self.presets(context)));
        parts.push((3, self.caution()));
        StackPanel::new().grid_row(row).spacing(8.0).keyed_children(parts)
    }

    /// The hamburger's panel, in the page under the command bar. Three groups, so a log, a calibration and Settings are not one flat list.
    fn menu_panel(&self, context: &ViewContext<Self>) -> View {
        let p = self.palette();
        let ready = self.model.takes_parameters() && !self.model.calibrating();
        let group = |column: i32, title: Phrase, items: &[(lang::Menu, bool)]| {
            let mut rows: Vec<(u64, View)> = vec![(
                0,
                TextBlock::new()
                    .text(self.t(title))
                    .font_size(12.0)
                    .font_weight(FontWeight::SEMI_BOLD)
                    .foreground(brush(p.text_secondary))
                    .into(),
            )];
            for (i, (item, on)) in items.iter().enumerate() {
                let item = *item;
                rows.push((
                    1 + i as u64,
                    Button::new()
                        .is_enabled(*on)
                        .horizontal_alignment(HorizontalAlignment::Stretch)
                        .on_click(context.callback(move |()| Message::MenuPick(item)))
                        .content(lang::menu_label(self.settings.language, item)),
                ));
            }
            StackPanel::new().grid_column(column).spacing(6.0).keyed_children(rows)
        };
        let files = group(0, Phrase::MenuFiles, &[
            (lang::Menu::Log, self.model.log_path.is_some()),
            (lang::Menu::Logs, true),
            (lang::Menu::Results, self.model.results_path().is_some()),
            (lang::Menu::ResultsFolder, true),
            (lang::Menu::Sanitize, true),
            (lang::Menu::SanitizeSingle, true),
        ]);
        let tools = group(1, Phrase::MenuTools, &[
            (lang::Menu::Calibrate, ready),
            (lang::Menu::Readings, ready && !self.getting_readings && !tmr::home::elevated()),
        ]);
        let app = group(2, Phrase::MenuApp, &[(lang::Menu::Settings, true), (lang::Menu::About, true)]);
        Border::new()
            .background(brush(p.card_background))
            .border_brush(brush(p.card_stroke))
            .border_thickness(1.0)
            .corner_radius(8.0)
            .padding(Thickness::new(12.0, 10.0, 12.0, 12.0))
            .content(
                Grid::new()
                    .columns([GridLength::Star(1.0), GridLength::Star(1.0), GridLength::Star(1.0)])
                    .column_spacing(16.0)
                    .children((files, tools, app)),
            )
    }

    /// Plan, memory, cycles and errors, written into the same parameters box. The box stays the run's only parser.
    fn presets(&self, context: &ViewContext<Self>) -> View {
        let on = self.model.takes_parameters();
        let menu = |label: &str, key: &'static str, values: &[(&'static str, &'static str)]| {
            let items = values.iter().enumerate().map(|(i, (value, _))| MenuItem::item(i, *value)).collect::<Vec<_>>();
            let picked = values.to_vec();
            Button::new().is_enabled(on).content(label).menu(Menu::new(
                items,
                context.callback(move |which: String| {
                    picked
                        .iter()
                        .find(|(value, _)| *value == which)
                        .map_or(Message::Preset(key, ""), |(_, stored)| Message::Preset(key, stored))
                }),
            ))
        };
        StackPanel::new().orientation(Orientation::Horizontal).spacing(8.0).children((
            menu(self.t(Phrase::Plan), "plan", &[
                ("stability", "stability"),
                ("quick", "quick"),
                ("perf", "perf"),
                ("widths", "widths"),
                ("all", "all"),
            ]),
            menu(self.t(Phrase::Memory), "memory", &[
                ("50% reserve", "50%"),
                ("80% reserve", "80%"),
                ("90% reserve", "90%"),
                ("2 GiB reserve", "2048MB"),
                ("8 GiB reserve", "8GiB"),
            ]),
            menu(self.t(Phrase::Cycles), "cycles", &[("1", "1"), ("3", "3"), ("5", "5"), ("10", "10")]),
            menu(self.t(Phrase::Errors), "errors", &[
                ("log", "log"),
                ("halt", "halt"),
                ("halt:1", "halt:1"),
                ("halt:10", "halt:10"),
                ("halt:100", "halt:100"),
            ]),
        ))
    }

    /// Always visible: an unstable overclock can freeze the machine, and how much RAM is free before Start.
    fn caution(&self) -> View {
        let p = self.palette();
        let ram = match tmr::memory_status() {
            Some(status) => lang::ram(
                self.settings.language,
                status.avail_phys as f64 / (1024.0 * 1024.0 * 1024.0),
                status.total_phys as f64 / (1024.0 * 1024.0 * 1024.0),
            ),
            None => self.t(Phrase::NoRam).to_string(),
        };
        let extra = if self.readings_line.is_empty() {
            format!(" · {}", self.t(Phrase::NoTimings))
        } else {
            format!(" · {}", self.readings_line)
        };
        TextBlock::new()
            .text(format!("{} {ram}{extra}", self.t(Phrase::Caution)))
            .foreground(brush(p.text_secondary))
            .text_wrapping(TextWrapping::Wrap)
            .into()
    }

    /// The run's error count, beside the badge, so leaving the Run tab does not hide a failing soak.
    fn error_badge(&self) -> TextBlock {
        let p = self.palette();
        let (text, colour) = match &self.model.error_count {
            Some(count) => (count.text.clone(), if count.alarm { p.error } else { p.text_secondary }),
            None => (String::new(), p.text_secondary),
        };
        TextBlock::new().text(text).foreground(brush(colour)).vertical_alignment(VerticalAlignment::Center)
    }

    /// The run button (TODO 109): Start, then Stop in the error colour, Stopping… while the step ends, Finishing… while the session saves
    /// its results, then Run again. One button, its width fixed, so the bar doesn't move as it changes.
    fn run_button(&self, column: i32, context: &ViewContext<Self>) -> View {
        let p = self.palette();
        let button = Button::new().width(RUN_BUTTON).grid_column(column);
        match self.model.status {
            // A calibration of the window's own first (TODO 110)
            _ if self.model.calibrating() => {
                button.resource_overrides(stop_colours(p)).on_click(context.callback(|()| Message::Stop)).content(self.t(Phrase::Stop))
            }
            Status::Ready => {
                button.style(ButtonStyle::Accent).on_click(context.callback(|()| Message::Start)).content(self.t(Phrase::Start))
            }
            Status::Running => {
                button.resource_overrides(stop_colours(p)).on_click(context.callback(|()| Message::Stop)).content(self.t(Phrase::Stop))
            }
            Status::Stopping => button.is_enabled(false).content(self.t(Phrase::Stopping)),
            _ if self.model.session_over() => {
                button.style(ButtonStyle::Accent).on_click(context.callback(|()| Message::RunAgain)).content(self.t(Phrase::RunAgain))
            }
            _ => button.is_enabled(false).content(self.t(Phrase::Finishing)),
        }
    }

    /// The tabs (TODO 109): Run, the window's own view, then the log, the console's startup sections and Compare. A SelectorBar: 0.100.0's
    /// NavigationView can clear its selection on a stale event (#5210), and a Pivot would build every tab's view each redraw.
    fn tab_bar(&self, row: i32, context: &ViewContext<Self>) -> View {
        let items = Tab::ALL.iter().enumerate().map(|(i, &tab)| {
            (i as u64, View::from(SelectorBarItem::new().text(tab.label(self.settings.language)).is_selected(tab == self.tab)))
        });
        SelectorBar::new()
            .grid_row(row)
            .on_selected_text_changed(context.callback(Message::Tab))
            .collection_slot(SelectorBarSlot::Items, items)
    }

    /// The tab chosen, below the tabs
    fn tab_view(&self, row: i32, context: &ViewContext<Self>) -> View {
        let p = self.palette();
        match self.tab {
            Tab::Run => Grid::new()
                .grid_row(row)
                .row_spacing(12.0)
                .rows([
                    GridLength::Auto,      // position + progress
                    GridLength::Auto,      // chart
                    GridLength::Star(3.0), // results
                    GridLength::Star(1.4), // errors
                ])
                .children((self.position(0), self.chart_card(1, context), self.results(2, context), self.error_list(3))),
            Tab::Log => self.log_card(row),
            Tab::Plan => section_card(
                row,
                "Plan · the plan, its tests' configuration, the memory channels, the patterns' seed",
                &self.model.sections.plan,
                p,
            ),
            Tab::Compare => self.compare_card(row, context),
            Tab::System => section_card(
                row,
                "System · the CPU, its caches and calibration, the TSC, the memory modules",
                &self.model.sections.system,
                p,
            ),
            Tab::Threads => section_card(
                row,
                "Threads · the CPUs chosen, the cores and each thread's CPU, then the run's work",
                &self.model.sections.threads,
                p,
            ),
            Tab::Memory => section_card(
                row,
                "Memory · Windows' memory, the large pages, the plan, each thread's allocation",
                &self.model.sections.memory,
                p,
            ),
        }
    }

    /// The log's lines in their order, oldest at the top, as a log reads (the owner, 2026-10-31), a warning's and an error's in the error
    /// colour: the log file's, the records at its level (`RUST_LOG`) and the console's text of each event between them (TODO 109), so the
    /// run as the console shows it. It doesn't follow its newest line: Reactor has no scroll offset, scroll-to or anchoring (0.100.0, nor
    /// its master), so that waits for it. Not in Cascadia Mono, as Windows Terminal has them: 0.100.0's TextBlock takes no font family, so
    /// the console's tables don't line up until the next Reactor, whose TextBlock does (its master has it).
    fn log_card(&self, row: i32) -> View {
        let p = self.palette();
        let log = &self.model.log;
        let body: View = if log.is_empty() {
            TextBlock::new().text(self.t(Phrase::LogHint)).foreground(brush(p.text_secondary)).into()
        } else {
            let lines = log.last(LOG_SHOWN).map(|(number, text)| {
                // `[12:00:00 WARN tmr::tests] ...`: the level follows the 10-character stamp
                let alarm = text.get(10..).is_some_and(|rest| rest.starts_with("WARN") || rest.starts_with("ERROR"));
                let line = TextBlock::new().text(text.to_string()).foreground(brush(if alarm { p.error } else { p.text_primary }));
                (number, View::from(line.is_text_selection_enabled(true)))
            });
            ScrollViewer::new()
                .vertical_scroll_bar_visibility(ScrollBarVisibility::Auto)
                .horizontal_scroll_bar_visibility(ScrollBarVisibility::Auto)
                .content(StackPanel::new().keyed_children(lines))
        };
        let total = log.total();
        let shown =
            if total > LOG_SHOWN as u64 { format!(" · the last {LOG_SHOWN} shown, the log file has them all") } else { String::new() };
        let dropped = self.model.dropped_line().map_or(String::new(), |line| format!(" · {line}"));
        card(row, p, card_title(format!("Log · {total} line{}{shown}{dropped}", if total == 1 { "" } else { "s" }), p), body)
    }

    /// The finished steps, newest first, the final summary's row above them once the run has one. A row opens its report beside the list
    /// (TODO 109), which then shows only each row's cycle, step and label, the label in the error colour for a step that found errors.
    fn results(&self, row: i32, context: &ViewContext<Self>) -> View {
        let p = self.palette();
        let report = self.model.opened_report();
        let compact = report.is_some();
        let header = table_row(&StepRow::HEADINGS.map(str::to_string), p, None, true, [false; StepRow::COLUMNS], compact, false);
        let stripe = brush(p.stripe);
        let opened = self.model.opened.filter(|_| compact);
        // Clickable rows are Borders: a Button's padding would inset the stripes. Transparent, not unfilled, so the whole row takes the
        // click: XAML doesn't hit-test an element with no background. A click opens on the press, as Explorer selects.
        let clickable = |line: View, which: Opened| -> View {
            Border::new()
                .background(brush(Color::transparent()))
                .on_pointer_pressed(context.callback(move |_: PointerEventInfo| Message::Open(which)))
                .content(line)
        };
        let mut rows: Vec<(u64, View)> = Vec::new();
        if let Some(summary) = &self.model.summary {
            let line = table_row(&summary.cells, p, None, false, summary.flagged, compact, opened == Some(Opened::Summary));
            rows.push((u64::MAX, clickable(line, Opened::Summary)));
        }
        // Newest first: reactor 0.100.0 exposes no ScrollIntoView / scroll offset, so a list in step order could not follow the latest row.
        // Rows are keyed by their place in the run, so a new step inserts one native row and leaves the others alone.
        for (i, r) in self.model.rows.iter().enumerate().rev().take(SHOWN) {
            let selected = opened == Some(Opened::Step(i));
            let line = table_row(&r.cells(), p, (i % 2 == 1).then_some(stripe), false, r.flagged(), compact, selected);
            rows.push((i as u64, if self.model.opens(i) { clickable(line, Opened::Step(i)) } else { line }));
        }
        let body: View = if rows.is_empty() {
            TextBlock::new().text(self.t(Phrase::NoSteps)).foreground(brush(p.text_secondary)).margin(Thickness::xy(8.0, 6.0)).into()
        } else {
            ScrollViewer::new().vertical_scroll_bar_visibility(ScrollBarVisibility::Auto).content(StackPanel::new().keyed_children(rows))
        };
        let list = Grid::new()
            .rows([GridLength::Auto, GridLength::STAR])
            .row_spacing(2.0)
            .children((Border::new().grid_row(0).content(header), Border::new().grid_row(1).content(body)));
        let steps = self.model.rows.len();
        let shown = if steps > SHOWN { format!(", the newest {SHOWN} shown") } else { String::new() };
        let hint = if steps > 0 && !compact { " · a row opens its report" } else { "" };
        let title = card_title(format!("Results · {steps} step{}, newest first{shown}{hint}", if steps == 1 { "" } else { "s" }), p);
        let content: View = match report {
            Some(report) => {
                Grid::new().columns([GridLength::Star(1.0), GridLength::Auto, GridLength::Star(1.8)]).column_spacing(12.0).children((
                    Border::new().grid_column(0).content(list),
                    Border::new().grid_column(1).width(1.0).background(brush(p.chart_grid)),
                    Border::new().grid_column(2).content(report_panel(report, p, context)),
                ))
            }
            None => list,
        };
        card(row, p, title, content)
    }

    fn chart_card(&self, row: i32, context: &ViewContext<Self>) -> View {
        let p = self.palette();
        let series = &self.model.series;
        let latest = match series.latest() {
            Some(crate::model::Sample::Test(mib_s)) => crate::model::thousands(mib_s),
            Some(crate::model::Sample::Seal(mib_s)) => format!("{} (seal)", crate::model::thousands(mib_s)),
            Some(crate::model::Sample::SealedPass(mib_s)) => format!("{} (sealed pass)", crate::model::thousands(mib_s)),
            _ => "–".to_owned(),
        };
        let marks = series.marks().count();
        let title = format!("Throughput (MiB/s, over 3 s) · last 60 s · now {latest}");
        // Its ▾ hides it, and its ▸ shows it (TODO 109): hidden, its title line stays, with the speed now, and its shapes aren't built
        let chevron = Button::new()
            .style(ButtonStyle::Subtle)
            .margin(Thickness::new(-11.0, -5.0, 0.0, -6.0))
            .grid_column(0)
            .on_click(context.callback(|()| Message::Chart))
            .content(if self.settings.chart { "▾" } else { "▸" });
        if !self.settings.chart {
            let header = Grid::new()
                .columns([GridLength::Auto, GridLength::STAR])
                .column_spacing(4.0)
                .children((chevron, card_title(title, p).vertical_alignment(VerticalAlignment::Center).grid_column(1)));
            return Border::new()
                .grid_row(row)
                .background(brush(p.card_background))
                .border_brush(brush(p.card_stroke))
                .border_thickness(1.0)
                .corner_radius(8.0)
                .padding(Thickness::new(CARD_PADDING_X, 10.0, CARD_PADDING_X, 10.0))
                .content(header);
        }
        // The chevron and the title on the left, a legend on the right (line = MiB/s, the seal's line, red mark = errors rose). A
        // temperature series would add an item here.
        let header = Grid::new().columns([GridLength::Auto, GridLength::STAR, GridLength::Auto]).column_spacing(4.0).children((
            chevron,
            card_title(title, p).vertical_alignment(VerticalAlignment::Center).grid_column(1),
            StackPanel::new().grid_column(2).orientation(Orientation::Horizontal).spacing(6.0).children((
                Border::new().width(16.0).height(2.0).vertical_alignment(VerticalAlignment::Center).background(brush(p.accent)),
                legend_text("MiB/s", p),
                Border::new()
                    .width(16.0)
                    .height(2.0)
                    .margin(Thickness::new(8.0, 0.0, 0.0, 0.0))
                    .vertical_alignment(VerticalAlignment::Center)
                    .background(brush(p.chart_seal)),
                legend_text("seal", p).tooltip(
                    "The seal's work, and a step's first pass while the seal wraps each chunk: the check before a chunk leaves it in cache \
                     for the test's first touch, so that pass can run apart from the rest",
                ),
                Border::new()
                    .width(2.0)
                    .height(12.0)
                    .margin(Thickness::new(8.0, 0.0, 0.0, 0.0))
                    .vertical_alignment(VerticalAlignment::Center)
                    .background(brush(p.error)),
                legend_text(&if marks == 0 { "errors".to_owned() } else { format!("errors ({marks} shown)") }, p),
            )),
        ));
        let width = (self.width - 2.0 * PAGE_MARGIN_X - 2.0 * CARD_PADDING_X - 2.0).max(100.0);
        card(row, p, header, chart::shapes_chart(series, width, p))
    }

    fn error_list(&self, row: i32) -> View {
        let p = self.palette();
        let body: View = if self.model.errors.is_empty() {
            TextBlock::new().text(self.t(Phrase::NoErrors)).foreground(brush(p.text_secondary)).into()
        } else {
            let entries = self.model.errors.newest(SHOWN).map(|(number, text)| {
                (
                    number,
                    Grid::new().column_spacing(8.0).margin(Thickness::xy(0.0, 3.0)).columns([GridLength::Auto, GridLength::STAR]).children(
                        (
                            TextBlock::new().text("●").foreground(brush(p.error)).grid_column(0),
                            TextBlock::new()
                                .text(text.to_string())
                                .foreground(brush(p.text_primary))
                                .text_wrapping(TextWrapping::Wrap)
                                .is_text_selection_enabled(true)
                                .grid_column(1),
                        ),
                    ),
                )
            });
            ScrollViewer::new().vertical_scroll_bar_visibility(ScrollBarVisibility::Auto).content(StackPanel::new().keyed_children(entries))
        };
        let total = self.model.errors.total();
        let shown = if total > SHOWN as u64 { format!(" · the newest {SHOWN} shown") } else { String::new() };
        let dropped = self.model.dropped_line().map_or(String::new(), |line| format!(" · {line}"));
        let strip = bit_strip(self.model.failing_mask, p);
        let title = StackPanel::new().spacing(6.0).children((card_title(format!("Errors · {total}{shown}{dropped}"), p), strip));
        card(row, p, title, body)
    }

    /// Replace the stored calibration, or keep it (TODO 110). Primary replaces; secondary, close and Esc keep what is stored, so a
    /// double-click or a closed window does not overwrite it. Not open while the large-page question is: WinUI opens one dialog at a time.
    fn save_dialog(&self, context: &ViewContext<Self>) -> View {
        let open = self.model.save_pending() && !self.model.large_pages_pending();
        ContentDialog::new()
            .title(self.t(Phrase::ReplaceTitle))
            .primary_button_text(self.t(Phrase::Keep))
            .secondary_button_text(self.t(Phrase::Replace))
            .is_open(open)
            .on_closed(context.callback(Message::SaveAnswer))
            .content(TextBlock::new().text(self.model.save_question()).text_wrapping(TextWrapping::Wrap))
    }

    /// The last run did not finish (a crash, a reset, or the window closed on it). The note is what was flushed, not a picture of the
    /// moment the machine died. Close marks it seen.
    fn unfinished_dialog(&self, context: &ViewContext<Self>) -> View {
        let open =
            self.unfinished.is_some() && !self.model.large_pages_pending() && !self.model.save_pending() && self.admin_answer.is_none();
        let mut dialog = ContentDialog::new()
            .title(self.t(Phrase::UnfinishedTitle))
            .close_button_text(self.t(Phrase::Close))
            .is_open(open)
            .on_closed(context.callback(Message::UnfinishedClosed));
        if self.unfinished.as_ref().is_some_and(|note| note.results.is_some()) {
            dialog = dialog.primary_button_text(self.t(Phrase::OpenTheResults));
        }
        if !open {
            return dialog.content(TextBlock::new());
        }
        let note = self.unfinished.as_ref();
        let body = note.map(|note| {
            let readings = if note.readings.is_empty() { String::new() } else { format!("\n{}", note.readings) };
            let errors = if note.errors.is_empty() { String::new() } else { format!("\n{}", note.errors) };
            format!("{}\n\n{}{}\n{}{readings}", self.t(Phrase::UnfinishedBody), note.written, errors, note.position)
        });
        dialog.content(TextBlock::new().text(body.unwrap_or_default()).text_wrapping(TextWrapping::Wrap))
    }

    /// The large-page question, the console's as a dialog, open while the session's thread waits for its answer, in the words of the last
    /// question asked (`Model::large_page_dialog`), which hold as it closes: the sign-in's, the grant's with its candidates as a list
    /// (every choice and its note in sight, as on the console; WinUI's dialog scrolls a long list), and the administrator's.
    /// Its secondary button is 4 KiB pages this time; Esc is the close button's, the quit, as an empty answer is on the console (TODO
    /// 113); an empty button text leaves the button out. The grant's primary button waits on a choice.
    fn large_page_dialog(&self, context: &ViewContext<Self>) -> View {
        let words = self.model.large_page_dialog();
        let enabled = words.choices.is_empty() || words.chosen.is_some();
        let text = TextBlock::new().text(words.text.clone()).text_wrapping(TextWrapping::Wrap);
        // The text in the dialog's own colours: it follows the system's theme, not the palette's
        let content = dialog_content(text, &words, context.callback(Message::LargePageTarget));
        ContentDialog::new()
            .title(words.title)
            .primary_button_text(words.primary)
            .is_primary_button_enabled(enabled)
            .secondary_button_text(words.secondary)
            .close_button_text(words.close)
            .is_open(self.model.large_pages_asked())
            .on_closed(context.callback(Message::LargePages))
            .content(content)
    }

    fn admin_dialog(&self, context: &ViewContext<Self>) -> View {
        ContentDialog::new()
            .title("Run as Administrator?")
            .primary_button_text("No")
            .secondary_button_text("Yes")
            .is_open(self.admin_answer.is_some())
            .on_closed(context.callback(Message::AdminAnswer))
            .content(
                TextBlock::new()
                    .text(if self.admin_answer.is_some() {
                        format!("Run memory tests as Administrator using {}?", tmr::home::dir().display())
                    } else {
                        String::new()
                    })
                    .text_wrapping(TextWrapping::Wrap),
            )
    }

    /// The second dialog after a broad pick in the first (TODO 113): what the group reaches and risks, its secondary button the grant,
    /// its primary button No, as Esc is, which goes back to the first. The first button is No because WinUI focuses it when the text has
    /// nothing to focus, so Enter or Space pressed on, and a double-click's second click, which lands where the first dialog's grant was,
    /// grant nothing.
    fn broad_dialog(&self, context: &ViewContext<Self>) -> View {
        let words = self.model.broad_dialog();
        ContentDialog::new()
            .title(words.title)
            .primary_button_text("Don't grant it: back to the list")
            .secondary_button_text(words.grant)
            .is_open(self.model.broad_asked())
            .on_closed(context.callback(Message::LargePageBroad))
            .content(TextBlock::new().text(words.text).text_wrapping(TextWrapping::Wrap))
    }
}

impl Window {
    /// Settings (TODO 105, the owner 2026-11-04): what the window keeps in window.json that a user sets, each saved as it changes; not open
    /// while the large-page question is, as WinUI opens one dialog at a time
    fn settings_dialog(&self, context: &ViewContext<Self>) -> View {
        let open = self.settings_open
            && !self.model.large_pages_pending()
            && !self.model.save_pending()
            && self.admin_answer.is_none()
            && self.unfinished.is_none();
        let dialog = ContentDialog::new()
            .title(self.t(Phrase::Settings))
            .close_button_text(self.t(Phrase::Close))
            .is_open(open)
            .on_closed(context.callback(|_: ContentDialogResult| Message::SettingsClosed));
        if !open {
            return dialog.content(TextBlock::new());
        }
        dialog.content(settings_content(
            &self.settings,
            context.callback(Message::StartOnOpen),
            context.callback(Message::KeepSerials),
            tmr::home::elevated().then(|| context.callback(Message::AllowAdmin)),
            context.callback(Message::Notify),
            (context.callback(Message::Theme), context.callback(Message::Language)),
            Updates {
                status: &self.update,
                on_check: context.callback(|()| Message::CheckUpdates),
                on_open: context.callback(|()| Message::OpenRelease),
            },
        ))
    }

    /// The ••• menu's Sanitize, once it has run: the copies it wrote beside the pair (or the file alone) and what it replaced, or why it
    /// wrote nothing, the text selectable for the copies' paths. Not open while the large-page question, About or Settings is, as WinUI
    /// opens one dialog at a time.
    fn sanitized_dialog(&self, context: &ViewContext<Self>) -> View {
        let open = self.sanitized.is_some()
            && !self.model.large_pages_pending()
            && self.admin_answer.is_none()
            && self.unfinished.is_none()
            && !self.model.save_pending()
            && !self.about
            && !self.settings_open;
        let (title, text) = match &self.sanitized {
            Some(Ok(text)) => ("Sanitized", text.clone()),
            Some(Err(e)) => ("Not sanitized", format!("Nothing was written: {e}")),
            None => ("Sanitized", String::new()),
        };
        ContentDialog::new()
            .title(title)
            .close_button_text("Close")
            .is_open(open)
            .on_closed(context.callback(|_: ContentDialogResult| Message::SanitizedClosed))
            .content(sanitized_content(&text))
    }

    /// About (TODO 109): the logo, what TMR is, its version and build, its licence and its GitHub page. Its content is built only while it
    /// is open, so the logo isn't decoded at each redraw; and it isn't open while the large-page question is, as WinUI opens one dialog at
    /// a time. In the dialog's own colours, as the large-page dialog's.
    fn about_dialog(&self, context: &ViewContext<Self>) -> View {
        let open = self.about
            && !self.model.large_pages_pending()
            && !self.model.save_pending()
            && self.admin_answer.is_none()
            && self.unfinished.is_none();
        let dialog = ContentDialog::new()
            .title(self.t(Phrase::About))
            .close_button_text(self.t(Phrase::Close))
            .is_open(open)
            .on_closed(context.callback(|_: ContentDialogResult| Message::AboutClosed));
        if !open {
            return dialog.content(TextBlock::new());
        }
        let line = |text: String| {
            View::from(
                TextBlock::new()
                    .text(text)
                    .text_wrapping(TextWrapping::Wrap)
                    .is_text_selection_enabled(true)
                    .horizontal_alignment(HorizontalAlignment::Center),
            )
        };
        let mut parts: Vec<(u64, View)> = Vec::new();
        if let Some(png) = crate::ICON.get().and_then(|ico| largest_png(ico)) {
            let logo = Image::new().source_data(EncodedImage::new(png)).width(72.0).height(72.0);
            parts.push((0, logo.horizontal_alignment(HorizontalAlignment::Center).into()));
        }
        parts.push((
            1,
            TextBlock::new()
                .text("Test Memory R")
                .font_size(20.0)
                .font_weight(FontWeight::SEMI_BOLD)
                .horizontal_alignment(HorizontalAlignment::Center)
                .into(),
        ));
        parts.push((2, line(lang::version_line(self.settings.language, tmr::BUILD.version))));
        let author = env!("CARGO_PKG_AUTHORS").split(':').next().unwrap_or("");
        parts.push((3, line(lang::by_line(self.settings.language, author))));
        let repository = env!("CARGO_PKG_REPOSITORY").to_string();
        parts.push((4, line(tmr::BUILD.describe())));
        parts.push((5, line(format!("Licence: {}", env!("CARGO_PKG_LICENSE")))));
        // Keep the text visible so the link has a name in the accessibility tree.
        let mark_bytes: &'static [u8] = if self.dark { include_bytes!("github-light.png") } else { include_bytes!("github-dark.png") };
        let mark = Image::new().source_data(EncodedImage::from_static(mark_bytes)).width(22.0).height(22.0);
        parts.push((
            6,
            Button::new()
                .style(ButtonStyle::Subtle)
                .automation_name("GitHub")
                .horizontal_alignment(HorizontalAlignment::Center)
                .on_click(context.callback(move |()| Message::OpenUrl(repository.clone())))
                .content(
                    StackPanel::new()
                        .orientation(Orientation::Horizontal)
                        .spacing(8.0)
                        .children((mark, TextBlock::new().text("GitHub").vertical_alignment(VerticalAlignment::Center))),
                ),
        ));
        dialog.content(
            StackPanel::new().spacing(8.0).min_width(280.0).horizontal_alignment(HorizontalAlignment::Center).keyed_children(parts),
        )
    }

    /// The Compare tab (TODO 109): the baseline and the run to compare with it, each chosen in the Open dialog or the run's own results
    /// file, both at once, swapped; then their comparison, as `--compare-results` gives it: what differs between the runs, the plans'
    /// changes (TODO 72: the steps only one has, the settings that differ) and the overall changes, then the steps both have as a table
    fn compare_card(&self, row: i32, context: &ViewContext<Self>) -> View {
        let p = self.palette();
        let tab = &self.model.compare;
        let this_run = self.model.results_path().is_some();
        let slot = |r: i32, label: &str, slot: Slot, path: Option<&PathBuf>| -> [(u64, View); 4] {
            let name = path.map_or("none chosen".to_string(), |path| path.display().to_string());
            let colour = if path.is_some() { p.text_primary } else { p.text_secondary };
            let key = |column: u64| (r as u64) * 10 + column;
            [
                (
                    key(0),
                    View::from(card_title(label.to_string(), p).vertical_alignment(VerticalAlignment::Center).grid_row(r).grid_column(0)),
                ),
                (
                    key(1),
                    TextBlock::new()
                        .text(name.clone())
                        .foreground(brush(colour))
                        .text_trimming(TextTrimming::CharacterEllipsis)
                        .vertical_alignment(VerticalAlignment::Center)
                        .grid_row(r)
                        .grid_column(1)
                        .tooltip(name),
                ),
                (
                    key(2),
                    Button::new()
                        .grid_row(r)
                        .grid_column(2)
                        .on_click(context.callback(move |()| Message::Choose(slot)))
                        .content(self.t(Phrase::Choose)),
                ),
                (
                    key(3),
                    Button::new()
                        .is_enabled(this_run)
                        .grid_row(r)
                        .grid_column(3)
                        .on_click(context.callback(move |()| Message::ThisRun(slot)))
                        .content(self.t(Phrase::ThisRun)),
                ),
            ]
        };
        let mut cells: Vec<(u64, View)> = Vec::new();
        cells.extend(slot(0, self.t(Phrase::Baseline), Slot::Baseline, tab.baseline.as_ref()));
        cells.extend(slot(1, self.t(Phrase::Compared), Slot::Compared, tab.compared.as_ref()));
        let actions = StackPanel::new().orientation(Orientation::Horizontal).spacing(8.0).grid_row(2).grid_column(1).grid_column_span(3);
        let actions = actions.children((
            Button::new().on_click(context.callback(|()| Message::ChooseBoth)).content(self.t(Phrase::ChooseBoth)),
            Button::new()
                .is_enabled(tab.baseline.is_some() && tab.compared.is_some())
                .on_click(context.callback(|()| Message::Swap))
                .content(self.t(Phrase::Swap)),
        ));
        cells.push((20, actions));
        let files = Grid::new()
            .grid_row(0)
            .column_spacing(12.0)
            .row_spacing(6.0)
            .columns([GridLength::Auto, GridLength::STAR, GridLength::Auto, GridLength::Auto])
            .rows([GridLength::Auto; 3])
            .keyed_children(cells);
        let note = |text: &str, colour: Color| {
            View::from(TextBlock::new().text(text.to_string()).foreground(brush(colour)).text_wrapping(TextWrapping::Wrap))
        };
        let body: View = match tab.result() {
            Some(Ok(comparison)) => {
                let parts: Vec<(u64, View)> = vec![
                    (
                        0,
                        TextBlock::new()
                            .text(comparison.text.trim_end().to_string())
                            .is_text_selection_enabled(true)
                            .foreground(brush(p.text_primary))
                            .into(),
                    ),
                    (1, report_table(&comparison.per_test, p)),
                ];
                ScrollViewer::new()
                    .vertical_scroll_bar_visibility(ScrollBarVisibility::Auto)
                    .horizontal_scroll_bar_visibility(ScrollBarVisibility::Auto)
                    .content(
                        Border::new()
                            .padding(Thickness::new(0.0, 0.0, 12.0, 12.0))
                            .content(StackPanel::new().spacing(8.0).keyed_children(parts)),
                    )
            }
            Some(Err(e)) => note(&format!("They couldn't be compared: {e}"), p.error),
            None if tab.is_comparing() => note("Comparing…", p.text_secondary),
            None => note(
                "Choose a baseline and a run to compare with it: two results files, from the results folder. This run is the run's own \
                 file, once it has saved one.",
                p.text_secondary,
            ),
        };
        card(
            row,
            p,
            card_title("Compare · two runs' results files, the baseline first".to_string(), p),
            Grid::new()
                .rows([GridLength::Auto, GridLength::STAR])
                .row_spacing(12.0)
                .children((files, Border::new().grid_row(1).content(body))),
        )
    }
}

/// The Stop button's colours, the palette's error fill and the badge's text on it, a little darker under the pointer and pressed: WinUI has
/// no red button style, so the button's own resources take them (lightweight styling)
fn stop_colours(p: &Palette) -> ResourceOverrides {
    let shade = |c: Color, f: f64| Color::argb(c.a, (f64::from(c.r) * f) as u8, (f64::from(c.g) * f) as u8, (f64::from(c.b) * f) as u8);
    let (fill, hover, pressed, text) = (p.critical, shade(p.critical, 0.9), shade(p.critical, 0.8), p.on_status);
    ResourceOverrides::new()
        .set("ButtonBackground", fill)
        .set("ButtonBackgroundPointerOver", hover)
        .set("ButtonBackgroundPressed", pressed)
        .set("ButtonForeground", text)
        .set("ButtonForegroundPointerOver", text)
        .set("ButtonForegroundPressed", text)
        .set("ButtonBorderBrush", fill)
        .set("ButtonBorderBrushPointerOver", hover)
        .set("ButtonBorderBrushPressed", pressed)
}

/// The largest PNG frame of an .ico, for About's logo (tmr.ico's is 256 px): an .ico's 6-byte header gives its frames' count, then its
/// directory, 16 bytes a frame, each one's width (0 for 256), its bytes' length and their offset. Only a PNG frame: WinUI decodes it.
fn largest_png(ico: &[u8]) -> Option<&[u8]> {
    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n";
    let le32 = |bytes: &[u8]| u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    let count = usize::from(u16::from_le_bytes([*ico.get(4)?, *ico.get(5)?]));
    (0..count)
        .filter_map(|i| {
            let entry = ico.get(6 + 16 * i..22 + 16 * i)?;
            let width = if entry[0] == 0 { 256 } else { usize::from(entry[0]) };
            let at = le32(&entry[12..16]);
            let frame = ico.get(at..at.checked_add(le32(&entry[8..12]))?)?;
            frame.starts_with(PNG).then_some((width, frame))
        })
        .max_by_key(|&(width, _)| width)
        .map(|(_, frame)| frame)
}

/// A report beside the results (TODO 109): its title and the close button, then, scrolling both ways, its verdict, its lines and its
/// tables. A wide table scrolls rather than squeezing its columns, and a line doesn't wrap: the console's report is as wide.
fn report_panel(report: &Report, p: &Palette, context: &ViewContext<Window>) -> View {
    let close = Button::new().style(ButtonStyle::Subtle).grid_column(1).on_click(context.callback(|()| Message::Close)).content("✕");
    let title =
        card_title(report.title.clone(), p).text_trimming(TextTrimming::CharacterEllipsis).vertical_alignment(VerticalAlignment::Center);
    let head = Grid::new().columns([GridLength::STAR, GridLength::Auto]).column_spacing(8.0).children((title.grid_column(0), close));
    let verdict = TextBlock::new()
        .text(report.verdict.clone())
        .font_weight(FontWeight::SEMI_BOLD)
        .foreground(brush(if report.failed { p.error } else { p.success }))
        .is_text_selection_enabled(true);
    let line = |text: &str| TextBlock::new().text(text.to_string()).foreground(brush(p.text_secondary)).is_text_selection_enabled(true);
    let mut parts: Vec<(u64, View)> = vec![(0, verdict.into())];
    parts.extend(report.lines.iter().enumerate().map(|(i, text)| (1 + i as u64, line(text).into())));
    parts.extend(block_views(&report.blocks, 1 + report.lines.len() as u64, p));
    let body = ScrollViewer::new()
        .grid_row(1)
        .vertical_scroll_bar_visibility(ScrollBarVisibility::Auto)
        .horizontal_scroll_bar_visibility(ScrollBarVisibility::Auto)
        .content(Border::new().padding(Thickness::new(0.0, 0.0, 12.0, 12.0)).content(StackPanel::new().spacing(6.0).keyed_children(parts)));
    Grid::new().rows([GridLength::Auto, GridLength::STAR]).row_spacing(6.0).children((Border::new().grid_row(0).content(head), body))
}

/// A startup section's tab (TODO 109): its blocks as the report panel draws them, scrolling both ways, or until setup sends them, when
/// they come
fn section_card(row: i32, title: &str, blocks: &[Block], p: &Palette) -> View {
    let body: View = if blocks.is_empty() {
        TextBlock::new().text("At Start, once setup has detected them").foreground(brush(p.text_secondary)).into()
    } else {
        ScrollViewer::new()
            .vertical_scroll_bar_visibility(ScrollBarVisibility::Auto)
            .horizontal_scroll_bar_visibility(ScrollBarVisibility::Auto)
            .content(
                Border::new()
                    .padding(Thickness::new(0.0, 0.0, 12.0, 12.0))
                    .content(StackPanel::new().spacing(6.0).keyed_children(block_views(blocks, 0, p))),
            )
    };
    card(row, p, card_title(title.to_string(), p), body)
}

/// A report's blocks (`report::Block`), keyed from `first`: a heading, a line, a warning in the error colour, a rule, an engine's table
fn block_views(blocks: &[Block], first: u64, p: &Palette) -> Vec<(u64, View)> {
    let line = |text: &str| TextBlock::new().text(text.to_string()).foreground(brush(p.text_secondary)).is_text_selection_enabled(true);
    blocks
        .iter()
        .enumerate()
        .map(|(i, block)| {
            let view: View = match block {
                Block::Heading(text) => card_title(text.clone(), p).margin(Thickness::new(0.0, 6.0, 0.0, 0.0)).into(),
                Block::Text(text) => line(text).into(),
                Block::Warning(text) => TextBlock::new().text(text.clone()).foreground(brush(p.error)).into(),
                Block::Separator => Border::new().height(1.0).margin(Thickness::xy(0.0, 4.0)).background(brush(p.chart_grid)).into(),
                Block::Table(table) => report_table(table, p),
            };
            (first + i as u64, view)
        })
        .collect()
}

/// One of the engine's tables (`TableData`), as the console draws it in text: its title, its headings, its rows, each column aligned as its
/// heading says, then its footer. A headline table (`emphasis`, TODO 119's grid) is larger, its first column strong, its rows striped.
fn report_table(table: &TableData, p: &Palette) -> View {
    let columns = table.headers.len().max(table.rows.iter().map(Vec::len).max().unwrap_or(0));
    let size = if table.emphasis { 14.0 } else { 12.0 };
    let align = |column: usize| match table.headers.get(column).map(|heading| heading.alignment) {
        Some(ColumnAlignment::Right) => HorizontalAlignment::Right,
        Some(ColumnAlignment::Center) => HorizontalAlignment::Center,
        _ => HorizontalAlignment::Left,
    };
    let cell = |text: &str, row: usize, column: usize, heading: bool| -> (u64, View) {
        let colour = if heading { p.text_secondary } else { p.text_primary };
        let strong = heading || (table.emphasis && column == 0);
        let block = TextBlock::new()
            .text(text.to_string())
            .font_size(size)
            .font_weight(if strong { FontWeight::SEMI_BOLD } else { FontWeight::NORMAL })
            .foreground(brush(colour))
            .horizontal_alignment(align(column))
            .grid_row(row as i32)
            .grid_column(column as i32);
        (((row as u64) << 16) | column as u64, block.into())
    };
    // A headline table's every other row on the stripe, behind its cells (drawn first), across every column
    let mut cells: Vec<(u64, View)> = if table.emphasis {
        (0..table.rows.len())
            .filter(|row| row % 2 == 1)
            .map(|row| {
                let stripe = Border::new()
                    .background(brush(p.stripe))
                    .corner_radius(3.0)
                    .grid_row(row as i32 + 1)
                    .grid_column(0)
                    .grid_column_span(columns.max(1) as i32);
                ((1 << 48) | row as u64, stripe.into())
            })
            .collect()
    } else {
        Vec::new()
    };
    cells.extend(table.headers.iter().enumerate().map(|(column, heading)| cell(&heading.text, 0, column, true)));
    for (row, values) in table.rows.iter().enumerate() {
        cells.extend(values.iter().enumerate().map(|(column, text)| cell(text, row + 1, column, false)));
    }
    let grid = Grid::new()
        .column_spacing(14.0)
        .row_spacing(1.0)
        .columns(vec![GridLength::Auto; columns])
        .rows(vec![GridLength::Auto; table.rows.len() + 1])
        .keyed_children(cells);
    let mut parts: Vec<(u64, View)> = Vec::new();
    if let Some(title) = &table.title {
        parts.push((0, card_title(title.clone(), p).margin(Thickness::new(0.0, 6.0, 0.0, 0.0)).into()));
    }
    parts.push((1, grid));
    if let Some(footer) = &table.footer {
        parts.push((2, TextBlock::new().text(footer.clone()).font_size(12.0).foreground(brush(p.text_secondary)).into()));
    }
    StackPanel::new().spacing(4.0).keyed_children(parts)
}

/// egui's progress bar, from reactor primitives: a fully rounded track, `width` wide, a rounded fill sized to the fraction by two star
/// columns, and the percentage centred over both, the model's, rounded down as the ticker's are. Its tooltip says what it follows.
/// Determinate, no animation; a stopped run dims the fill. A fill begun is never narrower than it is high (egui's `min_width`), so its
/// rounding stays round: a run's first percent is a dot.
fn progress_bar(bar: &Bar, paused: bool, width: f64, p: &Palette) -> View {
    let f = bar.fraction.clamp(0.0, 1.0);
    let inner_radius = (PROGRESS_HEIGHT - 2.0) / 2.0;
    let fill = if f > 0.0 { f.max(PROGRESS_HEIGHT / width.max(PROGRESS_HEIGHT)) } else { 0.0 };
    Border::new()
        .height(PROGRESS_HEIGHT)
        .corner_radius(PROGRESS_HEIGHT / 2.0)
        .background(brush(p.progress_track))
        .border_brush(brush(p.card_stroke))
        .border_thickness(1.0)
        .content(
            Grid::new().columns([GridLength::Star(fill), GridLength::Star(1.0 - fill)]).children((
                Border::new().grid_column(0).corner_radius(inner_radius).background(brush(p.progress_fill)).opacity(
                    match (f > 0.0, paused) {
                        (false, _) => 0.0,
                        (true, true) => 0.5,
                        (true, false) => 1.0,
                    },
                ),
                TextBlock::new()
                    .text(format!("{}%", bar.percent))
                    .font_size(12.0)
                    .font_weight(FontWeight::SEMI_BOLD)
                    .foreground(brush(p.progress_text))
                    .horizontal_alignment(HorizontalAlignment::Center)
                    .vertical_alignment(VerticalAlignment::Center)
                    .grid_column(0)
                    .grid_column_span(2),
            )),
        )
        .tooltip(bar.about)
}

/// The large-page dialog's content: its text, and for the grant its candidates as a list under a header (TODO 113, the owner: radio
/// buttons spread too far apart), Type, Name and Note in fixed widths so the header lines up with the rows; a long name wraps; then the
/// broad mark's legend, once, as the console has it. A ListView, for its keyboard and selection, and its own scrolling past a height. No tooltip in it: one inside a dialog's content fails reactor 0.100.0's planner (StructureUnsupported,
/// the window gone), as `a_tooltip_in_a_dialog_still_fails` keeps watch on.
fn dialog_content(text: TextBlock, words: &crate::model::LargePageDialog, on_select: impl IntoPayloadCallback<Option<usize>>) -> View {
    if words.choices.is_empty() {
        return text.into();
    }
    // The note is short ("You, recommended", or the broad mark, whose legend sits under the list), so the name takes the room
    const COLUMNS: [GridLength; 3] = [GridLength::Pixel(48.0), GridLength::Pixel(290.0), GridLength::Pixel(120.0)];
    // The list's height, past which it scrolls (TODO 113, the owner: a dozen candidates ran past the dialog, the last out of reach): six
    // rows and part of the next, so it shows there are more
    const LIST_HEIGHT: f64 = 260.0;
    let line = |kind: &str, name: &str, note: &str, bold: bool| {
        let weight = if bold { FontWeight::SEMI_BOLD } else { FontWeight::NORMAL };
        Grid::new().columns(COLUMNS).column_spacing(8.0).children((
            TextBlock::new().text(kind).font_weight(weight).grid_column(0),
            TextBlock::new().text(name).font_weight(weight).text_wrapping(TextWrapping::Wrap).grid_column(1),
            TextBlock::new().text(note).font_weight(weight).text_wrapping(TextWrapping::Wrap).grid_column(2),
        ))
    };
    // A list item's content sits 12 px in, so the header does too
    let header = Border::new().margin(Thickness::new(12.0, 0.0, 0.0, 0.0)).content(line("Type", "Name", "Note", true));
    let items: Vec<KeyedView> = words
        .choices
        .iter()
        .enumerate()
        .map(|(i, row)| KeyedView::new(i, ListViewItem::new().tag(i.to_string()).content(line(row.kind, &row.name, &row.note, false))))
        .collect();
    let list = ListView::new()
        .selection_mode(ListViewSelectionMode::Single)
        .selected_index(words.chosen)
        .on_selection_changed(on_select)
        .max_height(LIST_HEIGHT)
        .collection_slot(ListViewSlot::Items, items);
    let mut parts: Vec<(u64, View)> = vec![(0, text.into()), (1, header), (2, list)];
    if let Some(legend) = &words.legend {
        parts.push((3, TextBlock::new().text(legend.clone()).text_wrapping(TextWrapping::Wrap).into()));
    }
    StackPanel::new().spacing(8.0).keyed_children(parts)
}

/// The Settings dialog's content: a row a switch, its words on the left; the theme's choices; and where they're kept. No tooltip: one
/// inside a dialog fails reactor 0.100.0 (`a_tooltip_in_a_dialog_still_fails`).
/// The update check's state and its two buttons, so Settings' builder stays within a handful of arguments
struct Updates<'a, C, O> {
    status: &'a crate::update::Status,
    on_check: C,
    on_open: O,
}

fn settings_content(
    settings: &crate::settings::Settings, on_start: impl IntoPayloadCallback<bool>, on_serials: impl IntoPayloadCallback<bool>,
    administrator: Option<impl IntoPayloadCallback<bool>>, on_notify: impl IntoPayloadCallback<bool>,
    appearance: (impl IntoPayloadCallback<Option<usize>>, impl IntoPayloadCallback<Option<usize>>),
    updates: Updates<impl IntoUnitCallback, impl IntoUnitCallback>,
) -> View {
    let row = |words: &str, switch: ToggleSwitch| -> View {
        Grid::new().columns([GridLength::STAR, GridLength::Auto]).column_spacing(12.0).children((
            TextBlock::new().text(words).text_wrapping(TextWrapping::Wrap).vertical_alignment(VerticalAlignment::Center),
            Border::new().grid_column(1).content(switch.automation_name(words)),
        ))
    };
    let (on_theme, on_language) = appearance;
    let say = |phrase| lang::text(settings.language, phrase);
    let start = row(say(Phrase::StartOnOpen), ToggleSwitch::new().is_on(settings.start_on_open).on_toggled(on_start));
    let serials = row(say(Phrase::Serials), ToggleSwitch::new().is_on(settings.keep_serials).on_toggled(on_serials));
    let notify = row(say(Phrase::Notify), ToggleSwitch::new().is_on(settings.notify).on_toggled(on_notify));
    let theme = StackPanel::new().spacing(4.0).children((
        TextBlock::new().text(say(Phrase::Theme)),
        RadioButtons::new()
            .items_source([say(Phrase::AsWindows), say(Phrase::Light), say(Phrase::Dark)])
            .max_columns(3)
            .selected_index(settings.theme.index())
            .on_selection_changed(on_theme),
    ));
    let language = StackPanel::new().spacing(4.0).children((
        TextBlock::new().text(say(Phrase::LanguageLabel)),
        RadioButtons::new()
            .items_source(crate::lang::Language::LABELS)
            .max_columns(3)
            .selected_index(settings.language.index())
            .on_selection_changed(on_language),
    ));
    let kept = TextBlock::new().text(say(Phrase::Kept)).text_wrapping(TextWrapping::Wrap);
    let updates = update_row(settings.language, updates);
    let panel = StackPanel::new().spacing(14.0);
    if let Some(on_admin) = administrator {
        let admin = row(
            "Allow administrator runs without asking in this home for my account",
            ToggleSwitch::new().is_on(settings.allow_admin).on_toggled(on_admin),
        );
        panel.children((start, serials, admin, notify, theme, language, kept, updates))
    } else {
        panel.children((start, serials, notify, theme, language, kept, updates))
    }
}

/// Settings' check for a newer release. The result stays in the dialog; a newer one opens its page. No download.
fn update_row(language: crate::lang::Language, updates: Updates<impl IntoUnitCallback, impl IntoUnitCallback>) -> View {
    let say = |phrase| lang::text(language, phrase);
    let Updates { status: update, on_check, on_open } = updates;
    let checking = matches!(update, crate::update::Status::Checking);
    let rule = Border::new().height(1.0).margin(Thickness::new(0.0, 6.0, 0.0, 2.0)).background(ThemeBrush::CardStroke);
    let heading = TextBlock::new().text(say(Phrase::Updates)).font_weight(FontWeight::SEMI_BOLD);
    let button = Button::new().is_enabled(!checking).on_click(on_check).content(say(if checking {
        Phrase::CheckingUpdates
    } else {
        Phrase::CheckUpdates
    }));
    let note = match update {
        crate::update::Status::Idle => None,
        crate::update::Status::Checking => None,
        crate::update::Status::Current { version } | crate::update::Status::Ahead { version } => {
            let text = if matches!(update, crate::update::Status::Ahead { .. }) {
                lang::ahead_of_release(language, version)
            } else {
                lang::up_to_date(language, version)
            };
            Some(TextBlock::new().text(text).text_wrapping(TextWrapping::Wrap).into())
        }
        crate::update::Status::Available { version, url: _ } => Some(StackPanel::new().spacing(8.0).children((
            TextBlock::new().text(lang::update_available(language, version)).text_wrapping(TextWrapping::Wrap),
            Button::new().on_click(on_open).content(say(Phrase::OpenRelease)),
        ))),
        crate::update::Status::Failed(why) => {
            Some(TextBlock::new().text(lang::update_failed(language, why)).text_wrapping(TextWrapping::Wrap).into())
        }
    };
    let mut parts: Vec<(u64, View)> = vec![(0, rule.into()), (1, heading.into()), (2, button)];
    if let Some(note) = note {
        parts.push((3, note));
    }
    StackPanel::new().spacing(10.0).keyed_children(parts)
}

/// The Sanitize dialog's content: what `tmr::sanitize::sanitize` (or `sanitize_single`) said, selectable
fn sanitized_content(text: &str) -> View {
    TextBlock::new().text(text).text_wrapping(TextWrapping::Wrap).is_text_selection_enabled(true).into()
}

/// A sound, not a toast: an unpackaged window has no shortcut to hang a toast on. `alarm` is the error sound; a clean end is the asterisk.
fn sound(alarm: bool) {
    // windows 0.62's Win32_UI_WindowsAndMessaging feature does not expose MessageBeep. The id is the Win32 one: 0x10 hand, 0x40 asterisk.
    unsafe extern "system" {
        fn MessageBeep(kind: u32) -> i32;
    }
    // SAFETY: one integer, no pointer. It plays a system sound and returns.
    let _ = unsafe { MessageBeep(if alarm { 0x10 } else { 0x40 }) };
}

/// 64 squares, one per data bit, filled where a check failed. Colour is not the only signal: the count sits in the errors title.
fn bit_strip(mask: u64, p: &Palette) -> View {
    if mask == 0 {
        return TextBlock::new().text("").into();
    }
    let cells = (0u64..64).map(|bit| {
        let on = mask & (1u64 << bit) != 0;
        let cell = Border::new().width(8.0).height(12.0).background(brush(if on { p.error } else { p.chart_grid }));
        (bit, View::from(cell))
    });
    StackPanel::new()
        .orientation(Orientation::Horizontal)
        .spacing(1.0)
        .keyed_children(cells)
        .tooltip("Failed data bits, bit 0 on the left. A filled square is a bit a check found wrong.")
}

fn card_title(title: String, p: &Palette) -> TextBlock {
    TextBlock::new().text(title).font_weight(FontWeight::SEMI_BOLD).foreground(brush(p.text_primary))
}

fn legend_text(text: &str, p: &Palette) -> TextBlock {
    TextBlock::new().text(text).font_size(12.0).foreground(brush(p.text_secondary)).vertical_alignment(VerticalAlignment::Center)
}

/// A WinUI-style card: card fill, hairline stroke, 8 px corners, a title line.
fn card(row: i32, p: &Palette, title: impl Into<View>, content: impl Into<View>) -> View {
    Border::new()
        .grid_row(row)
        .background(brush(p.card_background))
        .border_brush(brush(p.card_stroke))
        .border_thickness(1.0)
        .corner_radius(8.0)
        .padding(Thickness::new(CARD_PADDING_X, 10.0, CARD_PADDING_X, 12.0))
        .content(
            Grid::new()
                .rows([GridLength::Auto, GridLength::STAR])
                .row_spacing(8.0)
                .children((Border::new().grid_row(0).content(title), Border::new().grid_row(1).content(content))),
        )
}

/// `StepRow::HEADINGS`'s columns: C# and T# hold 3 digits, a night's cycles, and Time a step's 00:00:15
const TABLE_COLUMNS: [GridLength; StepRow::COLUMNS] = [
    GridLength::Pixel(32.0),
    GridLength::Pixel(32.0),
    GridLength::Star(1.0),
    GridLength::Pixel(64.0),
    GridLength::Pixel(88.0),
    GridLength::Pixel(64.0),
    GridLength::Pixel(80.0),
    GridLength::Pixel(56.0),
];

/// The columns a row shows beside a report: the cycle, the step and its label
const COMPACT: [usize; 3] = [0, 1, 2];

/// One table line; the header uses the same columns and padding so the columns line up. `flagged` marks a non-zero count: palette error
/// red, semi-bold (egui's `.strong()`). `compact`, beside a report: the cycle, the step and the label alone, the label flagged when any
/// count is. `selected`: its report is the one open, an accent outline.
fn table_row(
    cells: &[String; StepRow::COLUMNS], p: &Palette, stripe: Option<Brush>, header: bool, flagged: [bool; StepRow::COLUMNS], compact: bool,
    selected: bool,
) -> View {
    let weight = if header { FontWeight::SEMI_BOLD } else { FontWeight::NORMAL };
    let text = if header { p.text_secondary } else { p.text_primary };
    let all: [usize; StepRow::COLUMNS] = std::array::from_fn(|column| column);
    let shown: &[usize] = if compact { &COMPACT } else { &all };
    let any_flagged = flagged.contains(&true);
    let cell = |(at, &column): (usize, &usize)| -> (u64, View) {
        let flag = if compact && column == 2 { any_flagged } else { flagged[column] };
        let block = TextBlock::new()
            .text(cells[column].clone())
            .font_weight(if flag { FontWeight::SEMI_BOLD } else { weight })
            .foreground(brush(if flag { p.error } else { text }))
            .text_trimming(TextTrimming::CharacterEllipsis)
            .horizontal_alignment(if column == 2 { HorizontalAlignment::Left } else { HorizontalAlignment::Right })
            .grid_column(at as i32);
        (column as u64, block.into())
    };
    let columns: Vec<GridLength> = shown.iter().map(|&column| TABLE_COLUMNS[column]).collect();
    // The outline's 1 px comes out of the padding, so the columns stay where the header's are
    let (outline, padding) = if selected { (1.0, Thickness::new(7.0, 3.0, 11.0, 3.0)) } else { (0.0, Thickness::new(8.0, 4.0, 12.0, 4.0)) };
    Border::new()
        .background_optional(stripe)
        .border_brush(brush(p.accent))
        .border_thickness(outline)
        .corner_radius(4.0)
        .padding(padding)
        .content(Grid::new().columns(columns).column_spacing(12.0).keyed_children(shown.iter().enumerate().map(cell)))
}

#[cfg(test)]
mod tests {
    use windows_reactor::test::{Pump, RecordingRuntime};
    use windows_reactor::*;

    use super::largest_png;

    /// A dialog in a window, open or not, as the window draws its large-page dialog
    fn dialog(open: bool, content: View) -> View {
        Grid::new().children((TextBlock::new().text("the window"), ContentDialog::new().title("t").is_open(open).content(content)))
    }

    /// The grant's dialog as a real question gives it, a long name among its candidates
    fn grant_words(chosen: Option<usize>) -> crate::model::LargePageDialog {
        let mut words = crate::model::grant_dialog_for_tests();
        words.chosen = chosen;
        words
    }

    /// TODO 113: the grant's list opens, takes another pick and closes through reactor's planner, which failed the window on 2026-11-04
    /// (StructureUnsupported, from a tooltip in it)
    #[test]
    fn the_grant_dialog_opens_and_closes() {
        let content = |chosen| super::dialog_content(TextBlock::new().text("words"), &grant_words(chosen), |_: Option<usize>| {});
        let mut pump = Pump::new(RecordingRuntime::default());
        pump.mount_view(dialog(false, TextBlock::new().text("no question yet").into())).unwrap();
        for (open, chosen) in [(true, Some(0)), (true, Some(2)), (true, Some(2)), (true, None), (false, None)] {
            pump.update_view(dialog(open, content(chosen))).unwrap_or_else(|e| panic!("open {open}, chosen {chosen:?}: {e:?}"));
        }
    }

    /// TODO 105: the Settings dialog opens, takes a change and closes through reactor's planner, as the grant dialog's
    #[test]
    fn the_settings_dialog_opens_and_closes() {
        let mut settings = crate::settings::Settings::default();
        let content = |settings: &crate::settings::Settings| {
            super::settings_content(
                settings,
                |_: bool| {},
                |_: bool| {},
                Some(|_: bool| {}),
                |_: bool| {},
                (|_: Option<usize>| {}, |_: Option<usize>| {}),
                super::Updates { status: &crate::update::Status::Idle, on_check: || {}, on_open: || {} },
            )
        };
        let mut pump = Pump::new(RecordingRuntime::default());
        pump.mount_view(dialog(false, TextBlock::new().text("closed").into())).unwrap();
        pump.update_view(dialog(true, content(&settings))).unwrap();
        (settings.keep_serials, settings.start_on_open) = (true, true);
        pump.update_view(dialog(true, content(&settings))).unwrap();
        pump.update_view(dialog(false, content(&settings))).unwrap();
    }

    /// The Sanitize dialog opens with what it wrote, a pair's then one file's, then with a refusal, and closes, through reactor's planner,
    /// as the Settings dialog's
    #[test]
    fn the_sanitize_dialog_opens_and_closes() {
        let mut pump = Pump::new(RecordingRuntime::default());
        pump.mount_view(dialog(false, TextBlock::new().text("closed").into())).unwrap();
        let done = "🧹 Sanitized copies written, the originals as they were:\n   C:\\tmr\\results\\x.sanitized.json\n";
        pump.update_view(dialog(true, super::sanitized_content(done))).unwrap();
        let alone =
            "🧹 Sanitized copy written, the original as it was:\n   C:\\tmr\\logs\\x.sanitized.log\n   Without its results file: …\n";
        pump.update_view(dialog(true, super::sanitized_content(alone))).unwrap();
        pump.update_view(dialog(true, super::sanitized_content("Nothing was written: x.json's log isn't there"))).unwrap();
        pump.update_view(dialog(false, super::sanitized_content(""))).unwrap();
    }

    /// Reactor 0.100.0 can't plan a tooltip inside a dialog's content, so the dialogs have none. When this fails, a reactor upgrade
    /// fixed it, and the grant's long names could have their tooltip back.
    #[test]
    fn a_tooltip_in_a_dialog_still_fails() {
        let mut pump = Pump::new(RecordingRuntime::default());
        pump.mount_view(dialog(false, TextBlock::new().text("closed").into())).unwrap();
        let tipped = Grid::new().children((TextBlock::new().text("a long name").tooltip("a long name"),));
        assert!(pump.update_view(dialog(true, tipped)).is_err());
    }

    /// TODO 119: the grid, a headline table (its rows striped behind the cells, across every column), plans and mounts through reactor's
    /// planner, and so does the summary panel it opens on
    #[test]
    fn the_grid_mounts() {
        let summary = tmr::test_support::perf_summary_report();
        let report = crate::report::Report::of_summary(&summary, Vec::new());
        let p = crate::palette::Palette::LIGHT;
        let grid = report.blocks.iter().find_map(|b| if let crate::report::Block::Table(t) = b { t.emphasis.then_some(t) } else { None });
        let mut pump = Pump::new(RecordingRuntime::default());
        pump.mount_view(super::report_table(grid.expect("the grid"), &p)).unwrap();
        let mut pump = Pump::new(RecordingRuntime::default());
        pump.mount_view(StackPanel::new().spacing(6.0).keyed_children(super::block_views(&report.blocks, 0, &p))).unwrap();
    }

    /// About's logo is the icon's largest PNG frame (a width of 0 is 256), a larger BMP frame passed over; nothing from a directory that
    /// points past the file's end, or from bytes that aren't an icon
    #[test]
    fn the_logo_is_the_icons_largest_png_frame() {
        let png = |name: &str| [&b"\x89PNG\r\n\x1a\n"[..], name.as_bytes()].concat();
        let (small, large, bmp) = (png("small"), png("large"), b"a BMP frame".to_vec());
        let frames = [(48u8, &small), (0, &large), (255, &bmp)];
        let mut ico = vec![0, 0, 1, 0, frames.len() as u8, 0];
        let mut at = 6 + 16 * frames.len();
        for (width, bytes) in frames {
            ico.extend([width, width, 0, 0, 1, 0, 32, 0]);
            ico.extend((bytes.len() as u32).to_le_bytes());
            ico.extend((at as u32).to_le_bytes());
            at += bytes.len();
        }
        for (_, bytes) in frames {
            ico.extend(bytes);
        }
        assert_eq!(largest_png(&ico), Some(&large[..]));
        assert_eq!(largest_png(&ico[..60]), None, "the frames cut off");
        assert_eq!(largest_png(b"not an icon"), None);
    }
}
