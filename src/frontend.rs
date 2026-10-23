//! The engine's one interface to whoever shows a run (TODO 29): the CLI (`crates/tmr-cli`) and the
//! window (`crates/tmr-gui`). Where the engine printed text it hands the frontend a plain value, at
//! the same moment, and the frontend formats it. The fast figures (progress, bytes, rates, live
//! errors) don't travel: they are already shared, and a frontend reads them off the
//! `ProgressTracker` on its own timer, the CLI on the engine's monitor thread (`Frontend::tick`).
//!
//! One event replaces one run of prints with no engine call between them, where that run printed,
//! and no event spans a log call. So a frontend that renders an event before `event` returns keeps
//! the console's order against the log records, which go through the logger as before. The CLI
//! renders that way and must not become asynchronous: the 'Halting test suite' record logged right
//! after a step's report (`execute_test_cycle`) would then overtake the report.
//!
//! A session has two kinds: its setup's (`Setup`, from `session::run`: the parameters, the CPUs
//! chosen, large pages, the machine) and its run's (`Run`, from the memory plan to the results
//! file). It asks one question, during setup, when large pages need a restart
//! (`Frontend::large_page_restart`); the one-shot commands' question, whether to save a
//! calibration, is the CLI's alone (the `confirm` that `commands::run` takes).
//!
//! The types the events carry that a frontend outside the engine names are re-exported here, and
//! only those: the CLI's (`crates/tmr-cli`) and the GUI's, their tests' included, and the reports'
//! tables, which a frontend renders through a `Renderer` of its own (the CLI's `ConsoleRenderer`).
//! The rest of the engine stays private, so `dead_code` still checks it. A type an event carries is
//! reachable from outside even when it isn't re-exported, and so are its `pub` methods, which are
//! then exported: compiled as such, their callers change too (S3 found two rescans' error paths
//! changed by `FailingBits`'). So a payload's methods that only the engine calls are `pub(crate)`
//! where it matters: `FailingBits`' recording ones, which the rescans call, and `ProgressTracker`'s
//! bookkeeping, as a frontend holds the live tracker and must only read it (`snapshot`,
//! `take_whea_descriptions`). Other payloads' accessors that the engine alone calls (a `key`, a
//! `detail`) are still `pub`: narrow one when its callers' asm moves, as `FailingBits`' did.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};

pub use crate::ErrorMode;
use crate::failing_bits::FailingBits;
pub use crate::memory::allocator::PageSizeLevel;
pub use crate::progress::{LiveFigures, ProgressSnapshot, ProgressTracker, RunOutcome, TestSnapshot, TestSummary};
// The reports as tables, for a frontend to render: the default formatting's, through its renderer
pub use crate::reporting::Reporter;
pub use crate::reporting::models::{
    AllocationReport, AllocationThread, CacheInfo, ColumnAlignment, CpuInfo, CpuPerformance, CpuTopologyEntry, CpuTopologyReport,
    LatencyLevelSummary, LatencyPercentiles, LatencyTestSummaryReport, LatencyThreadResult, PerformanceByCpuReport,
    PerformanceByPhysicalCoreReport, PerformanceByThreadReport, PhysicalCorePerformance, SystemInfoReport, TableData,
    TestConfigurationReport, ThreadPerformance, ThreadTiming, ThreadTimingReport, TopologySummary, TscCalibrationInfo,
};
use crate::reporting::models::{ConsolidatedMemoryReport, FinalTestSummaryReport};
pub use crate::reporting::renderers::Renderer;
pub use crate::runner::RunStatus;
pub use crate::smbios::MemoryModule;
pub use crate::tests::Stage;
pub use crate::thread_pool::SealOp;
pub use crate::whea::WheaCounts;

/// Whoever shows a run. Two engine threads call it, so it is `Send + Sync` and takes `&self`: the
/// thread that runs the suite sends the events, and the monitor thread ticks.
pub trait Frontend: Send + Sync {
    /// A low-rate fact, on the engine thread that printed it before. The CLI renders it before
    /// returning: the console's order against the log records depends on it.
    fn event(&self, event: Event);
    /// Every 500 ms while a suite runs, from the engine's monitor thread, after its WHEA poll.
    fn tick(&self, _progress: &ProgressTracker) {}
    /// The suite is over: the monitor thread's last call.
    fn ticks_done(&self, _progress: &ProgressTracker) {}
    /// Large pages are set up, but take effect only after Windows restarts (`Setup::LargePageCheck`
    /// came first): exit to restart, or run on 4 KiB pages. Blocks the session's thread, idle then,
    /// until answered. A frontend that can't ask answers `Continue4K`, as an empty answer does.
    fn large_page_restart(&self) -> RestartChoice;
}

/// The answer to `Frontend::large_page_restart`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestartChoice {
    /// End the session, to restart Windows (`session::SessionEnd::RestartChosen`)
    Exit,
    /// Run on 4 KiB pages: the session sets `minpage=regular`
    Continue4K,
}

/// What the engine tells a frontend.
pub enum Event {
    /// A session's setup facts, before its run
    Setup(Setup),
    /// A run's facts, from its memory plan to its results file
    Run(Run),
}

impl From<Setup> for Event {
    fn from(setup: Setup) -> Self {
        Event::Setup(setup)
    }
}

impl From<Run> for Event {
    fn from(run: Run) -> Self {
        Event::Run(run)
    }
}

/// A session's setup facts, in the order a session has them (`session::run`). The large ones are
/// boxed, so the enum stays small.
pub enum Setup {
    /// `test=` picks the tests `pattern` matches, once each unless `cycles=` says otherwise. One for
    /// each `test=` given.
    TestFilter { pattern: String },
    /// `--quick-test`: plan=quick, with its defaults
    QuickTest,
    /// What the allocation and pinning parameters set, without a config. Never empty.
    ParamNotes(Vec<ParamNote>),
    /// The command line, with the defaults the modes added, and whether this is a release build.
    /// Its own event, because the build's record is logged between it and `StartupParams`.
    StartupHeader { command_line: Vec<String>, release: bool },
    /// The logging environment (`RUST_LOG`, and `RUST_BACKTRACE` when set), and which parameters
    /// were given and which defaults stand (none with a config)
    StartupParams { rust_log: String, rust_backtrace: Option<String>, explicit: Vec<String>, defaults: Vec<String> },
    /// Whether the run counts logical CPUs or physical cores (`cputype=`), avoiding SMT for cores
    CpuType { cputype: String, avoid_smt: bool },
    /// `skip-cores=` as a range: the core ids left out
    CpuFilter { lo: usize, hi: usize },
    /// `cpus=` as a share: of how many available, and the threads it gives
    CpuCount { percent: u32, available: usize, threads: usize },
    /// The CPUs chosen, and the run's error mode, limits and SIMD
    Selection(Box<Selection>),
    /// Large pages are checked next: `Frontend::large_page_restart` follows when they need a
    /// restart, `LargePageDiagnostic` otherwise
    LargePageCheck,
    /// What the large-page check found and did (`memory::diagnose_and_setup_large_pages`)
    LargePageDiagnostic { report: String },
    /// Whether this run can take large pages, and its `minpage=`, which stops the run before it
    /// allocates when it needs them (`min_needs_large`) and they aren't usable
    LargePageStatus { usable: bool, min_page: String, min_needs_large: bool },
    /// The cache calibration the tests' sizes take, from tmr-cfg.json
    CacheCalibration(CacheCalibrationState),
    /// The seal calibration `seal-width=auto` takes. Its own event, because checking it against
    /// this machine logs (`AppConfig::valid_seal_calibration`).
    SealCalibration(SealCalibrationState),
    /// The CPUs and the threads assigned them; on a hybrid CPU, how many of its P-cores have SMT
    /// (`None` on one that isn't), and whether skip-cores= skipped E-cores first
    CpuTopology { report: Box<CpuTopologyReport>, smt_p_cores: Option<usize>, e_cores_skipped_first: bool },
    /// The system's memory now, `None` when Windows doesn't say
    MemoryStatus(Option<MemoryStatus>),
    /// The CPU, its caches and TSC, and the memory modules SMBIOS lists
    SystemInfo { report: Box<SystemInfoReport>, memory_modules: Vec<MemoryModule> },
    /// The tests are about to start (the CLI takes Ctrl+C from here)
    Starting,
    /// `priority=realtime` with a worker on every logical CPU, which can freeze the console and input
    RealtimeOnEveryCpu,
    /// The built-in plan the run takes; `is_default` when none was named
    Plan { name: &'static str, about: &'static str, is_default: bool },
}

/// What a parameter set, as `Setup::ParamNotes` lists it.
pub enum ParamNote {
    /// `minpage=`, `maxpage=`
    MinPage(String),
    MaxPage(String),
    /// A block-sizing parameter (`hugechunk=`, `blkround=`, ...) and its value
    Block(&'static str, String),
    /// `topology=`: the detector it names
    Topology(&'static str),
    /// `skip-cores=` was given: the skip count as it stands before the spec is resolved against the
    /// cores (later, in setup), so the default's
    SkipCores(usize),
    /// `--disable-pinning`
    PinningOff,
}

/// The CPUs a run takes and how it runs, as setup chose them.
pub struct Selection {
    pub error_mode: ErrorMode,
    /// Threads, of the CPUs (or cores, `cputype`) available after skip-cores=
    pub threads: usize,
    pub available: usize,
    pub cputype: String,
    /// Each thread's CPU, in thread order
    pub cpu_list: Vec<usize>,
    /// Pinned to them; otherwise the list only places each thread's memory (its NUMA node)
    pub pinned: bool,
    pub cpus_to_skip: usize,
    pub avoid_smt_doubling: bool,
    /// The suite's limits: cycles, seconds
    pub cycles: Option<u32>,
    pub duration_secs: Option<u32>,
    /// The SIMD extensions the CPU has (`simd::detect_simd_capabilities`)
    pub simd: String,
}

/// The cache calibration at startup (`Setup::CacheCalibration`).
pub enum CacheCalibrationState {
    /// This machine's, measured `at`: the extended one when there is one (`--calibrate-cache-ext`)
    Loaded { extended: bool, at: DateTime<Utc> },
    /// None stored
    None,
    /// One measured on other hardware: not used
    Stale,
    /// `--no-calibration`
    Disabled,
}

/// The seal calibration at startup (`Setup::SealCalibration`).
#[derive(Debug, PartialEq)]
pub enum SealCalibrationState {
    /// This machine's, measured `at`: `seal-width=auto` takes its width, `bits`
    Loaded { bits: u32, at: DateTime<Utc> },
    /// One measured on other hardware: `auto` takes the rule's width
    Stale,
    /// None stored
    None,
}

/// The system's memory, in bytes (`GlobalMemoryStatusEx`).
pub struct MemoryStatus {
    pub total_phys: u64,
    pub avail_phys: u64,
    pub total_page_file: u64,
    pub avail_page_file: u64,
}

/// A run's facts, in the order a run has them (`run_tests_with_layout_and_timing_filtered`).
/// The large ones are boxed, so the enum stays small.
pub enum Run {
    /// The memory status and the allocation plan, the run's first fact (`layout.rs`)
    MemoryPlan(Box<ConsolidatedMemoryReport>),
    /// The run stops before testing, and why. Each config error is one: each is logged after it.
    NotRun(NotRun),
    /// What a filter that matched no test could have matched, right after its
    /// `NotRun::NoTestMatches`: the named plan's tests, or the whole suite's. Its own event,
    /// because collecting them logs (each auto-dispatch test's width, at debug level).
    Matchable { tests: Vec<String> },
    /// The tests this CPU can't run, left out of the run: each one's name and what the CPU lacks.
    /// Never empty.
    LeftOut { tests: Vec<(String, String)> },
    /// The channel count the stride formulas take, and the cache line: the test plan's first line.
    /// Its own event, sent before the plan is built, because building it logs (each extent and
    /// chunk it resolves, at debug level).
    Channels { channels: u32, cache_line: usize },
    /// The test plan, right after `Channels`
    TestPlan(Box<TestConfigurationReport>),
    /// Whether WHEA is monitored, or why not
    WheaMonitoring(Result<(), String>),
    /// Allocation starts, between these page sizes (`minpage=`, `maxpage=`)
    AllocationStarting { min: PageSizeLevel, max: PageSizeLevel },
    /// What allocation got (TODO 81)
    Allocated(Box<AllocationReport>),
    /// The suite's clock started: where it stands from now on, for a frontend that reads it on its
    /// own timer (the GUI; the CLI reads it on `tick`), the native patterns' seed, and the transfer
    /// width they take and where it came from, `None` when it is unknown
    SuiteStarted { progress: Arc<ProgressTracker>, seed: u64, transfer_bits: Option<u32>, transfer_source: &'static str },
    /// A cycle starts, of `limit` when there is one
    CycleStarted { cycle: u32, limit: Option<u32> },
    /// A cycle is over, after its results were saved when a save was due: `partial` when it
    /// stopped before its end (Ctrl+C, Stop, or a halt on a step's errors), its last steps and its
    /// final seal check not run, as its results record it. One whose final seal check halts the run
    /// ran to its end, and isn't.
    CycleFinished { cycle: u32, partial: bool },
    /// A seal stage ended (TODO 74): the seal's kernel, the bytes, the errors a final check found
    /// and their bits, and the stage's wall time
    SealStage { op: SealOp, kernel: String, bytes: u64, errors: u64, bits: FailingBits, duration: Duration },
    /// A step ended
    StepFinished(Box<StepReport>),
    /// What WHEA reported after the monitor's last tick, one line each. Never empty.
    LateWhea(Vec<String>),
    /// What each thread, CPU and physical core did over the whole run
    CpuPerformance(Box<CpuPerformanceSummary>),
    /// The suite ended: how, the verdict, and the run's errors (`status` is `Failed` exactly when
    /// the run failed)
    SuiteEnded { outcome: RunOutcome, status: RunStatus, memory_errors: u64, whea: WheaCounts },
    /// The final summary of the run
    FinalSummary(Box<FinalTestSummaryReport>),
    /// The results file was saved, at the end of the run: once, after the final save. The saves
    /// between cycles are silent.
    ResultsSaved { path: String },
}

/// Why a run stops before testing.
pub enum NotRun {
    /// A config error: the config's, an override's, a test parameter's, the seal's, or a step's
    /// in the plan (`step N: ...`)
    Config(String),
    /// The built-in plan can't be taken from the suite
    Plan(String),
    /// No test matches `filter`: of the named `plan` that picked the tests, or of the whole suite.
    /// `Run::Matchable` follows, with what it could have matched.
    NoTestMatches { filter: String, plan: Option<&'static str> },
    /// Every test was left out (`Run::LeftOut`)
    NothingLeftToRun,
    /// Allocation failed
    Allocation(String),
}

/// A step as it ended: what its report shows.
pub struct StepReport {
    /// The suite's cycle it ran in
    pub cycle: u64,
    /// The step as the results file gets it, its threads' parts too
    pub summary: TestSummary,
    /// Its wall time, without the seal's work before it, and its test's own time, its slowest
    /// thread's (TODO 90), which its speed divides by
    pub wall: Duration,
    pub timed_secs: f64,
    /// Its cycle limit, and whether its time limit stopped a thread
    pub cycle_limit: Option<u32>,
    pub stopped_by_time: bool,
    /// The run is sealed: the step's seal errors are part of its verdict
    pub sealed: bool,
    pub config: StepConfigLine,
    /// Each thread's time, bytes and errors against the others'
    pub threads: ThreadTimingReport,
    /// A latency step's latencies, by thread
    pub latency: Option<LatencyTestSummaryReport>,
    /// Where the suite stood as the step ended, its results in
    pub stamp: Option<ProgressSnapshot>,
}

/// A step's settings, as its report's Config line gives them: the parameter, the pattern mode, the
/// extent and the chunk with their sizes (the engine's `DefaultFormatter`), and the repetitions.
pub struct StepConfigLine {
    pub parameter: String,
    pub mode: String,
    pub extent: String,
    pub chunk: String,
    pub test_reps: u32,
    pub verify_reps: u32,
    pub write_read_cycles: u32,
}

/// The run's work by thread, by logical CPU and by physical core, each with the run's WHEA total on
/// its aggregate row. A table with no rows isn't shown.
pub struct CpuPerformanceSummary {
    pub by_thread: PerformanceByThreadReport,
    pub by_cpu: PerformanceByCpuReport,
    pub by_core: PerformanceByPhysicalCoreReport,
}
