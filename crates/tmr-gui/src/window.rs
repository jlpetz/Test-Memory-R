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
use std::time::Duration;

use tmr::commands::ResultsComparison;
use tmr::frontend::{ColumnAlignment, Event, ProgressSnapshot, ProgressTracker, Run, TableData, ThreadPriority};
use windows_reactor::*;

use crate::dialogs::{self, Open};
use crate::logger::{self, LogLine, LogLines};
use crate::model::{Bar, DialogButton, Model, Opened, Slot, Status, StepRow};
use crate::palette::{Palette, Palettes, brush};
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
        let path = std::env::temp_dir().join("tmr-icon.ico");
        std::fs::write(&path, crate::ICON.get()?).ok()?;
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
    /// An item of the `…` menu: its label
    Menu(String),
    /// A file or a folder opened, or why the shell couldn't
    Opened(Result<(), String>),
    AboutClosed,
    Scheme(ColorScheme),
    Size(WindowSize),
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
    /// `window.json`'s (TODO 109): the chart shown or hidden, the theme, the parameters, the size
    settings: Settings,
    dark: bool,
    width: f64,
    palettes: Palettes,
    /// The priority this thread, the UI's, was last set to (`follow_priority`): normal as it starts
    priority: ThreadPriority,
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

    fn label(self) -> &'static str {
        match self {
            Tab::Run => "Run",
            Tab::Log => "Log",
            Tab::Plan => "Plan",
            Tab::System => "System",
            Tab::Threads => "Threads",
            Tab::Memory => "Memory",
            Tab::Compare => "Compare",
        }
    }
}

/// The window's size as it opens the first time, before `window.json` has one
const WIDTH: f64 = 860.0;
const HEIGHT: f64 = 960.0;

/// The run button's width: it says Start, Stop, Stopping…, Finishing… or Run again, and keeps its place
const RUN_BUTTON: f64 = 112.0;
/// The command bar's buttons beside the parameters box, Load config and the run button, with their spacing, and the menu's at the right
const COMMANDS_WIDTH: f64 = 128.0 + RUN_BUTTON + 48.0 + 3.0 * 8.0;
/// The parameters box's widest: the parameters a run takes fit, and the buttons stay beside it
const PARAMETERS_MAX: f64 = 640.0;

/// `--start`, which the window takes out of its parameters: it starts the run as it opens (TODO 109), as Run again asks
pub(crate) const START: &str = "--start";

/// Waits on a thread-pool thread for the feed, up to `TICK`, then returns everything queued as one message, with where the suite stands
/// then and the lines logged. A wait that brought nothing returns as well: that is the window's clock, as Reactor 0.100.0 has no timer.
/// Re-armed from `update` after each delivery, so there is one waiter at a time, and the feeds travel with it.
fn listen(feeds: Feeds, progress: Option<Arc<ProgressTracker>>, context: &ComponentContext<Window>) {
    context.spawn_background(move |_| {
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

    /// Runs the parameters box's command line on the session's own thread, its events to the window's feed. The log file opens now, with
    /// the window's logger, so its name pairs with the run's results.
    fn start(&mut self, context: &ComponentContext<Self>) {
        // A run as a new process's first (TODO 109): the last run's settings, stamp and stop cleared, then a log file of its own
        tmr::session::reset_run();
        let log = self.log.take().unwrap_or_else(logger::install);
        logger::open_log_file();
        self.model.log_path = Some(tmr::logfile::path());
        let (tx, rx) = mpsc::channel();
        let fe = Arc::new(GuiFrontend { feed: tx.clone(), transcript: Some(log.transcript()) });
        let args = self.model.command();
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
                listen(Feeds { events: rx, log }, None, context);
            }
            Err(e) => self.model.ended(Err(format!("the engine's thread didn't start: {e}"))),
        }
    }

    fn take(&mut self, batch: Batch, context: &ComponentContext<Self>) {
        let Batch { feed, snapshot, whea, log, dropped, feeds, closed } = batch;
        let mut ended = false;
        for item in feed {
            match item {
                Feed::Event(event) => self.model.apply(event),
                Feed::Ask { question, reply } => self.model.ask_large_pages(question, reply),
                Feed::Ended(result) => {
                    self.model.ended(result);
                    ended = true;
                }
            }
        }
        self.model.logged(log, dropped);
        self.model.tick(snapshot, whea);
        if closed && !ended {
            // A panic ends the thread before it says how the session went (a release build aborts)
            self.model.ended(Err("the session's thread ended without a result: the log file has why".to_string()));
            ended = true;
        }
        // Raised at setup's selection, normal again as the session ends: here, as the model changes it, and before the log file closes
        self.follow_priority();
        if ended {
            // The lines logged since the waiter took the batch's, the priority's line among them: this run's Log tab's, not the next's
            let (log, dropped) = feeds.log.take();
            self.model.logged(log, dropped);
            // The run's log file is done with, and the logger's side waits for the next run (TODO 109)
            tmr::logfile::close();
            self.log = Some(feeds.log);
        } else {
            listen(feeds, self.model.progress().cloned(), context);
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
        match wanted.apply() {
            Ok(()) => log::info!("The window's thread at {wanted} priority ({})", wanted.win32_name()),
            Err(e) => {
                log::warn!(target: tmr::logfile::FILE_ONLY_TARGET, "The window's thread at {wanted} priority: {e}");
                self.model.errors.push(format!("The window's thread at {wanted} priority: {e}"));
            }
        }
        self.priority = wanted;
    }

    /// Writes `window.json`, or says in the errors list why it couldn't
    fn save_settings(&mut self) {
        if let Err(e) = self.settings.save() {
            self.model.errors.push(format!("window.json: {e}"));
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

    /// An item of the `…` menu: a file or a folder to open, on a thread of its own, or About
    fn menu(&mut self, label: &str, context: &ComponentContext<Self>) {
        let path = match label {
            MENU_LOG => self.model.log_path.clone(),
            MENU_LOGS => Some(tmr::home::logs()),
            MENU_RESULTS => self.model.results_path().map(PathBuf::from),
            MENU_RESULTS_FOLDER => Some(tmr::home::results()),
            MENU_ABOUT => {
                self.about = true;
                None
            }
            _ => None,
        };
        if let Some(path) = path {
            context.spawn_background(move |_| Message::Opened(dialogs::shell_open(path)));
        }
    }
}

/// The Open dialog's filter for results files
const RESULTS_FILES: (&str, &str) = ("TMR results", "*.json");

/// The `…` menu's items, by their labels, which the menu gives back
const MENU_LOG: &str = "Open the log";
const MENU_LOGS: &str = "Open the logs folder";
const MENU_RESULTS: &str = "Open the results file";
const MENU_RESULTS_FOLDER: &str = "Open the results folder";
const MENU_ABOUT: &str = "About TMR";

/// The window's size, as it closes, into `window.json` (TODO 109): Run again's window opens as this one was
impl Drop for Window {
    fn drop(&mut self) {
        let _ = self.settings.save();
    }
}

impl Component for Window {
    type Input = Vec<String>;
    type Message = Message;

    fn create(args: &Vec<String>, context: &ComponentContext<Self>) -> Self {
        crate::window_opened();
        let (settings, palettes, problems) = Settings::load();
        let start = args.iter().any(|arg| arg == START) || settings.start_on_open;
        let mut args: Vec<String> = args.iter().filter(|arg| *arg != START).cloned().collect();
        // Given no parameters, the last run's (TODO 109)
        if args.len() <= 1
            && let Some(parameters) = &settings.parameters
        {
            args.extend(crate::model::args_of(parameters));
        }
        let mut model = Model::new(&args);
        // Before Start there is no log file to take them, and maybe no console
        model.errors.extend(problems.into_iter().map(|problem| format!("window.json: {problem}")));
        let (width, dark) = (settings.size.map_or(WIDTH, |(width, _)| width), settings.theme == Theme::Dark);
        let priority = ThreadPriority::Normal;
        let mut window = Self { model, log: None, tab: Tab::Run, about: false, settings, dark, width, palettes, priority };
        if start {
            window.start(context);
        }
        window
    }

    fn update(&mut self, message: Message, context: &ComponentContext<Self>) {
        match message {
            Message::Feed(batch) => self.take(*batch, context),
            // One session a process: the engine's run settings are set once (TODO 29), so another run is another process (Run again)
            Message::Start if self.model.status == Status::Ready => {
                // The parameters the window opens with next time, when given none
                self.settings.parameters = Some(self.model.args.clone());
                self.save_settings();
                self.start(context);
            }
            Message::Start => {}
            // In this window (TODO 109): a new run's model, the Compare tab's files kept, and the engine readied as a new process's
            Message::RunAgain if self.model.session_over() => {
                self.model = self.model.again();
                self.settings.parameters = Some(self.model.args.clone());
                self.save_settings();
                self.start(context);
            }
            Message::RunAgain => {}
            Message::Stop => {
                if self.model.status == Status::Running {
                    tmr::request_stop();
                    self.model.stop();
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
                    title: "Load a config: TMR's (.json) or TM5's (.cfg)",
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
                    Slot::Baseline => "Compare: the baseline's results file",
                    Slot::Compared => "Compare: the results file to compare with the baseline",
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
                let ask = Open {
                    title: "Compare two results files: choose both, the second with Ctrl",
                    filter: RESULTS_FILES,
                    folder: tmr::home::results(),
                    multiple: true,
                };
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
            Message::Menu(label) => self.menu(&label, context),
            Message::Opened(Ok(())) => {}
            Message::Opened(Err(e)) => self.model.errors.push(e),
            Message::AboutClosed => self.about = false,
            Message::Tab(label) => {
                if let Some(tab) = Tab::ALL.into_iter().find(|tab| label.as_deref() == Some(tab.label())) {
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
            // Saved as the window closes (`Drop`)
            Message::Size(size) => {
                self.width = size.width;
                self.settings.size = Some((size.width, size.height));
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
                self.about_dialog(context),
            ))
    }
}

// ---- view pieces -------------------------------------------------------------------------

impl Window {
    /// The badge's fill: success while running and for a pass, critical for a fail
    fn status_fill(&self) -> Color {
        let p = self.palette();
        match self.model.status {
            Status::Running | Status::Passed => p.success,
            Status::Failed => p.critical,
            Status::Ready | Status::Stopping | Status::Interrupted | Status::NotRun => p.neutral,
        }
    }

    /// Three rows on the left: the title, the badge and the runtime; the run's errors, and under the runtime the time its limit leaves;
    /// the info line. The system panel is on the right, right-justified like the runtime.
    fn header(&self, row: i32, context: &ViewContext<Self>) -> View {
        let p = self.palette();
        let info = self.model.info_line();
        let top = Grid::new()
            .grid_row(0)
            .column_spacing(14.0)
            .columns([
                GridLength::Auto, // title
                GridLength::Auto, // badge
                GridLength::STAR,
                GridLength::Auto, // runtime
            ])
            .children((
                TextBlock::new()
                    .text("Test Memory R")
                    .font_size(28.0)
                    .font_weight(FontWeight::SEMI_BOLD)
                    .foreground(brush(p.text_primary))
                    .vertical_alignment(VerticalAlignment::Center)
                    .grid_column(0),
                Border::new()
                    .grid_column(1)
                    .vertical_alignment(VerticalAlignment::Center)
                    .corner_radius(12.0)
                    .padding(Thickness::new(10.0, 3.0, 10.0, 4.0))
                    .background(brush(self.status_fill()))
                    .content(
                        TextBlock::new()
                            .text(self.model.status.label())
                            .font_size(12.0)
                            .font_weight(FontWeight::SEMI_BOLD)
                            .foreground(brush(p.on_status)),
                    ),
                // The runtime keeps reactor's default font: 0.100.0's TextBlock has no font family; the next release adds it (owner's note
                // 6).
                TextBlock::new()
                    .text(self.model.runtime_line())
                    .font_size(20.0)
                    .font_weight(FontWeight::SEMI_LIGHT)
                    .foreground(brush(p.text_primary))
                    .vertical_alignment(VerticalAlignment::Center)
                    .grid_column(3)
                    // A tooltip wraps its target: the target's grid placement goes first
                    .tooltip("The suite's runtime, from its first cycle, as the console's ticker counts it"),
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
                .text("System info: at Start")
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
            .placeholder_text("Parameters, as on the command line: memory=50% cycles=3 (none: the defaults)")
            .is_enabled(takes)
            .width(box_width)
            .on_text_changed(context.callback(Message::Args))
            .grid_column(0)
            .tooltip(
                "What Start runs, in tmr.exe's key=value syntax (tmr.exe --help lists them). \
                 Quote one with a space in it: \"config=C:\\My configs\\x.cfg\"",
            );
        // A config's path into the box, as `config=`, from the home's configs\ (TODO 109)
        let load =
            Button::new().is_enabled(takes).grid_column(1).on_click(context.callback(|()| Message::LoadConfig)).content("Load config…");
        let item = |on: bool, key: usize, label: &str| if on { MenuItem::item(key, label) } else { MenuItem::disabled(key, label) };
        let menu = [
            item(self.model.log_path.is_some(), 0, MENU_LOG),
            MenuItem::item(1usize, MENU_LOGS),
            MenuItem::separator(2usize),
            item(self.model.results_path().is_some(), 3, MENU_RESULTS),
            MenuItem::item(4usize, MENU_RESULTS_FOLDER),
            MenuItem::separator(5usize),
            MenuItem::item(6usize, MENU_ABOUT),
        ];
        let more = Button::new()
            .grid_column(4)
            .style(ButtonStyle::Subtle)
            .vertical_alignment(VerticalAlignment::Center)
            .content("•••")
            .menu(Menu::new(menu, context.callback(Message::Menu)));
        Grid::new()
            .grid_row(row)
            .column_spacing(8.0)
            .columns([GridLength::Auto, GridLength::Auto, GridLength::Auto, GridLength::STAR, GridLength::Auto])
            .children((parameters, load, self.run_button(2, context), more))
    }

    /// The run button (TODO 109): Start, then Stop in the error colour, Stopping… while the step ends, Finishing… while the session saves
    /// its results, then Run again. One button, its width fixed, so the bar doesn't move as it changes.
    fn run_button(&self, column: i32, context: &ViewContext<Self>) -> View {
        let p = self.palette();
        let button = Button::new().width(RUN_BUTTON).grid_column(column);
        match self.model.status {
            Status::Ready => button.style(ButtonStyle::Accent).on_click(context.callback(|()| Message::Start)).content("Start"),
            Status::Running => button.resource_overrides(stop_colours(p)).on_click(context.callback(|()| Message::Stop)).content("Stop"),
            Status::Stopping => button.is_enabled(false).content("Stopping…"),
            _ if self.model.session_over() => {
                button.style(ButtonStyle::Accent).on_click(context.callback(|()| Message::RunAgain)).content("Run again")
            }
            _ => button.is_enabled(false).content("Finishing…"),
        }
    }

    /// The tabs (TODO 109): Run, the window's own view, then the log, the console's startup sections and Compare. A SelectorBar: 0.100.0's
    /// NavigationView can clear its selection on a stale event (#5210), and a Pivot would build every tab's view each redraw.
    fn tab_bar(&self, row: i32, context: &ViewContext<Self>) -> View {
        let items = Tab::ALL
            .iter()
            .enumerate()
            .map(|(i, &tab)| (i as u64, View::from(SelectorBarItem::new().text(tab.label()).is_selected(tab == self.tab))));
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
            TextBlock::new().text("The log's lines: from Start").foreground(brush(p.text_secondary)).into()
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
            TextBlock::new().text("No steps finished yet").foreground(brush(p.text_secondary)).margin(Thickness::xy(8.0, 6.0)).into()
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
            TextBlock::new().text("No errors so far").foreground(brush(p.text_secondary)).into()
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
        card(row, p, card_title(format!("Errors · {total}{shown}{dropped}"), p), body)
    }

    /// The large-page question, the console's as a dialog, open while the session's thread waits for its answer, in the words of the last
    /// question asked (`Model::large_page_dialog`), which hold as it closes: the sign-in's, the grant's with its candidates as radio
    /// buttons (every choice and its warning in sight, as on the console; WinUI's dialog scrolls a long list), and the administrator's.
    /// Its secondary button is 4 KiB pages this time; Esc is the close button's, the quit, as an empty answer is on the console (TODO
    /// 113); an empty button text leaves the button out. The grant's primary button waits on a choice.
    fn large_page_dialog(&self, context: &ViewContext<Self>) -> View {
        let words = self.model.large_page_dialog();
        let enabled = words.choices.is_empty() || words.chosen.is_some();
        let text = TextBlock::new().text(words.text).text_wrapping(TextWrapping::Wrap);
        // The text in the dialog's own colours: it follows the system's theme, not the palette's
        let content: View = if words.choices.is_empty() {
            text.into()
        } else {
            let choices = RadioButtons::new()
                .items_source(words.choices)
                .selected_index(words.chosen)
                .on_selection_changed(context.callback(Message::LargePageTarget));
            StackPanel::new().spacing(12.0).children((text, choices))
        };
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
    /// About (TODO 109): the logo, what TMR is, its version and build, its licence and its GitHub page. Its content is built only while it
    /// is open, so the logo isn't decoded at each redraw; and it isn't open while the large-page question is, as WinUI opens one dialog at
    /// a time. In the dialog's own colours, as the large-page dialog's.
    fn about_dialog(&self, context: &ViewContext<Self>) -> View {
        let open = self.about && !self.model.large_pages_pending();
        let dialog = ContentDialog::new()
            .title("About TMR")
            .close_button_text("Close")
            .is_open(open)
            .on_closed(context.callback(|_: ContentDialogResult| Message::AboutClosed));
        if !open {
            return dialog.content(TextBlock::new());
        }
        let build = tmr::BUILD;
        let commit = build.commit.get(..12).unwrap_or(build.commit);
        let changes = if build.dirty { ", with changes" } else { "" };
        let line = |text: String| View::from(TextBlock::new().text(text).text_wrapping(TextWrapping::Wrap).is_text_selection_enabled(true));
        let mut parts: Vec<(u64, View)> = Vec::new();
        if let Some(png) = crate::ICON.get().and_then(|ico| largest_png(ico)) {
            let logo = Image::new().source_data(EncodedImage::new(png)).width(128.0).height(128.0);
            parts.push((0, logo.horizontal_alignment(HorizontalAlignment::Center).into()));
        }
        parts.push((1, TextBlock::new().text("Test Memory R (TMR)").font_size(20.0).font_weight(FontWeight::SEMI_BOLD).into()));
        parts.push((2, line("A memory tester for Windows: is this overclock stable?".to_string())));
        parts.push((3, line(format!("Version {} ({} build) · commit {commit}{changes}", build.version, build.profile))));
        parts.push((4, line(build.rustc.to_string())));
        parts.push((5, line(format!("Licence: {}, at your choice", env!("CARGO_PKG_LICENSE").replace(" OR ", " or ")))));
        let repository = env!("CARGO_PKG_REPOSITORY");
        if let Ok(link) = HyperlinkButton::new().navigate_uri(repository) {
            parts.push((6, link.content(repository.trim_start_matches("https://"))));
        }
        dialog.content(StackPanel::new().spacing(6.0).keyed_children(parts))
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
                    Button::new().grid_row(r).grid_column(2).on_click(context.callback(move |()| Message::Choose(slot))).content("Choose…"),
                ),
                (
                    key(3),
                    Button::new()
                        .is_enabled(this_run)
                        .grid_row(r)
                        .grid_column(3)
                        .on_click(context.callback(move |()| Message::ThisRun(slot)))
                        .content("This run"),
                ),
            ]
        };
        let mut cells: Vec<(u64, View)> = Vec::new();
        cells.extend(slot(0, "Baseline", Slot::Baseline, tab.baseline.as_ref()));
        cells.extend(slot(1, "Compared", Slot::Compared, tab.compared.as_ref()));
        let actions = StackPanel::new().orientation(Orientation::Horizontal).spacing(8.0).grid_row(2).grid_column(1).grid_column_span(3);
        let actions = actions.children((
            Button::new().on_click(context.callback(|()| Message::ChooseBoth)).content("Choose both…"),
            Button::new()
                .is_enabled(tab.baseline.is_some() && tab.compared.is_some())
                .on_click(context.callback(|()| Message::Swap))
                .content("Swap"),
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
/// heading says, then its footer
fn report_table(table: &TableData, p: &Palette) -> View {
    let columns = table.headers.len().max(table.rows.iter().map(Vec::len).max().unwrap_or(0));
    let align = |column: usize| match table.headers.get(column).map(|heading| heading.alignment) {
        Some(ColumnAlignment::Right) => HorizontalAlignment::Right,
        Some(ColumnAlignment::Center) => HorizontalAlignment::Center,
        _ => HorizontalAlignment::Left,
    };
    let cell = |text: &str, row: usize, column: usize, heading: bool| -> (u64, View) {
        let colour = if heading { p.text_secondary } else { p.text_primary };
        let weight = if heading { FontWeight::SEMI_BOLD } else { FontWeight::NORMAL };
        let block = TextBlock::new()
            .text(text.to_string())
            .font_size(12.0)
            .font_weight(weight)
            .foreground(brush(colour))
            .horizontal_alignment(align(column))
            .grid_row(row as i32)
            .grid_column(column as i32);
        (((row as u64) << 16) | column as u64, block.into())
    };
    let mut cells: Vec<(u64, View)> =
        table.headers.iter().enumerate().map(|(column, heading)| cell(&heading.text, 0, column, true)).collect();
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
    use super::largest_png;

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
