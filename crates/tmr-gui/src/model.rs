//! The window's state (TODO 7): what the session's events and the progress snapshots say, in the
//! form the window draws it. Plain Rust with no Reactor type, so the translation is tested apart
//! from the window, which `window.rs` builds from it.
//!
//! The events come as the engine sends them, at most a batch every 500 ms; a snapshot comes with
//! each batch, and with each 500 ms that brought none (`tick`), and so do the warnings and errors
//! logged since (`logged`). The fast figures, the runtime, the position, the progress and the
//! chart's speed, are the snapshot's; the events give the rest: the setup's the system panel and
//! the first facts, the run's the rest.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::mpsc::SyncSender;
use std::time::Duration;

use tmr::frontend::{
    AllocationReport, Event, NotRun, ProgressSnapshot, ProgressTracker, RestartChoice, Run, RunOutcome, RunStatus, SealOp, Setup,
    StepReport, TestConfigurationReport, WheaCounts,
};
use tmr::session::{SessionEnd, Stopped};

use crate::sysinfo::{Machine, SystemPanel};

const MIB: f64 = 1024.0 * 1024.0;
const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

/// The entries the errors list keeps, the newest (the window shows the newest 200 of them): an
/// error storm over a night would hold gigabytes of log lines. The log file has every line.
pub const ERRORS_KEPT: usize = 10_000;

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
    /// The session ended with no suite run: setup or a config stopped it, `--startup-debug` showed
    /// only the plan, or it named a one-shot command (`--help`), which the window doesn't run
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
            mib_s: s.throughput_mib_s,
            errors: s.errors,
            seal_errors: report.sealed.then_some(s.seal_errors),
            whea: s.whea_total,
        }
    }

    /// Its cells: `cycle·step`, the step's label, MiB/s, errors, seal errors (`–` unsealed), WHEA
    pub fn cells(&self) -> [String; 6] {
        [
            format!("{}·{}", self.cycle, self.step),
            self.label.clone(),
            thousands(self.mib_s),
            self.errors.to_string(),
            self.seal_errors.map_or("–".to_string(), |errors| errors.to_string()),
            self.whea.to_string(),
        ]
    }
}

/// What the chart plots: the last `SAMPLES` speeds, one per 500 ms, and the samples at which the
/// run's errors rose.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Series {
    pub samples: VecDeque<f64>,
    /// Samples received since the run started; the newest is number `received - 1`
    pub received: u64,
    /// The numbers of the samples at which the errors rose, oldest first. Marks that scroll off
    /// the left edge are dropped.
    pub error_marks: VecDeque<u64>,
    // A temperature series would go here: one DIMM temperature per sample (SPD hub sensor, or
    // HWiNFO's shared memory; see doc/todo/7-gui.md note 3), drawn on a second y axis on the
    // right so temperature, errors and throughput share this one box.
}

/// The samples the chart shows, a minute of them
pub const SAMPLES: usize = 120;

impl Series {
    pub fn push(&mut self, sample: f64) {
        if self.samples.len() == SAMPLES {
            self.samples.pop_front();
        }
        self.samples.push_back(sample);
        self.received += 1;
        let oldest = self.received - self.samples.len() as u64;
        while self.error_marks.front().is_some_and(|&m| m < oldest) {
            self.error_marks.pop_front();
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

    pub fn latest(&self) -> Option<f64> {
        self.samples.back().copied()
    }

    /// Each visible mark: its sample's number, how many samples before the newest it is, and its
    /// value
    pub fn marks(&self) -> impl Iterator<Item = (u64, usize, f64)> + '_ {
        let n = self.samples.len();
        let oldest = self.received - n as u64;
        self.error_marks.iter().filter_map(move |&m| {
            let index = (m - oldest) as usize;
            self.samples.get(index).map(|&v| (m, n - 1 - index, v))
        })
    }

    /// The top of the y axis: the samples' largest, rounded up to 1, 2, 4, 6 or 8 times a power of
    /// ten, so its four divisions take round labels
    pub fn axis_top(&self) -> f64 {
        axis_top(self.samples.iter().copied().fold(0.0, f64::max))
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

/// The errors list: its newest `ERRORS_KEPT` entries, oldest first, and how many came before them.
/// Each entry has a number, its place among all of them, which stays its own as older ones go: the
/// window's key for it.
#[derive(Debug, Default)]
pub struct Errors {
    kept: VecDeque<String>,
    /// The entries let go, the oldest, to keep `ERRORS_KEPT`
    let_go: u64,
}

impl Errors {
    pub fn push(&mut self, entry: String) {
        if self.kept.len() == ERRORS_KEPT {
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
}

impl Extend<String> for Errors {
    fn extend<I: IntoIterator<Item = String>>(&mut self, entries: I) {
        for entry in entries {
            self.push(entry);
        }
    }
}

/// The entries kept, oldest first, as a test lists them
#[cfg(test)]
impl<const N: usize> PartialEq<[&str; N]> for Errors {
    fn eq(&self, entries: &[&str; N]) -> bool {
        self.kept == *entries
    }
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
    /// Where the run is, one line
    pub position: String,
    /// How far the run is, 0 to 1: its cycles, or its time when that ends it sooner
    pub fraction: f64,
    /// The physical memory Windows reports, from setup's memory status and the memory plan
    pub ram_bytes: Option<u64>,
    /// The system panel, once setup has detected the machine
    pub system: Option<SystemPanel>,
    pub rows: Vec<StepRow>,
    /// Steps with errors, final seal checks that found some, WHEA reports, the warnings and errors
    /// logged, and why a run stopped, oldest first
    pub errors: Errors,
    pub series: Series,
    /// The parameters Start runs, as the parameters box has them: the CLI's `key=value` syntax
    /// (`args_of`)
    pub args: String,
    /// The exe's name, which Start puts before them
    program: String,
    facts: Facts,
    /// The large-page restart question, while it waits for the dialog's answer: the session's
    /// thread waits on the reply
    restart: Option<SyncSender<RestartChoice>>,
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
}

impl Model {
    /// Ready to run `args`, the command line the exe was given (its name first), which the parameters
    /// box starts with
    pub fn new(args: &[String]) -> Self {
        Model {
            status: Status::Ready,
            runtime: Duration::ZERO,
            position: "Ready".to_string(),
            fraction: 0.0,
            ram_bytes: None,
            system: None,
            rows: Vec::new(),
            errors: Errors::default(),
            series: Series::default(),
            args: args_text(args.get(1..).unwrap_or_default()),
            program: args.first().cloned().unwrap_or_else(|| "tmr".to_string()),
            facts: Facts::default(),
            restart: None,
            log_lines_dropped: 0,
            progress: None,
            cycle: 0,
            cycles_finished: 0,
            last_partial: false,
            errors_seen: 0,
            over: false,
        }
    }

    /// The command line Start runs: the exe's name, then the parameters box's
    pub fn command(&self) -> Vec<String> {
        std::iter::once(self.program.clone()).chain(args_of(&self.args)).collect()
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

    /// The suite's progress, once it started
    pub fn progress(&self) -> Option<&Arc<ProgressTracker>> {
        self.progress.as_ref()
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
        if !facts.is_empty() {
            facts.join(" · ")
        } else if self.args.trim().is_empty() {
            "tmr".to_string()
        } else {
            format!("tmr {}", self.args.trim())
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
        match setup {
            // The console's alone: the parameters as given, and the detail behind the facts below
            Setup::TestFilter { .. }
            | Setup::QuickTest
            | Setup::ParamNotes(_)
            | Setup::StartupHeader { .. }
            | Setup::StartupParams { .. }
            | Setup::CpuType { .. }
            | Setup::CpuFilter { .. }
            | Setup::CpuCount { .. }
            | Setup::LargePageDiagnostic { .. }
            | Setup::CacheCalibration(_)
            | Setup::SealCalibration(_)
            | Setup::CpuTopology { .. } => {}
            Setup::Selection(selection) => {
                self.facts.allocation = Some(format!("{} thread{}", selection.threads, plural(selection.threads)));
            }
            Setup::LargePageCheck => self.position = "Checking large pages".to_string(),
            Setup::LargePageStatus { usable, .. } => self.facts.no_large_pages = !usable,
            Setup::MemoryStatus(status) => {
                if let Some(status) = status {
                    self.ram_bytes = Some(status.total_phys);
                }
            }
            Setup::SystemInfo { report, memory_modules } => {
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

    /// Large pages need Windows restarted: the dialog asks whether to exit or continue on 4 KiB
    /// pages, and `answer_restart` replies on `reply`
    pub fn ask_restart(&mut self, reply: SyncSender<RestartChoice>) {
        self.position = "Large pages need Windows restarted".to_string();
        self.restart = Some(reply);
    }

    /// The restart question is waiting for its answer: the dialog is open
    pub fn restart_asked(&self) -> bool {
        self.restart.is_some()
    }

    /// The dialog's answer, to the session's thread. On 4 KiB pages, the errors list says so.
    pub fn answer_restart(&mut self, choice: RestartChoice) {
        let Some(reply) = self.restart.take() else {
            return;
        };
        // The session's thread waits for it while the window is open
        let _ = reply.send(choice);
        match choice {
            RestartChoice::Exit => self.position = "Exiting, to restart Windows".to_string(),
            RestartChoice::Continue4K => {
                self.position = "Setting up, on 4 KiB pages".to_string();
                self.errors.push(
                    "Large pages are set up but take effect after Windows restarts: this run takes 4 KiB pages \
                                  (minpage=regular)"
                        .to_string(),
                );
            }
        }
    }

    /// The warnings and errors logged since the last batch, into the errors list, and how many the
    /// logger has dropped so far (`logger::LogLines`)
    pub fn logged(&mut self, lines: Vec<String>, dropped: u64) {
        self.errors.extend(lines);
        self.log_lines_dropped = dropped;
    }

    /// `1,234 log lines dropped, all in the log file`, when the logger's channel was full for some
    pub fn dropped_line(&self) -> Option<String> {
        let dropped = self.log_lines_dropped;
        (dropped > 0).then(|| format!("{} log line{} dropped, all in the log file", thousands(dropped as f64), plural(dropped as usize)))
    }

    fn run(&mut self, run: Run) {
        match run {
            Run::MemoryPlan(plan) => self.ram_bytes = Some(plan.total_physical_bytes),
            Run::NotRun(why) => self.errors.push(not_run(&why)),
            Run::Matchable { tests } => self.errors.push(format!("It could match: {}", tests.join(", "))),
            Run::LeftOut { tests } => {
                let lacks = tests.first().map_or("", |(_, lacks)| lacks.as_str());
                self.facts.left_out = Some(format!("{} test{} left out ({lacks})", tests.len(), plural(tests.len())));
            }
            Run::Channels { .. } => {}
            Run::TestPlan(plan) => self.facts.plan = Some(plan_fact(&plan, self.facts.plan_name)),
            Run::WheaMonitoring(monitored) => self.facts.whea_unmonitored = monitored.is_err(),
            Run::AllocationStarting { .. } => self.position = "Allocating memory".to_string(),
            Run::Allocated(report) => {
                self.facts.allocation = Some(allocation_fact(&report));
                self.position = "Allocated".to_string();
            }
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
            }
            Run::LateWhea(lines) => self.errors.extend(lines),
            Run::CpuPerformance(_) => {}
            Run::SuiteEnded { outcome, status, memory_errors, whea } => {
                self.status = Status::of(status);
                self.position = self.ended_line(outcome, memory_errors, whea);
                if matches!(outcome, RunOutcome::Completed | RunOutcome::TimeLimit) {
                    self.fraction = 1.0;
                }
                self.over = true;
            }
            Run::FinalSummary(_) => {}
            Run::ResultsSaved { path } => self.facts.results = Some(path),
        }
    }

    /// The 500 ms clock: where the suite stands, if it has begun, and what WHEA reported since the
    /// last tick
    pub fn tick(&mut self, snapshot: Option<ProgressSnapshot>, whea: Vec<String>) {
        self.errors.extend(whea);
        let Some(s) = snapshot.filter(|_| !self.over) else {
            return;
        };
        self.runtime = s.runtime;
        self.position = position(&s);
        self.fraction = fraction(&s);
        self.series.push(speed_mib_s(&s));
        let errors = s.finished_errors + s.live_errors + s.whea.total;
        if errors > self.errors_seen {
            self.errors_seen = errors;
            self.series.mark_newest();
        }
    }

    /// The session's thread ended: how the session did, or the error that stopped it
    pub fn ended(&mut self, result: Result<SessionEnd, String>) {
        let mut position = "Stopped before testing";
        match &result {
            Err(e) => self.errors.push(format!("Error: {e}")),
            Ok(SessionEnd::Stopped(why)) => self.errors.push(stopped(why)),
            Ok(SessionEnd::Command(_)) => {
                self.errors
                    .push("A one-shot command (--help, --calibrate-cache, ...) runs on the console: tmr.exe without --gui".to_string());
            }
            Ok(SessionEnd::RestartChosen) => position = "Restart Windows for large pages, then run again",
            // As the console has it, only a plan that was shown; one stopped before it is Stopped before testing
            Ok(SessionEnd::Ran { plan_only: true, status: RunStatus::Passed, .. }) => {
                position = "The plan only (--startup-debug): nothing allocated, no test run";
            }
            Ok(SessionEnd::Ran { .. }) => {}
        }
        // Without a suite: stopped in setup, or a plan or a command was all it was to show
        if !self.over {
            self.status = Status::NotRun;
            self.position = position.to_string();
            self.over = true;
        }
    }

    /// The position line when the suite has ended: how, its cycles and steps, and its errors
    fn ended_line(&self, outcome: RunOutcome, memory_errors: u64, whea: WheaCounts) -> String {
        let how = match outcome {
            RunOutcome::Completed => "Completed",
            RunOutcome::TimeLimit => "Time limit reached",
            RunOutcome::Interrupted => "Stopped",
            RunOutcome::Halted => "Halted on an error",
        };
        let cycles = self.cycles_finished as usize;
        let last = if self.last_partial { ", the last stopped early" } else { "" };
        format!(
            "{how} · {cycles} cycle{}{last}, {} steps · {memory_errors} errors + {} WHEA{}",
            plural(cycles),
            self.rows.len(),
            whea.total,
            whea.split_suffix()
        )
    }
}

/// Why setup stopped the session, as the console words it
fn stopped(why: &Stopped) -> String {
    match why {
        Stopped::UnknownFlags(flags) => format!("Unknown parameter(s): {}", flags.join(" ")),
        // The parser's, its first line without the console's glyph: the second is `--help`'s hint
        Stopped::Invalid(e) => e.trim_start_matches("❌ ").lines().next().unwrap_or_default().to_string(),
        Stopped::MemoryOverride { spec, error } => format!("Invalid memory override '{spec}': {error}"),
        Stopped::CpuSelection(e) => format!("CPU selection error: {e}"),
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

/// A step that found errors, or saw a WHEA event, as the console's step headline counts them;
/// `None` for one that passed
fn step_errors(report: &StepReport) -> Option<String> {
    let s = &report.summary;
    if s.errors == 0 && s.seal_errors == 0 && s.whea_total == 0 {
        return None;
    }
    let seal = if report.sealed { format!(" + {} seal{}", s.seal_errors, s.seal_failing_bits.brief()) } else { String::new() };
    let whea = WheaCounts { total: s.whea_total, corrected: s.whea_corrected };
    Some(format!(
        "Cycle {} · step {} · {}: {} errors{}{seal} + {} WHEA{}",
        report.cycle,
        s.step,
        s.label,
        s.errors,
        s.failing_bits.brief(),
        whea.total,
        whea.split_suffix()
    ))
}

/// What allocation got: `8 threads · 28.0 GiB · 1 GiB pages`, the page sizes by their share when
/// it got more than one
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

/// The plan: `plan stability · 16 steps a cycle · sealed`, without the name when the tests aren't a
/// built-in plan's (a config's)
fn plan_fact(plan: &TestConfigurationReport, name: Option<&str>) -> String {
    let name = name.map_or(String::new(), |name| format!("plan {name} · "));
    let sealed = if plan.sealed { " · sealed" } else { "" };
    format!("{name}{} step{} a cycle{sealed}", plan.step_count, plural(plan.step_count))
}

/// `Cycle 2 of 3 · Test 4 of 16 · Mem-StuckBit256 · Checking seal`, as the ticker counts them
fn position(s: &ProgressSnapshot) -> String {
    let mut line = match s.cycle_limit {
        Some(limit) => format!("Cycle {} of {limit}", s.cycle),
        None => format!("Cycle {}", s.cycle),
    };
    if let Some(test) = &s.test {
        line += &format!(" · Test {} of {} · {}", test.number, s.tests_per_cycle, test.name);
        if let Some(stage) = test.live.stage.map(|stage| stage.label()).filter(|label| !label.is_empty()) {
            line += &format!(" · {stage}");
        }
    }
    line
}

/// How far the run is: its finished tests against its cycle limit's, as the ticker's percentage
/// counts them, or its runtime against its time limit, whichever is further; with neither, the
/// cycle's finished tests
fn fraction(s: &ProgressSnapshot) -> f64 {
    let tests = s.tests_per_cycle.max(1) as f64;
    let done = f64::from(s.cycle.saturating_sub(1)) * tests + s.tests_done as f64;
    let by_cycles = s.cycle_limit.filter(|&limit| limit > 0).map(|limit| done / (f64::from(limit) * tests));
    let by_time = s.time_limit.filter(|limit| !limit.is_zero()).map(|limit| s.runtime.as_secs_f64() / limit.as_secs_f64());
    let fraction = match (by_cycles, by_time) {
        (Some(cycles), Some(time)) => cycles.max(time),
        (Some(fraction), None) | (None, Some(fraction)) => fraction,
        (None, None) => s.tests_done as f64 / tests,
    };
    fraction.clamp(0.0, 1.0)
}

/// The running test's speed in MiB/s, as the ticker gives it: 0 between tests, and before its
/// workers have published
fn speed_mib_s(s: &ProgressSnapshot) -> f64 {
    s.test.as_ref().filter(|test| !test.finished && test.live.published).map_or(0.0, |test| test.live.bytes_per_sec / MIB)
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

/// The parameters box's text for `args`, the CLI's `key=value` syntax: each argument apart, in
/// double quotes when it has a space in it (`"config=C:\My configs\x.cfg"`), or is empty
pub fn args_text(args: &[String]) -> String {
    args.iter()
        .map(|arg| if arg.is_empty() || arg.contains(char::is_whitespace) { format!("\"{arg}\"") } else { arg.clone() })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The arguments in the parameters box's `text`, split as a shell splits them for the CLI: at
/// whitespace outside double quotes, the quotes taken out. A backslash is the path's, as in a
/// Windows path, never an escape.
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
        AllocationThread, CacheInfo, CpuInfo, ErrorMode, LiveFigures, MemoryModule, MemoryStatus, Selection, StepConfigLine,
        SystemInfoReport, TestSnapshot, TestSummary, ThreadTimingReport, TscCalibrationInfo,
    };

    use super::*;

    /// A step of `cycle` that found `errors`, and `seal` errors in a sealed run (`None` unsealed),
    /// while WHEA saw `whea`. Every count is its own, so a row or a line that read the wrong one
    /// shows it: the alarms (failed checks) are 2 more than the errors.
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

    /// Where the suite stands: cycle 2 of 4, its third test of 6 running at 19,108.7 MiB/s with
    /// `live_errors` found so far
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
            whea: WheaCounts::default(),
            test: Some(TestSnapshot {
                number: 3,
                name: "Mem-Refresh128".to_string(),
                elapsed: Duration::from_secs(5),
                time_limit: None,
                finished: false,
                publishes: true,
                live: LiveFigures { published: true, bytes: 100 << 30, errors: live_errors, bytes_per_sec: 19108.7 * MIB, stage: None },
            }),
        }
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
    fn selection() -> Selection {
        Selection {
            error_mode: ErrorMode::Log,
            threads: 6,
            available: 6,
            cputype: "cores".to_string(),
            cpu_list: vec![0, 2, 4, 6, 8, 10],
            pinned: true,
            cpus_to_skip: 0,
            avoid_smt_doubling: false,
            cycles: Some(2),
            duration_secs: None,
            simd: "SSE, AVX2".to_string(),
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

    /// A run as the window takes it (TODO 29): Ready, Running once started, a row per step and an
    /// error entry for the one that found errors, the facts on the info line, and the verdict with
    /// the cycles the run finished
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
            Run::SuiteStarted { progress: Arc::new(ProgressTracker::new()), seed: 1, transfer_bits: None, transfer_source: "none" },
            Run::CycleStarted { cycle: 1, limit: Some(2) },
            step(1, 1, 0, Some(0), clean),
            step(1, 2, 3, Some(4), clean),
            Run::CycleFinished { cycle: 1, partial: false },
            Run::CycleStarted { cycle: 2, limit: Some(2) },
            step(2, 1, 0, Some(0), whea),
            Run::CycleFinished { cycle: 2, partial: true },
            Run::SuiteEnded { outcome: RunOutcome::Interrupted, status: RunStatus::Failed, memory_errors: 7, whea },
            Run::ResultsSaved { path: "results/run.json".to_string() },
        ] {
            m.apply(run.into());
        }
        assert!(m.progress().is_some());
        assert_eq!(m.info_line(), "2 threads · 9.0 GiB · pages 1 GiB 89%, 2 MiB 11% · 3 steps a cycle · sealed · results/run.json");
        let cells: Vec<[String; 6]> = m.rows.iter().map(StepRow::cells).collect();
        let row = |id: &str, test: usize, errors: &str, seal: &str, whea: &str| {
            [id, &format!("Mem-StuckBit256 (Test {test})"), "20,480", errors, seal, whea].map(str::to_string)
        };
        assert_eq!(cells, [row("1·1", 1, "0", "0", "0"), row("1·2", 2, "3", "4", "0"), row("2·1", 1, "0", "0", "2")]);
        assert_eq!(m.errors, [
            "Cycle 1 · step 2 · Mem-StuckBit256 (Test 2): 3 errors + 4 seal + 0 WHEA",
            "Cycle 2 · step 1 · Mem-StuckBit256 (Test 1): 0 errors + 0 seal + 2 WHEA (1C/1UC)"
        ]);
        assert_eq!(m.status, Status::Failed);
        assert_eq!(m.position, "Stopped · 2 cycles, the last stopped early, 3 steps · 7 errors + 2 WHEA (1C/1UC)");

        // The thread's end agrees, and changes nothing
        m.ended(Ok(ran(RunStatus::Failed)));
        assert_eq!((m.status, m.errors.total()), (Status::Failed, 2));
    }

    /// Setup's facts (TODO 29): the threads and the plan on the info line until the run's replace
    /// them, no large pages, the RAM and the system panel, and its warnings in the errors
    #[test]
    fn setup_fills_the_info_line_and_the_system_panel() {
        let mut m = model();
        m.start();
        assert_eq!(m.position, "Setting up");
        let module = MemoryModule { locator: "DIMM_A2".to_string(), capacity_bytes: 32 << 30, memory_type: 0x22, ..Default::default() };
        let status = MemoryStatus { total_phys: 62 << 30, avail_phys: 50 << 30, total_page_file: 70 << 30, avail_page_file: 60 << 30 };
        for setup in [Setup::Selection(Box::new(selection())), Setup::LargePageCheck] {
            m.apply(setup.into());
        }
        assert_eq!(m.position, "Checking large pages");
        let (reply, answer) = std::sync::mpsc::sync_channel(1);
        m.ask_restart(reply);
        m.answer_restart(RestartChoice::Continue4K);
        assert_eq!(answer.try_recv(), Ok(RestartChoice::Continue4K));
        for setup in [
            Setup::LargePageStatus { usable: false, min_page: "regular".to_string(), min_needs_large: false },
            Setup::MemoryStatus(Some(status)),
            Setup::MemoryStatus(None),
            Setup::SystemInfo { report: Box::new(system_info()), memory_modules: vec![module] },
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
        assert_eq!(system.dimm_title, "1 × DDR5 · 32 GiB total");
        assert_eq!(m.errors, [
            "Large pages are set up but take effect after Windows restarts: this run takes 4 KiB pages (minpage=regular)",
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

    /// A session setup stopped, or that named a command, is Not run, saying why as the console does;
    /// the restart chosen says what to do
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
        let memory = Stopped::MemoryOverride { spec: "lots".to_string(), error: "not a size".to_string() };
        assert_eq!(end(Ok(SessionEnd::Stopped(memory))), (stopped.clone(), "Invalid memory override 'lots': not a size".to_string()));
        assert_eq!(
            end(Ok(SessionEnd::Stopped(Stopped::CpuSelection("8 threads asked, 6 available".to_string())))),
            (stopped.clone(), "CPU selection error: 8 threads asked, 6 available".to_string())
        );
        assert_eq!(
            end(Ok(SessionEnd::Command(tmr::commands::Command::Version))),
            (stopped, "A one-shot command (--help, --calibrate-cache, ...) runs on the console: tmr.exe without --gui".to_string())
        );
        assert_eq!(end(Ok(SessionEnd::RestartChosen)), ("Restart Windows for large pages, then run again".to_string(), String::new()));
    }

    /// A session that ends with no suite run is Not run, its reason in the errors: an unsealed
    /// run's rows have no seal count
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
        assert_eq!(m.rows[0].cells()[4], "–");
    }

    /// The restart question (TODO 29 S6): the dialog is open while the question waits, the answer
    /// goes to the session's thread once, and only 4 KiB pages say so in the errors
    #[test]
    fn the_restart_question_waits_for_one_answer() {
        let mut m = model();
        m.start();
        assert!(!m.restart_asked());
        m.answer_restart(RestartChoice::Exit);
        assert_eq!(m.position, "Setting up", "no question, no answer");

        let (reply, answer) = std::sync::mpsc::sync_channel(1);
        m.ask_restart(reply);
        assert_eq!((m.restart_asked(), m.position.as_str()), (true, "Large pages need Windows restarted"));
        m.answer_restart(RestartChoice::Exit);
        m.answer_restart(RestartChoice::Continue4K);
        assert_eq!(
            (answer.try_recv(), answer.try_recv()),
            (Ok(RestartChoice::Exit), Err(std::sync::mpsc::TryRecvError::Disconnected)),
            "one answer, then the reply is gone"
        );
        assert_eq!((m.restart_asked(), m.position.as_str(), m.errors.total()), (false, "Exiting, to restart Windows", 0));
        m.ended(Ok(SessionEnd::RestartChosen));
        assert_eq!(m.position, "Restart Windows for large pages, then run again");

        // The session's thread stopped waiting (the window closed): the answer goes nowhere
        let (reply, answer) = std::sync::mpsc::sync_channel(1);
        drop(answer);
        let mut m = model();
        m.ask_restart(reply);
        m.answer_restart(RestartChoice::Continue4K);
        assert_eq!(m.position, "Setting up, on 4 KiB pages");
        assert_eq!(m.errors, [
            "Large pages are set up but take effect after Windows restarts: this run takes 4 KiB pages (minpage=regular)"
        ]);
    }

    /// The warnings and errors logged join the errors list, and the logger's drops are one line;
    /// the list keeps its newest `ERRORS_KEPT` entries, each with its number for good, and the
    /// newest are what the window shows (TODO 29 S6)
    #[test]
    fn logged_lines_join_the_errors_and_the_newest_are_kept() {
        let mut m = model();
        m.logged(Vec::new(), 0);
        assert_eq!((m.errors.total(), m.dropped_line()), (0, None));
        m.logged(vec!["[12:00:00 ERROR tmr::tests] bad word".to_string()], 0);
        m.logged(Vec::new(), 1);
        assert_eq!(m.errors, ["[12:00:00 ERROR tmr::tests] bad word"]);
        assert_eq!(m.dropped_line().as_deref(), Some("1 log line dropped, all in the log file"));

        // A storm past the cap: the oldest go, and the newest, a step's errors last, are kept
        m.logged((0..ERRORS_KEPT + 2).map(|i| format!("line {i}")).collect(), 1_500);
        m.apply(step(1, 1, 3, None, WheaCounts::default()).into());
        assert_eq!(m.errors.total(), ERRORS_KEPT as u64 + 4);
        let newest: Vec<(u64, &str)> = m.errors.newest(2).collect();
        assert_eq!(newest[1], (ERRORS_KEPT as u64 + 2, format!("line {}", ERRORS_KEPT + 1).as_str()));
        assert!(newest[0].0 == ERRORS_KEPT as u64 + 3 && newest[0].1.starts_with("Cycle 1 · step 1"), "{newest:?}");
        assert_eq!(m.errors.newest(usize::MAX).count(), ERRORS_KEPT);
        assert_eq!(m.errors.newest(usize::MAX).last(), Some((4, "line 3")), "the bad word and the first 3 lines gone");
        assert_eq!(m.dropped_line().as_deref(), Some("1,500 log lines dropped, all in the log file"), "the logger's");
    }

    /// The parameters box (TODO 29 S6): it starts with the exe's parameters, and Start runs what it
    /// holds, split as a shell splits them, a quoted path with spaces as one
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

    /// Each tick: the runtime, the position and the progress as the ticker gives them, a speed on
    /// the chart, and a mark where the errors rose; WHEA's lines into the errors; nothing once the
    /// suite has ended
    #[test]
    fn ticks_move_the_clock_position_progress_and_chart() {
        let mut m = model();
        m.tick(None, Vec::new());
        assert_eq!((m.runtime, m.series.received), (Duration::ZERO, 0), "before the suite begins");

        m.tick(Some(snapshot(0)), Vec::new());
        assert_eq!(m.runtime, Duration::from_secs(95));
        assert_eq!(m.position, "Cycle 2 of 4 · Test 3 of 6 · Mem-Refresh128");
        assert_eq!(m.fraction, 8.0 / 24.0, "a cycle and 2 tests of 4 cycles of 6");
        assert_eq!(m.series.latest().map(|v| (v * 10.0).round() / 10.0), Some(19108.7));
        assert_eq!(m.series.marks().count(), 1, "the finished test's error");

        m.tick(Some(snapshot(0)), vec!["WHEA [corrected] id=47 Warning: memory".to_string()]);
        m.tick(Some(snapshot(2)), Vec::new());
        let marks: Vec<u64> = m.series.marks().map(|(sample, _, _)| sample).collect();
        assert_eq!(marks, [0, 2], "the errors rose at the first and the third");
        assert_eq!(m.errors, ["WHEA [corrected] id=47 Warning: memory"]);

        // Between tests the speed is 0, and at the suite's end the clock holds
        let mut between = snapshot(0);
        between.test.as_mut().unwrap().finished = true;
        m.tick(Some(between), Vec::new());
        assert_eq!(m.series.latest(), Some(0.0));
        m.apply(
            Run::SuiteEnded { outcome: RunOutcome::Completed, status: RunStatus::Failed, memory_errors: 3, whea: WheaCounts::default() }
                .into(),
        );
        assert_eq!(m.fraction, 1.0);
        let later = ProgressSnapshot { runtime: Duration::from_secs(500), ..snapshot(0) };
        m.tick(Some(later), Vec::new());
        assert_eq!((m.runtime, m.series.received), (Duration::from_secs(95), 4));
    }

    /// The progress takes whichever limit is nearer, or with neither, the cycle's tests
    #[test]
    fn the_progress_follows_the_nearer_limit() {
        let at = |cycle_limit, time_limit: Option<u64>| {
            fraction(&ProgressSnapshot { cycle_limit, time_limit: time_limit.map(Duration::from_secs), ..snapshot(0) })
        };
        assert_eq!(at(Some(4), None), 8.0 / 24.0);
        assert_eq!(at(Some(4), Some(190)), 0.5, "95 s of 190 s");
        assert_eq!(at(None, Some(950)), 0.1);
        assert_eq!(at(None, None), 2.0 / 6.0);
        assert_eq!(at(Some(0), Some(0)), 2.0 / 6.0, "a zero limit divides nothing");
    }

    /// The chart keeps a minute of samples, and its marks scroll off with them; its axis rounds
    /// up to round labels
    #[test]
    fn the_chart_keeps_a_minute_and_rounds_its_axis() {
        let mut series = Series::default();
        series.mark_newest();
        assert_eq!(series.marks().count(), 0, "no sample to mark");
        for i in 0..SAMPLES + 10 {
            series.push(i as f64);
            if i == 5 || i == 50 {
                series.mark_newest();
                series.mark_newest();
            }
        }
        assert_eq!(series.samples.len(), SAMPLES);
        assert_eq!(series.marks().collect::<Vec<_>>(), [(50, SAMPLES + 9 - 50, 50.0)], "one mark a sample; sample 5 scrolled off");
        assert_eq!(series.axis_top(), 200.0);

        assert_eq!([0.0, 100.0, 101.0, 450.0, 19_108.7, 80_000.0, 2_100_000.0].map(axis_top), [
            100.0, 100.0, 200.0, 600.0, 20_000.0, 80_000.0, 4_000_000.0
        ]);
        assert_eq!([0.0, 250.0, 1_500.0, 20_000.0, 2_500_000.0].map(axis_label), ["0", "250", "1.5K", "20K", "2.5M"]);
        assert_eq!([0.4, 999.5, 20_480.4, 1_234_567.0].map(thousands), ["0", "1,000", "20,480", "1,234,567"]);
    }
}
