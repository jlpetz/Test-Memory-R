//! The window's state (TODO 7): what the session's events and the progress snapshots say, in the form the window draws it. Plain Rust with
//! no Reactor type, so the translation is tested apart from the window, which `window.rs` builds from it.
//!
//! The events come as the engine sends them, at most a batch every 500 ms; a snapshot comes with each batch, and with each 500 ms that
//! brought none (`tick`), and so do the lines logged since (`logged`). The fast figures, the runtime and the time left, the live error
//! count, the position, the test's figures, the progress and the chart's speed, are the snapshot's, each in the words of the console's
//! ticker (crates/tmr-cli's `render/ticker.rs`, whose golden the tests check them against); the events give the rest: the setup's the
//! system panel, the first facts and the tabs' sections (`sections.rs`), the run's the rest, each step's report and the final summary's
//! among them (`report.rs`, TODO 109).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::SyncSender;
use std::time::Duration;

use tmr::commands::ResultsComparison;
use tmr::frontend::{
    AllocationReport, BROAD_MARK, BROAD_RISK, ErrorCounts, Event, LargePageAnswer, LargePageQuestion, NotRun, ProgressSnapshot,
    ProgressTracker, Run, RunOutcome, RunStatus, SealOp, Setup, StepReport, TestConfigurationReport, TestSnapshot, ThreadPriority,
};
use tmr::session::{SessionEnd, Stopped};

use crate::logger::LogLine;
use crate::report::Report;
use crate::sections::Sections;
use crate::sysinfo::{Machine, MemoryLine, SystemPanel};

const MIB: f64 = 1024.0 * 1024.0;
const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

/// The entries the errors list keeps, the newest (the window shows the newest 200 of them): an error storm over a night would hold
/// gigabytes of log lines. The log file has every line.
pub const ERRORS_KEPT: usize = 10_000;

/// The log lines the Log tab keeps, the newest (TODO 109): the log file has every line
pub const LOG_KEPT: usize = 10_000;

/// The large-page dialog's words (`Model::large_page_dialog`), as `window.rs` draws them (TODO 113): its title, its buttons' (the
/// secondary 4 KiB pages this time; the close button, as Esc, quits, and an empty one has none), its text, and for the grant the
/// candidates as its list shows them and the one chosen, which mirrors the control: none chosen disables the grant; and the broad mark's
/// legend, under the list, when the list has the mark (as the console has it).
#[derive(Debug, PartialEq, Eq)]
pub struct LargePageDialog {
    pub title: &'static str,
    pub primary: &'static str,
    pub secondary: &'static str,
    pub close: &'static str,
    pub text: String,
    pub choices: Vec<GrantRow>,
    pub chosen: Option<usize>,
    pub legend: Option<String>,
}

/// A candidate's row in the grant's list (TODO 113, the owner: a list, not radio buttons spread apart): User or Group, its name, and a
/// note, "You, recommended" for the account and the broad mark (its legend under the list) for a broad group
#[derive(Debug, PartialEq, Eq)]
pub struct GrantRow {
    pub kind: &'static str,
    pub name: String,
    pub note: String,
}

/// The grant's dialog for the window's tests (`window.rs`): the account, a group, and two broad ones, one with a long name
#[cfg(test)]
pub(crate) fn grant_dialog_for_tests() -> LargePageDialog {
    let row = |kind, name: &str, note: &str| GrantRow { kind, name: name.to_string(), note: note.to_string() };
    let broad = BROAD_MARK;
    LargePageDialog {
        title: "Large pages need the 'Lock pages in memory' right",
        primary: "Grant and exit",
        secondary: "Use 4 KiB pages",
        close: "Quit",
        text: "words".to_string(),
        choices: vec![
            row("User", r"PC\owner", "You, recommended"),
            row("Group", r"BUILTIN\Administrators", ""),
            row("Group", r"NT AUTHORITY\Local account and member of Administrators group", broad),
            row("Group", "Everyone", broad),
        ],
        chosen: Some(0),
        legend: Some(format!("{BROAD_MARK} {}", tmr::frontend::broad_legend())),
    }
}

/// Which of a large-page dialog's buttons closed it (`Model::answer_large_pages`), as `window.rs` reads WinUI's result: the primary, the
/// secondary, or neither (the close button, Esc, or the dialog closed any other way)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DialogButton {
    Primary,
    Secondary,
    Close,
}

/// The second dialog after a broad pick (`Model::broad_dialog`): its title, its secondary button's (`grant`), the only one that grants,
/// and its text. Its primary button is No, its default, which goes back to the first: WinUI's dialog has no default button, but it
/// focuses its first button when its text has nothing to focus, and a double-click on the first dialog's grant lands there too, so the
/// first button never grants.
#[derive(Debug, PartialEq, Eq)]
pub struct BroadDialog {
    pub title: String,
    pub grant: String,
    pub text: String,
}

/// What 4 KiB pages mean, the end of each question
const FOUR_KIB_PAGES: &str = "standard 4 KiB pages, about 5-10% slower: every test still runs and every pattern is still checked, but \
                              slower memory traffic is less stress, so marginal errors that only appear at full bandwidth may go \
                              undetected.";

/// What a quit, or the dialog closed, does (TODO 113, decision 5a): the last words of the questions that have a Quit button
const QUIT_RUNS_NOTHING: &str = "Quit, or this dialog closed, runs nothing: TMR takes 4 KiB pages only when told to.";

/// The step reports kept, the newest steps': as many as the window draws rows (TODO 109), so every row drawn opens one. A report holds a
/// line for each thread.
pub const REPORTS_KEPT: usize = 200;

/// The status badge: where the session is, then how its run ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// Waiting for Start
    Ready,
    Running,
    /// Stop was pressed: the step running ends at its next check, and the run reports what it has
    Stopping,
    Passed,
    Failed,
    Interrupted,
    /// The session ended with no suite run: setup or a config stopped it, `--startup-debug` showed only the plan, or it named a one-shot
    /// command (`--help`), which the window doesn't run
    NotRun,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Status::Ready => "Ready",
            Status::Running => "Running",
            Status::Stopping => "Stopping",
            Status::Passed => "Passed",
            Status::Failed => "Failed",
            Status::Interrupted => "Interrupted",
            Status::NotRun => "Not run",
        }
    }

    fn of(status: RunStatus) -> Self {
        match status {
            RunStatus::Passed => Status::Passed,
            RunStatus::Failed => Status::Failed,
            RunStatus::NotRun => Status::NotRun,
            RunStatus::Interrupted => Status::Interrupted,
        }
    }
}

/// A finished step: a row of the results table.
#[derive(Clone, Debug, PartialEq)]
pub struct StepRow {
    pub cycle: u64,
    pub step: usize,
    pub label: String,
    /// How long it ran, the time its report gives and the console's stamp under it (`Test 6/6 Mem-Refresh512 (00:00:15)`)
    pub time: Duration,
    /// Its test's speed, the figure the console's step report gives
    pub mib_s: f64,
    pub errors: u64,
    /// What the seal's checks before its chunks found; `None` in an unsealed run
    pub seal_errors: Option<u64>,
    /// WHEA events while it ran
    pub whea: u64,
}

impl StepRow {
    fn of(report: &StepReport) -> Self {
        let s = &report.summary;
        StepRow {
            cycle: report.cycle,
            step: s.step,
            label: s.label.clone(),
            time: report.wall,
            mib_s: s.throughput_mib_s,
            errors: s.errors,
            seal_errors: report.sealed.then_some(s.seal_errors),
            whea: s.whea_total,
        }
    }

    /// The table's columns
    pub const COLUMNS: usize = 8;

    /// The table's headings: the cycle, the step's number in it (the ticker's `Test 2/6`), then the rest of `cells`, its errors in their
    /// parts (TODO 109): the test's, the seal's, WHEA's
    pub const HEADINGS: [&str; Self::COLUMNS] = ["C#", "T#", "Step", "Time", "MiB/s", "Test", "Seal", "WHEA"];

    /// Its cells: the cycle, the step, the step's label, its time, MiB/s, its test's errors, the seal's (`–` unsealed), WHEA's
    pub fn cells(&self) -> [String; Self::COLUMNS] {
        [
            self.cycle.to_string(),
            self.step.to_string(),
            self.label.clone(),
            hms(self.time),
            thousands(self.mib_s),
            self.errors.to_string(),
            self.seal_errors.map_or("–".to_string(), |errors| errors.to_string()),
            self.whea.to_string(),
        ]
    }

    /// The cells a non-zero count flags, which take the error colour: the errors, the seal's and WHEA's
    pub fn flagged(&self) -> [bool; Self::COLUMNS] {
        [false, false, false, false, false, self.errors > 0, self.seal_errors.is_some_and(|errors| errors > 0), self.whea > 0]
    }
}

/// The report open beside the results (TODO 109): a step's, by its row, or the run's final summary
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Opened {
    Step(usize),
    Summary,
}

/// The run's final summary (TODO 109): its row, above the steps', in their columns, and its report
#[derive(Clone, Debug)]
pub struct Summary {
    pub cells: [String; StepRow::COLUMNS],
    pub flagged: [bool; StepRow::COLUMNS],
    pub report: Report,
}

impl Summary {
    fn of(report: &tmr::frontend::FinalTestSummaryReport, work: Vec<crate::report::Block>) -> Self {
        // The run's errors in the parts the rows count (TODO 109)
        let counts = report.errors();
        let (errors, seal) = (counts.test, counts.sealed.then_some(counts.seal));
        let cells = [
            String::new(),
            String::new(),
            "Final summary".to_string(),
            report.total_runtime.clone(),
            thousands(report.overall_throughput_mib_s),
            errors.to_string(),
            seal.map_or("–".to_string(), |errors| errors.to_string()),
            report.whea_total.to_string(),
        ];
        let flagged = [false, false, false, false, false, errors > 0, seal.is_some_and(|errors| errors > 0), report.whea_total > 0];
        Summary { cells, flagged, report: Report::of_summary(report, work) }
    }
}

/// One of the chart's samples (TODO 109): a test's speed in MiB/s; the seal's, a run-wide seal stage's or the seal's work before a step's
/// test; or none, a gap, where there are no figures: between tests, and before a test's first, through the seal's checks inside its chunks
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Sample {
    None,
    Test(f64),
    Seal(f64),
    /// The test in the pass that wraps each chunk in the seal, of a step that makes more (TODO 109): its chunks start in cache, so it can
    /// run apart from the rest, and the chart draws it in the seal's colour
    SealedPass(f64),
}

impl Sample {
    /// Its speed, or `None` for a gap
    pub fn value(self) -> Option<f64> {
        match self {
            Sample::None => None,
            Sample::Test(mib_s) | Sample::Seal(mib_s) | Sample::SealedPass(mib_s) => Some(mib_s),
        }
    }
}

/// What the chart plots: the last `SAMPLES` speeds, one per 500 ms, the samples at which the run's errors rose, and those at which a stage
/// began (TODO 109).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Series {
    pub samples: VecDeque<Sample>,
    /// Samples received since the run started; the newest is number `received - 1`
    pub received: u64,
    /// The numbers of the samples at which the errors rose, oldest first. Marks that scroll off the left edge are dropped.
    pub error_marks: VecDeque<u64>,
    /// The numbers of the samples at which a stage began, with its label (`stage_label`), oldest first; dropped as the error marks are
    pub stage_marks: VecDeque<(u64, String)>,
    // A temperature series would go here: one DIMM temperature per sample (SPD hub sensor, or HWiNFO's shared memory; see doc/todo/7-gui.md
    // note 3), drawn on a second y axis on the right so temperature, errors and throughput share this one box.
}

/// The samples the chart shows, a minute of them
pub const SAMPLES: usize = 120;

impl Series {
    pub fn push(&mut self, sample: Sample) {
        if self.samples.len() == SAMPLES {
            self.samples.pop_front();
        }
        self.samples.push_back(sample);
        self.received += 1;
        let oldest = self.received - self.samples.len() as u64;
        while self.error_marks.front().is_some_and(|&m| m < oldest) {
            self.error_marks.pop_front();
        }
        while self.stage_marks.front().is_some_and(|(m, _)| *m < oldest) {
            self.stage_marks.pop_front();
        }
    }

    /// The errors rose by the newest sample
    pub fn mark_newest(&mut self) {
        if let Some(newest) = self.received.checked_sub(1)
            && self.error_marks.back() != Some(&newest)
        {
            self.error_marks.push_back(newest);
        }
    }

    /// A stage began by the newest sample, `label` it
    pub fn mark_stage(&mut self, label: String) {
        if let Some(newest) = self.received.checked_sub(1) {
            self.stage_marks.push_back((newest, label));
        }
    }

    pub fn latest(&self) -> Option<Sample> {
        self.samples.back().copied()
    }

    /// How many samples before the newest one sample number `m` is, if it is still shown, and its index
    fn shown(&self, m: u64) -> Option<(usize, usize)> {
        let n = self.samples.len();
        let index = usize::try_from(m.checked_sub(self.received - n as u64)?).ok().filter(|&i| i < n)?;
        Some((n - 1 - index, index))
    }

    /// Each visible error mark: its sample's number, how many samples before the newest it is, and its value (`None` in a gap)
    pub fn marks(&self) -> impl Iterator<Item = (u64, usize, Option<f64>)> + '_ {
        self.error_marks.iter().filter_map(|&m| self.shown(m).map(|(ago, index)| (m, ago, self.samples[index].value())))
    }

    /// Each visible stage mark: its sample's number, how many samples before the newest it is, and its label
    pub fn stages(&self) -> impl Iterator<Item = (u64, usize, &str)> + '_ {
        self.stage_marks.iter().filter_map(|(m, label)| self.shown(*m).map(|(ago, _)| (*m, ago, label.as_str())))
    }

    /// The top of the y axis: the samples' largest, rounded up to 1, 2, 4, 6 or 8 times a power of ten, so its four divisions take round
    /// labels
    pub fn axis_top(&self) -> f64 {
        axis_top(self.samples.iter().filter_map(|s| s.value()).fold(0.0, f64::max))
    }
}

/// The top of an axis that has to hold `max`, at least 100: 1, 2, 4, 6 or 8 times a power of ten.
pub fn axis_top(max: f64) -> f64 {
    let max = max.max(100.0);
    let power = 10f64.powf(max.log10().floor());
    [1.0, 2.0, 4.0, 6.0, 8.0, 10.0].into_iter().map(|m| m * power).find(|&top| top >= max).unwrap_or(10.0 * power)
}

/// An axis label: `750`, `1.5K`, `80K`, `2.5M`.
pub fn axis_label(value: f64) -> String {
    let (scaled, unit) = if value >= 1e6 {
        (value / 1e6, "M")
    } else if value >= 1e3 {
        (value / 1e3, "K")
    } else {
        (value, "")
    };
    let text = format!("{scaled:.1}");
    format!("{}{unit}", text.trim_end_matches('0').trim_end_matches('.'))
}

/// A list of lines, the errors list's and the log's: its newest `cap` entries, oldest first, and how many came before them. Each entry has
/// a number, its place among all of them, which stays its own as older ones go: the window's key for it.
#[derive(Debug)]
pub struct Lines {
    kept: VecDeque<String>,
    /// The entries let go, the oldest, to keep `cap`
    let_go: u64,
    cap: usize,
}

impl Lines {
    pub fn new(cap: usize) -> Self {
        Lines { kept: VecDeque::new(), let_go: 0, cap }
    }

    pub fn push(&mut self, entry: String) {
        if self.kept.len() == self.cap {
            self.kept.pop_front();
            self.let_go += 1;
        }
        self.kept.push_back(entry);
    }

    /// Every entry so far, those let go included
    pub fn total(&self) -> u64 {
        self.let_go + self.kept.len() as u64
    }

    pub fn is_empty(&self) -> bool {
        self.kept.is_empty()
    }

    /// The newest `n` entries, newest first, each with its number
    pub fn newest(&self, n: usize) -> impl Iterator<Item = (u64, &str)> {
        self.kept.iter().enumerate().rev().take(n).map(|(i, entry)| (self.let_go + i as u64, entry.as_str()))
    }

    /// The newest `n` entries in their order, oldest first, as a log reads (TODO 109), each with its number
    pub fn last(&self, n: usize) -> impl Iterator<Item = (u64, &str)> {
        let skip = self.kept.len().saturating_sub(n);
        self.kept.iter().enumerate().skip(skip).map(|(i, entry)| (self.let_go + i as u64, entry.as_str()))
    }
}

impl Extend<String> for Lines {
    fn extend<I: IntoIterator<Item = String>>(&mut self, entries: I) {
        for entry in entries {
            self.push(entry);
        }
    }
}

/// The entries kept, oldest first, as a test lists them
#[cfg(test)]
impl<const N: usize> PartialEq<[&str; N]> for Lines {
    fn eq(&self, entries: &[&str; N]) -> bool {
        self.kept == *entries
    }
}

/// The run's error count, as the ticker gives it: `0 errors (0 test + 0 seal + 0 WHEA)`, `⚠️ 7 errors (2 test + 2 seal + 3 WHEA
/// 1C/2UC)` (`ErrorCounts`, TODO 109). It counts the running test's errors so far; the WHEA split is corrected and uncorrected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ErrorCount {
    pub text: String,
    /// Errors or WHEA reports: the ticker's ⚠️, and the count in the error colour
    pub alarm: bool,
}

impl ErrorCount {
    fn of(errors: ErrorCounts) -> Self {
        let alarm = errors.total() > 0;
        let flag = if alarm { "⚠️ " } else { "" };
        ErrorCount { text: format!("{flag}{errors}"), alarm }
    }
}

/// The progress bar: how far the run is, the fill and its label, and what it follows, which its tooltip says.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bar {
    /// 0 to 1
    pub fraction: f64,
    /// Rounded down from the counts the fraction is of, as the ticker's percentages are: a bar the cycle limit drives says what the
    /// position line does, `39%` where rounding would say 40%
    pub percent: u128,
    pub about: &'static str,
}

impl Bar {
    /// Before the suite
    const NONE: Bar = Bar { fraction: 0.0, percent: 0, about: "How far the run is, once its suite starts" };
    const CYCLES: &str = "How far the run is: its finished tests, against all its cycles'";
    const TIME: &str = "How far the run is: its runtime, against its time limit";
    const CYCLE: &str = "How far the cycle is: its finished tests (no limit ends the run)";
}

/// The session's facts, for the info line under the title.
#[derive(Debug, Default)]
struct Facts {
    /// `8 threads · 28.0 GiB · 1 GiB pages`, and before allocation the threads alone
    allocation: Option<String>,
    /// `plan stability · 16 steps a cycle · sealed`, and before the test plan the plan's name alone
    plan: Option<String>,
    /// The built-in plan the run takes, from setup
    plan_name: Option<&'static str>,
    /// Large pages can't be had: the run takes 4 KiB pages, or stops when its minpage= needs them
    no_large_pages: bool,
    /// `2 tests left out (no AVX-512F)`
    left_out: Option<String>,
    whea_unmonitored: bool,
    /// The results file, once the run saved it
    results: Option<String>,
}

/// The window's state.
pub struct Model {
    pub status: Status,
    /// The suite's clock: 0 until it starts, held when it ends
    pub runtime: Duration,
    /// Under the runtime, while the suite runs to a time limit: `remaining 00:18:50`, then `limit reached, finishing cycle`
    pub time_left: Option<String>,
    /// The run's errors and WHEA's, live, from the suite's start on
    pub error_count: Option<ErrorCount>,
    /// Where the run is: the cycle, with its clock, as the ticker gives it (`cycle_line`), and before the suite and after it what the
    /// session is doing and how the run ended. The run's errors go beside it (TODO 109: the dynamic figures together, as the ticker's).
    pub position: String,
    /// The test running, with its clock, as the ticker's second line starts (`test_line`); its figures go beside it (`live`)
    pub test_position: Option<String>,
    /// What the running test is doing, beside the position: its stage and its figures (`live`)
    pub live: Option<String>,
    /// How far the run is: its cycles, or its time when that ends it sooner
    pub bar: Bar,
    /// The physical memory Windows reports, from setup's memory status and the memory plan
    pub ram_bytes: Option<u64>,
    /// The system panel, once setup has detected the machine
    pub system: Option<SystemPanel>,
    pub rows: Vec<StepRow>,
    /// The newest steps' reports, `REPORTS_KEPT` of them, the last row's last
    reports: VecDeque<Report>,
    /// The run's final summary, once it has one
    pub summary: Option<Summary>,
    /// The report open beside the results
    pub opened: Option<Opened>,
    /// The run's work by thread, CPU and core, which comes just before the final summary, whose report takes it (TODO 109)
    work: Vec<crate::report::Block>,
    /// The Compare tab's files and comparison
    pub compare: CompareTab,
    /// Steps with errors, final seal checks that found some, WHEA reports, the warnings and errors logged, and why a run stopped, oldest
    /// first, the newest `ERRORS_KEPT`
    pub errors: Lines,
    /// The lines logged, the Log tab's, the newest `LOG_KEPT` (TODO 109)
    pub log: Lines,
    /// The System, Threads and Memory tabs' (TODO 109)
    pub sections: Sections,
    pub series: Series,
    /// The stage the chart last marked: its cycle, its number and its name (TODO 109)
    stage: Option<(u32, u64, String)>,
    /// The parameters Start runs, as the parameters box has them: the CLI's `key=value` syntax (`args_of`)
    pub args: String,
    /// The exe's name, which Start puts before them
    program: String,
    /// The run's log file, from Start on (`tmr::logfile::path`, whose name is fixed as Start asks for it)
    pub log_path: Option<PathBuf>,
    facts: Facts,
    /// The large-page question, while it waits for the dialog's answer: the session's thread waits on the reply
    reply: Option<SyncSender<LargePageAnswer>>,
    /// What the last large-page question asked, kept after its answer, so the dialog's words hold while it closes
    question: Option<LargePageQuestion>,
    /// The grant's candidate chosen, as its radio buttons have it (`choose_target`): none chosen disables the grant
    chosen: Option<usize>,
    /// A broad candidate picked, its second dialog open, waiting for its answer (`confirm_broad`)
    confirming: Option<usize>,
    /// The lines the logger's channel dropped, as it last counted them
    log_lines_dropped: u64,
    /// The suite's progress, from its start on
    progress: Option<Arc<ProgressTracker>>,
    /// The cycle running, and the last one finished, whether it stopped early
    cycle: u32,
    cycles_finished: u32,
    last_partial: bool,
    /// The run's errors and WHEA events the snapshots have shown, so a rise marks the chart
    errors_seen: u64,
    /// The suite has ended, or the session did with no suite run: the snapshots are ignored
    over: bool,
    /// The session's thread has ended: Run again can start another (TODO 109)
    session_over: bool,
    /// The priority the window's thread takes (`window-priority=`): the selection's from setup's selection until the session ends,
    /// normal before and after
    window_priority: ThreadPriority,
    /// A calibration runs: the run's, once it has allocated, before its tests (`calibrate=`, TODO 22), or the window's own (TODO 110)
    calibrating: bool,
    /// The cache calibration tmr-cfg.json holds, as the info line says it before a run (TODO 110); the window reads it as it opens and
    /// after its own calibration
    pub stored_calibration: Option<String>,
}

impl Model {
    /// Ready to run `args`, the command line the exe was given (its name first), which the parameters box starts with
    pub fn new(args: &[String]) -> Self {
        Model {
            status: Status::Ready,
            runtime: Duration::ZERO,
            time_left: None,
            error_count: None,
            position: "Ready".to_string(),
            test_position: None,
            live: None,
            bar: Bar::NONE,
            ram_bytes: None,
            system: None,
            rows: Vec::new(),
            reports: VecDeque::new(),
            summary: None,
            opened: None,
            work: Vec::new(),
            compare: CompareTab::default(),
            errors: Lines::new(ERRORS_KEPT),
            log: Lines::new(LOG_KEPT),
            sections: Sections::default(),
            series: Series::default(),
            stage: None,
            args: args_text(args.get(1..).unwrap_or_default()),
            program: args.first().cloned().unwrap_or_else(|| "tmr".to_string()),
            log_path: None,
            facts: Facts::default(),
            reply: None,
            question: None,
            chosen: None,
            confirming: None,
            log_lines_dropped: 0,
            progress: None,
            cycle: 0,
            cycles_finished: 0,
            last_partial: false,
            errors_seen: 0,
            over: false,
            session_over: false,
            window_priority: ThreadPriority::Normal,
            calibrating: false,
            stored_calibration: None,
        }
    }

    /// The command line Start runs: the exe's name, then the parameters box's
    pub fn command(&self) -> Vec<String> {
        std::iter::once(self.program.clone()).chain(args_of(&self.args)).collect()
    }

    /// A config chosen in the Open dialog (TODO 109): `config=<path>` in the parameters box, in place of the one there
    pub fn set_config(&mut self, path: &Path) {
        let mut args: Vec<String> = args_of(&self.args).into_iter().filter(|arg| !arg.starts_with("config=")).collect();
        args.push(format!("config={}", path.display()));
        self.args = args_text(&args);
    }

    /// The results file, once the run has saved it
    pub fn results_path(&self) -> Option<&str> {
        self.facts.results.as_deref()
    }

    /// The next run's, Run again's (TODO 109): a new run's state, for the command the parameters box has, the Compare tab's files kept
    pub fn again(&mut self) -> Model {
        let mut next = Model::new(&self.command());
        next.compare = std::mem::take(&mut self.compare);
        next
    }

    /// The session was started
    pub fn start(&mut self) {
        self.status = Status::Running;
        self.position = "Setting up".to_string();
    }

    /// Stop was pressed
    pub fn stop(&mut self) {
        if self.status == Status::Running {
            self.status = Status::Stopping;
        }
    }

    /// The header's clock, the suite's runtime: `00:41:10`
    pub fn runtime_line(&self) -> String {
        hms(self.runtime)
    }

    /// The suite's progress, once it started
    pub fn progress(&self) -> Option<&Arc<ProgressTracker>> {
        self.progress.as_ref()
    }

    /// The system panel's memory line, once setup has detected the machine: the modules' total and count, and Windows' figure
    pub fn memory_line(&self) -> Option<MemoryLine> {
        self.system.as_ref().map(|system| system.memory_line(self.ram_bytes))
    }

    /// A DIMM was chosen in the system panel's selector, by its menu item (`Dimm::choice`)
    pub fn choose_dimm(&mut self, choice: &str) {
        if let Some(system) = &mut self.system {
            system.choose(choice);
        }
    }

    /// Row `row`'s report, while it is kept
    fn step_report(&self, row: usize) -> Option<&Report> {
        let first = self.rows.len() - self.reports.len();
        row.checked_sub(first).and_then(|i| self.reports.get(i))
    }

    /// Row `row` opens a report: its report is kept
    pub fn opens(&self, row: usize) -> bool {
        self.step_report(row).is_some()
    }

    /// The report open beside the results, if it is still kept
    pub fn opened_report(&self) -> Option<&Report> {
        match self.opened? {
            Opened::Step(row) => self.step_report(row),
            Opened::Summary => self.summary.as_ref().map(|summary| &summary.report),
        }
    }

    /// A row was clicked: its report opens, or closes when it is the one open
    pub fn open(&mut self, which: Opened) {
        self.opened = if self.opened == Some(which) { None } else { Some(which) };
    }

    /// The info line: the session's facts, or until there are any, the command Start runs (ran)
    pub fn info_line(&self) -> String {
        let f = &self.facts;
        let facts: Vec<&str> = [
            f.allocation.as_deref(),
            f.plan.as_deref(),
            f.no_large_pages.then_some("no large pages"),
            f.left_out.as_deref(),
            f.whea_unmonitored.then_some("WHEA not monitored"),
            f.results.as_deref(),
        ]
        .into_iter()
        .flatten()
        .collect();
        // Before a run, after the command: what calibration the run finds (TODO 110)
        let stored = self.stored_calibration.as_deref().map_or(String::new(), |stored| format!(" · {stored}"));
        if !facts.is_empty() {
            facts.join(" · ")
        } else if self.args.trim().is_empty() {
            format!("tmr{stored}")
        } else {
            format!("tmr {}{stored}", self.args.trim())
        }
    }

    pub fn apply(&mut self, event: Event) {
        // No wildcard arm, as the CLI's: an event the window doesn't know fails to compile
        match event {
            Event::Setup(setup) => self.setup(setup),
            Event::Run(run) => self.run(run),
        }
    }

    fn setup(&mut self, setup: Setup) {
        self.sections.setup(&setup);
        match setup {
            // The console's alone: the parameters as given, and the detail behind the facts below
            Setup::TestFilter { .. }
            | Setup::QuickTest
            | Setup::ParamNotes(_)
            | Setup::StartupHeader { .. }
            | Setup::StartupParams { .. }
            | Setup::CpuType { .. }
            | Setup::CpuCount { .. }
            | Setup::LargePageDiagnostic { .. }
            | Setup::SealCalibration(_)
            | Setup::CpuTopology { .. } => {}
            Setup::Selection(selection) => {
                self.facts.allocation = Some(format!("{} thread{}", selection.threads, plural(selection.threads)));
                self.window_priority = selection.window_priority;
            }
            Setup::CacheCalibration(state) => {
                use tmr::frontend::CacheCalibrationState::{Measuring, Rechecking};
                self.calibrating = matches!(state, Measuring(_) | Rechecking { .. });
            }
            Setup::LargePageCheck => self.position = "Checking large pages".to_string(),
            // After the dialog's grant
            Setup::LargePageGrant { outcome: Ok(()), .. } => self.position = "Exiting, to sign out and back in".to_string(),
            Setup::LargePageGrant { account, outcome: Err(e) } => {
                self.position = "Not granted: stopping before allocating".to_string();
                self.errors.push(format!("Couldn't grant 'Lock pages in memory' to {account}: {e}"));
            }
            Setup::LargePageStatus { usable, .. } => self.facts.no_large_pages = !usable,
            Setup::MemoryStatus(status) => {
                if let Some(status) = status {
                    self.ram_bytes = Some(status.total_phys);
                }
            }
            Setup::SystemInfo { report, memory_modules, .. } => {
                self.system = Some(SystemPanel::of(&Machine::of(&report, memory_modules)));
            }
            Setup::Starting => self.position = "Starting".to_string(),
            Setup::RealtimeOnEveryCpu => self
                .errors
                .push("priority=realtime with a worker on every logical CPU: the window can freeze until the run ends".to_string()),
            Setup::Plan { name, .. } => {
                self.facts.plan_name = Some(name);
                self.facts.plan = Some(format!("plan {name}"));
            }
        }
    }

    /// This sign-in lacks the right large pages need (TODO 113): the dialog asks, as `question` says, and `answer_large_pages` (and, after
    /// a broad pick, `confirm_broad`) replies on `reply`. The grant's dialog starts with the account chosen.
    pub fn ask_large_pages(&mut self, question: LargePageQuestion, reply: SyncSender<LargePageAnswer>) {
        self.position = match question {
            LargePageQuestion::SignIn { .. } => "Large pages need a new sign-in",
            LargePageQuestion::Grant { .. } => "Large pages need a right granted",
            LargePageQuestion::NeedsAdministrator { .. } => "Large pages need TMR run as administrator",
        }
        .to_string();
        self.chosen = matches!(question, LargePageQuestion::Grant { .. }).then_some(0);
        self.confirming = None;
        self.reply = Some(reply);
        self.question = Some(question);
    }

    /// The large-page question is waiting for its answer, in either dialog: no other dialog opens meanwhile
    pub fn large_pages_pending(&self) -> bool {
        self.reply.is_some()
    }

    /// The large-page dialog is open
    pub fn large_pages_asked(&self) -> bool {
        self.reply.is_some() && self.confirming.is_none()
    }

    /// The second dialog, after a broad pick, is open
    pub fn broad_asked(&self) -> bool {
        self.reply.is_some() && self.confirming.is_some()
    }

    /// The grant's radio buttons' selection, mirrored: none (as WinUI's control sends when its items are replaced), or one past the list,
    /// chooses nothing, which disables the grant
    pub fn choose_target(&mut self, chosen: Option<usize>) {
        let candidates = match &self.question {
            Some(LargePageQuestion::Grant { candidates, .. }) => candidates.len(),
            _ => 0,
        };
        self.chosen = chosen.filter(|&i| i < candidates);
    }

    /// The dialog's words for the last question asked (none before any): the console's question (crates/tmr-cli's `prompts.rs`) in a
    /// window's
    pub fn large_page_dialog(&self) -> LargePageDialog {
        const CONTINUE: &str = "Use 4 KiB pages";
        const QUIT: &str = "Quit";
        let elevated_lines = |uac: bool, domain: bool| {
            let uac = if uac { " After the next sign-in TMR has it in every run, run as administrator or not." } else { "" };
            let domain =
                if domain { " This PC is in a domain: a domain Group Policy that sets this right replaces a local grant." } else { "" };
            format!("{uac}{domain}")
        };
        match &self.question {
            None => LargePageDialog {
                title: "Large pages",
                primary: "",
                secondary: "",
                close: QUIT,
                text: String::new(),
                choices: Vec::new(),
                chosen: None,
                legend: None,
            },
            Some(LargePageQuestion::SignIn { account, via, uac, domain }) => {
                let holder = match via {
                    Some(group) => format!("{group}, a group {account} is in"),
                    None => format!("{account}, the account TMR runs as"),
                };
                LargePageDialog {
                    title: "Large pages need a new sign-in",
                    primary: "Exit to sign out",
                    secondary: CONTINUE,
                    close: QUIT,
                    text: format!(
                        "The security policy gives the right large pages need, 'Lock pages in memory', to {holder}, but this sign-in \
                         began before it did. Exit TMR, then sign out of Windows and back in (it needn't restart): TMR then runs on \
                         large pages (recommended for the most memory stress).{}\n\nOr continue this time on {FOUR_KIB_PAGES}\n\n\
                         {QUIT_RUNS_NOTHING}",
                        elevated_lines(*uac, *domain)
                    ),
                    choices: Vec::new(),
                    chosen: None,
                    legend: None,
                }
            }
            Some(LargePageQuestion::Grant { candidates, uac, domain }) => {
                let account = candidates.first().map_or("", |you| you.name.as_str());
                let choices = candidates
                    .iter()
                    .enumerate()
                    .map(|(i, candidate)| GrantRow {
                        kind: if i == 0 { "User" } else { "Group" },
                        name: candidate.name.clone(),
                        note: match candidate.reach.is_broad() {
                            _ if i == 0 => "You, recommended".to_string(),
                            true => BROAD_MARK.to_string(),
                            false => String::new(),
                        },
                    })
                    .collect();
                LargePageDialog {
                    title: "Large pages need the 'Lock pages in memory' right",
                    primary: "Grant and exit",
                    secondary: CONTINUE,
                    close: QUIT,
                    text: format!(
                        "TMR runs as {account}, and neither it nor a group it is in has the right large pages need, 'Lock pages in \
                         memory' (SeLockMemoryPrivilege). As administrator, TMR can grant it to one of these, who keeps it until it is \
                         removed (tmr.exe --remove-large-pages); it takes effect at the next sign-in, so this session ends once it is \
                         granted.{}\n\nOr grant nothing, and continue this time on {FOUR_KIB_PAGES}\n\n{QUIT_RUNS_NOTHING}",
                        elevated_lines(*uac, *domain)
                    ),
                    legend: candidates
                        .iter()
                        .any(|candidate| candidate.reach.is_broad())
                        .then(|| format!("{BROAD_MARK} {}", tmr::frontend::broad_legend())),
                    choices,
                    chosen: self.chosen,
                }
            }
            Some(LargePageQuestion::NeedsAdministrator { account, uac }) => LargePageDialog {
                title: "Large pages need TMR run as administrator",
                primary: "Quit",
                secondary: CONTINUE,
                // The primary is the quit, and Esc quits too
                close: "",
                text: format!(
                    "TMR runs as {account} without the right large pages need, 'Lock pages in memory' (SeLockMemoryPrivilege). Only \
                     TMR run as administrator can set them up: right-click it and Run as administrator, or run tmr.exe \
                     --setup-large-pages from a console run as administrator. If it was just granted, sign out of Windows and back in \
                     instead.{}\n\nQuit, to do that, or continue this time on \
                     {FOUR_KIB_PAGES} Closing this dialog quits too.",
                    if *uac {
                        " You sign in as an administrator: run TMR as administrator once to grant it; after the next sign-in it has it in every run."
                    } else {
                        ""
                    }
                ),
                choices: Vec::new(),
                chosen: None,
                legend: None,
            },
        }
    }

    /// The second dialog's words, for the broad candidate picked (the last picked, so they hold while it closes)
    pub fn broad_dialog(&self) -> BroadDialog {
        let picked = match (&self.question, self.confirming.or(self.chosen)) {
            (Some(LargePageQuestion::Grant { candidates, .. }), Some(i)) => candidates.get(i),
            _ => None,
        };
        let Some(candidate) = picked else {
            return BroadDialog { title: String::new(), grant: String::new(), text: String::new() };
        };
        let name = &candidate.name;
        let warning = candidate.reach.warning().unwrap_or("it reaches more than you");
        BroadDialog {
            title: format!("Grant 'Lock pages in memory' to {name}?"),
            grant: format!("Grant it to {name}"),
            text: format!(
                "{name} is BROAD: {warning}, and {BROAD_RISK}.\n\nGrant it to {name} anyway, or go back to \
                 the list, granting nothing."
            ),
        }
    }

    /// The dialog's answer, by the button that closed it. The primary: the sign-in's sign-out, the grant to the candidate chosen (a broad
    /// one only after the second dialog's yes, `confirm_broad`), the administrator's quit; the secondary, 4 KiB pages this time; the close
    /// button, Esc or nothing quits (TODO 113, decision 5a), as the grant's primary does with none chosen.
    pub fn answer_large_pages(&mut self, button: DialogButton) {
        if !self.large_pages_asked() {
            return;
        }
        let broad = |candidates: &[tmr::frontend::Candidate], i: usize| candidates.get(i).is_some_and(|c| c.reach.is_broad());
        let answer = match (&self.question, button) {
            (_, DialogButton::Secondary) => LargePageAnswer::Regular,
            (Some(LargePageQuestion::SignIn { .. }), DialogButton::Primary) => LargePageAnswer::SignOut,
            (Some(LargePageQuestion::Grant { candidates, .. }), DialogButton::Primary) => match self.chosen {
                Some(i) if broad(candidates, i) => {
                    self.confirming = Some(i);
                    self.position = "Large pages: a broad group picked, asking again".to_string();
                    return;
                }
                Some(i) if i < candidates.len() => LargePageAnswer::Grant(i),
                _ => LargePageAnswer::Quit,
            },
            // The administrator's primary, and every close
            _ => LargePageAnswer::Quit,
        };
        self.send(answer);
    }

    /// The second dialog's answer, after a broad pick: `yes` grants to it; No, Esc or nothing grants nothing and goes back to the first
    /// dialog, the pick still chosen, to choose again
    pub fn confirm_broad(&mut self, yes: bool) {
        let Some(picked) = self.confirming.take() else {
            return;
        };
        if yes {
            self.send(LargePageAnswer::Grant(picked));
        } else {
            self.position = "Large pages need a right granted".to_string();
        }
    }

    /// `answer` to the session's thread, and what it means in the position, and on 4 KiB pages in the errors list
    fn send(&mut self, answer: LargePageAnswer) {
        let Some(reply) = self.reply.take() else {
            return;
        };
        // The session's thread waits for it while the window is open
        let _ = reply.send(answer);
        let regular = match &self.question {
            Some(LargePageQuestion::Grant { .. }) | None => "Large pages need the 'Lock pages in memory' right, not granted",
            Some(LargePageQuestion::SignIn { .. }) => "Large pages take effect at the next sign-in",
            Some(LargePageQuestion::NeedsAdministrator { .. }) => "Large pages need TMR run as administrator to set them up",
        };
        self.position = match (answer, &self.question) {
            (LargePageAnswer::SignOut, _) => "Exiting, to sign out and back in".to_string(),
            (LargePageAnswer::Grant(i), Some(LargePageQuestion::Grant { candidates, .. })) => {
                format!("Granting the large-page right to {}", candidates.get(i).map_or("", |c| c.name.as_str()))
            }
            (LargePageAnswer::Grant(_), _) => "Granting the large-page right".to_string(),
            (LargePageAnswer::Quit, _) => "Stopping before allocating: nothing granted, nothing run".to_string(),
            (LargePageAnswer::Regular, _) => {
                self.errors.push(format!("{regular}: this run takes 4 KiB pages (minpage=regular maxpage=regular)"));
                "Setting up, on 4 KiB pages".to_string()
            }
        };
    }

    /// The lines logged since the last batch, into the log, the warnings and errors into the errors list too, and how many the logger has
    /// dropped so far (`logger::LogLines`)
    pub fn logged(&mut self, lines: Vec<LogLine>, dropped: u64) {
        for LogLine { text, alarm } in lines {
            if alarm {
                self.errors.push(text.clone());
            }
            self.log.push(text);
        }
        self.log_lines_dropped = dropped;
    }

    /// `1,234 log lines dropped, all in the log file`, when the logger's channel was full for some
    pub fn dropped_line(&self) -> Option<String> {
        let dropped = self.log_lines_dropped;
        (dropped > 0).then(|| format!("{} log line{} dropped, all in the log file", thousands(dropped as f64), plural(dropped as usize)))
    }

    fn run(&mut self, run: Run) {
        self.sections.run(&run);
        match run {
            Run::MemoryPlan(plan) => self.ram_bytes = Some(plan.total_physical_bytes),
            Run::NotRun(why) => self.errors.push(not_run(&why)),
            Run::Matchable { tests } => self.errors.push(format!("It could match: {}", tests.join(", "))),
            Run::LeftOut { tests } => {
                // Each reason once: a 512-bit test without AVX-512, the tile test without AMX-INT8 (TODO 102)
                let mut reasons: Vec<&str> = Vec::new();
                for (_, lacks) in &tests {
                    if !reasons.contains(&lacks.as_str()) {
                        reasons.push(lacks);
                    }
                }
                self.facts.left_out = Some(format!("{} test{} left out ({})", tests.len(), plural(tests.len()), reasons.join("; ")));
            }
            Run::Channels { .. } => {}
            Run::TestPlan(plan) => self.facts.plan = Some(plan_fact(&plan, self.facts.plan_name)),
            Run::WheaMonitoring(monitored) => self.facts.whea_unmonitored = monitored.is_err(),
            Run::AllocationStarting { .. } => self.position = "Allocating memory".to_string(),
            Run::Allocated(report) => {
                self.facts.allocation = Some(allocation_fact(&report));
                // The calibration comes next when the run measures one (TODO 22): about 2 minutes, its sweeps in the Log tab
                self.position =
                    if self.calibrating { "Measuring the cache calibration, about 2 minutes (Log tab)" } else { "Allocated" }.to_string();
            }
            Run::Calibrated(_) => {
                self.calibrating = false;
                self.position = "Calibrated".to_string();
            }
            // A recheck that held ends the calibration; one that didn't, a measure follows (TODO 116)
            Run::Rechecked(Ok(recheck)) if recheck.held => {
                self.calibrating = false;
                self.position = "Calibration rechecked: it holds".to_string();
            }
            Run::Rechecked(_) => self.position = "The calibration has drifted: measuring it again, about 2 minutes".to_string(),
            Run::SuiteStarted { progress, .. } => {
                self.progress = Some(progress);
                self.position = "Starting the suite".to_string();
            }
            Run::CycleStarted { cycle, .. } => self.cycle = cycle,
            Run::CycleFinished { cycle, partial } => (self.cycles_finished, self.last_partial) = (cycle, partial),
            Run::SealStage { op, errors, bits, .. } => {
                if op == SealOp::FinalCheck && errors > 0 {
                    self.errors.push(format!("Cycle {} · final seal check: {errors} errors{}", self.cycle, bits.brief()));
                }
            }
            Run::StepFinished(report) => {
                if let Some(error) = step_errors(&report) {
                    self.errors.push(error);
                }
                self.rows.push(StepRow::of(&report));
                if self.reports.len() == REPORTS_KEPT {
                    self.reports.pop_front();
                }
                self.reports.push_back(Report::of_step(&report));
            }
            Run::LateWhea(lines) => self.errors.extend(lines),
            Run::CpuPerformance(summary) => self.work = crate::report::cpu_performance(&summary),
            // The console's line is the Log tab's (the transcript); a worker that didn't start as asked is an error too
            Run::Workers(report) => {
                for start in &report.starts {
                    if let Some(Err(e)) = &start.pinned {
                        self.errors.push(format!("Thread {} not pinned to CPU {}: {e}", start.thread_id, start.cpu_id));
                    }
                    if let Err(e) = &start.priority {
                        self.errors.push(format!("Thread {} priority not set: {e}", start.thread_id));
                    }
                }
            }
            Run::SuiteEnded { outcome, status, errors } => {
                self.status = Status::of(status);
                self.position = self.ended_line(outcome);
                // The run's count holds, its last, beside the position line; no test runs and no time is left
                self.error_count = Some(ErrorCount::of(errors));
                (self.time_left, self.live, self.test_position) = (None, None, None);
                if matches!(outcome, RunOutcome::Completed | RunOutcome::TimeLimit) {
                    (self.bar.fraction, self.bar.percent) = (1.0, 100);
                }
                self.over = true;
            }
            // Its row opens itself: the run's verdict, the first thing to read when it ends
            Run::FinalSummary(report) => {
                self.summary = Some(Summary::of(&report, std::mem::take(&mut self.work)));
                self.opened = Some(Opened::Summary);
            }
            Run::ResultsSaved { path } => self.facts.results = Some(path),
        }
    }

    /// The 500 ms clock: where the suite stands, if it has begun, and what WHEA reported since the last tick
    pub fn tick(&mut self, snapshot: Option<ProgressSnapshot>, whea: Vec<String>) {
        self.errors.extend(whea);
        let Some(s) = snapshot.filter(|_| !self.over) else {
            return;
        };
        self.runtime = s.runtime;
        self.time_left = time_left(&s);
        self.error_count = Some(ErrorCount::of(s.errors()));
        (self.position, self.test_position) = (cycle_line(&s), test_line(&s));
        self.live = live(&s);
        self.bar = bar(&s);
        self.series.push(sample(&s));
        if let Some(test) = s.test.as_ref().filter(|test| !test.finished) {
            let stage = (s.cycle, test.number, test.name.clone());
            if self.stage.as_ref() != Some(&stage) {
                self.series.mark_stage(stage_label(&s, test));
                self.stage = Some(stage);
            }
        }
        let errors = s.errors().total() + s.whea.total;
        if errors > self.errors_seen {
            self.errors_seen = errors;
            self.series.mark_newest();
        }
    }

    /// The priority the window's thread is to be at now (`window-priority=`, the owner's, 2026-11-02): the session's, from setup's
    /// selection until the session ends however it ends, normal otherwise. The window sets it on its thread (`Window::follow_priority`).
    pub fn window_priority(&self) -> ThreadPriority {
        self.window_priority
    }

    /// A calibration runs, the window's or the run's: Start and the window's Calibrate wait for it (TODO 110)
    pub fn calibrating(&self) -> bool {
        self.calibrating
    }

    /// The window's own calibration started (the ••• menu's, TODO 110)
    pub fn calibration_started(&mut self) {
        self.calibrating = true;
        self.position = "Calibrating the caches, about 2 minutes: the Log tab shows it".to_string();
    }

    /// The window's own calibration ended: how it did
    pub fn calibration_ended(&mut self, result: Result<RunStatus, String>) {
        self.calibrating = false;
        self.position = match result {
            Ok(RunStatus::Passed) => "Calibrated: the next run sizes its tests from it".to_string(),
            Ok(_) => "The calibration failed: the Log tab says why".to_string(),
            Err(e) => format!("The calibration failed: {e}"),
        };
    }

    /// The session's thread has ended, so another run can start: Run again's (TODO 109)
    pub fn session_over(&self) -> bool {
        self.session_over
    }

    /// The parameters box takes parameters: before Start, and once the session is over, for Run again
    pub fn takes_parameters(&self) -> bool {
        self.status == Status::Ready || self.session_over
    }

    /// The session's thread ended: how the session did, or the error that stopped it
    pub fn ended(&mut self, result: Result<SessionEnd, String>) {
        self.session_over = true;
        self.window_priority = ThreadPriority::Normal;
        let mut position = "Stopped before testing";
        match &result {
            Err(e) => self.errors.push(format!("Error: {e}")),
            Ok(SessionEnd::Stopped(why)) => self.errors.push(stopped(why)),
            Ok(SessionEnd::Command(_)) => {
                self.errors
                    .push("A one-shot command (--help, --calibrate-cache, ...) runs on the console: tmr.exe without --gui".to_string());
            }
            Ok(SessionEnd::SignOutChosen) => position = "Sign out and back in for large pages, then run again",
            // As the console has it, only a plan that was shown; one stopped before it is Stopped before testing
            Ok(SessionEnd::Ran { plan_only: true, status: RunStatus::Passed, .. }) => {
                position = "The plan only (--startup-debug): nothing allocated, no test run";
            }
            Ok(SessionEnd::Ran { .. }) => {}
        }
        if self.over {
            return;
        }
        if self.progress.is_some() {
            // Mid-suite, with no verdict (a panic, or the session's error): no test runs and no time is left, and the last error count
            // holds. The status is the one the engine's verdict would give the steps finished: Failed on an error, Interrupted after a
            // step, Not run before one.
            self.status = if self.error_count.as_ref().is_some_and(|count| count.alarm) {
                Status::Failed
            } else if self.rows.is_empty() {
                Status::NotRun
            } else {
                Status::Interrupted
            };
            position = "The session ended mid-suite, with no verdict: the log file has why";
        } else {
            // Without a suite: stopped in setup, or a plan or a command was all it was to show
            self.status = Status::NotRun;
        }
        self.position = position.to_string();
        (self.time_left, self.live, self.test_position) = (None, None, None);
        self.over = true;
    }

    /// The position line when the suite has ended: how, and its cycles and steps. The run's errors are beside it (`error_count`).
    fn ended_line(&self, outcome: RunOutcome) -> String {
        let how = match outcome {
            RunOutcome::Completed => "Completed",
            RunOutcome::TimeLimit => "Time limit reached",
            RunOutcome::Interrupted => "Stopped",
            RunOutcome::Halted => "Halted on an error",
        };
        let cycles = self.cycles_finished as usize;
        let last = if self.last_partial { ", the last stopped early" } else { "" };
        format!("{how} · {cycles} cycle{}{last}, {} steps", plural(cycles), self.rows.len())
    }
}

/// One of the Compare tab's two results files (TODO 109)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Slot {
    Baseline,
    Compared,
}

/// Two results files and what comparing them gave: the engine's comparison (`--compare-results`' text, the plans' changes in it, and the
/// steps both plans have as a table), or why not
type Compared = ((PathBuf, PathBuf), Result<ResultsComparison, String>);

/// The Compare tab (TODO 109): its two results files, the baseline first, and their comparison once both are chosen, run on a pool thread
/// so a slow disk doesn't hold the window
#[derive(Debug, Default)]
pub struct CompareTab {
    pub baseline: Option<PathBuf>,
    pub compared: Option<PathBuf>,
    /// The two being compared
    comparing: Option<(PathBuf, PathBuf)>,
    /// The last two compared, and what came of it
    done: Option<Compared>,
}

impl CompareTab {
    pub fn set(&mut self, slot: Slot, path: PathBuf) {
        match slot {
            Slot::Baseline => self.baseline = Some(path),
            Slot::Compared => self.compared = Some(path),
        }
    }

    pub fn swap(&mut self) {
        std::mem::swap(&mut self.baseline, &mut self.compared);
    }

    /// Both at once, from the Open dialog: the older is the baseline, as a run's file is written as it goes, and last as it ends. None, the
    /// dialog cancelled, changes nothing; one, or three, says it takes two.
    pub fn set_both(&mut self, mut files: Vec<PathBuf>) -> Result<(), String> {
        match files.len() {
            0 => return Ok(()),
            2 => {}
            n => return Err(format!("Compare takes two results files, not {n}: choose both in the dialog, the second with Ctrl")),
        }
        files.sort_by_key(|file| std::fs::metadata(file).and_then(|m| m.modified()).ok());
        let mut files = files.into_iter();
        (self.baseline, self.compared) = (files.next(), files.next());
        Ok(())
    }

    /// The two chosen
    fn pair(&self) -> Option<(PathBuf, PathBuf)> {
        Some((self.baseline.clone()?, self.compared.clone()?))
    }

    /// The two to compare now: both chosen, and neither compared nor being compared; they are from now (`finished` takes the result)
    pub fn next(&mut self) -> Option<(PathBuf, PathBuf)> {
        let pair = self.pair()?;
        if self.comparing.as_ref() == Some(&pair) || self.done.as_ref().is_some_and(|(done, _)| *done == pair) {
            return None;
        }
        self.comparing = Some(pair.clone());
        Some(pair)
    }

    pub fn finished(&mut self, pair: (PathBuf, PathBuf), result: Result<ResultsComparison, String>) {
        if self.comparing.as_ref() == Some(&pair) {
            self.comparing = None;
        }
        self.done = Some((pair, result));
    }

    /// The two chosen are being compared
    pub fn is_comparing(&self) -> bool {
        self.comparing.is_some() && self.comparing == self.pair()
    }

    /// The comparison of the two chosen, once it has come
    pub fn result(&self) -> Option<&Result<ResultsComparison, String>> {
        let pair = self.pair()?;
        self.done.as_ref().filter(|(done, _)| *done == pair).map(|(_, result)| result)
    }
}

/// Compares `baseline` with `current`, as `--compare-results` does (`tmr::commands::compare_results`)
pub fn compare((baseline, current): (PathBuf, PathBuf)) -> Result<ResultsComparison, String> {
    tmr::commands::compare_results(&baseline.display().to_string(), &current.display().to_string())
}

/// Why setup stopped the session, as the console words it
fn stopped(why: &Stopped) -> String {
    match why {
        Stopped::UnknownFlags(flags) => format!("Unknown parameter(s): {}", flags.join(" ")),
        // The parser's, its first line without the console's glyph: the second is `--help`'s hint
        Stopped::Invalid(e) => e.trim_start_matches("❌ ").lines().next().unwrap_or_default().to_string(),
        Stopped::CpuSelection(e) => format!("CPU selection error: {e}"),
        Stopped::NeedsLargePages => "This sign-in lacks the right large pages need, and TMR runs nothing on 4 KiB pages unasked \
                                     (minpage=regular maxpage=regular runs on them without asking)"
            .to_string(),
    }
}

/// Why a run stopped before testing, as the console words it
fn not_run(why: &NotRun) -> String {
    match why {
        NotRun::Config(e) => format!("Config error: {e}"),
        NotRun::Plan(e) => e.clone(),
        NotRun::NoTestMatches { filter, plan: Some(plan) } => format!("No test of plan {plan} matches filter '{filter}'"),
        NotRun::NoTestMatches { filter, plan: None } => format!("No tests match filter '{filter}'"),
        NotRun::NothingLeftToRun => "Nothing left to run: this CPU can run none of the tests".to_string(),
        NotRun::Allocation(e) => format!("Failed to allocate memory blocks: {e}"),
    }
}

/// A step that found errors, or saw a WHEA event, as the console's step headline counts them, and its failing bits; `None` for one that
/// passed
fn step_errors(report: &StepReport) -> Option<String> {
    let (s, errors) = (&report.summary, report.errors());
    if errors.total() == 0 && s.whea_total == 0 {
        return None;
    }
    let bits = report.failing_bits_line().map_or(String::new(), |bits| format!(" · {bits}"));
    Some(format!("Cycle {} · step {} · {}: {errors}{bits}", report.cycle, s.step, s.label))
}

/// What allocation got: `8 threads · 28.0 GiB · 1 GiB pages`, the page sizes by their share when it got more than one
fn allocation_fact(report: &AllocationReport) -> String {
    let threads = report.threads.len();
    let bytes: u64 = report.threads.iter().map(|t| t.allocated_bytes).sum();
    let sum = |of: fn(&tmr::frontend::AllocationThread) -> u64| report.threads.iter().map(of).sum::<u64>();
    let sizes: Vec<(&str, u64)> =
        [("1 GiB", sum(|t| t.huge_bytes)), ("2 MiB", sum(|t| t.large_bytes)), ("4 KiB", sum(|t| t.regular_bytes))]
            .into_iter()
            .filter(|&(_, bytes)| bytes > 0)
            .collect();
    let total: u64 = sizes.iter().map(|&(_, bytes)| bytes).sum();
    let pages = match sizes.as_slice() {
        [] => String::new(),
        [(size, _)] => format!(" · {size} pages"),
        _ => format!(
            " · pages {}",
            sizes.iter().map(|&(size, bytes)| format!("{size} {:.0}%", bytes as f64 * 100.0 / total as f64)).collect::<Vec<_>>().join(", ")
        ),
    };
    format!("{threads} thread{} · {:.1} GiB{pages}", plural(threads), bytes as f64 / GIB)
}

/// The plan: `plan stability · 16 steps a cycle · sealed`, without the name when the tests aren't a built-in plan's (a config's)
fn plan_fact(plan: &TestConfigurationReport, name: Option<&str>) -> String {
    let name = name.map_or(String::new(), |name| format!("plan {name} · "));
    let sealed = if plan.sealed { " · sealed" } else { "" };
    format!("{name}{} step{} a cycle{sealed}", plan.step_count, plural(plan.step_count))
}

/// The time the suite's limit leaves, in the ticker's words: `remaining 00:18:50`, and once it is reached `limit reached, finishing cycle`
/// (a cycle is never cut short by the clock); `None` with no time limit
fn time_left(s: &ProgressSnapshot) -> Option<String> {
    let limit = s.time_limit?;
    Some(match limit.checked_sub(s.runtime) {
        Some(left) if !left.is_zero() => format!("remaining {}", hms(left)),
        _ => "limit reached, finishing cycle".to_string(),
    })
}

/// What the test running is doing, in the ticker's words: its stage when it isn't just testing, then its figures, the data it has
/// processed and its speed, `285.00 GiB @ 19108.7 MiB/s (18.66 GiB/s)`, or `pending` before its workers have published them, or
/// `no live data` for a test that publishes none. `None` before the first test.
fn live(s: &ProgressSnapshot) -> Option<String> {
    let test = s.test.as_ref()?;
    let live = &test.live;
    let figures = if live.published {
        format!("{:.2} GiB @ {:.1} MiB/s ({:.2} GiB/s)", live.bytes as f64 / GIB, live.bytes_per_sec / MIB, live.bytes_per_sec / GIB)
    } else if test.publishes {
        "pending".to_string()
    } else {
        "no live data".to_string()
    };
    Some(match live.stage.map(|stage| stage.label()).filter(|label| !label.is_empty()) {
        // Mem-Refresh's wait, with what's left of it (TODO 112)
        Some(stage) => match &live.wait {
            Some(wait) => format!("{stage}, {} · {figures}", wait.text()),
            None => format!("{stage} · {figures}"),
        },
        None => figures,
    })
}

/// The cache calibration tmr-cfg.json holds, as the info line says it before a run (TODO 110): its date and CPU, and what the run does
/// when it measured one thread only; with none, that the first run measures one
pub fn stored_calibration_text(stored: Option<&tmr::session::StoredCalibration>) -> String {
    match stored {
        None => "no cache calibration: the first run measures one, about 2 minutes".to_string(),
        Some(stored) => {
            let at = stored.at.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M");
            let cpu = if stored.cpu.is_empty() { String::new() } else { format!(", {}", stored.cpu) };
            let then = if stored.shared { "" } else { ", one thread only: the next run measures it again" };
            format!("cache calibration {at}{cpu}{then}")
        }
    }
}

/// The cycle running, with how long it has run, in the ticker's words (crates/tmr-cli's `render/ticker.rs`): `Cycle 4/8 (00:12:34,
/// 39%)`, the percentage the whole run's, where a cycle limit gives one. The ticker's first line, as `test_line` is its second's start.
fn cycle_line(s: &ProgressSnapshot) -> String {
    let (cycle, tests) = (u64::from(s.cycle), s.tests_per_cycle);
    match s.cycle_limit {
        Some(limit) => {
            // Finished tests only, as the ticker counts them: Cycle 4/8 with 1 of 6 tests done is (3*6 + 1) / (8*6) = 39%
            let done = cycle.saturating_sub(1) * tests + s.tests_done;
            format!("Cycle {cycle}/{limit} ({}, {}%)", hms(s.cycle_elapsed), percent(done.into(), u128::from(limit) * u128::from(tests)))
        }
        None => format!("Cycle {cycle} ({})", hms(s.cycle_elapsed)),
    }
}

/// The test running, with how long it has run, in the ticker's words: `Test 2/6 Mem-Refresh128 (00:05:12, 52%)`, the percentage its own
/// time limit's, where it has one; `None` before the first test
fn test_line(s: &ProgressSnapshot) -> Option<String> {
    let test = s.test.as_ref()?;
    let mut line = format!("Test {}/{} {} ({}", test.number, s.tests_per_cycle, test.name, hms(test.elapsed));
    if let Some(limit) = test.time_limit {
        line += &format!(", {}%", percent(test.elapsed.as_millis(), limit.as_millis()));
    }
    line.push(')');
    Some(line)
}

/// `part` as a whole percentage of `whole`, rounded down and capped at 100, as the ticker's; 0 when `whole` is 0
fn percent(part: u128, whole: u128) -> u128 {
    (part * 100).checked_div(whole).map_or(0, |p| p.min(100))
}

/// `00:41:10`, the ticker's clock
fn hms(d: Duration) -> String {
    let secs = d.as_secs();
    format!("{:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
}

/// How far the run is: its finished tests against its cycle limit's, as the ticker's percentage counts them, or its runtime against its
/// time limit, whichever is further; with neither, the cycle's finished tests. The percentage is of the same counts, rounded down.
fn bar(s: &ProgressSnapshot) -> Bar {
    let of = |part: u128, whole: u128, about| Bar { fraction: (part as f64 / whole as f64).min(1.0), percent: percent(part, whole), about };
    let tests = u128::from(s.tests_per_cycle.max(1));
    let done = u128::from(s.cycle.saturating_sub(1)) * tests + u128::from(s.tests_done);
    let by_cycles = s.cycle_limit.filter(|&limit| limit > 0).map(|limit| of(done, u128::from(limit) * tests, Bar::CYCLES));
    let by_time = s.time_limit.map(|limit| limit.as_millis()).filter(|&ms| ms > 0).map(|ms| of(s.runtime.as_millis(), ms, Bar::TIME));
    // The further; of two as far, the cycles', whose percentage the position line has (`max_by` takes the last)
    [by_time, by_cycles]
        .into_iter()
        .flatten()
        .max_by(|a, b| a.fraction.total_cmp(&b.fraction))
        .unwrap_or_else(|| of(u128::from(s.tests_done), tests, Bar::CYCLE))
}

/// The chart's sample for `s` (TODO 109): the running test's speed in MiB/s over the last seconds of its own time (the ticker's is since
/// it began), marked the seal's when its figures are the seal's work, and the sealed pass's while a step that makes more than one pass
/// wraps its chunks; none between tests, and before its workers have published
fn sample(s: &ProgressSnapshot) -> Sample {
    match s.test.as_ref().filter(|test| !test.finished && test.live.published) {
        Some(test) if test.sealing => Sample::Seal(test.live.recent_bytes_per_sec / MIB),
        Some(test) if test.live.sealed_pass => Sample::SealedPass(test.live.recent_bytes_per_sec / MIB),
        Some(test) => Sample::Test(test.live.recent_bytes_per_sec / MIB),
        None => Sample::None,
    }
}

/// A stage's label on the chart: `C1 T5 Mem-MovingInv256`, and a run-wide seal stage's, which has no number, `C1 Sealing memory`
fn stage_label(s: &ProgressSnapshot, test: &TestSnapshot) -> String {
    match test.number {
        0 => format!("C{} {}", s.cycle, test.name),
        number => format!("C{} T{number} {}", s.cycle, test.name),
    }
}

/// `{:.0}`, the console's rounding, with thousands separated: `19,109`
pub fn thousands(value: f64) -> String {
    let digits = format!("{value:.0}");
    let (sign, digits) = digits.split_at(usize::from(digits.starts_with('-')));
    let mut out = String::from(sign);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// The parameters box's text for `args`, the CLI's `key=value` syntax: each argument apart, in double quotes when it has a space in it
/// (`"config=C:\My configs\x.cfg"`), or is empty
pub fn args_text(args: &[String]) -> String {
    args.iter()
        .map(|arg| if arg.is_empty() || arg.contains(char::is_whitespace) { format!("\"{arg}\"") } else { arg.clone() })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The arguments in the parameters box's `text`, split as a shell splits them for the CLI: at whitespace outside double quotes, the quotes
/// taken out. A backslash is the path's, as in a Windows path, never an escape.
pub fn args_of(text: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut arg: Option<String> = None;
    let mut quoted = false;
    for ch in text.chars() {
        match ch {
            '"' => {
                quoted = !quoted;
                arg.get_or_insert_with(String::new);
            }
            ch if ch.is_whitespace() && !quoted => args.extend(arg.take()),
            ch => arg.get_or_insert_with(String::new).push(ch),
        }
    }
    args.extend(arg);
    args
}

#[cfg(test)]
mod tests {
    use tmr::frontend::{
        AllocationThread, CacheInfo, CpuInfo, ErrorMode, LiveFigures, MemoryModule, MemoryStatus, Selection, SkipSpec, Stage,
        StepConfigLine, SystemInfoReport, TestSnapshot, TestSummary, ThreadTimingReport, TscCalibrationInfo, WheaCounts,
    };

    use super::*;
    use crate::sysinfo::Dimm;

    /// A step of `cycle` that found `errors`, and `seal` errors in a sealed run (`None` unsealed), while WHEA saw `whea`. Every count is
    /// its own, so a row or a line that read the wrong one shows it: the alarms (failed checks) are 2 more than the errors.
    fn step(cycle: u64, step: usize, errors: u64, seal: Option<u64>, whea: WheaCounts) -> Run {
        Run::StepFinished(Box::new(StepReport {
            cycle,
            summary: TestSummary {
                step,
                test_index: step - 1,
                id: None,
                label: format!("Mem-StuckBit256 (Test {step})"),
                seal_errors: seal.unwrap_or(0),
                name: "Mem-StuckBit256".to_string(),
                duration_ms: 10_500,
                timed_ms: 10_000.0,
                bytes_processed: 200 << 30,
                operations: 1_000_000,
                cycles_completed: (2, 2),
                stopped_by: Some("cycles"),
                ops: Default::default(),
                throughput_mib_s: 20_480.4,
                errors,
                alarms: errors + 2,
                not_reproduced: 0,
                failing_bits: Default::default(),
                seal_failing_bits: Default::default(),
                whea_total: whea.total,
                whea_corrected: whea.corrected,
                latency_samples: None,
                latency_p5_ns: None,
                latency_p10_ns: None,
                latency_p25_ns: None,
                latency_p50_ns: None,
                latency_p75_ns: None,
                latency_p90_ns: None,
                latency_p95_ns: None,
                latency_p99_ns: None,
                latency_p99_9_ns: None,
                latency_spread: None,
                threads: Vec::new(),
            },
            wall: Duration::from_millis(10_500),
            timed_secs: 10.0,
            cycle_limit: Some(2),
            stopped_by_time: false,
            sealed: seal.is_some(),
            config: StepConfigLine {
                parameter: "-".to_string(),
                mode: "TMR-10".to_string(),
                extent: "FullAllocation".to_string(),
                chunk: "Absolute (4.00 MiB)".to_string(),
                test_reps: 1,
                verify_reps: 1,
                write_read_cycles: 1,
            },
            threads: ThreadTimingReport { average_elapsed_ms: 10_000, thread_timings: Vec::new(), whea },
            latency: None,
            stamp: None,
        }))
    }

    /// Where the suite stands: cycle 2 of 4, its third test of 6 running at 19,108.7 MiB/s with `live_errors` found so far
    fn snapshot(live_errors: u64) -> ProgressSnapshot {
        ProgressSnapshot {
            runtime: Duration::from_secs(95),
            time_limit: None,
            cycle: 2,
            cycle_limit: Some(4),
            cycle_elapsed: Duration::from_secs(30),
            tests_done: 2,
            tests_per_cycle: 6,
            finished_errors: 1,
            live_errors,
            finished_seal_errors: 0,
            live_seal_errors: 0,
            sealed: true,
            whea: WheaCounts::default(),
            test: Some(TestSnapshot {
                number: 3,
                name: "Mem-Refresh128".to_string(),
                elapsed: Duration::from_secs(5),
                time_limit: None,
                finished: false,
                publishes: true,
                sealing: false,
                live: LiveFigures {
                    published: true,
                    bytes: 100 << 30,
                    errors: live_errors,
                    bytes_per_sec: 19108.7 * MIB,
                    // A steady test: its last seconds' speed is its speed since it began (`progress::live_figures`)
                    recent_bytes_per_sec: 19108.7 * MIB,
                    ..Default::default()
                },
            }),
        }
    }

    /// TODO 110: before a run, the info line says what cache calibration the run finds; the window's own calibration says it runs, and
    /// how it did
    #[test]
    fn the_info_line_says_the_stored_calibration_and_a_calibration_its_progress() {
        let at = chrono::Utc::now();
        let mut m = model();
        m.stored_calibration = Some(stored_calibration_text(None));
        assert_eq!(m.info_line(), "tmr memory=50% cycles=2 · no cache calibration: the first run measures one, about 2 minutes");
        let stored = tmr::session::StoredCalibration { at, cpu: "Intel(R) Xeon(R) 6975P-C".to_string(), shared: false };
        let local = at.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M");
        assert_eq!(
            stored_calibration_text(Some(&stored)),
            format!("cache calibration {local}, Intel(R) Xeon(R) 6975P-C, one thread only: the next run measures it again")
        );
        m.calibration_started();
        assert!(m.calibrating() && m.position.starts_with("Calibrating the caches"));
        m.calibration_ended(Ok(RunStatus::Passed));
        assert_eq!((m.calibrating(), m.position.as_str()), (false, "Calibrated: the next run sizes its tests from it"));
        m.calibration_ended(Ok(RunStatus::NotRun));
        assert_eq!(m.position, "The calibration failed: the Log tab says why");
    }

    fn model() -> Model {
        let args: Vec<String> = ["tmr.exe", "memory=50%", "cycles=2"].iter().map(|a| a.to_string()).collect();
        Model::new(&args)
    }

    /// A session that ran, ending `status`
    fn ran(status: RunStatus) -> SessionEnd {
        SessionEnd::Ran { status, elapsed: Duration::from_secs(60), plan_only: false }
    }

    /// Setup's choice of 6 threads of 6 available cores
    /// The sign-in's question, the policy giving the right to the account or `via` a group
    fn sign_in(via: Option<&str>) -> LargePageQuestion {
        LargePageQuestion::SignIn { account: r"PC\owner".to_string(), via: via.map(str::to_string), uac: false, domain: false }
    }

    /// The grant's question: the account, a group, Users and Everyone
    fn grant(uac: bool) -> LargePageQuestion {
        use tmr::frontend::{Candidate, Reach, Sid};
        let candidate = |name: &str, reach, sid: &str| Candidate::new(name, reach, sid.parse::<Sid>().unwrap());
        let candidates = vec![
            candidate(r"PC\owner", Reach::You, "S-1-5-21-1-2-3-1001"),
            candidate(r"BUILTIN\Administrators", Reach::Group, "S-1-5-32-544"),
            candidate(r"BUILTIN\Users", Reach::EveryLocalAccount, "S-1-5-32-545"),
            candidate("Everyone", Reach::Automatic, "S-1-1-0"),
        ];
        LargePageQuestion::Grant { candidates, uac, domain: false }
    }

    fn selection() -> Selection {
        Selection {
            error_mode: ErrorMode::Log,
            threads: 6,
            available: 6,
            cputype: "cores".to_string(),
            cpu_list: vec![0, 2, 4, 6, 8, 10],
            pinned: true,
            skip: SkipSpec::Count(0),
            skipped: Vec::new(),
            avoid_smt_doubling: false,
            cycles: Some(2),
            duration_secs: None,
            simd: "SSE, AVX2".to_string(),
            window_priority: ThreadPriority::High,
        }
    }

    /// A Ryzen 5 8600G's, as setup reports it
    fn system_info() -> SystemInfoReport {
        SystemInfoReport {
            cpu_info: CpuInfo {
                brand: "AMD Ryzen 5 8600G w/ Radeon 760M Graphics".to_string(),
                vendor: "AuthenticAMD".to_string(),
                family: 25,
                model: 117,
                stepping: 2,
                physical_cores: 6,
                logical_cores: 12,
                has_hyperthreading: true,
                simd_capabilities: vec!["AVX2".to_string()],
            },
            cache_info: CacheInfo {
                l1_data_total: 6 * (32 << 10),
                l1_instruction_total: 6 * (32 << 10),
                l2_total: 6 << 20,
                l3_total: 16 << 20,
                per_core_l1d: 32 << 10,
                per_core_l1i: 32 << 10,
                per_core_l2: 1 << 20,
                cache_line_size: 64,
                detection_method: "raw_cpuid leaf 4".to_string(),
            },
            tsc_info: TscCalibrationInfo {
                frequency_ghz: 4.3,
                detection_method: "cpuid".to_string(),
                is_invariant: true,
                confidence_percent: 99.0,
                samples: 5,
                calibration_time_ms: 50,
                std_dev_ghz: 0.001,
                converged: true,
            },
        }
    }

    /// A run as the window takes it (TODO 29): Ready, Running once started, a row per step and an error entry for the one that found
    /// errors, the facts on the info line, and the verdict with the cycles the run finished
    #[test]
    fn a_run_fills_the_rows_errors_and_verdict() {
        let mut m = model();
        assert_eq!((m.status, m.info_line().as_str()), (Status::Ready, "tmr memory=50% cycles=2"));
        m.start();
        assert_eq!(m.status, Status::Running);

        let thread = |huge: u64, large: u64| AllocationThread {
            thread_id: 0,
            numa_node: Some(0),
            target_bytes: huge + large,
            allocated_bytes: huge + large,
            huge_bytes: huge,
            large_bytes: large,
            regular_bytes: 0,
            remote_bytes: 0,
            commits: Vec::new(),
        };
        let allocated = AllocationReport {
            allocator: "stitched".to_string(),
            elapsed: Duration::from_millis(900),
            numa_nodes: 1,
            threads: vec![thread(3 << 30, 1 << 30), thread(5 << 30, 0)],
            nodes: Vec::new(),
            commit_groups: Vec::new(),
            fairness: Vec::new(),
        };
        let plan = TestConfigurationReport {
            suite_timing: String::new(),
            test_count: 2,
            step_count: 3,
            sealed: true,
            tests: Vec::new(),
            notes: Vec::new(),
        };
        let (clean, whea) = (WheaCounts::default(), WheaCounts { total: 2, corrected: 1 });
        for run in [
            Run::Allocated(Box::new(allocated)),
            Run::TestPlan(Box::new(plan)),
            Run::SuiteStarted {
                progress: Arc::new(ProgressTracker::new()),
                seed: 1,
                transfer_bits: None,
                transfer_source: "none".to_string(),
                rescan: true,
            },
            Run::CycleStarted { cycle: 1, limit: Some(2) },
            step(1, 1, 0, Some(0), clean),
            step(1, 2, 3, Some(4), clean),
            Run::CycleFinished { cycle: 1, partial: false },
            Run::CycleStarted { cycle: 2, limit: Some(2) },
            step(2, 1, 0, Some(0), whea),
            Run::CycleFinished { cycle: 2, partial: true },
            Run::SuiteEnded {
                outcome: RunOutcome::Interrupted,
                status: RunStatus::Failed,
                errors: ErrorCounts { test: 3, seal: 4, whea, sealed: true },
            },
            Run::ResultsSaved { path: "results/run.json".to_string() },
        ] {
            m.apply(run.into());
        }
        assert!(m.progress().is_some());
        assert_eq!(m.info_line(), "2 threads · 9.0 GiB · pages 1 GiB 89%, 2 MiB 11% · 3 steps a cycle · sealed · results/run.json");
        let cells: Vec<[String; StepRow::COLUMNS]> = m.rows.iter().map(StepRow::cells).collect();
        let row = |cycle: &str, test: usize, errors: &str, seal: &str, whea: &str| {
            let label = format!("Mem-StuckBit256 (Test {test})");
            [cycle, &test.to_string(), &label, "00:00:10", "20,480", errors, seal, whea].map(str::to_string)
        };
        assert_eq!(cells, [row("1", 1, "0", "0", "0"), row("1", 2, "3", "4", "0"), row("2", 1, "0", "0", "2")]);
        let flagged: Vec<[bool; StepRow::COLUMNS]> = m.rows.iter().map(StepRow::flagged).collect();
        let count = |errors, seal, whea| [false, false, false, false, false, errors, seal, whea];
        assert_eq!(flagged, [count(false, false, false), count(true, true, false), count(false, false, true)], "each count flags its cell");
        assert_eq!(m.errors, [
            "Cycle 1 · step 2 · Mem-StuckBit256 (Test 2): 7 errors (3 test + 4 seal + 0 WHEA)",
            "Cycle 2 · step 1 · Mem-StuckBit256 (Test 1): 2 errors (0 test + 0 seal + 2 WHEA 1C/1UC)"
        ]);
        assert_eq!(m.status, Status::Failed);
        assert_eq!(m.position, "Stopped · 2 cycles, the last stopped early, 3 steps", "its errors beside it");
        assert_eq!(m.error_count.as_ref().map(|count| count.text.as_str()), Some("⚠️ 9 errors (3 test + 4 seal + 2 WHEA 1C/1UC)"));

        // The thread's end agrees, and changes nothing
        m.ended(Ok(ran(RunStatus::Failed)));
        assert_eq!((m.status, m.errors.total()), (Status::Failed, 2));
    }

    /// The window's thread (the owner, 2026-11-02): normal until setup's selection, the selection's then, and normal again once the session
    /// ends, however it ends, a new run's (Run again's) normal until its own selection
    #[test]
    fn the_windows_priority_is_the_sessions_until_it_ends() {
        let selected = |priority| {
            let mut m = model();
            assert_eq!(m.window_priority(), ThreadPriority::Normal, "before Start");
            m.start();
            m.apply(Setup::CpuType { cputype: "cores".to_string(), avoid_smt: false }.into());
            assert_eq!(m.window_priority(), ThreadPriority::Normal, "before the selection");
            m.apply(Setup::Selection(Box::new(Selection { window_priority: priority, ..selection() })).into());
            assert_eq!(m.window_priority(), priority, "from the selection");
            m
        };
        // How the session ends, and the suite's end before it when it ran one
        let suite_ended = |outcome, status| Some(Run::SuiteEnded { outcome, status, errors: Default::default() });
        let ends = [
            ("refused in setup", None, Ok(SessionEnd::Stopped(Stopped::Invalid("bad value".to_string())))),
            ("failed", None, Err("bad config".to_string())),
            ("the sign-out chosen", None, Ok(SessionEnd::SignOutChosen)),
            ("the plan only", None, Ok(SessionEnd::Ran { status: RunStatus::Passed, elapsed: Duration::ZERO, plan_only: true })),
            ("finished", suite_ended(RunOutcome::Completed, RunStatus::Passed), Ok(ran(RunStatus::Passed))),
            ("stopped", suite_ended(RunOutcome::Interrupted, RunStatus::Interrupted), Ok(ran(RunStatus::Interrupted))),
        ];
        for (how, suite, end) in ends {
            let mut m = selected(ThreadPriority::Realtime);
            if let Some(suite) = suite {
                m.apply(suite.into());
                assert_eq!(m.window_priority(), ThreadPriority::Realtime, "{how}: the suite over, the session not");
            }
            m.ended(end);
            assert_eq!(m.window_priority(), ThreadPriority::Normal, "{how}");
        }
        for priority in [ThreadPriority::Normal, ThreadPriority::High] {
            assert_eq!(selected(priority).window_priority(), priority);
        }

        // Run again: a new run's, normal until its selection
        let mut m = selected(ThreadPriority::High);
        m.ended(Ok(ran(RunStatus::Passed)));
        let mut next = m.again();
        assert_eq!(next.window_priority(), ThreadPriority::Normal, "Run again");
        next.start();
        next.apply(Setup::Selection(Box::new(selection())).into());
        assert_eq!(next.window_priority(), ThreadPriority::High, "Run again's selection");
    }

    /// Setup's facts (TODO 29): the threads and the plan on the info line until the run's replace them, no large pages, the RAM and the
    /// system panel, and its warnings in the errors
    #[test]
    fn setup_fills_the_info_line_and_the_system_panel() {
        let mut m = model();
        m.start();
        assert_eq!(m.position, "Setting up");
        let module = MemoryModule { locator: "DIMM_A2".to_string(), capacity_bytes: 32 << 30, memory_type: 0x22, ..Default::default() };
        let second = MemoryModule { locator: "DIMM_B2".to_string(), capacity_bytes: 16 << 30, ..module.clone() };
        m.choose_dimm("DIMM_B2 · 16 GiB");
        assert!(m.system.is_none(), "no DIMM to choose before setup has detected them");
        let status = MemoryStatus { total_phys: 62 << 30, avail_phys: 50 << 30, total_page_file: 70 << 30, avail_page_file: 60 << 30 };
        for setup in [Setup::Selection(Box::new(selection())), Setup::LargePageCheck] {
            m.apply(setup.into());
        }
        assert_eq!(m.position, "Checking large pages");
        let (reply, answer) = std::sync::mpsc::sync_channel(1);
        m.ask_large_pages(sign_in(None), reply);
        m.answer_large_pages(DialogButton::Secondary);
        assert_eq!(answer.try_recv(), Ok(LargePageAnswer::Regular));
        for setup in [
            Setup::LargePageStatus { usable: false, min_page: "regular".to_string(), min_needs_large: false },
            Setup::MemoryStatus(Some(status)),
            Setup::MemoryStatus(None),
            Setup::SystemInfo {
                report: Box::new(system_info()),
                memory_modules: vec![module, second],
                system: Default::default(),
                bios: Default::default(),
                hypervisor: None,
            },
            Setup::Starting,
            Setup::RealtimeOnEveryCpu,
            Setup::Plan { name: "stability", about: "the stability tests", is_default: true },
        ] {
            m.apply(setup.into());
        }
        assert_eq!((m.position.as_str(), m.ram_bytes), ("Starting", Some(62 << 30)));
        assert_eq!(m.info_line(), "6 threads · plan stability · no large pages");
        let system = m.system.as_ref().expect("the system panel");
        assert_eq!(
            (system.cpu.as_str(), system.cache_line.as_str()),
            ("AMD Ryzen 5 8600G w/ Radeon 760M Graphics", "L1 32 KiB · L2 1 MiB per core · L3 16 MiB")
        );
        let memory = m.memory_line().expect("the memory line");
        assert_eq!((memory.total.as_deref(), memory.dimms.as_str()), (Some("48 GiB DDR5"), "2 DIMMs"));
        assert!(memory.about.contains("Windows reports 62 GiB"), "{}", memory.about);
        let chosen = |m: &Model| m.system.as_ref().and_then(SystemPanel::chosen).map(Dimm::choice);
        assert_eq!(chosen(&m).as_deref(), Some("DIMM_A2 · 32 GiB"), "the first, before any choice");
        m.choose_dimm("DIMM_B2 · 16 GiB");
        assert_eq!(chosen(&m).as_deref(), Some("DIMM_B2 · 16 GiB"), "the one chosen");
        assert_eq!(m.errors, [
            "Large pages take effect at the next sign-in: this run takes 4 KiB pages (minpage=regular maxpage=regular)",
            "priority=realtime with a worker on every logical CPU: the window can freeze until the run ends"
        ]);

        let plan = TestConfigurationReport {
            suite_timing: String::new(),
            test_count: 2,
            step_count: 3,
            sealed: false,
            tests: Vec::new(),
            notes: Vec::new(),
        };
        m.apply(Run::TestPlan(Box::new(plan)).into());
        assert_eq!(m.info_line(), "6 threads · plan stability · 3 steps a cycle · no large pages");
    }

    /// A session setup stopped, or that named a command, is Not run, saying why as the console does, the quit for large pages too; the
    /// sign-out chosen says what to do
    #[test]
    fn a_session_setup_stopped_says_why() {
        let end = |end: Result<SessionEnd, String>| {
            let mut m = model();
            m.start();
            m.ended(end);
            assert_eq!(m.status, Status::NotRun);
            (m.position, m.errors.newest(usize::MAX).map(|(_, entry)| entry).collect::<Vec<_>>().join(" | "))
        };
        let stopped = "Stopped before testing".to_string();
        assert_eq!(
            end(Ok(SessionEnd::Stopped(Stopped::UnknownFlags(vec!["--latency-test".to_string(), "--fast".to_string()])))),
            (stopped.clone(), "Unknown parameter(s): --latency-test --fast".to_string())
        );
        let invalid = "❌ Invalid value for cycles: 'x' (must be a number)\n💡 Run with --help to see all valid parameters".to_string();
        assert_eq!(
            end(Ok(SessionEnd::Stopped(Stopped::Invalid(invalid)))),
            (stopped.clone(), "Invalid value for cycles: 'x' (must be a number)".to_string())
        );
        assert_eq!(
            end(Ok(SessionEnd::Stopped(Stopped::CpuSelection("8 threads asked, 6 available".to_string())))),
            (stopped.clone(), "CPU selection error: 8 threads asked, 6 available".to_string())
        );
        assert_eq!(
            end(Ok(SessionEnd::Command(tmr::commands::Command::Version))),
            (stopped, "A one-shot command (--help, --calibrate-cache, ...) runs on the console: tmr.exe without --gui".to_string())
        );
        assert_eq!(
            end(Ok(SessionEnd::Stopped(Stopped::NeedsLargePages))),
            (
                "Stopped before testing".to_string(),
                "This sign-in lacks the right large pages need, and TMR runs nothing on 4 KiB pages unasked (minpage=regular \
                 maxpage=regular runs on them without asking)"
                    .to_string()
            )
        );
        assert_eq!(end(Ok(SessionEnd::SignOutChosen)), ("Sign out and back in for large pages, then run again".to_string(), String::new()));
    }

    /// A session that ends with no suite run is Not run, its reason in the errors: an unsealed run's rows have no seal count
    #[test]
    fn a_session_with_no_suite_is_not_run() {
        let mut m = model();
        m.start();
        m.apply(Run::NotRun(NotRun::NoTestMatches { filter: "Mem-Nope".to_string(), plan: None }).into());
        m.apply(Run::Matchable { tests: vec!["Mem-StuckBit256".to_string(), "Mem-Refresh128".to_string()] }.into());
        m.ended(Ok(ran(RunStatus::NotRun)));
        assert_eq!((m.status, m.position.as_str()), (Status::NotRun, "Stopped before testing"));
        assert_eq!(m.errors, ["No tests match filter 'Mem-Nope'", "It could match: Mem-StuckBit256, Mem-Refresh128"]);

        // An error, or `--startup-debug`'s plan, before any event
        let mut m = model();
        m.start();
        m.ended(Err("bad value".to_string()));
        assert_eq!(m.status, Status::NotRun);
        assert_eq!(m.errors, ["Error: bad value"]);
        let mut m = model();
        m.start();
        m.ended(Ok(SessionEnd::Ran { status: RunStatus::Passed, elapsed: Duration::ZERO, plan_only: true }));
        assert_eq!(
            (m.status, m.position.as_str()),
            (Status::NotRun, "The plan only (--startup-debug): nothing allocated, no test run"),
            "no suite ran"
        );
        // `--startup-debug` stopped before its plan: its reason is in the errors
        let mut m = model();
        m.start();
        m.apply(Run::NotRun(NotRun::NoTestMatches { filter: "Mem-Nope".to_string(), plan: None }).into());
        m.ended(Ok(SessionEnd::Ran { status: RunStatus::NotRun, elapsed: Duration::ZERO, plan_only: true }));
        assert_eq!((m.status, m.position.as_str()), (Status::NotRun, "Stopped before testing"), "no plan shown");

        let mut m = model();
        m.apply(step(1, 1, 0, None, WheaCounts::default()).into());
        assert_eq!(m.rows[0].cells()[6], "–");
    }

    /// The large-page question (TODO 29 S6, TODO 113): the dialog is open while the question waits, the answer goes to the session's thread
    /// once, by the button that closed the dialog (the close button and Esc quit, decision 5a), and only 4 KiB pages say so in the errors
    #[test]
    fn the_large_page_question_waits_for_one_answer() {
        let mut m = model();
        m.start();
        assert!(!m.large_pages_asked() && !m.large_pages_pending());
        m.answer_large_pages(DialogButton::Primary);
        assert_eq!(m.position, "Setting up", "no question, no answer");
        assert_eq!((m.large_page_dialog().title, m.large_page_dialog().close), ("Large pages", "Quit"), "before any");

        let (reply, answer) = std::sync::mpsc::sync_channel(1);
        m.ask_large_pages(sign_in(None), reply);
        assert_eq!((m.large_pages_asked(), m.position.as_str()), (true, "Large pages need a new sign-in"));
        let words = m.large_page_dialog();
        assert_eq!(
            (words.title, words.primary, words.secondary, words.close, words.choices.len()),
            ("Large pages need a new sign-in", "Exit to sign out", "Use 4 KiB pages", "Quit", 0)
        );
        for said in [r"to PC\owner, the account TMR runs as, but this sign-in began before it did", "it needn't restart", "runs nothing"] {
            assert!(words.text.contains(said), "{said}: {}", words.text);
        }
        m.answer_large_pages(DialogButton::Primary);
        m.answer_large_pages(DialogButton::Secondary);
        assert_eq!(
            (answer.try_recv(), answer.try_recv()),
            (Ok(LargePageAnswer::SignOut), Err(std::sync::mpsc::TryRecvError::Disconnected)),
            "one answer, then the reply is gone"
        );
        assert_eq!((m.large_pages_asked(), m.position.as_str(), m.errors.total()), (false, "Exiting, to sign out and back in", 0));
        assert_eq!(m.large_page_dialog(), words, "the words hold as the dialog closes");
        m.ended(Ok(SessionEnd::SignOutChosen));
        assert_eq!(m.position, "Sign out and back in for large pages, then run again");

        // Closed, or Esc: the quit, nothing in the errors until the session's end says why
        let (reply, answer) = std::sync::mpsc::sync_channel(1);
        let mut m = model();
        m.ask_large_pages(sign_in(None), reply);
        m.answer_large_pages(DialogButton::Close);
        assert_eq!(answer.try_recv(), Ok(LargePageAnswer::Quit));
        assert_eq!((m.position.as_str(), m.errors.total()), ("Stopping before allocating: nothing granted, nothing run", 0));

        // Through a group, under UAC on a domain PC; the session's thread stopped waiting (the window closed): the answer goes nowhere
        let (reply, answer) = std::sync::mpsc::sync_channel(1);
        drop(answer);
        let mut m = model();
        m.ask_large_pages(
            LargePageQuestion::SignIn {
                account: r"PC\owner".to_string(),
                via: Some(r"BUILTIN\Administrators".to_string()),
                uac: true,
                domain: true,
            },
            reply,
        );
        let text = m.large_page_dialog().text;
        for said in [r"to BUILTIN\Administrators, a group PC\owner is in,", "run as administrator or not", "a domain Group Policy"] {
            assert!(text.contains(said), "{said}: {text}");
        }
        m.answer_large_pages(DialogButton::Secondary);
        assert_eq!(m.position, "Setting up, on 4 KiB pages");
        assert_eq!(m.errors, ["Large pages take effect at the next sign-in: this run takes 4 KiB pages (minpage=regular maxpage=regular)"]);
    }

    /// TODO 113: the grant's dialog lists the candidates, the account first and chosen, the broad ones warned of; the choice mirrors the
    /// radio buttons, none chosen granting nothing; a broad pick asks again, its No the default, which goes back to the list, before
    /// anything is granted; the words hold as each dialog closes; a yes is the session's to make, which then shows the grant or why it
    /// failed; 4 KiB pages say nothing was granted, and the close button quits
    #[test]
    fn the_grant_dialog_grants_to_the_choice_and_asks_again_for_a_broad_one() {
        let ask = |m: &mut Model| {
            let (reply, answer) = std::sync::mpsc::sync_channel(1);
            m.ask_large_pages(grant(false), reply);
            answer
        };
        let mut m = model();
        m.start();
        let answer = ask(&mut m);
        assert_eq!((m.large_pages_asked(), m.position.as_str()), (true, "Large pages need a right granted"));
        let words = m.large_page_dialog();
        assert_eq!(
            (words.title, words.primary, words.secondary, words.close),
            ("Large pages need the 'Lock pages in memory' right", "Grant and exit", "Use 4 KiB pages", "Quit")
        );
        let rows: Vec<(&str, &str, &str)> = words.choices.iter().map(|r| (r.kind, r.name.as_str(), r.note.as_str())).collect();
        assert_eq!(rows, [
            ("User", r"PC\owner", "You, recommended"),
            ("Group", r"BUILTIN\Administrators", ""),
            ("Group", r"BUILTIN\Users", "⚠️"),
            ("Group", "Everyone", "⚠️"),
        ]);
        assert_eq!(
            words.legend.as_deref(),
            Some(
                "⚠️ BROAD, use with caution: most or all accounts are in it, and any program of any of these \
            accounts can then lock RAM away from Windows."
            ),
            "the mark's legend, once"
        );
        assert_eq!(words.chosen, Some(0), "the account chosen");
        for said in [r"TMR runs as PC\owner,", "keeps it until it is removed (tmr.exe --remove-large-pages)", "next sign-in", "4 KiB pages"]
        {
            assert!(words.text.contains(said), "{said}: {}", words.text);
        }
        assert!(!words.text.contains("UAC"), "no UAC line unless the token says so");
        m.choose_target(Some(1));
        m.choose_target(None);
        assert_eq!(m.large_page_dialog().chosen, None, "none chosen, as the control has it");
        m.choose_target(Some(9));
        assert_eq!(m.large_page_dialog().chosen, None, "one past the list chooses nothing");
        m.choose_target(Some(1));
        m.answer_large_pages(DialogButton::Primary);
        assert_eq!(answer.try_recv(), Ok(LargePageAnswer::Grant(1)));
        assert_eq!((m.large_pages_asked(), m.position.as_str()), (false, r"Granting the large-page right to BUILTIN\Administrators"));
        m.apply(Setup::LargePageGrant { account: r"BUILTIN\Administrators".to_string(), outcome: Ok(()) }.into());
        assert_eq!((m.position.as_str(), m.errors.total()), ("Exiting, to sign out and back in", 0));

        // A broad pick: the second dialog, whose No grants nothing and goes back to the list, the pick still chosen, and whose yes grants
        let mut m = model();
        let answer = ask(&mut m);
        m.choose_target(Some(2));
        m.answer_large_pages(DialogButton::Primary);
        assert_eq!(answer.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty), "nothing answered yet");
        assert!(m.broad_asked() && !m.large_pages_asked() && m.large_pages_pending());
        let broad = m.broad_dialog();
        assert_eq!(
            (broad.title.as_str(), broad.grant.as_str()),
            (r"Grant 'Lock pages in memory' to BUILTIN\Users?", r"Grant it to BUILTIN\Users")
        );
        assert!(broad.text.contains("any program of any of these accounts can then lock RAM away from Windows"), "{}", broad.text);
        m.answer_large_pages(DialogButton::Primary);
        assert!(m.broad_asked(), "the first dialog's buttons do nothing while the second is open");
        m.confirm_broad(false);
        assert_eq!(answer.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty), "No: nothing answered");
        assert!(m.large_pages_asked() && !m.broad_asked(), "back to the list");
        assert_eq!((m.large_page_dialog().chosen, m.position.as_str()), (Some(2), "Large pages need a right granted"));
        m.confirm_broad(true);
        assert_eq!(
            answer.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty),
            "the second dialog's answer, once it has closed, does nothing"
        );
        m.answer_large_pages(DialogButton::Primary);
        assert!(m.broad_asked(), "asked again");
        m.confirm_broad(true);
        assert_eq!(answer.try_recv(), Ok(LargePageAnswer::Grant(2)), "the yes grants it");
        assert!(!m.large_pages_pending());
        assert_eq!(m.broad_dialog(), broad, "the words hold as it closes");
        m.confirm_broad(true);
        assert_eq!(answer.try_recv(), Err(std::sync::mpsc::TryRecvError::Disconnected), "answered once");

        // None chosen: the grant waits on a choice, and its button, should it be pressed, grants nothing and quits
        let mut m = model();
        let answer = ask(&mut m);
        m.choose_target(None);
        m.answer_large_pages(DialogButton::Primary);
        assert_eq!(answer.try_recv(), Ok(LargePageAnswer::Quit));

        // The grant failed: the session stops before allocating, and the errors say why
        let mut m = model();
        let _answer = ask(&mut m);
        m.answer_large_pages(DialogButton::Primary);
        let failed = "LsaAddAccountRights: Access is denied. (5, NTSTATUS 0xC0000022)";
        m.apply(Setup::LargePageGrant { account: r"PC\owner".to_string(), outcome: Err(failed.to_string()) }.into());
        assert_eq!(m.position, "Not granted: stopping before allocating");
        assert_eq!(m.errors, [
            r"Couldn't grant 'Lock pages in memory' to PC\owner: LsaAddAccountRights: Access is denied. (5, NTSTATUS 0xC0000022)"
        ]);

        for (button, expected, position) in [
            (DialogButton::Secondary, LargePageAnswer::Regular, "Setting up, on 4 KiB pages"),
            (DialogButton::Close, LargePageAnswer::Quit, "Stopping before allocating: nothing granted, nothing run"),
        ] {
            let mut m = model();
            let answer = ask(&mut m);
            m.answer_large_pages(button);
            assert_eq!((answer.try_recv(), m.position.as_str()), (Ok(expected), position), "{button:?}");
        }
        let mut m = model();
        let answer = ask(&mut m);
        m.answer_large_pages(DialogButton::Secondary);
        assert_eq!(answer.try_recv(), Ok(LargePageAnswer::Regular));
        assert_eq!(m.errors, [
            "Large pages need the 'Lock pages in memory' right, not granted: this run takes 4 KiB pages (minpage=regular maxpage=regular)"
        ]);
        let mut m = model();
        let (reply, _answer) = std::sync::mpsc::sync_channel(1);
        m.ask_large_pages(grant(true), reply);
        assert!(m.large_page_dialog().text.contains("in every run, run as administrator or not"), "the UAC line");
    }

    /// TODO 113: TMR not run as administrator: its primary button quits, to run TMR as administrator, as Esc does (it has no close
    /// button); its secondary takes 4 KiB pages this time
    #[test]
    fn the_administrators_dialog_quits_unless_told_to_continue() {
        for (button, expected, position) in [
            (DialogButton::Primary, LargePageAnswer::Quit, "Stopping before allocating: nothing granted, nothing run"),
            (DialogButton::Secondary, LargePageAnswer::Regular, "Setting up, on 4 KiB pages"),
            (DialogButton::Close, LargePageAnswer::Quit, "Stopping before allocating: nothing granted, nothing run"),
        ] {
            let mut m = model();
            m.start();
            let (reply, answer) = std::sync::mpsc::sync_channel(1);
            m.ask_large_pages(LargePageQuestion::NeedsAdministrator { account: r"PC\owner".to_string(), uac: true }, reply);
            assert_eq!(m.position, "Large pages need TMR run as administrator");
            let words = m.large_page_dialog();
            assert_eq!((words.primary, words.secondary, words.close), ("Quit", "Use 4 KiB pages", ""));
            for said in ["Run as administrator", "--setup-large-pages", "once to grant it", "Closing this dialog quits"] {
                assert!(words.text.contains(said), "{said}: {}", words.text);
            }
            m.answer_large_pages(button);
            assert_eq!((answer.try_recv(), m.position.as_str()), (Ok(expected), position), "{button:?}");
        }
    }

    /// Every line logged joins the log, the warnings and errors the errors list too, and the logger's drops are one line; the lists keep
    /// their newest entries, `ERRORS_KEPT` and `LOG_KEPT`, each with its number for good, and the newest are what the window shows (TODO 29
    /// S6, TODO 109)
    #[test]
    fn logged_lines_join_the_log_and_the_errors_and_the_newest_are_kept() {
        let line = |text: &str, alarm| LogLine { text: text.to_string(), alarm };
        let mut m = model();
        m.logged(Vec::new(), 0);
        assert_eq!((m.errors.total(), m.log.total(), m.dropped_line()), (0, 0, None));
        m.logged(vec![line("[12:00:00 INFO tmr::tests] a step began", false), line("[12:00:00 ERROR tmr::tests] bad word", true)], 0);
        m.logged(Vec::new(), 1);
        assert_eq!(m.errors, ["[12:00:00 ERROR tmr::tests] bad word"]);
        assert_eq!(m.log, ["[12:00:00 INFO tmr::tests] a step began", "[12:00:00 ERROR tmr::tests] bad word"]);
        assert_eq!(m.log.last(1).collect::<Vec<_>>(), [(1, "[12:00:00 ERROR tmr::tests] bad word")], "the last, in order");
        assert_eq!(m.log.last(5).map(|(number, _)| number).collect::<Vec<_>>(), [0, 1], "all there are, oldest first");
        assert_eq!(m.dropped_line().as_deref(), Some("1 log line dropped, all in the log file"));

        // A storm past the cap: the oldest go, and the newest, a step's errors last, are kept
        m.logged((0..ERRORS_KEPT + 2).map(|i| line(&format!("line {i}"), true)).collect(), 1_500);
        assert_eq!((m.log.total(), m.log.newest(usize::MAX).count()), (ERRORS_KEPT as u64 + 4, LOG_KEPT));
        m.apply(step(1, 1, 3, None, WheaCounts::default()).into());
        assert_eq!(m.errors.total(), ERRORS_KEPT as u64 + 4);
        let newest: Vec<(u64, &str)> = m.errors.newest(2).collect();
        assert_eq!(newest[1], (ERRORS_KEPT as u64 + 2, format!("line {}", ERRORS_KEPT + 1).as_str()));
        assert!(newest[0].0 == ERRORS_KEPT as u64 + 3 && newest[0].1.starts_with("Cycle 1 · step 1"), "{newest:?}");
        assert_eq!(m.errors.newest(usize::MAX).count(), ERRORS_KEPT);
        assert_eq!(m.errors.newest(usize::MAX).last(), Some((4, "line 3")), "the bad word and the first 3 lines gone");
        assert_eq!(m.dropped_line().as_deref(), Some("1,500 log lines dropped, all in the log file"), "the logger's");
    }

    /// The parameters box (TODO 29 S6): it starts with the exe's parameters, and Start runs what it holds, split as a shell splits them, a
    /// quoted path with spaces as one
    #[test]
    fn the_parameters_box_gives_the_command_line() {
        let strings = |args: &[&str]| args.iter().map(|a| a.to_string()).collect::<Vec<_>>();
        let mut m = Model::new(&strings(&["C:\\TMR\\tmr.exe", "memory=50%", "config=C:\\My configs\\x.cfg"]));
        assert_eq!(m.args, r#"memory=50% "config=C:\My configs\x.cfg""#);
        assert_eq!(m.command(), strings(&["C:\\TMR\\tmr.exe", "memory=50%", "config=C:\\My configs\\x.cfg"]));
        assert_eq!(m.info_line(), r#"tmr memory=50% "config=C:\My configs\x.cfg""#);

        m.args = "  cycles=2   test=\"Mem-Refresh*\" config=\"C:\\a b\\c.cfg\"\t".to_string();
        assert_eq!(m.command(), strings(&["C:\\TMR\\tmr.exe", "cycles=2", "test=Mem-Refresh*", "config=C:\\a b\\c.cfg"]));
        m.args = " ".to_string();
        assert_eq!((m.command(), m.info_line()), (strings(&["C:\\TMR\\tmr.exe"]), "tmr".to_string()), "no parameters: the defaults");

        assert_eq!(args_of(r#"a "" b"#), strings(&["a", "", "b"]), "an empty argument quoted");
        for args in [&["memory=50%", "cputype=cores"][..], &["config=C:\\a b\\c.cfg", ""], &[]] {
            assert_eq!(args_of(&args_text(&strings(args))), strings(args), "{args:?} round-trips");
        }
        assert_eq!(Model::new(&[]).command(), strings(&["tmr"]));
    }

    /// TODO 109: a config chosen takes the place of the one in the parameters box, quoted when its path has a space
    #[test]
    fn a_config_chosen_takes_the_place_of_the_one_there() {
        let mut m = model();
        m.set_config(Path::new(r"C:\TMR\configs\anta777.cfg"));
        assert_eq!(m.args, r"memory=50% cycles=2 config=C:\TMR\configs\anta777.cfg");
        m.set_config(Path::new(r"C:\My configs\x.cfg"));
        assert_eq!(m.args, r#"memory=50% cycles=2 "config=C:\My configs\x.cfg""#, "the one chosen last, in place of the first");
        assert!(m.takes_parameters(), "before Start");
        m.start();
        assert!(!m.takes_parameters(), "while the session runs");
        m.ended(Ok(ran(RunStatus::Passed)));
        assert!(m.takes_parameters() && m.session_over(), "once it is over, for Run again");
    }

    /// TODO 109: the Compare tab compares its two files once both are chosen, once each pair, and swapped they are another pair; both at
    /// once take the older as the baseline, and one or three say it takes two
    #[test]
    fn the_compare_tab_compares_each_pair_once() {
        let mut tab = CompareTab::default();
        tab.set(Slot::Baseline, PathBuf::from("a.json"));
        assert_eq!(tab.next(), None, "one file");
        tab.set(Slot::Compared, PathBuf::from("b.json"));
        let pair = (PathBuf::from("a.json"), PathBuf::from("b.json"));
        assert_eq!(tab.next(), Some(pair.clone()));
        assert!(tab.is_comparing() && tab.result().is_none());
        assert_eq!(tab.next(), None, "being compared");
        tab.finished(pair.clone(), Err("not a results file".to_string()));
        assert_eq!((tab.is_comparing(), tab.result().map(|r| r.is_err())), (false, Some(true)));
        assert_eq!(tab.next(), None, "compared");
        tab.swap();
        assert!(tab.result().is_none(), "swapped, another pair");
        assert_eq!(tab.next(), Some((PathBuf::from("b.json"), PathBuf::from("a.json"))));

        assert_eq!(tab.set_both(Vec::new()), Ok(()), "the dialog cancelled");
        let three = tab.set_both(vec![PathBuf::from("a.json"); 3]).expect_err("three files");
        assert!(three.starts_with("Compare takes two results files, not 3"), "{three}");
        let dir = std::env::temp_dir().join(format!("tmr-gui-compare-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a temp folder");
        let (older, newer) = (dir.join("older.json"), dir.join("newer.json"));
        std::fs::write(&older, "not a results file").expect("a file");
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(&newer, "nor this").expect("a file");
        tab.set_both(vec![newer.clone(), older.clone()]).expect("two files");
        assert_eq!((tab.baseline.as_ref(), tab.compared.as_ref()), (Some(&older), Some(&newer)), "the older the baseline");
        let failed = compare((older, newer)).expect_err("not results files");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(failed.starts_with("Failed to parse test result JSON"), "{failed}");
    }

    /// TODO 109: Run again's model is a new run's, for the command the parameters box has, keeping the Compare tab's files
    #[test]
    fn run_again_is_a_new_run_with_the_compare_tabs_files() {
        let mut m = model();
        m.start();
        m.apply(step(1, 1, 3, Some(0), WheaCounts::default()).into());
        m.compare.set(Slot::Baseline, PathBuf::from("a.json"));
        m.args = "memory=25% cycles=1".to_string();
        m.ended(Ok(ran(RunStatus::Failed)));
        let next = m.again();
        assert_eq!((next.status, next.session_over(), next.rows.len(), next.errors.total()), (Status::Ready, false, 0, 0));
        assert_eq!(next.command(), ["tmr.exe", "memory=25%", "cycles=1"].map(str::to_string), "the box's parameters");
        assert_eq!(next.compare.baseline.as_deref(), Some(Path::new("a.json")), "the Compare tab's files kept");
    }

    /// Stop moves a running session to Stopping, and nothing else
    #[test]
    fn stop_stops_only_a_running_session() {
        let mut m = model();
        m.stop();
        assert_eq!(m.status, Status::Ready);
        m.start();
        m.stop();
        assert_eq!(m.status, Status::Stopping);
    }

    /// Each tick: the runtime, the position and the progress as the ticker gives them, a speed on the chart, and a mark where the errors
    /// rose; WHEA's lines into the errors; nothing once the suite has ended
    #[test]
    fn ticks_move_the_clock_position_progress_and_chart() {
        let mut m = model();
        m.tick(None, Vec::new());
        assert_eq!((m.runtime, m.series.received), (Duration::ZERO, 0), "before the suite begins");

        m.tick(Some(snapshot(0)), Vec::new());
        assert_eq!((m.runtime, m.runtime_line().as_str()), (Duration::from_secs(95), "00:01:35"));
        assert_eq!(
            (m.position.as_str(), m.test_position.as_deref()),
            ("Cycle 2/4 (00:00:30, 33%)", Some("Test 3/6 Mem-Refresh128 (00:00:05)"))
        );
        assert_eq!((m.bar.fraction, m.bar.percent), (8.0 / 24.0, 33), "a cycle and 2 tests of 4 cycles of 6");
        assert_eq!(m.series.latest().and_then(Sample::value).map(|v| (v * 10.0).round() / 10.0), Some(19108.7));
        assert_eq!(m.series.marks().count(), 1, "the finished test's error");

        m.tick(Some(snapshot(0)), vec!["WHEA [corrected] id=47 Warning: memory".to_string()]);
        m.tick(Some(snapshot(2)), Vec::new());
        let marks: Vec<u64> = m.series.marks().map(|(sample, _, _)| sample).collect();
        assert_eq!(marks, [0, 2], "the errors rose at the first and the third");
        assert_eq!(m.errors, ["WHEA [corrected] id=47 Warning: memory"]);

        // Between tests there is no speed, a gap, and at the suite's end the clock holds
        let mut between = snapshot(0);
        between.test.as_mut().unwrap().finished = true;
        m.tick(Some(between), Vec::new());
        assert_eq!(m.series.latest(), Some(Sample::None));
        m.apply(
            Run::SuiteEnded {
                outcome: RunOutcome::Completed,
                status: RunStatus::Failed,
                errors: ErrorCounts { test: 3, seal: 0, whea: WheaCounts::default(), sealed: true },
            }
            .into(),
        );
        assert_eq!((m.bar.fraction, m.bar.percent), (1.0, 100));
        let ended = ErrorCount { text: "⚠️ 3 errors (3 test + 0 seal + 0 WHEA)".to_string(), alarm: true };
        assert_eq!((m.error_count.as_ref(), m.live.as_ref()), (Some(&ended), None), "the run's count holds, and no test runs");
        let later = ProgressSnapshot { runtime: Duration::from_secs(500), time_limit: Some(Duration::from_secs(600)), ..snapshot(0) };
        m.tick(Some(later), Vec::new());
        assert_eq!((m.runtime, m.series.received, m.time_left.as_ref()), (Duration::from_secs(95), 4, None));
    }

    /// TODO 109: the chart plots the speed over the last seconds of the test's own time, the figures beside it its speed since it began,
    /// and a step's sealed first pass in the seal's colour
    #[test]
    fn the_chart_plots_the_recent_speed_and_the_sealed_pass_apart() {
        let mut m = model();
        let mut s = snapshot(0);
        let test = s.test.as_mut().unwrap();
        test.sealing = false;
        test.live = LiveFigures {
            published: true,
            bytes: 1 << 30,
            bytes_per_sec: 900.0 * MIB,
            recent_bytes_per_sec: 600.0 * MIB,
            sealed_pass: true,
            ..Default::default()
        };
        m.tick(Some(s.clone()), Vec::new());
        assert_eq!(m.series.latest(), Some(Sample::SealedPass(600.0)));
        assert!(m.live.as_deref().is_some_and(|live| live.contains("@ 900.0 MiB/s")), "the figures since it began: {:?}", m.live);
        s.test.as_mut().unwrap().live.sealed_pass = false;
        m.tick(Some(s), Vec::new());
        assert_eq!(m.series.latest(), Some(Sample::Test(600.0)), "the plain passes");
    }

    /// TODO 109: the chart's samples are the test's speed, the seal's when its figures are the seal's work, and a gap where there are
    /// none; where a stage begins, one labelled mark, a run-wide seal stage's without a number
    #[test]
    fn the_chart_has_gaps_the_seals_figures_and_a_mark_for_each_stage() {
        let mut m = model();
        let with = |number: u64, name: &str, sealing: bool, published: bool| {
            let mut s = snapshot(0);
            let test = s.test.as_mut().unwrap();
            (test.number, test.name, test.sealing, test.live.published) = (number, name.to_string(), sealing, published);
            s
        };
        m.tick(Some(with(0, "Sealing memory", true, true)), Vec::new());
        assert!(matches!(m.series.latest(), Some(Sample::Seal(_))), "a run-wide seal stage's figures are the seal's");
        m.tick(Some(with(3, "Mem-Refresh128", true, true)), Vec::new());
        assert!(matches!(m.series.latest(), Some(Sample::Seal(_))), "and the seal's before a step's test");
        m.tick(Some(with(3, "Mem-Refresh128", false, false)), Vec::new());
        assert_eq!(m.series.latest(), Some(Sample::None), "the test, before its first figures: a gap");
        m.tick(Some(with(3, "Mem-Refresh128", false, true)), Vec::new());
        assert!(matches!(m.series.latest(), Some(Sample::Test(_))));
        m.tick(Some(with(4, "Mem-MovingInv256", false, true)), Vec::new());

        let stages: Vec<(u64, &str)> = m.series.stages().map(|(sample, _, label)| (sample, label)).collect();
        assert_eq!(stages, [(0, "C2 Sealing memory"), (1, "C2 T3 Mem-Refresh128"), (4, "C2 T4 Mem-MovingInv256")], "one mark a stage");

        // An error mark in a gap has no value, so no dot; a gap doesn't lift the axis
        let mut series = Series::default();
        series.push(Sample::Test(150.0));
        series.push(Sample::None);
        series.mark_newest();
        assert_eq!(series.marks().collect::<Vec<_>>(), [(1, 0, None)]);
        assert_eq!(series.axis_top(), 200.0);
    }

    /// TODO 109: a row opens its step's report, and a second click closes it; the newest `REPORTS_KEPT` steps' are kept. The final
    /// summary's row opens itself, its errors the tests' and the seal's apart, as the steps' rows count them.
    #[test]
    fn a_row_opens_its_report_and_the_summary_opens_itself() {
        let mut m = model();
        let clean = WheaCounts::default();
        m.apply(step(1, 1, 0, Some(0), clean).into());
        m.apply(step(1, 2, 3, Some(4), clean).into());
        assert!(m.opens(0) && m.opens(1) && !m.opens(2));
        assert!(m.opened_report().is_none());
        m.open(Opened::Step(1));
        let report = m.opened_report().expect("step 2's report");
        assert_eq!(report.title, "Cycle 1 · step 2 · Mem-StuckBit256 (Test 2)");
        assert_eq!((report.verdict.as_str(), report.failed), ("❌ Failed · 10.5 s · 7 errors (3 test + 4 seal + 0 WHEA)", true));
        assert_eq!(report.lines[0], "Test: 10.0 s, 200.00 GiB @ 20,480 MiB/s, 1.00M ops @ 100.00K ops/s");
        assert!(report.lines[1].starts_with("Intermediate: "), "{}", report.lines[1]);
        assert_eq!(report.lines[2..], [
            "Config: parameter=-, mode=TMR-10, extent=FullAllocation, chunk=Absolute (4.00 MiB)",
            "Cycles: 2/2, ended by its cycle limit"
        ]);
        assert!(matches!(report.blocks.as_slice(), [crate::report::Block::Table(_)]), "the threads' table: {:?}", report.blocks);
        m.open(Opened::Step(1));
        assert!(m.opened_report().is_none(), "the second click closes it");
        m.open(Opened::Step(0));
        assert!(m.opened_report().is_some_and(|report| !report.failed && report.verdict.starts_with("✅ Passed")));

        for i in 0..REPORTS_KEPT {
            m.apply(step(2, i + 1, 0, None, clean).into());
        }
        assert!(!m.opens(1) && m.opens(2), "the oldest reports go");
        assert!(m.opened_report().is_none(), "the open one with them");

        let summary = tmr::test_support::final_summary_report();
        let throughput = thousands(summary.overall_throughput_mib_s);
        m.apply(Run::FinalSummary(Box::new(summary)).into());
        assert_eq!(m.opened, Some(Opened::Summary));
        let summary = m.summary.as_ref().expect("the summary row");
        assert_eq!(summary.cells, ["", "", "Final summary", "01:02:05", &throughput, "3", "2", "1"].map(str::to_string));
        assert_eq!(summary.flagged, [false, false, false, false, false, true, true, true]);
        assert_eq!(m.opened_report().map(|report| report.title.as_str()), Some("Final summary"));
    }

    /// A section's outline: each table's title, each line or heading as it is
    fn outline(blocks: &[crate::report::Block]) -> Vec<String> {
        use crate::report::Block;
        blocks
            .iter()
            .map(|block| match block {
                Block::Heading(text) | Block::Text(text) | Block::Warning(text) => text.clone(),
                Block::Table(table) => format!("table {}", table.title.as_deref().unwrap_or("")),
                Block::Separator => "—".to_string(),
            })
            .collect()
    }

    /// TODO 109: the System, Threads and Memory tabs take the console's sections as setup and the run send them, the reports as the
    /// engine's tables
    #[test]
    fn the_tabs_take_the_consoles_startup_sections() {
        let mut m = model();
        let module = MemoryModule {
            locator: "DIMM_A2".to_string(),
            capacity_bytes: 32 << 30,
            memory_type: 0x22,
            speed_mts: 6000,
            manufacturer: "G.Skill ".to_string(),
            part_number: "F5-6000J3040G32G ".to_string(),
            serial_number: "0x1234".to_string(),
            ..Default::default()
        };
        let status = MemoryStatus { total_phys: 62 << 30, avail_phys: 50 << 30, total_page_file: 70 << 30, avail_page_file: 60 << 30 };
        for setup in [
            Setup::CpuType { cputype: "cores".to_string(), avoid_smt: false },
            Setup::Selection(Box::new(selection())),
            Setup::CacheCalibration(tmr::frontend::CacheCalibrationState::Measuring(tmr::frontend::MeasureWhy::NoneStored)),
            Setup::LargePageStatus { usable: true, min_page: "large".to_string(), min_needs_large: true },
            Setup::MemoryStatus(Some(status)),
            Setup::SystemInfo {
                report: Box::new(system_info()),
                memory_modules: vec![module],
                system: Default::default(),
                bios: Default::default(),
                hypervisor: None,
            },
        ] {
            m.apply(setup.into());
        }
        for run in [
            Run::MemoryPlan(Box::new(tmr::test_support::consolidated_memory_report())),
            Run::Channels { channels: 2, cache_line: 64 },
            Run::TestPlan(Box::new(tmr::test_support::test_configuration_report())),
            Run::Allocated(Box::new(tmr::test_support::allocation_report_one_node())),
        ] {
            m.apply(run.into());
        }

        let s = &m.sections;
        assert_eq!(outline(&s.threads), [
            "CPU type: cores", "Using 6/6 cores for testing", "CPUs: 0, 2, 4, 6, 8, 10", "Error mode: log and continue",
            "Limits: 2 cycles, no time limit", "SIMD: SSE, AVX2"
        ]);
        assert_eq!(outline(&s.system), [
            "Cache calibration: none stored, measured before the tests (calibrate=auto)",
            "System Information",
            "table CPU Information",
            "table Cache Architecture (raw_cpuid leaf 4)",
            "table TSC Calibration",
            "table Memory modules (SMBIOS)"
        ]);
        let Some(crate::report::Block::Table(modules)) = s.system.last() else { panic!("the modules' table") };
        assert_eq!(modules.rows, [["DIMM_A2", "32 GiB", "DDR5", "6000 MT/s", "G.Skill", "F5-6000J3040G32G"]], "no serial number");
        let memory = outline(&s.memory);
        assert_eq!(memory[..3], ["Large pages: on, tested", "table Windows' memory", "table Memory Status & Allocation Plan"]);
        assert!(
            memory.iter().any(|line| line.starts_with("table Memory Allocation (")) && memory.contains(&"table Threads".to_string()),
            "{memory:?}"
        );
        assert_eq!(outline(&s.plan)[..2], [
            "Memory channels: 2, which set SimpleTest's stride: (channels × parameter − 1) × the 64 B cache line",
            "table Test Configuration"
        ]);
    }

    /// The ticker's golden (crates/tmr-cli's `testdata/console/ticker.txt`), each `## case` and its lines. Read when the test runs, not
    /// built in, so the published crate, which the CLI's isn't beside, builds its tests (TODO 104): `None` there.
    fn ticker_golden() -> Option<Vec<(String, String)>> {
        let cli = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../tmr-cli");
        if !cli.join("Cargo.toml").exists() {
            return None;
        }
        let text = std::fs::read_to_string(cli.join("testdata/console/ticker.txt")).expect("the CLI's ticker golden").replace("\r\n", "\n");
        Some(
            text.split("## ")
                .filter(|case| !case.is_empty())
                .filter_map(|case| case.split_once('\n'))
                .map(|(n, t)| (n.into(), t.into()))
                .collect(),
        )
    }

    /// A golden case's parts: its lines cut at ` | `, the runtime without its `Runtime: `, and the time left in its bracket a part of its
    /// own
    fn ticker_parts(text: &str) -> Vec<&str> {
        let mut parts = Vec::new();
        for part in text.lines().filter(|line| !line.is_empty()).flat_map(|line| line.split(" | ")) {
            match part.strip_prefix("Runtime: ").map(|runtime| runtime.split_once(" (").ok_or(runtime)) {
                Some(Ok((clock, left))) => parts.extend([clock, left.strip_suffix(')').unwrap_or(left)]),
                Some(Err(clock)) => parts.push(clock),
                None => parts.push(part),
            }
        }
        parts
    }

    /// `parts`, a window line's each, cut at its ` · `, are the golden `case`'s parts word for word, every one of them and no other (in any
    /// order): the window and the console's ticker say the same, which a change to the ticker that the window doesn't follow breaks, a
    /// part the ticker adds as much as a part reworded
    fn in_the_ticker(case: &str, parts: &[&str]) {
        let Some(golden) = ticker_golden() else {
            eprintln!("the CLI's goldens aren't beside this crate: {case:?} not compared with the ticker");
            return;
        };
        let (_, text) = golden.iter().find(|(name, _)| name == case).unwrap_or_else(|| panic!("no case {case:?} in {golden:?}"));
        let mut window: Vec<&str> = parts.iter().flat_map(|part| part.split(" · ")).collect();
        let mut ticker = ticker_parts(text);
        window.sort_unstable();
        ticker.sort_unstable();
        assert_eq!(window, ticker, "the window's parts, and the ticker's {case:?}:\n{text}");
    }

    /// The ticker golden's snapshot (crates/tmr-cli's `render/tests.rs`): 41:10 into a run of 8 cycles or an hour, cycle 4 12:34 in with 1
    /// of its 6 tests done, its second 5:12 into its own 10 minutes, its figures in
    fn ticker_snapshot() -> ProgressSnapshot {
        ProgressSnapshot {
            runtime: Duration::from_secs(41 * 60 + 10),
            time_limit: Some(Duration::from_secs(3600)),
            cycle: 4,
            cycle_limit: Some(8),
            cycle_elapsed: Duration::from_secs(12 * 60 + 34),
            tests_done: 1,
            tests_per_cycle: 6,
            finished_errors: 0,
            live_errors: 0,
            finished_seal_errors: 0,
            live_seal_errors: 0,
            sealed: true,
            whea: WheaCounts::default(),
            test: Some(TestSnapshot {
                number: 2,
                name: "Mem-Refresh128".to_string(),
                elapsed: Duration::from_secs(5 * 60 + 12),
                time_limit: Some(Duration::from_secs(600)),
                finished: false,
                publishes: true,
                sealing: false,
                live: LiveFigures { published: true, bytes: 285 << 30, bytes_per_sec: 19108.7 * MIB, ..Default::default() },
            }),
        }
    }

    /// What a tick shows of the suite: the header's runtime, time left and error count, the position and the test's live figures
    #[derive(Debug, PartialEq)]
    struct Shown {
        runtime: String,
        time_left: Option<String>,
        errors: Option<ErrorCount>,
        position: String,
        live: Option<String>,
    }

    /// What a tick of `snapshot` shows, each part checked word for word against the ticker golden's `case` when it has one
    fn shown(snapshot: ProgressSnapshot, case: Option<&str>) -> Shown {
        let mut m = model();
        m.tick(Some(snapshot), Vec::new());
        // The ticker's two lines' starts, as the window has them, one under the other
        let runtime = m.runtime_line();
        let position = std::iter::once(m.position).chain(m.test_position).collect::<Vec<_>>().join(" · ");
        let shown = Shown { runtime, time_left: m.time_left, errors: m.error_count, position, live: m.live };
        if let Some(case) = case {
            let errors = shown.errors.as_ref().map(|count| &count.text);
            let parts = [Some(&shown.runtime), shown.time_left.as_ref(), errors, Some(&shown.position), shown.live.as_ref()];
            in_the_ticker(case, &parts.into_iter().flatten().map(String::as_str).collect::<Vec<_>>());
        }
        shown
    }

    /// The header's and the position line's figures are the ticker's, in its golden's cases: a cycle limit, a time limit and a test's
    /// own, each with its percentage, and the test's figures; no limits, errors and WHEA split, a stage and no figures yet; the limit
    /// reached and a test that publishes none; before the first test; and a cycle limit alone
    #[test]
    fn the_figures_are_the_tickers() {
        let some = |text: &str| Some(text.to_string());
        let count = |text: &str, alarm| Some(ErrorCount { text: text.to_string(), alarm });
        assert_eq!(shown(ticker_snapshot(), Some("limits, a test on its own time limit, its figures in")), Shown {
            runtime: "00:41:10".to_string(),
            time_left: some("remaining 00:18:50"),
            errors: count("0 errors (0 test + 0 seal + 0 WHEA)", false),
            position: "Cycle 4/8 (00:12:34, 39%) · Test 2/6 Mem-Refresh128 (00:05:12, 52%)".to_string(),
            live: some("285.00 GiB @ 19108.7 MiB/s (18.66 GiB/s)"),
        });

        let mut unlimited = ticker_snapshot();
        (unlimited.time_limit, unlimited.cycle_limit, unlimited.cycle) = (None, None, 12);
        (unlimited.finished_errors, unlimited.live_errors, unlimited.whea) = (2, 1, WheaCounts { total: 3, corrected: 1 });
        (unlimited.finished_seal_errors, unlimited.live_seal_errors) = (1, 1);
        let test = unlimited.test.as_mut().unwrap();
        test.time_limit = None;
        test.live = LiveFigures { stage: Some(Stage::CheckingSeal), ..Default::default() };
        assert_eq!(shown(unlimited, Some("no limits, errors and WHEA, a stage, no figures yet")), Shown {
            runtime: "00:41:10".to_string(),
            time_left: None,
            errors: count("⚠️ 6 errors (1 test + 2 seal + 3 WHEA 1C/2UC)", true),
            position: "Cycle 12 (00:12:34) · Test 2/6 Mem-Refresh128 (00:05:12)".to_string(),
            live: some("Checking seal · pending"),
        });

        // TODO 112: Mem-Refresh's wait, with what's left of it
        let mut waiting = ticker_snapshot();
        let wait = tmr::frontend::WaitLeft { left: Duration::from_millis(380), of: Duration::from_millis(500) };
        waiting.test.as_mut().unwrap().live = LiveFigures { stage: Some(Stage::Waiting), wait: Some(wait), ..Default::default() };
        assert_eq!(shown(waiting, None).live, some("Waiting, 0.4 s of 0.5 s left (wait_ms) · pending"));

        let mut limit_reached = ProgressSnapshot { runtime: Duration::from_secs(3600), ..ticker_snapshot() };
        let test = limit_reached.test.as_mut().unwrap();
        (test.publishes, test.live) = (false, LiveFigures::default());
        let reached = shown(limit_reached, Some("the time limit reached, a test with no live figures"));
        let expected = ("01:00:00", some("limit reached, finishing cycle"), some("no live data"));
        assert_eq!((reached.runtime.as_str(), reached.time_left, reached.live), expected);

        let before = ProgressSnapshot { test: None, cycle: 1, tests_done: 0, cycle_elapsed: Duration::ZERO, ..ticker_snapshot() };
        let before = shown(before, Some("before the first test"));
        let expected = (some("remaining 00:18:50"), count("0 errors (0 test + 0 seal + 0 WHEA)", false), "Cycle 1/8 (00:00:00, 0%)", None);
        assert_eq!((before.time_left, before.errors, before.position.as_str(), before.live), expected);

        // A cycle limit alone, and a WHEA report with no memory error: the count alarms, its split giving both parts
        let cycles_only = ProgressSnapshot { time_limit: None, whea: WheaCounts { total: 1, corrected: 1 }, ..ticker_snapshot() };
        let cycles_only = shown(cycles_only, None);
        assert_eq!((cycles_only.time_left, cycles_only.errors), (None, count("⚠️ 1 error (0 test + 0 seal + 1 WHEA 1C/0UC)", true)));
        assert!(cycles_only.position.starts_with("Cycle 4/8 (00:12:34, 39%)"), "{}", cycles_only.position);

        // The last cycle's last test past its time: each percentage caps at 100
        let mut over = ProgressSnapshot { cycle: 8, tests_done: 6, ..ticker_snapshot() };
        over.test.as_mut().unwrap().elapsed = Duration::from_secs(700);
        assert_eq!(
            (cycle_line(&over), test_line(&over)),
            ("Cycle 8/8 (00:12:34, 100%)".to_string(), Some("Test 2/6 Mem-Refresh128 (00:11:40, 100%)".to_string()))
        );
        assert_eq!(
            (hms(Duration::from_secs(100 * 3600 + 61)), percent(1, 0)),
            ("100:01:01".to_string(), 0),
            "a night's clock; nothing of 0"
        );
    }

    /// A session's thread that ends mid-suite with no verdict leaves no test running and no time left, and says so; the last error count
    /// holds, and the status is the one the engine's verdict would give: Failed on an error, Interrupted after a step, Not run before one
    #[test]
    fn a_thread_that_ends_mid_suite_leaves_no_test_running() {
        let mid_suite = |steps: Vec<Run>, live_errors| {
            let mut m = model();
            m.start();
            let started = Run::SuiteStarted {
                progress: Arc::new(ProgressTracker::new()),
                seed: 1,
                transfer_bits: None,
                transfer_source: "none".to_string(),
                rescan: true,
            };
            for run in std::iter::once(started).chain(steps) {
                m.apply(run.into());
            }
            m.tick(Some(ProgressSnapshot { live_errors, ..ticker_snapshot() }), Vec::new());
            assert!(m.live.is_some() && m.time_left.is_some());
            m.ended(Err("the session's thread ended without a result: the log file has why".to_string()));
            assert_eq!((m.live, m.time_left), (None, None));
            assert_eq!(m.position, "The session ended mid-suite, with no verdict: the log file has why");
            (m.status, m.error_count.map(|count| count.text).unwrap_or_default())
        };
        let clean = || vec![step(1, 1, 0, None, WheaCounts::default())];
        assert_eq!(mid_suite(clean(), 2), (Status::Failed, "⚠️ 2 errors (2 test + 0 seal + 0 WHEA)".to_string()));
        assert_eq!(mid_suite(clean(), 0), (Status::Interrupted, "0 errors (0 test + 0 seal + 0 WHEA)".to_string()));
        assert_eq!(mid_suite(Vec::new(), 0), (Status::NotRun, "0 errors (0 test + 0 seal + 0 WHEA)".to_string()), "no step finished");
    }

    /// The progress takes whichever limit is nearer, or with neither, the cycle's tests, its tooltip saying which; its percentage is
    /// rounded down from the same counts, as the ticker's
    #[test]
    fn the_progress_follows_the_nearer_limit() {
        let at = |cycle_limit, time_limit: Option<u64>| {
            let bar = bar(&ProgressSnapshot { cycle_limit, time_limit: time_limit.map(Duration::from_secs), ..snapshot(0) });
            (bar.fraction, bar.percent, bar.about)
        };
        assert_eq!(at(Some(4), None), (8.0 / 24.0, 33, Bar::CYCLES));
        assert_eq!(at(Some(4), Some(190)), (0.5, 50, Bar::TIME), "95 s of 190 s");
        assert_eq!(at(None, Some(950)), (0.1, 10, Bar::TIME));
        assert_eq!(at(None, None), (2.0 / 6.0, 33, Bar::CYCLE));
        assert_eq!(at(Some(0), Some(0)), (2.0 / 6.0, 33, Bar::CYCLE), "a zero limit divides nothing");

        // The ticker golden's: its time limit is the nearer. A cycle limit alone gives 19 of 48 tests, the position line's 39%, where
        // rounding would give 40%.
        assert_eq!(bar(&ticker_snapshot()), Bar { fraction: 2470.0 / 3600.0, percent: 68, about: Bar::TIME });
        let cycles_only = ProgressSnapshot { time_limit: None, ..ticker_snapshot() };
        assert_eq!(bar(&cycles_only), Bar { fraction: 19.0 / 48.0, percent: 39, about: Bar::CYCLES });
        assert_eq!(cycle_line(&cycles_only), "Cycle 4/8 (00:12:34, 39%)");
    }

    /// The chart keeps a minute of samples, and its marks scroll off with them; its axis rounds up to round labels
    #[test]
    fn the_chart_keeps_a_minute_and_rounds_its_axis() {
        let mut series = Series::default();
        series.mark_newest();
        assert_eq!(series.marks().count(), 0, "no sample to mark");
        for i in 0..SAMPLES + 10 {
            series.push(Sample::Test(i as f64));
            if i == 5 || i == 50 {
                series.mark_newest();
                series.mark_newest();
            }
        }
        assert_eq!(series.samples.len(), SAMPLES);
        assert_eq!(series.marks().collect::<Vec<_>>(), [(50, SAMPLES + 9 - 50, Some(50.0))], "one mark a sample; sample 5 scrolled off");
        assert_eq!(series.axis_top(), 200.0);

        assert_eq!([0.0, 100.0, 101.0, 450.0, 19_108.7, 80_000.0, 2_100_000.0].map(axis_top), [
            100.0, 100.0, 200.0, 600.0, 20_000.0, 80_000.0, 4_000_000.0
        ]);
        assert_eq!([0.0, 250.0, 1_500.0, 20_000.0, 2_500_000.0].map(axis_label), ["0", "250", "1.5K", "20K", "2.5M"]);
        assert_eq!([0.4, 999.5, 20_480.4, 1_234_567.0].map(thousands), ["0", "1,000", "20,480", "1,234,567"]);
    }
}
