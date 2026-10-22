//! The engine's one interface to whoever shows a run (TODO 29): the CLI (`cli_render`) and the
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
//! The run's events are here; the setup's join them in S4 (`doc/todo/29-split-design.md`).
//!
//! The types the events carry that a frontend outside the engine names are re-exported here, and
//! only those: the GUI's (`crates/tmr-gui`), its tests' included. The rest of the engine stays
//! private, so `dead_code` still checks it. A type an event carries is reachable from outside even
//! when it isn't re-exported, and so are its `pub` methods, which are then exported: compiled as
//! such, their callers change too (S3 found two rescans' error paths changed by `FailingBits`'). So
//! a payload's methods that only the engine calls are `pub(crate)` where it matters: `FailingBits`'
//! recording ones, which the rescans call, and `ProgressTracker`'s bookkeeping, as a frontend holds
//! the live tracker and must only read it (`snapshot`, `take_whea_descriptions`). Other payloads'
//! accessors that the engine alone calls (a `key`, a `detail`) are still `pub`: narrow one when its
//! callers' asm moves, as `FailingBits`' did.

use std::sync::Arc;
use std::time::Duration;

use crate::failing_bits::FailingBits;
use crate::memory::allocator::PageSizeLevel;
use crate::reporting::models::{
    ConsolidatedMemoryReport, FinalTestSummaryReport, LatencyTestSummaryReport, PerformanceByCpuReport,
    PerformanceByPhysicalCoreReport, PerformanceByThreadReport,
};

pub use crate::progress::{LiveFigures, ProgressSnapshot, ProgressTracker, RunOutcome, TestSnapshot, TestSummary};
pub use crate::reporting::models::{AllocationReport, AllocationThread, TestConfigurationReport, ThreadTimingReport};
pub use crate::runner::RunStatus;
pub use crate::smbios::MemoryModule;
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
}

/// What the engine tells a frontend.
pub enum Event {
    /// A run's facts, from its memory plan to its results file
    Run(Run),
}

impl From<Run> for Event {
    fn from(run: Run) -> Self {
        Event::Run(run)
    }
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

/// The machine under test, as a frontend's system panel shows it (TODO 7): the CPU, its caches and
/// the memory modules SMBIOS lists.
pub struct Machine {
    pub cpu: String,
    pub physical_cores: usize,
    pub logical_cpus: usize,
    /// Per core: L1 data, L1 instruction and L2; the L3, one instance's (shared by the cores of a
    /// CCD, or of the whole CPU); and the cache line. What `detection` found, or its fallbacks.
    pub l1_data: usize,
    pub l1_instruction: usize,
    pub l2: usize,
    pub l3: usize,
    pub cache_line: usize,
    pub detection: String,
    pub memory_modules: Vec<MemoryModule>,
}

/// The machine under test. The session detects it during setup, before the run's first event, and
/// logs what it found as it does; read after that event, this is that detection and logs nothing.
/// Until S4 hands the setup's facts over as events, it is how the GUI gets them.
pub fn machine() -> Machine {
    let system = crate::tests::get_system_info();
    let cache = system.get_cache_info();
    Machine {
        cpu: system.cpu_brand.clone(),
        physical_cores: system.physical_cores,
        logical_cpus: system.logical_cores,
        l1_data: cache.per_core_l1d,
        l1_instruction: cache.per_core_l1i,
        l2: cache.per_core_l2,
        l3: cache.l3_cache,
        cache_line: cache.cache_line_size,
        detection: cache.detection_method.clone(),
        memory_modules: crate::smbios::get_smbios().memory_modules.clone(),
    }
}
