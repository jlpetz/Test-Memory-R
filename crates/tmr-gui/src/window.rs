//! The window (TODO 7), the owner's layout from the demos, top to bottom: the header (the title, the status badge, the runtime and the
//! system panel on the right, the info line under them), the position and the progress bar, the throughput chart with its error marks, the
//! results, the errors, then the parameters box with Start and Stop. The chart is never between the results and the errors. The large-page
//! restart question is a dialog over it (TODO 29 S6).
//!
//! It draws `Model`, and re-renders only when a message arrives: a batch from the feed (at least every 500 ms while the session runs), a
//! button, the dialog's answer, a keystroke in the box, a theme change or a resize. Nothing animates.

use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::Duration;

use tmr::frontend::{Event, ProgressSnapshot, ProgressTracker, RestartChoice, Run};
use windows_reactor::*;

use crate::logger::{self, LogLines};
use crate::model::{Model, Status, StepRow};
use crate::palette::{Palette, Palettes, brush};
use crate::sysinfo::{self, SystemPanel};
use crate::{Feed, GuiFrontend, chart};

const PAGE_MARGIN_X: f64 = 20.0;
const CARD_PADDING_X: f64 = 16.0;
/// egui's look: a tall, fully rounded bar with the percentage inside.
const PROGRESS_HEIGHT: f64 = 22.0;
/// The window's clock: the waiter returns at least this often while the session runs
const TICK: Duration = Duration::from_millis(500);
/// The rows and error lines drawn, the newest: a night's run has thousands, and every redraw builds the ones drawn
const SHOWN: usize = 200;

pub(crate) enum Message {
    Feed(Box<Batch>),
    Start,
    Stop,
    /// The parameters box's text, as typed
    Args(String),
    /// The restart dialog's answer: its primary button exits, its close button (and Esc) continues on 4 KiB pages
    Restart(ContentDialogResult),
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
    log: Vec<String>,
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
    dark: bool,
    width: f64,
    palettes: Palettes,
}

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
        let log = logger::install();
        let (tx, rx) = mpsc::channel();
        let fe = Arc::new(GuiFrontend { feed: tx.clone() });
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
                Feed::Ask(reply) => self.model.ask_restart(reply),
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
        if !ended {
            listen(feeds, self.model.progress().cloned(), context);
        }
    }
}

impl Component for Window {
    type Input = Vec<String>;
    type Message = Message;

    fn create(args: &Vec<String>, _context: &ComponentContext<Self>) -> Self {
        crate::window_opened();
        let (palettes, problems) = Palettes::load();
        let mut model = Model::new(args);
        // Before Start there is no log file to take them, and maybe no console
        model.errors.extend(problems.into_iter().map(|problem| format!("colours.json: {problem}")));
        Self { model, dark: false, width: 860.0, palettes }
    }

    fn update(&mut self, message: Message, context: &ComponentContext<Self>) {
        match message {
            Message::Feed(batch) => self.take(*batch, context),
            // One session a process: the engine's run settings are set once (TODO 29)
            Message::Start if self.model.status == Status::Ready => self.start(context),
            Message::Start => {}
            Message::Stop => {
                if self.model.status == Status::Running {
                    tmr::request_stop();
                    self.model.stop();
                }
            }
            Message::Args(text) => self.model.args = text,
            Message::Restart(result) => self.model.answer_restart(match result {
                ContentDialogResult::Primary => RestartChoice::Exit,
                _ => RestartChoice::Continue4K,
            }),
            Message::Scheme(scheme) => self.dark = scheme == ColorScheme::Dark,
            Message::Size(size) => self.width = size.width,
        }
    }

    fn view(&self, _args: &Vec<String>, context: &mut ViewContext<Self>) -> View {
        context.window_title(format!("Test Memory R (TMR) v{}", env!("CARGO_PKG_VERSION")));
        context.window_visuals(
            WindowVisuals::new()
                .backdrop(WindowBackdrop::Mica)
                .theme(WindowTheme::System)
                .client_size(860.0, 960.0)
                .constraints(WindowConstraints { min_width: Some(720.0), min_height: Some(680.0), ..Default::default() }),
        );
        context.on_color_scheme(context.callback(Message::Scheme));
        context.on_window_size(context.callback(Message::Size));

        Grid::new()
            .margin(Thickness::new(PAGE_MARGIN_X, 14.0, PAGE_MARGIN_X, 16.0))
            .row_spacing(12.0)
            .rows([
                GridLength::Auto,      // header: title, badge, timer, system info; info line
                GridLength::Auto,      // position + progress
                GridLength::Auto,      // chart
                GridLength::Star(3.0), // results
                GridLength::Star(1.4), // errors
                GridLength::Auto,      // parameters, buttons
            ])
            .children((
                self.header(0),
                self.position(1),
                self.chart_card(2),
                self.results(3),
                self.error_list(4),
                self.controls(5, context),
                self.restart_dialog(context),
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

    /// Two rows on the left (title + badge + timer, then the info line); the system panel spans both on the right, right-justified like the
    /// timer.
    fn header(&self, row: i32) -> View {
        let p = self.palette();
        let secs = self.model.runtime.as_secs();
        let runtime = format!("{:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60);
        let info = self.model.info_line();
        Grid::new()
            .grid_row(row)
            .column_spacing(14.0)
            .row_spacing(2.0)
            .rows([GridLength::Auto, GridLength::Auto])
            .columns([
                GridLength::Auto, // title
                GridLength::Auto, // badge
                GridLength::STAR,
                GridLength::Auto, // timer
                GridLength::Auto, // divider
                GridLength::Auto, // system info
            ])
            .children((
                TextBlock::new()
                    .text("Test Memory R")
                    .font_size(28.0)
                    .font_weight(FontWeight::SEMI_BOLD)
                    .foreground(brush(p.text_primary))
                    .vertical_alignment(VerticalAlignment::Center)
                    .grid_row(0)
                    .grid_column(0),
                Border::new()
                    .grid_row(0)
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
                // The timer keeps reactor's default font: 0.100.0's TextBlock has no font family; the next release adds it (owner's note
                // 6).
                TextBlock::new()
                    .text(runtime)
                    .font_size(20.0)
                    .font_weight(FontWeight::SEMI_LIGHT)
                    .foreground(brush(p.text_primary))
                    .vertical_alignment(VerticalAlignment::Center)
                    .grid_row(0)
                    .grid_column(3)
                    // A tooltip wraps its target: the target's grid placement goes first
                    .tooltip("The suite's runtime, from its first cycle, as the console's ticker counts it"),
                Border::new()
                    .grid_row(0)
                    .grid_row_span(2)
                    .grid_column(4)
                    .width(1.0)
                    .margin(Thickness::xy(0.0, 4.0))
                    .background(brush(p.chart_grid)),
                Border::new()
                    .grid_row(0)
                    .grid_row_span(2)
                    .grid_column(5)
                    .vertical_alignment(VerticalAlignment::Center)
                    .content(self.system_info()),
                TextBlock::new()
                    .text(info.clone())
                    .foreground(brush(p.text_secondary))
                    .text_trimming(TextTrimming::CharacterEllipsis)
                    .vertical_alignment(VerticalAlignment::Center)
                    .grid_row(1)
                    .grid_column(0)
                    .grid_column_span(4)
                    .tooltip(info),
            ))
    }

    /// CPU, RAM + the DIMM drop-down, caches; every line right-justified. Empty until the session's setup has detected them.
    fn system_info(&self) -> View {
        let p = self.palette();
        let Some(s) = &self.model.system else {
            return TextBlock::new()
                .text("System info: at Start")
                .foreground(brush(p.text_secondary))
                .horizontal_alignment(HorizontalAlignment::Right)
                .into();
        };
        let dimms = s.dimms.len();
        let dimm_label = format!("{dimms} DIMM{} ▾", if dimms == 1 { "" } else { "s" });
        // DropDownButton takes no flyout in 0.100.0 (the native side attaches flyouts to Button and SplitButton only), so this is a subtle
        // Button with a ▾ and a flyout. The negative margin cancels the button's padding so the line keeps text height and lines up right.
        let dimm_button = Button::new()
            .style(ButtonStyle::Subtle)
            .margin(Thickness::new(0.0, -5.0, -11.0, -6.0))
            .vertical_alignment(VerticalAlignment::Center)
            .content(TextBlock::new().text(dimm_label).foreground(brush(p.text_primary)))
            .flyout_with(Flyout::rich(self.dimm_table(s)).placement(FlyoutPlacement::BottomEdgeAlignedRight));
        let ram = self.model.ram_bytes.map_or("RAM ·".to_string(), |bytes| format!("{} RAM ·", sysinfo::size(bytes as usize)));
        StackPanel::new().spacing(2.0).horizontal_alignment(HorizontalAlignment::Right).children((
            TextBlock::new()
                .text(s.cpu.clone())
                .font_size(13.0)
                .font_weight(FontWeight::SEMI_BOLD)
                .foreground(brush(p.text_primary))
                .max_width(380.0)
                .text_trimming(TextTrimming::CharacterEllipsis)
                .horizontal_alignment(HorizontalAlignment::Right)
                .tooltip(s.cpu_detail.clone()),
            StackPanel::new().orientation(Orientation::Horizontal).horizontal_alignment(HorizontalAlignment::Right).spacing(6.0).children(
                (
                    TextBlock::new()
                        .text(ram)
                        .foreground(brush(p.text_secondary))
                        .vertical_alignment(VerticalAlignment::Center)
                        .tooltip("The physical memory Windows reports (GlobalMemoryStatusEx), a little under what is installed"),
                    dimm_button,
                ),
            ),
            TextBlock::new()
                .text(s.cache_line.clone())
                .foreground(brush(p.text_secondary))
                .horizontal_alignment(HorizontalAlignment::Right)
                .tooltip(s.cache_detail.clone()),
        ))
    }

    /// The DIMM flyout: one line per module, from SMBIOS's memory devices. Manufacturer and part number share a column: WinUI's
    /// FlyoutPresenter caps the width at 456 px (FlyoutThemeMaxWidth, not settable from reactor 0.100.0) and scrolls sideways past it,
    /// which five columns overran.
    fn dimm_table(&self, s: &SystemPanel) -> View {
        let p = self.palette();
        const HEADINGS: [&str; 4] = ["Locator", "Manufacturer · part number", "Capacity", "MT/s"];
        let numeric = |c: usize| c >= 2;
        let align = |c: usize| if numeric(c) { HorizontalAlignment::Right } else { HorizontalAlignment::Left };
        let mut cells: Vec<(u64, View)> = Vec::new();
        for (c, heading) in HEADINGS.iter().enumerate() {
            cells.push((
                c as u64,
                TextBlock::new()
                    .text(*heading)
                    .font_weight(FontWeight::SEMI_BOLD)
                    .foreground(brush(p.text_secondary))
                    .horizontal_alignment(align(c))
                    .grid_row(0)
                    .grid_column(c as i32)
                    .into(),
            ));
        }
        for (r, d) in s.dimms.iter().enumerate() {
            let values = [&d.locator, &d.part, &d.capacity, &d.speed];
            for (c, value) in values.into_iter().enumerate() {
                cells.push((
                    ((r + 1) * 8 + c) as u64,
                    TextBlock::new()
                        .text(value.clone())
                        .foreground(brush(p.text_primary))
                        .is_text_selection_enabled(true)
                        .horizontal_alignment(align(c))
                        .grid_row((r + 1) as i32)
                        .grid_column(c as i32)
                        .into(),
                ));
            }
        }
        StackPanel::new().spacing(10.0).children((
            TextBlock::new()
                .text(format!("Memory modules · {}", s.dimm_title))
                .font_weight(FontWeight::SEMI_BOLD)
                .foreground(brush(p.text_primary)),
            Grid::new()
                .column_spacing(16.0)
                .row_spacing(4.0)
                .rows(std::iter::repeat_n(GridLength::Auto, s.dimms.len() + 1))
                .columns([GridLength::Auto; 4])
                .keyed_children(cells),
        ))
    }

    fn position(&self, row: i32) -> View {
        let p = self.palette();
        let paused = matches!(self.model.status, Status::Stopping | Status::Interrupted);
        StackPanel::new().grid_row(row).spacing(6.0).children((
            TextBlock::new()
                .text(self.model.position.clone())
                .font_size(16.0)
                .font_weight(FontWeight::SEMI_BOLD)
                .foreground(brush(p.text_primary))
                .text_trimming(TextTrimming::CharacterEllipsis),
            progress_bar(self.model.fraction, paused, self.width - 2.0 * PAGE_MARGIN_X, p),
        ))
    }

    fn results(&self, row: i32) -> View {
        let p = self.palette();
        let header = table_row(&StepRow::HEADINGS.map(str::to_string), p, None, true, [false; 7]);
        let stripe = brush(p.stripe);
        // Newest first: reactor 0.100.0 exposes no ScrollIntoView / scroll offset, so a list in step order could not follow the latest row.
        // Rows are keyed by their place in the run, so a new step inserts one native row and leaves the others alone.
        let newest = self.model.rows.iter().enumerate().rev().take(SHOWN);
        let rows = newest
            .map(|(i, r): (usize, &StepRow)| (i as u64, table_row(&r.cells(), p, (i % 2 == 1).then_some(stripe), false, r.flagged())));
        let body: View = if self.model.rows.is_empty() {
            TextBlock::new().text("No steps finished yet").foreground(brush(p.text_secondary)).margin(Thickness::xy(8.0, 6.0)).into()
        } else {
            ScrollViewer::new().vertical_scroll_bar_visibility(ScrollBarVisibility::Auto).content(StackPanel::new().keyed_children(rows))
        };
        let steps = self.model.rows.len();
        let shown = if steps > SHOWN { format!(", the newest {SHOWN} shown") } else { String::new() };
        card(
            row,
            p,
            card_title(format!("Results · {steps} step{}, newest first{shown}", if steps == 1 { "" } else { "s" }), p),
            Grid::new()
                .rows([GridLength::Auto, GridLength::STAR])
                .row_spacing(2.0)
                .children((Border::new().grid_row(0).content(header), Border::new().grid_row(1).content(body))),
        )
    }

    fn chart_card(&self, row: i32) -> View {
        let p = self.palette();
        let series = &self.model.series;
        let latest = series.latest().map_or_else(|| "–".to_owned(), crate::model::thousands);
        let marks = series.marks().count();
        let title = format!("Throughput (MiB/s) · last 60 s · now {latest}");
        // Title on the left, a two-item legend on the right (line = MiB/s, red mark = errors rose). A temperature series would add a third
        // legend item here.
        let header = Grid::new().columns([GridLength::STAR, GridLength::Auto]).children((
            card_title(title, p).grid_column(0),
            StackPanel::new().grid_column(1).orientation(Orientation::Horizontal).spacing(6.0).children((
                Border::new().width(16.0).height(2.0).vertical_alignment(VerticalAlignment::Center).background(brush(p.accent)),
                legend_text("MiB/s", p),
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

    /// The parameters box, then Start and Stop, which keep WinUI's own button colours (their hover, pressed and disabled states). The box
    /// takes the CLI's parameters, as tmr.exe does; Start runs it once, one session a process, and from then on it holds what ran.
    fn controls(&self, row: i32, context: &ViewContext<Self>) -> View {
        let ready = self.model.status == Status::Ready;
        Grid::new().grid_row(row).column_spacing(8.0).columns([GridLength::STAR, GridLength::Auto, GridLength::Auto]).children((
            TextBox::new()
                .text(self.model.args.clone())
                .placeholder_text("Parameters, as on the command line: memory=50% cycles=3 (none: the defaults)")
                .is_enabled(ready)
                .on_text_changed(context.callback(Message::Args))
                .grid_column(0)
                .tooltip(
                    "What Start runs, in tmr.exe's key=value syntax (tmr.exe --help lists them). \
                              Quote one with a space in it: \"config=C:\\My configs\\x.cfg\"",
                ),
            Button::new()
                .style(ButtonStyle::Accent)
                .is_enabled(ready)
                .width(96.0)
                .grid_column(1)
                .on_click(context.callback(|()| Message::Start))
                .content("Start"),
            Button::new()
                .is_enabled(self.model.status == Status::Running)
                .width(96.0)
                .grid_column(2)
                .on_click(context.callback(|()| Message::Stop))
                .content("Stop"),
        ))
    }

    /// The large-page restart question, the console's A/B as a dialog, open while the session's thread waits for its answer. Esc is the
    /// close button's: 4 KiB pages, as an empty answer is on the console.
    fn restart_dialog(&self, context: &ViewContext<Self>) -> View {
        // The text in the dialog's own colours: it follows the system's theme, not the palette's
        ContentDialog::new()
            .title("Large pages need Windows restarted")
            .primary_button_text("Exit, to restart Windows")
            .close_button_text("Continue on 4 KiB pages")
            .is_open(self.model.restart_asked())
            .on_closed(context.callback(Message::Restart))
            .content(TextBlock::new().text(RESTART_QUESTION).text_wrapping(TextWrapping::Wrap))
    }
}

/// The restart dialog's text, the console's question (`tmr_cli`'s prompts) in a window's words
const RESTART_QUESTION: &str = "The large-page privilege is set up, but takes effect only after Windows restarts.\n\n\
    Exit TMR and restart Windows: TMR then runs on large pages (recommended for the most memory stress).\n\n\
    Or continue on standard 4 KiB pages, about 5-10% slower: every test still runs and every pattern is still \
    checked, but slower memory traffic is less stress, so marginal errors that only appear at full bandwidth may \
    go undetected.";

/// egui's progress bar, from reactor primitives: a fully rounded track, `width` wide, a rounded fill sized to the fraction by two star
/// columns, and the percentage centred over both. Determinate, no animation; a stopped run dims the fill. A fill begun is never narrower
/// than it is high (egui's `min_width`), so its rounding stays round: a run's first percent is a dot.
fn progress_bar(fraction: f64, paused: bool, width: f64, p: &Palette) -> View {
    let f = fraction.clamp(0.0, 1.0);
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
                    .text(format!("{:.0}%", f * 100.0))
                    .font_size(12.0)
                    .font_weight(FontWeight::SEMI_BOLD)
                    .foreground(brush(p.progress_text))
                    .horizontal_alignment(HorizontalAlignment::Center)
                    .vertical_alignment(VerticalAlignment::Center)
                    .grid_column(0)
                    .grid_column_span(2),
            )),
        )
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

/// `StepRow::HEADINGS`'s columns: C# and T# hold 3 digits, a night's cycles
const TABLE_COLUMNS: [GridLength; 7] = [
    GridLength::Pixel(32.0),
    GridLength::Pixel(32.0),
    GridLength::Star(1.0),
    GridLength::Pixel(88.0),
    GridLength::Pixel(64.0),
    GridLength::Pixel(80.0),
    GridLength::Pixel(56.0),
];

/// One table line; the header uses the same columns and padding so the columns line up. `flagged` marks a non-zero count: palette error
/// red, semi-bold (egui's `.strong()`).
fn table_row(cells: &[String; 7], p: &Palette, stripe: Option<Brush>, header: bool, flagged: [bool; 7]) -> View {
    let weight = if header { FontWeight::SEMI_BOLD } else { FontWeight::NORMAL };
    let text = if header { p.text_secondary } else { p.text_primary };
    let cell = |column: usize| {
        let flag = flagged[column];
        TextBlock::new()
            .text(cells[column].clone())
            .font_weight(if flag { FontWeight::SEMI_BOLD } else { weight })
            .foreground(brush(if flag { p.error } else { text }))
            .text_trimming(TextTrimming::CharacterEllipsis)
            .horizontal_alignment(if column == 2 { HorizontalAlignment::Left } else { HorizontalAlignment::Right })
            .grid_column(column as i32)
    };
    Border::new().background_optional(stripe).corner_radius(4.0).padding(Thickness::new(8.0, 4.0, 12.0, 4.0)).content(
        Grid::new().columns(TABLE_COLUMNS).column_spacing(12.0).children((cell(0), cell(1), cell(2), cell(3), cell(4), cell(5), cell(6))),
    )
}
