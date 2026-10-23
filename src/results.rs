use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::constants::{BYTES_PER_GIB_F64, MB_F64};
use crate::failing_bits::FailingBits;
use crate::formatting::{serialize_round_2dp, serialize_round_int, serialize_round_opt_2dp};
use crate::progress::TestSummary;

/// For `skip_serializing_if`: a count that is zero is left out, and reads back as zero.
fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// For `skip_serializing_if`: a flag that is off is left out, and reads back as off.
fn is_false(b: &bool) -> bool {
    !*b
}

/// Writes `bytes` to `path` and flushes them to the disk (`FlushFileBuffers`) before it returns
/// (TODO 72). NTFS logs a rename at once but writes a file's data back later, so without the flush
/// a system that went down just after a save could keep the rename and lose the data it names.
fn write_to_disk(path: &str, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut file = fs::File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestRunResult {
    pub metadata: TestRunMetadata,
    /// Which machine produced this result (TODO #67). Filled in by `new()` rather than a setter —
    /// the field this replaced was an all-placeholder `SystemInfoSnapshot` whose setter nothing
    /// ever called, so every result claimed `CPU: Unknown, 0.00 GiB`.
    pub identity: crate::run_context::RunIdentity,
    /// What was actually allocated and executed (TODO #67). Required, and taken by `new()`, so a
    /// saved result can never be missing the context needed to interpret its own numbers.
    pub run_config: crate::run_context::RunConfigSnapshot,
    pub cycles: Vec<CycleResult>,
    pub overall_stats: OverallStats,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestRunMetadata {
    pub tmr_version: String,
    /// The rest of the build that wrote it (TODO 72); absent from results written before it
    #[serde(default)]
    pub build: Option<BuildRecord>,
    /// Unix timestamp — zone-free, the machine-readable anchor.
    pub start_time: u64,
    /// Start time in UTC. Kept because result files get collected from several machines and
    /// consolidated centrally, where local stamps from different boxes cannot be ordered.
    pub start_time_utc: String,
    /// The same instant in the running machine's local zone, **with its offset** (e.g.
    /// `2026-09-08 14:18:23 +10:00`). This is the one a human matches against `logs/` and against
    /// their own memory of when they ran it; the offset is recorded so it stays unambiguous once
    /// the file leaves this machine, and across DST.
    pub start_time_local: String,
    /// Generated filename. Shares its stem with this run's log file — see
    /// [`crate::run_context::run_file_stem`].
    pub filename: String,
    pub config_name: Option<String>, // If loaded from config
}

/// The build that wrote a results file (`build_info::BUILD`, TODO 72). Its version is
/// `tmr_version`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BuildRecord {
    pub commit: String,
    pub dirty: bool,
    pub rustc: String,
    pub profile: String,
}

impl From<crate::build_info::BuildInfo> for BuildRecord {
    fn from(build: crate::build_info::BuildInfo) -> Self {
        Self { commit: build.commit.to_string(), dirty: build.dirty, rustc: build.rustc.to_string(), profile: build.profile.to_string() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CycleResult {
    pub cycle_number: u32,
    pub duration_secs: u32,
    /// The cycle stopped early, on Ctrl+C or errors=halt, before its final seal check (TODO 72): a
    /// step was cut short or didn't run, or only the check was left. Absent for a cycle that ran to
    /// its end, and in results written before it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub partial: bool,
    /// One per step, in cycle order (TODO 74)
    pub tests: Vec<TestResult>,
    /// The seal's stages, in a sealed run (TODO 74)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seal: Option<CycleSeal>,
    pub cycle_stats: CycleStats,
}

/// A cycle's run-wide seal stages (TODO 74): sealing all memory at its start, checking it all at
/// its end. Their time is the cycle's, not any step's.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CycleSeal {
    pub seal_ms: u128,
    pub seal_bytes: u64,
    /// Checking and resealing between steps, around those that never take the seal
    pub between_steps_ms: u128,
    pub final_check_ms: u128,
    pub final_check_bytes: u64,
    /// TM5 numbers them 0
    pub final_check_errors: u64,
    /// The bits that failed the final check (TODO 97)
    #[serde(default, skip_serializing_if = "FailingBits::is_empty")]
    pub final_check_failing_bits: FailingBits,
}

/// One step of a cycle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestResult {
    /// The step's 1-based position in the cycle (TODO 74)
    pub step: usize,
    /// The test's config id (a TM5 import's test number)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Which of the plan's tests the step ran: steps of one test share it
    pub test_index: usize,
    pub name: String,
    /// The step's name in reports, e.g. `Mem-SimpleV2-512_A (Test 12)`
    pub label: String,
    pub duration_ms: u128,
    /// The test's own time, its slowest thread's: the step's without the setup a test leaves out of
    /// its clock (TODO 90), which its speed divides by; the console's `Test:` time (TODO 72). 0 in
    /// results written before it.
    #[serde(default, serialize_with = "serialize_round_2dp")]
    pub timed_ms: f64,
    pub bytes_processed: u64,
    /// The operations its threads counted, the console's `ops` (TODO 72); 0 in results written
    /// before it
    #[serde(default)]
    pub operations: u64,
    /// Passes over its extent, the fewest and the most any of the step's threads completed
    #[serde(default)]
    pub cycles_completed_min: u32,
    #[serde(default)]
    pub cycles_completed_max: u32,
    /// What ended the step: "cycles" (its cycle limit), "time" (its time limit) or "stopped"
    /// (errors=halt, Ctrl+C, or a test that ends without either). Absent for a step that didn't
    /// run, and in results written before it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped_by: Option<String>,
    /// The step's work around its test sequence, by operation (`ops::Op::key`): the seal's
    /// between the steps and around its chunks, its fills. Only the operations it did.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub intermediate: OpRecords,
    /// Recorded in MiB/s only. A GiB/s field alongside it would be the same measurement stored
    /// twice — every producer computed it as `mib_s / 1024.0` — and rounding the pair to
    /// different scales made the two disagree. Readers that want GiB/s divide by 1024.
    #[serde(serialize_with = "serialize_round_int")]
    pub throughput_mib_s: f64,
    pub errors: u64,
    /// Checks that found a difference (TODO 27). `errors` counts the bad words their rescans found,
    /// and one for each check a rescan didn't reproduce (`not_reproduced`): a failing check and a
    /// bad word are different observations.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub alarms: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub not_reproduced: u64,
    /// The bits that failed the step's own checks (TODO 97): each bit's failed checks, and the bad
    /// words its rescans found with it wrong, 0→1 and 1→0. None on a clean step.
    #[serde(default, skip_serializing_if = "FailingBits::is_empty")]
    pub failing_bits: FailingBits,
    /// What the seal checks before the step's chunks found (TODO 74): TM5's test 0 errors
    pub seal_errors: u64,
    /// The bits that failed them
    #[serde(default, skip_serializing_if = "FailingBits::is_empty")]
    pub seal_failing_bits: FailingBits,
    // OS-reported hardware errors during this test, and the corrected subset (see whea.rs).
    pub whea_total: u64,
    pub whea_corrected: u64,
    // Latency metrics (Some for latency tests, None for other tests)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_samples: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "serialize_round_opt_2dp")]
    pub latency_p5_ns: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "serialize_round_opt_2dp")]
    pub latency_p10_ns: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "serialize_round_opt_2dp")]
    pub latency_p25_ns: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "serialize_round_opt_2dp")]
    pub latency_p50_ns: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "serialize_round_opt_2dp")]
    pub latency_p75_ns: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "serialize_round_opt_2dp")]
    pub latency_p90_ns: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "serialize_round_opt_2dp")]
    pub latency_p95_ns: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "serialize_round_opt_2dp")]
    pub latency_p99_ns: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "serialize_round_opt_2dp")]
    pub latency_p99_9_ns: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "serialize_round_opt_2dp")]
    pub latency_spread: Option<f64>,
    /// Each thread's part of the step, by thread (TODO 72): what the console's thread table shows,
    /// and where the thread ran. Absent for a step that didn't run, and in results written before
    /// it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub threads: Vec<ThreadStepRecord>,
}

/// One thread's part of a step (TODO 72). With its CPU, core and node, the run's CPU Performance
/// Summary can be worked out again from the file.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ThreadStepRecord {
    pub thread: usize,
    /// The logical CPU it was assigned, that CPU's physical core and its NUMA node; absent for a
    /// thread the pool has no assignment for
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub core: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<u32>,
    pub bytes: u64,
    /// Its own time on the test (`WorkResult::timed`), else its time on the step
    #[serde(serialize_with = "serialize_round_2dp")]
    pub ms: f64,
    pub errors: u64,
    /// As the step's, its own
    #[serde(default, skip_serializing_if = "is_zero")]
    pub alarms: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub not_reproduced: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub seal_errors: u64,
    pub operations: u64,
    /// Passes over its extent
    pub cycles: u32,
    /// A latency test's figures, as the latency table's row for the thread
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency: Option<ThreadLatency>,
}

/// One thread's latencies in a step (`ThreadStepRecord::latency`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct ThreadLatency {
    #[serde(serialize_with = "serialize_round_2dp")]
    pub min_ns: f64,
    #[serde(serialize_with = "serialize_round_2dp")]
    pub p50_ns: f64,
    #[serde(serialize_with = "serialize_round_2dp")]
    pub p99_ns: f64,
    /// P95 / P5
    #[serde(serialize_with = "serialize_round_2dp")]
    pub spread: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CycleStats {
    pub total_bytes: u64,
    pub total_errors: u64,
    #[serde(serialize_with = "serialize_round_int")]
    pub avg_throughput_mib_s: f64,
}

/// One operation's work in the results file (TODO 90): its time and the bytes it covered.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct OpRecord {
    #[serde(serialize_with = "serialize_round_2dp")]
    pub ms: f64,
    pub bytes: u64,
}

/// Operations by their key (`ops::Op::key`)
pub type OpRecords = BTreeMap<String, OpRecord>;

/// The operations a tally did, as records.
pub fn op_records(ops: &crate::ops::OpTally) -> OpRecords {
    ops.iter()
        .filter(|(_, work)| !work.is_empty())
        .map(|(op, work)| (op.key().to_string(), OpRecord { ms: work.time.as_secs_f64() * 1000.0, bytes: work.bytes }))
        .collect()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverallStats {
    /// Saved between cycles with the run still going (TODO 72): after the first cycle, then at most
    /// once a minute (`runner::RESULTS_SAVE_INTERVAL`), and the figures are the cycles that had
    /// finished. A run killed, lost or crashed leaves its last such save. False in the final save,
    /// and in results written before it.
    #[serde(default)]
    pub in_progress: bool,
    /// How the run ended, in the final save: `completed` (its cycle limit), `time_limit`,
    /// `interrupted` (Ctrl+C) or `halted` (errors=halt); and its verdict, as its exit status gives
    /// it: `passed`, `failed`, `not_run` or `interrupted`. Absent mid-run, and in results written
    /// before them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    pub total_runtime_secs: u64,
    pub cycles_completed: u32,
    #[serde(serialize_with = "serialize_round_2dp")]
    pub total_data_processed_gib: f64,
    #[serde(serialize_with = "serialize_round_int")]
    pub overall_throughput_mib_s: f64,
    pub total_errors: u64,
    // Run-level WHEA tally, taken from the monitor's cumulative counters rather than summed from
    // the per-test figures (see `set_whea_totals`).
    pub whea_total: u64,
    pub whea_corrected: u64,
    /// Whether WHEA monitoring was actually running. Without this, `whea_total: 0` is ambiguous
    /// between "no hardware errors" and "we never looked" — which would read as a clean bill of
    /// health it did not earn.
    pub whea_monitored: bool,
    /// The seal's errors, all of them in `total_errors` too (TODO 74)
    pub seal_errors: u64,
    /// The bits that failed the tests' own checks over the run, and the seal's checks (TODO 97): as
    /// a step's, the seal's apart
    #[serde(default, skip_serializing_if = "FailingBits::is_empty")]
    pub failing_bits: FailingBits,
    #[serde(default, skip_serializing_if = "FailingBits::is_empty")]
    pub seal_failing_bits: FailingBits,
    /// Each step's figures over the cycles (TODO 74)
    /// The run's work around its tests, by operation (TODO 90): every step's, and each cycle's
    /// seal at its start and check at its end
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub intermediate: OpRecords,
    pub per_step_averages: Vec<TestAverage>,
}

/// One step's figures over the cycles it ran in.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestAverage {
    pub step: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub test_index: usize,
    pub name: String,
    pub label: String,
    pub avg_duration_ms: u128,
    pub avg_bytes_processed: u64,
    #[serde(serialize_with = "serialize_round_int")]
    pub avg_throughput_mib_s: f64,
    pub total_errors: u64,
    pub total_seal_errors: u64,
    /// The bits that failed the step's own checks and its seal checks, over every cycle (TODO 97)
    #[serde(default, skip_serializing_if = "FailingBits::is_empty")]
    pub failing_bits: FailingBits,
    #[serde(default, skip_serializing_if = "FailingBits::is_empty")]
    pub seal_failing_bits: FailingBits,
    /// Cycles the step ran in
    pub runs: u64,
    // WHEA counts summed across every cycle this test ran in (totals, not averages — a hardware
    // error is an event count, and averaging it would hide a single-cycle fault).
    pub whea_total: u64,
    pub whea_corrected: u64,
    // Latency metrics (averaged across cycles, Some for latency tests, None for other tests)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_samples: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "serialize_round_opt_2dp")]
    pub latency_p5_ns: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "serialize_round_opt_2dp")]
    pub latency_p10_ns: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "serialize_round_opt_2dp")]
    pub latency_p25_ns: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "serialize_round_opt_2dp")]
    pub latency_p50_ns: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "serialize_round_opt_2dp")]
    pub latency_p75_ns: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "serialize_round_opt_2dp")]
    pub latency_p90_ns: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "serialize_round_opt_2dp")]
    pub latency_p95_ns: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "serialize_round_opt_2dp")]
    pub latency_p99_ns: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "serialize_round_opt_2dp")]
    pub latency_p99_9_ns: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "serialize_round_opt_2dp")]
    pub latency_spread: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestComparison {
    pub baseline: TestRunMetadata,
    pub current: TestRunMetadata,
    pub comparison_time: String,
    pub overall_comparison: OverallComparison,
    pub per_test_comparisons: Vec<TestComparisonResult>,
    /// Hardware and configuration differences between the two runs (TODO #67). Non-empty means the
    /// percentages above are at least partly explained by something other than a code change; any
    /// entry with `invalidates` set means they cannot be attributed at all.
    pub run_differences: Vec<crate::run_context::RunDifference>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverallComparison {
    pub runtime_diff_secs: i64,
    #[serde(serialize_with = "serialize_round_2dp")]
    pub runtime_diff_percent: f64,
    #[serde(serialize_with = "serialize_round_int")]
    pub throughput_diff_mib_s: f64,
    #[serde(serialize_with = "serialize_round_2dp")]
    pub throughput_diff_percent: f64,
    #[serde(serialize_with = "serialize_round_2dp")]
    pub data_processed_diff_gib: f64,
    pub errors_diff: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestComparisonResult {
    pub step: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub name: String,
    pub label: String,
    pub duration_diff_ms: i128,
    #[serde(serialize_with = "serialize_round_2dp")]
    pub duration_diff_percent: f64,
    #[serde(serialize_with = "serialize_round_int")]
    pub throughput_diff_mib_s: f64,
    #[serde(serialize_with = "serialize_round_2dp")]
    pub throughput_diff_percent: f64,
    pub bytes_diff: i64,
    pub errors_diff: i64,
}

impl TestRunResult {
    /// Start a result file for this run.
    ///
    /// `run_config` is a constructor argument rather than a setter because its values are only
    /// *observed* facts once allocation has returned and the thread pool exists — and because the
    /// placeholder-snapshot bug this replaced was caused precisely by an optional setter nothing
    /// called. Callers must therefore build it after `ThreadPool::new`.
    pub fn new(run_config: crate::run_context::RunConfigSnapshot) -> Self {
        // Create results directory if it doesn't exist
        if let Err(e) = std::fs::create_dir_all("results") {
            log::warn!("Failed to create results directory: {}", e);
        }

        // Read from `run_start()` rather than sampling the clock here: this runs after allocation
        // and `ThreadPool::new`, so a fresh `now()` would be seconds later than the one that named
        // the log file, leaving the pair unmatchable. See `run_context::run_start`.
        let started = crate::run_context::run_start();
        let filename = format!("results/{}.json", crate::run_context::run_file_stem());

        Self {
            metadata: TestRunMetadata {
                tmr_version: crate::BUILD.version.to_string(),
                build: Some(crate::BUILD.into()),
                start_time: started.timestamp() as u64,
                start_time_utc: started.with_timezone(&chrono::Utc).format("%Y-%m-%d %H:%M:%S UTC").to_string(),
                start_time_local: started.format("%Y-%m-%d %H:%M:%S %:z").to_string(),
                filename: filename.clone(),
                config_name: run_config.config_name.clone(),
            },
            // Detected here rather than via a setter, for the same reason as `run_config`.
            identity: crate::run_context::RunIdentity::detect(),
            run_config,
            cycles: Vec::new(),
            overall_stats: OverallStats {
                in_progress: false,
                outcome: None,
                status: None,
                total_runtime_secs: 0,
                cycles_completed: 0,
                total_data_processed_gib: 0.0,
                overall_throughput_mib_s: 0.0,
                total_errors: 0,
                whea_total: 0,
                whea_corrected: 0,
                whea_monitored: false,
                seal_errors: 0,
                failing_bits: FailingBits::default(),
                seal_failing_bits: FailingBits::default(),
                intermediate: OpRecords::new(),
                per_step_averages: Vec::new(),
            },
        }
    }

    /// Adds a cycle's steps; `partial` when it stopped early (`CycleResult::partial`).
    pub fn add_cycle(
        &mut self, cycle_number: u32, duration_secs: u32, test_summaries: Vec<TestSummary>, seal: Option<CycleSeal>, partial: bool,
    ) {
        let total_bytes: u64 = test_summaries.iter().map(|t| t.bytes_processed).sum();
        let total_errors: u64 =
            test_summaries.iter().map(|t| t.errors + t.seal_errors).sum::<u64>() + seal.as_ref().map_or(0, |s| s.final_check_errors);
        let avg_throughput_mib = if duration_secs > 0 { (total_bytes as f64 / MB_F64) / duration_secs as f64 } else { 0.0 };

        let tests: Vec<TestResult> = test_summaries
            .iter()
            .map(|summary| {
                TestResult {
                    step: summary.step,
                    id: summary.id.clone(),
                    test_index: summary.test_index,
                    name: summary.name.clone(),
                    label: summary.label.clone(),
                    duration_ms: summary.duration_ms,
                    timed_ms: summary.timed_ms,
                    bytes_processed: summary.bytes_processed,
                    operations: summary.operations,
                    cycles_completed_min: summary.cycles_completed.0,
                    cycles_completed_max: summary.cycles_completed.1,
                    stopped_by: summary.stopped_by.map(str::to_string),
                    intermediate: op_records(&summary.ops),
                    throughput_mib_s: summary.throughput_mib_s,
                    errors: summary.errors,
                    alarms: summary.alarms,
                    not_reproduced: summary.not_reproduced,
                    failing_bits: summary.failing_bits.clone(),
                    seal_errors: summary.seal_errors,
                    seal_failing_bits: summary.seal_failing_bits.clone(),
                    whea_total: summary.whea_total,
                    whea_corrected: summary.whea_corrected,
                    // Copy latency data from TestSummary
                    latency_samples: summary.latency_samples,
                    latency_p5_ns: summary.latency_p5_ns,
                    latency_p10_ns: summary.latency_p10_ns,
                    latency_p25_ns: summary.latency_p25_ns,
                    latency_p50_ns: summary.latency_p50_ns,
                    latency_p75_ns: summary.latency_p75_ns,
                    latency_p90_ns: summary.latency_p90_ns,
                    latency_p95_ns: summary.latency_p95_ns,
                    latency_p99_ns: summary.latency_p99_ns,
                    latency_p99_9_ns: summary.latency_p99_9_ns,
                    latency_spread: summary.latency_spread,
                    threads: summary.threads.clone(),
                }
            })
            .collect();

        let cycle_result = CycleResult {
            cycle_number,
            duration_secs,
            partial,
            tests,
            seal,
            cycle_stats: CycleStats { total_bytes, total_errors, avg_throughput_mib_s: avg_throughput_mib },
        };

        self.cycles.push(cycle_result);
    }

    /// Every step's operations, and each cycle's seal at its start and check at its end.
    fn intermediate_totals(&self) -> OpRecords {
        let mut totals = OpRecords::new();
        let mut add = |key: &str, ms: f64, bytes: u64| {
            let record = totals.entry(key.to_string()).or_default();
            (record.ms, record.bytes) = (record.ms + ms, record.bytes + bytes);
        };
        for cycle in &self.cycles {
            for test in &cycle.tests {
                for (key, record) in &test.intermediate {
                    add(key, record.ms, record.bytes);
                }
            }
            if let Some(seal) = &cycle.seal {
                use crate::ops::Op;
                if seal.seal_bytes > 0 {
                    add(Op::SealFill.key(), seal.seal_ms as f64, seal.seal_bytes);
                }
                if seal.final_check_bytes > 0 {
                    add(Op::SealCheck.key(), seal.final_check_ms as f64, seal.final_check_bytes);
                }
            }
        }
        totals
    }

    pub fn finalize(&mut self, total_runtime: std::time::Duration) {
        self.overall_stats.total_runtime_secs = total_runtime.as_secs();
        self.overall_stats.cycles_completed = self.cycles.len() as u32;

        let total_bytes: u64 = self.cycles.iter().map(|c| c.cycle_stats.total_bytes).sum();
        self.overall_stats.total_data_processed_gib = total_bytes as f64 / BYTES_PER_GIB_F64;

        self.overall_stats.overall_throughput_mib_s =
            if total_runtime.as_secs() > 0 { (total_bytes as f64 / MB_F64) / total_runtime.as_secs() as f64 } else { 0.0 };

        self.overall_stats.total_errors = self.cycles.iter().map(|c| c.cycle_stats.total_errors).sum();
        self.overall_stats.seal_errors = self
            .cycles
            .iter()
            .map(|c| c.tests.iter().map(|t| t.seal_errors).sum::<u64>() + c.seal.as_ref().map_or(0, |s| s.final_check_errors))
            .sum();
        let (mut bits, mut seal_bits) = (FailingBits::default(), FailingBits::default());
        for cycle in &self.cycles {
            for test in &cycle.tests {
                bits.add(&test.failing_bits);
                seal_bits.add(&test.seal_failing_bits);
            }
            if let Some(seal) = &cycle.seal {
                seal_bits.add(&seal.final_check_failing_bits);
            }
        }
        (self.overall_stats.failing_bits, self.overall_stats.seal_failing_bits) = (bits, seal_bits);
        self.overall_stats.intermediate = self.intermediate_totals();

        // Calculate per-test averages
        self.calculate_per_test_averages();
    }

    fn calculate_per_test_averages(&mut self) {
        // Aggregation struct to track per-test statistics including latency
        #[derive(Default)]
        struct TestAggregate {
            id: Option<String>,
            test_index: usize,
            name: String,
            label: String,
            runs: u64,
            total_duration_ms: u128,
            /// Each run's own time (its bytes over its speed, which leaves setup out, TODO 90)
            total_timed_secs: f64,
            total_bytes: u64,
            total_errors: u64,
            total_seal_errors: u64,
            failing_bits: FailingBits,
            seal_failing_bits: FailingBits,
            total_whea: u64,
            total_whea_corrected: u64,
            // Latency aggregation
            latency_count: u64, // Number of cycles with latency data
            latency_samples_sum: u64,
            latency_p5_sum: f64,
            latency_p10_sum: f64,
            latency_p25_sum: f64,
            latency_p50_sum: f64,
            latency_p75_sum: f64,
            latency_p90_sum: f64,
            latency_p95_sum: f64,
            latency_p99_sum: f64,
            latency_p99_9_sum: f64,
            latency_spread_sum: f64,
        }

        let mut test_aggregates: HashMap<usize, TestAggregate> = HashMap::new();

        for cycle in &self.cycles {
            for test in &cycle.tests {
                let entry = test_aggregates.entry(test.step).or_default();
                if entry.runs == 0 {
                    (entry.id, entry.test_index, entry.name, entry.label) =
                        (test.id.clone(), test.test_index, test.name.clone(), test.label.clone());
                }
                // A step a halt stopped before it ran carries only the seal check's errors
                if test.duration_ms == 0 && test.bytes_processed == 0 {
                    entry.seal_failing_bits.add(&test.seal_failing_bits);
                    entry.total_errors += test.errors;
                    entry.total_seal_errors += test.seal_errors;
                    continue;
                }
                entry.runs += 1;
                entry.failing_bits.add(&test.failing_bits);
                entry.seal_failing_bits.add(&test.seal_failing_bits);
                entry.total_duration_ms += test.duration_ms;
                if test.throughput_mib_s > 0.0 {
                    entry.total_timed_secs += (test.bytes_processed as f64 / MB_F64) / test.throughput_mib_s;
                }
                entry.total_bytes += test.bytes_processed;
                entry.total_errors += test.errors;
                entry.total_seal_errors += test.seal_errors;
                entry.total_whea += test.whea_total;
                entry.total_whea_corrected += test.whea_corrected;

                // Aggregate latency data if present
                if let Some(samples) = test.latency_samples {
                    entry.latency_count += 1;
                    entry.latency_samples_sum += samples;
                    entry.latency_p5_sum += test.latency_p5_ns.unwrap_or(0.0);
                    entry.latency_p10_sum += test.latency_p10_ns.unwrap_or(0.0);
                    entry.latency_p25_sum += test.latency_p25_ns.unwrap_or(0.0);
                    entry.latency_p50_sum += test.latency_p50_ns.unwrap_or(0.0);
                    entry.latency_p75_sum += test.latency_p75_ns.unwrap_or(0.0);
                    entry.latency_p90_sum += test.latency_p90_ns.unwrap_or(0.0);
                    entry.latency_p95_sum += test.latency_p95_ns.unwrap_or(0.0);
                    entry.latency_p99_sum += test.latency_p99_ns.unwrap_or(0.0);
                    entry.latency_p99_9_sum += test.latency_p99_9_ns.unwrap_or(0.0);
                    entry.latency_spread_sum += test.latency_spread.unwrap_or(0.0);
                }
            }
        }

        self.overall_stats.per_step_averages = test_aggregates
            .into_iter()
            .map(|(step, agg)| {
                // A step a halt stopped before it ever ran has no runs: it keeps its seal errors and
                // averages nothing
                let (avg_duration_ms, avg_bytes) = match agg.runs {
                    0 => (0, 0),
                    runs => (agg.total_duration_ms / runs as u128, agg.total_bytes / runs),
                };
                // Over the runs' own time, as each step's speed is (TODO 90)
                let avg_throughput_mib = if agg.total_timed_secs > 0.0 {
                    (agg.total_bytes as f64 / MB_F64) / agg.total_timed_secs
                } else if avg_duration_ms > 0 {
                    (avg_bytes as f64 / MB_F64) / (avg_duration_ms as f64 / 1000.0)
                } else {
                    0.0
                };

                // Calculate averaged latency metrics if present
                let (
                    latency_samples,
                    latency_p5,
                    latency_p10,
                    latency_p25,
                    latency_p50,
                    latency_p75,
                    latency_p90,
                    latency_p95,
                    latency_p99,
                    latency_p99_9,
                    latency_spread,
                ) = if agg.latency_count > 0 {
                    let n = agg.latency_count as f64;
                    (
                        Some(agg.latency_samples_sum / agg.latency_count),
                        Some(agg.latency_p5_sum / n),
                        Some(agg.latency_p10_sum / n),
                        Some(agg.latency_p25_sum / n),
                        Some(agg.latency_p50_sum / n),
                        Some(agg.latency_p75_sum / n),
                        Some(agg.latency_p90_sum / n),
                        Some(agg.latency_p95_sum / n),
                        Some(agg.latency_p99_sum / n),
                        Some(agg.latency_p99_9_sum / n),
                        Some(agg.latency_spread_sum / n),
                    )
                } else {
                    (None, None, None, None, None, None, None, None, None, None, None)
                };

                TestAverage {
                    step,
                    id: agg.id,
                    test_index: agg.test_index,
                    name: agg.name,
                    label: agg.label,
                    avg_duration_ms,
                    avg_bytes_processed: avg_bytes,
                    avg_throughput_mib_s: avg_throughput_mib,
                    total_errors: agg.total_errors,
                    total_seal_errors: agg.total_seal_errors,
                    failing_bits: agg.failing_bits,
                    seal_failing_bits: agg.seal_failing_bits,
                    runs: agg.runs,
                    whea_total: agg.total_whea,
                    whea_corrected: agg.total_whea_corrected,
                    latency_samples,
                    latency_p5_ns: latency_p5,
                    latency_p10_ns: latency_p10,
                    latency_p25_ns: latency_p25,
                    latency_p50_ns: latency_p50,
                    latency_p75_ns: latency_p75,
                    latency_p90_ns: latency_p90,
                    latency_p95_ns: latency_p95,
                    latency_p99_ns: latency_p99,
                    latency_p99_9_ns: latency_p99_9,
                    latency_spread,
                }
            })
            .collect();

        self.overall_stats.per_step_averages.sort_by_key(|t| t.step);
    }

    /// Records the run-level WHEA tally.
    ///
    /// Deliberately not summed from the per-test figures: those are deltas around test execution,
    /// so an event logged between tests, during allocation, or after an early exit belongs to the
    /// run but to no single test. The monitor's cumulative counter has all of them. Call after
    /// [`finalize`](Self::finalize) — it does not touch these fields.
    pub fn set_whea_totals(&mut self, whea: crate::whea::WheaCounts, monitored: bool) {
        self.overall_stats.whea_total = whea.total;
        self.overall_stats.whea_corrected = whea.corrected;
        self.overall_stats.whea_monitored = monitored;
    }

    pub fn get_filename(&self) -> &str {
        &self.metadata.filename
    }

    /// Writes the file whole or not at all (TODO 72): to `<path>.tmp`, on the disk, then renamed
    /// over `path`, so a run killed mid-write, or a system that went down, leaves its last save,
    /// not part of one. A refused rename, as when a viewer holds the file without letting it be
    /// deleted, falls back to writing `path` in place, with a warning; the next save tries the
    /// rename again. When the viewer won't let it be written either, the save stays in the `.tmp`,
    /// and the error names both.
    pub fn save_atomic(&self, path: &str) -> Result<(), String> {
        let json = serde_json::to_string_pretty(self).map_err(|e| format!("Failed to serialize test result: {}", e))?;
        let temp = format!("{path}.tmp");
        write_to_disk(&temp, json.as_bytes()).map_err(|e| format!("Failed to write test result file {temp}: {e}"))?;
        if let Err(refused) = fs::rename(&temp, path) {
            log::warn!("Couldn't replace {path} with {temp} ({refused}): writing it in place");
            write_to_disk(path, json.as_bytes())
                .map_err(|e| format!("Couldn't replace {path} ({refused}) or write it in place ({e}): this save is in {temp}"))?;
            let _ = fs::remove_file(&temp);
        }
        Ok(())
    }

    pub fn load_from_file(path: &str) -> Result<Self, String> {
        if !Path::new(path).exists() {
            return Err(format!("Test result file does not exist: {}", path));
        }

        let content = fs::read_to_string(path).map_err(|e| format!("Failed to read test result file: {}", e))?;

        serde_json::from_str(&content).map_err(|e| format!("Failed to parse test result JSON: {}", e))
    }
}

pub fn compare_test_results(baseline_path: &str, current_path: &str) -> Result<TestComparison, String> {
    let baseline = TestRunResult::load_from_file(baseline_path)?;
    let current = TestRunResult::load_from_file(current_path)?;

    let comparison_time = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC").to_string();

    // Overall comparison
    let runtime_diff_secs = current.overall_stats.total_runtime_secs as i64 - baseline.overall_stats.total_runtime_secs as i64;
    let runtime_diff_percent = if baseline.overall_stats.total_runtime_secs > 0 {
        (runtime_diff_secs as f64 / baseline.overall_stats.total_runtime_secs as f64) * 100.0
    } else {
        0.0
    };

    let throughput_diff_mib_s = current.overall_stats.overall_throughput_mib_s - baseline.overall_stats.overall_throughput_mib_s;
    let throughput_diff_percent = if baseline.overall_stats.overall_throughput_mib_s > 0.0 {
        (throughput_diff_mib_s / baseline.overall_stats.overall_throughput_mib_s) * 100.0
    } else {
        0.0
    };

    let overall_comparison = OverallComparison {
        runtime_diff_secs,
        runtime_diff_percent,
        throughput_diff_mib_s,
        throughput_diff_percent,
        data_processed_diff_gib: current.overall_stats.total_data_processed_gib - baseline.overall_stats.total_data_processed_gib,
        errors_diff: current.overall_stats.total_errors as i64 - baseline.overall_stats.total_errors as i64,
    };

    // Per-step comparisons: a step matches the baseline's step at the same position running the
    // same test (TODO 74), so a test that runs twice is compared twice
    let mut per_test_comparisons = Vec::new();
    let baseline_tests: HashMap<(usize, Option<String>, String), &TestAverage> =
        baseline.overall_stats.per_step_averages.iter().map(|t| ((t.step, t.id.clone(), t.name.clone()), t)).collect();

    for current_test in &current.overall_stats.per_step_averages {
        if let Some(baseline_test) = baseline_tests.get(&(current_test.step, current_test.id.clone(), current_test.name.clone())) {
            let duration_diff_ms = current_test.avg_duration_ms as i128 - baseline_test.avg_duration_ms as i128;
            let duration_diff_percent = if baseline_test.avg_duration_ms > 0 {
                (duration_diff_ms as f64 / baseline_test.avg_duration_ms as f64) * 100.0
            } else {
                0.0
            };

            let throughput_diff_mib_s = current_test.avg_throughput_mib_s - baseline_test.avg_throughput_mib_s;
            let throughput_diff_percent = if baseline_test.avg_throughput_mib_s > 0.0 {
                (throughput_diff_mib_s / baseline_test.avg_throughput_mib_s) * 100.0
            } else {
                0.0
            };

            per_test_comparisons.push(TestComparisonResult {
                step: current_test.step,
                id: current_test.id.clone(),
                name: current_test.name.clone(),
                label: current_test.label.clone(),
                duration_diff_ms,
                duration_diff_percent,
                throughput_diff_mib_s,
                throughput_diff_percent,
                bytes_diff: current_test.avg_bytes_processed as i64 - baseline_test.avg_bytes_processed as i64,
                errors_diff: current_test.total_errors as i64 - baseline_test.total_errors as i64,
            });
        }
    }

    // A file saved mid-run first (TODO 72): its figures are only the cycles that had finished, of a
    // run that was killed or lost, or is still going. Then identity, then configuration: a hardware
    // change is the more fundamental explanation, and on a new machine the config differences are
    // usually downstream of it.
    let mut run_differences = Vec::new();
    if baseline.overall_stats.in_progress || current.overall_stats.in_progress {
        let saved = |stats: &OverallStats| {
            let state = if stats.in_progress { "saved mid-run" } else { "finished" };
            let cycles = stats.cycles_completed;
            format!("{state} after {cycles} cycle{}", if cycles == 1 { "" } else { "s" })
        };
        run_differences.push(crate::run_context::RunDifference::new(
            "Results file",
            saved(&baseline.overall_stats),
            saved(&current.overall_stats),
            false,
        ));
    }
    run_differences.extend(crate::run_context::compare_identity(&baseline.identity, &current.identity));
    run_differences.extend(crate::run_context::compare_config(&baseline.run_config, &current.run_config));

    Ok(TestComparison {
        baseline: baseline.metadata,
        current: current.metadata,
        comparison_time,
        overall_comparison,
        per_test_comparisons,
        run_differences,
    })
}

impl TestComparison {
    pub fn to_text_report(&self) -> String {
        let mut report = String::new();

        report.push_str("=== TMR Test Result Comparison ===\n");
        report.push_str(&format!("Comparison Time: {}\n", self.comparison_time));
        // UTC here, unlike the single-run report above, because the two files being compared may
        // come from different machines in different zones — UTC is the only stamp that makes
        // "which ran first" answerable, and it matches `comparison_time`.
        report.push_str(&format!("Baseline: {} ({})\n", self.baseline.start_time_utc, self.baseline.filename));
        report.push_str(&format!("Current:  {} ({})\n", self.current.start_time_utc, self.current.filename));
        report.push('\n');

        // Deliberately placed *before* the numbers: the whole point of TODO #67 was that a reader
        // who sees "+20% faster" first has already drawn a conclusion by the time they reach a
        // caveat at the bottom.
        report.push_str(&crate::run_context::render_differences(&self.run_differences));

        report.push_str("Overall Performance Changes:\n");
        report.push_str(&format!(
            "  Runtime: {}{} seconds ({:+.1}%)\n",
            if self.overall_comparison.runtime_diff_secs >= 0 { "+" } else { "" },
            self.overall_comparison.runtime_diff_secs,
            self.overall_comparison.runtime_diff_percent
        ));

        report.push_str(&format!(
            "  Throughput: {:+.1} MiB/s ({:+.1}%)\n",
            self.overall_comparison.throughput_diff_mib_s, self.overall_comparison.throughput_diff_percent
        ));

        report.push_str(&format!("  Data Processed: {:+.2} GiB\n", self.overall_comparison.data_processed_diff_gib));

        if self.overall_comparison.errors_diff != 0 {
            report.push_str(&format!(
                "  Errors: {:+} ({})\n",
                self.overall_comparison.errors_diff,
                if self.overall_comparison.errors_diff > 0 { "WORSE" } else { "BETTER" }
            ));
        } else {
            report.push_str("  Errors: No change\n");
        }
        report.push('\n');

        if !self.per_test_comparisons.is_empty() {
            report.push_str("Per-Test Performance Changes:\n");
            for test in &self.per_test_comparisons {
                let duration_symbol = if test.duration_diff_ms < 0 {
                    "⬇"
                } else if test.duration_diff_ms > 0 {
                    "⬆"
                } else {
                    "="
                };
                let throughput_symbol = if test.throughput_diff_mib_s > 0.0 {
                    "⬆"
                } else if test.throughput_diff_mib_s < 0.0 {
                    "⬇"
                } else {
                    "="
                };

                report.push_str(&format!(
                    "  Step {}. {} - Duration: {}{:.1}ms ({:+.1}%) {}, Throughput: {:+.1} MiB/s ({:+.1}%) {}{}\n",
                    test.step,
                    test.label,
                    if test.duration_diff_ms >= 0 { "+" } else { "" },
                    test.duration_diff_ms as f64,
                    test.duration_diff_percent,
                    duration_symbol,
                    test.throughput_diff_mib_s,
                    test.throughput_diff_percent,
                    throughput_symbol,
                    if test.errors_diff != 0 { format!(", Errors: {:+}", test.errors_diff) } else { String::new() }
                ));
            }
        }

        report
    }

    pub fn save_to_file(&self, path: &str) -> Result<(), String> {
        let json = serde_json::to_string_pretty(self).map_err(|e| format!("Failed to serialize comparison: {}", e))?;

        fs::write(path, json).map_err(|e| format!("Failed to write comparison file: {}", e))
    }
}

/// A directory of a test's own in the system's temp directory, for the results files it writes,
/// removed with what it holds when it drops, a failed test's too.
#[cfg(test)]
pub(crate) struct TempDir(std::path::PathBuf);

#[cfg(test)]
impl TempDir {
    pub(crate) fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("tmr-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    pub(crate) fn file(&self, name: &str) -> String {
        self.0.join(name).to_str().unwrap().to_string()
    }
}

#[cfg(test)]
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::summary;

    /// A step that never ran, because the seal check before its first run failed under halt, keeps
    /// its seal errors through finalization and serialization and isn't a run. Astra's review: the
    /// averages divided by its zero runs, so a release build aborted before saving its results.
    #[test]
    fn a_step_that_never_ran_keeps_its_seal_errors() {
        let mut result = TestRunResult::new(Default::default());
        result.add_cycle(1, 10, vec![summary(1, 5_000, 1 << 30, 0), summary(2, 0, 0, 3)], None, false);
        result.finalize(std::time::Duration::from_secs(10));
        let never = result.overall_stats.per_step_averages.iter().find(|a| a.step == 2).unwrap();
        assert_eq!((never.runs, never.total_seal_errors, never.avg_duration_ms, never.avg_bytes_processed), (0, 3, 0, 0));
        let ran = result.overall_stats.per_step_averages.iter().find(|a| a.step == 1).unwrap();
        assert_eq!(ran.runs, 1);
        assert_eq!(result.overall_stats.seal_errors, 3);
        assert!(serde_json::to_string(&result).unwrap().contains("\"total_seal_errors\":3"));
        // The final summary shows it with its seal errors and zero figures, not 0/0 (review A)
        let report = crate::reporting::converters::create_overall_stats_summary_report(&result);
        let row = &report.per_test_summaries[1];
        assert_eq!((row.runs, row.seal_errors), (0, 3));
        assert_eq!((row.average_duration_secs, row.average_data_gib, row.average_throughput_mib_s), (0.0, 0.0, 0.0));
    }

    /// Failing bits add up by step over the cycles, and over the run with the seal's, the final
    /// checks' included; the final summary's table has a row per test, the seal's, and all (TODO 97).
    #[test]
    fn failing_bits_add_up_by_step_and_run() {
        let bits = |mask: u64, want: u64, actual: u64| {
            let mut b = FailingBits::default();
            b.add_check(mask);
            if want != actual {
                b.add_word(want, actual);
            }
            b
        };
        let mut result = TestRunResult::new(Default::default());
        let step = |n, test: FailingBits, seal: FailingBits| TestSummary {
            name: format!("test {n}"),
            failing_bits: test,
            seal_failing_bits: seal,
            ..summary(n, 5_000, 1 << 30, 0)
        };
        let final_check = |b: FailingBits| Some(CycleSeal { final_check_failing_bits: b, ..Default::default() });
        result.add_cycle(
            1,
            10,
            vec![step(1, bits(1 << 31, 0, 1 << 31), FailingBits::default()), step(2, FailingBits::default(), FailingBits::default())],
            final_check(FailingBits::default()),
            false,
        );
        result.add_cycle(
            2,
            10,
            vec![step(1, bits(1 << 31, 0, 0), FailingBits::default()), step(2, bits(1 << 39, 1 << 39, 0), bits(1 << 7, 0, 0))],
            final_check(bits(1 << 7, 0, 1 << 7)),
            false,
        );
        result.finalize(std::time::Duration::from_secs(20));
        let by_step: Vec<String> = result.overall_stats.per_step_averages.iter().map(|a| a.failing_bits.detail()).collect();
        assert_eq!(by_step, ["31: 2 / 1 (1 + 0)", "39: 1 / 1 (0 + 1)"]);
        assert_eq!(result.overall_stats.seal_failing_bits.detail(), "7: 2 / 1 (1 + 0)");
        assert_eq!(result.overall_stats.failing_bits.detail(), "31: 2 / 1 (1 + 0), 39: 1 / 1 (0 + 1)");
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains(r#""failing_bits":[{"bit":31,"checks":1,"words_0to1":1,"words_1to0":0}]"#), "{json}");
        let back: TestRunResult = serde_json::from_str(&json).unwrap();
        assert_eq!(back.overall_stats.failing_bits, result.overall_stats.failing_bits);

        use crate::reporting::formatters::ReportFormatter;
        let report = crate::reporting::converters::create_overall_stats_summary_report(&result);
        let table = crate::reporting::formatters::DefaultFormatter::new().prepare_failing_bits_table(&report);
        let rows: Vec<(&str, &str)> = table.rows.iter().map(|r| (r[0].as_str(), r[1].as_str())).collect();
        assert_eq!(rows, [
            ("test 1", "31: 2 / 1 (1 + 0)"),
            ("test 2", "39: 1 / 1 (0 + 1)"),
            ("Seal", "7: 2 / 1 (1 + 0)"),
            ("All", "7: 2 / 1 (1 + 0), 31: 2 / 1 (1 + 0), 39: 1 / 1 (0 + 1)")
        ]);
    }

    /// A results file as TMR wrote it before TODO 72's fields (`testdata/result_files/before_todo_72.json`):
    /// two sealed cycles of three steps, a latency step among them, with errors, failing bits, the
    /// seal's work and a WHEA event; its identity and config mostly empty. Its text, and as loaded.
    fn written_before_todo_72() -> (String, TestRunResult) {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata").join("result_files").join("before_todo_72.json");
        let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let result = TestRunResult::load_from_file(path.to_str().unwrap()).unwrap();
        (text, result)
    }

    /// A results file written before TODO 72 still loads, and reads as a finished run of whole
    /// cycles by an unknown build, with no thread records or operations; the final summary renders
    /// from it as from a new one.
    #[test]
    fn a_results_file_written_before_todo_72_still_loads() {
        let (text, result) = written_before_todo_72();
        // `threads` is a key of the run config's already
        for field in ["in_progress", "outcome", "status", "build", "partial", "timed_ms", "operations", "tests_left_out"] {
            assert!(!text.contains(&format!("\"{field}\"")), "the fixture is from before {field}");
        }
        assert_eq!((result.metadata.tmr_version.as_str(), result.metadata.build.as_ref()), ("1.0.0", None));
        let stats = &result.overall_stats;
        assert_eq!((stats.in_progress, stats.outcome.as_deref(), stats.status.as_deref()), (false, None, None));
        assert_eq!((stats.cycles_completed, stats.total_errors, stats.per_step_averages.len()), (2, 5, 3));
        assert!(result.cycles.iter().all(|c| !c.partial));
        let steps = || result.cycles.iter().flat_map(|c| &c.tests);
        assert!(steps().all(|t| t.threads.is_empty() && t.operations == 0 && t.timed_ms == 0.0));
        assert_eq!(steps().map(|t| t.errors).sum::<u64>(), 3);
        assert!(result.run_config.tests_left_out.is_empty());
        // A row for each of its two tests
        let report = crate::reporting::converters::create_overall_stats_summary_report(&result);
        assert_eq!(report.per_test_summaries.len(), 2);
    }

    /// TODO 72's fields read back as they were written, and those that are empty or off are left
    /// out: a cycle that ran to its end has no `partial`, a step no `threads`, a thread with no CPU
    /// no `cpu`, `core` or `node`, and its zero counts aren't written.
    #[test]
    fn the_new_fields_round_trip() {
        let (_, mut result) = written_before_todo_72();
        result.metadata.build = Some(crate::BUILD.into());
        result.overall_stats.in_progress = true;
        (result.overall_stats.outcome, result.overall_stats.status) = (Some("halted".into()), Some("failed".into()));
        result.cycles[1].partial = true;
        result.run_config.tests_left_out = vec![crate::run_context::LeftOutTest {
            name: "Mem-StuckBit512".into(),
            reason: "width 512 needs AVX-512, which this CPU lacks".into(),
        }];
        let step = &mut result.cycles[1].tests[1];
        (step.timed_ms, step.operations) = (3_975.25, 163_840);
        step.threads = vec![
            ThreadStepRecord {
                thread: 0,
                cpu: Some(2),
                core: Some(1),
                node: Some(0),
                bytes: 24_576,
                ms: 3_975.25,
                errors: 2,
                alarms: 1,
                not_reproduced: 1,
                seal_errors: 1,
                operations: 81_920,
                cycles: 1,
                latency: Some(ThreadLatency { min_ns: 70.5, p50_ns: 72.5, p99_ns: 85.0, spread: 1.08 }),
            },
            ThreadStepRecord { thread: 1, bytes: 24_576, ms: 3_900.5, operations: 81_920, cycles: 1, ..Default::default() },
        ];

        let json = serde_json::to_string_pretty(&result).unwrap();
        let back: TestRunResult = serde_json::from_str(&json).unwrap();
        assert_eq!(serde_json::to_string_pretty(&back).unwrap(), json);
        assert_eq!(back.metadata.build, Some(crate::BUILD.into()));
        let stats = &back.overall_stats;
        assert_eq!((stats.in_progress, stats.outcome.as_deref(), stats.status.as_deref()), (true, Some("halted"), Some("failed")));
        assert_eq!((back.cycles[0].partial, back.cycles[1].partial), (false, true));
        assert_eq!(back.cycles[1].tests[1].threads, result.cycles[1].tests[1].threads);
        assert_eq!((back.cycles[1].tests[1].timed_ms, back.cycles[1].tests[1].operations), (3_975.25, 163_840));
        assert_eq!(back.run_config.tests_left_out, result.run_config.tests_left_out);

        let keys = |value: serde_json::Value| value.as_object().unwrap().keys().cloned().collect::<Vec<_>>();
        assert!(!keys(serde_json::to_value(&result.cycles[0]).unwrap()).contains(&"partial".to_string()));
        assert!(!keys(serde_json::to_value(&result.cycles[1].tests[0]).unwrap()).contains(&"threads".to_string()));
        assert_eq!(keys(serde_json::to_value(&result.cycles[1].tests[1].threads[1]).unwrap()), [
            "bytes", "cycles", "errors", "ms", "operations", "thread"
        ]);
    }

    /// TODO 72: a save replaces the file whole, by renaming a temp file over it, and leaves nothing
    /// of the temp file: a reader that had the old file open, and lets it be deleted, still reads
    /// the old one, which a write in place wouldn't leave it. When a viewer holds the file without
    /// letting it be deleted, so it can't be renamed over, the save writes it in place instead;
    /// when the viewer won't let it be written either, the save stays in the temp file, which the
    /// error names, and the file is as it was.
    #[test]
    fn a_save_replaces_the_file_whole() {
        use std::io::Read;
        use std::os::windows::fs::OpenOptionsExt;

        use windows::Win32::Storage::FileSystem::{FILE_SHARE_DELETE, FILE_SHARE_MODE, FILE_SHARE_READ, FILE_SHARE_WRITE};
        let dir = TempDir::new("save-atomic");
        let (path, temp) = (dir.file("result.json"), dir.file("result.json.tmp"));
        let viewer = |share: FILE_SHARE_MODE| fs::OpenOptions::new().read(true).share_mode(share.0).open(&path).unwrap();
        fs::write(&path, "the last cycle's save").unwrap();
        let (_, mut result) = written_before_todo_72();
        let mut reader = viewer(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE);
        result.save_atomic(&path).unwrap();
        let mut old = String::new();
        reader.read_to_string(&mut old).unwrap();
        assert_eq!(old, "the last cycle's save", "the file was written over, not replaced");
        drop(reader);
        assert_eq!(TestRunResult::load_from_file(&path).unwrap().cycles.len(), 2);
        assert!(!Path::new(&temp).exists());

        let held = viewer(FILE_SHARE_READ | FILE_SHARE_WRITE);
        fs::write(dir.file("other.json"), "{}").unwrap();
        assert!(fs::rename(dir.file("other.json"), &path).is_err(), "the viewer lets nothing rename over the file");
        result.cycles.truncate(1);
        result.save_atomic(&path).unwrap();
        drop(held);
        assert_eq!(TestRunResult::load_from_file(&path).unwrap().cycles.len(), 1);
        assert!(!Path::new(&temp).exists());

        let held = viewer(FILE_SHARE_READ);
        result.cycles.clear();
        let error = result.save_atomic(&path).unwrap_err();
        drop(held);
        assert!(error.contains(&path) && error.contains(&temp), "{error}");
        assert_eq!(TestRunResult::load_from_file(&path).unwrap().cycles.len(), 1, "the file as it was");
        assert!(TestRunResult::load_from_file(&temp).unwrap().cycles.is_empty(), "the save, in the temp file");
    }

    /// TODO 72: `--compare-results` warns, before its figures, of a file saved mid-run, with the
    /// cycles each file has; two finished files get no such warning.
    #[test]
    fn a_comparison_warns_of_a_file_saved_mid_run() {
        let dir = TempDir::new("compare-mid-run");
        let (_, finished) = written_before_todo_72();
        let mut mid_run = finished.clone();
        mid_run.cycles.truncate(1);
        mid_run.finalize(std::time::Duration::from_secs(31));
        mid_run.overall_stats.in_progress = true;
        let (baseline, current) = (dir.file("finished.json"), dir.file("mid_run.json"));
        finished.save_atomic(&baseline).unwrap();
        mid_run.save_atomic(&current).unwrap();

        let comparison = compare_test_results(&baseline, &current).unwrap();
        let first = &comparison.run_differences[0];
        assert_eq!(
            (first.field.as_str(), first.baseline.as_str(), first.current.as_str(), first.invalidates),
            ("Results file", "finished after 2 cycles", "saved mid-run after 1 cycle", false)
        );
        let report = comparison.to_text_report();
        assert!(report.find("Results file").unwrap() < report.find("Overall Performance Changes").unwrap(), "{report}");
        let both_finished = compare_test_results(&baseline, &baseline).unwrap();
        assert!(both_finished.run_differences.iter().all(|d| d.field != "Results file"));
    }

    /// The per-cycle save on a long run (TODO 72): 32 threads, 16 steps (2 of them latency steps)
    /// and 50 sealed cycles, every step with its thread records, saved after each cycle as a run of
    /// cycles of a minute or more saves. Prints the final file's size, pretty and compact, the time
    /// to serialize and to save it, and what the run writes in all; the figures are in
    /// doc/todo/72-results-console-parity.md.
    /// `cargo test --release -- --ignored a_long_runs_results_file --nocapture`
    #[test]
    #[ignore = "a measurement: run it by name"]
    fn a_long_runs_results_file() {
        use std::time::{Duration, Instant};

        use crate::ops::Op;
        const THREADS: usize = 32;
        const STEPS: usize = 16;
        const CYCLES: u32 = 50;
        let (_, mut result) = written_before_todo_72();
        result.cycles.clear();
        let record = |thread: usize, step: usize, latency: bool| ThreadStepRecord {
            thread,
            cpu: Some(thread * 2),
            core: Some(thread),
            node: Some((thread / 16) as u32),
            bytes: 1_073_741_824 + 4096 * thread as u64,
            ms: 4_123.45 + thread as f64,
            errors: 0,
            operations: 134_217_728 + step as u64,
            cycles: 3,
            latency: latency.then_some(ThreadLatency { min_ns: 70.12, p50_ns: 72.53, p99_ns: 85.07, spread: 1.08 }),
            ..Default::default()
        };
        let step = |n: usize| {
            let latency = n > STEPS - 2;
            let mut ops = crate::ops::OpTally::default();
            ops.add(Op::SealCheck, Duration::from_millis(350), 32 << 30);
            ops.add(Op::SealFill, Duration::from_millis(420), 32 << 30);
            TestSummary {
                timed_ms: 4_155.45,
                operations: 4_294_967_296 + n as u64,
                ops,
                latency_samples: latency.then_some(64_000),
                latency_p50_ns: latency.then_some(72.53),
                latency_p99_ns: latency.then_some(85.07),
                latency_spread: latency.then_some(1.08),
                threads: (0..THREADS).map(|thread| record(thread, n, latency)).collect(),
                ..summary(n, 4_200, 32 << 30, 0)
            }
        };
        let seal = CycleSeal {
            seal_ms: 3_410,
            seal_bytes: 32 << 30,
            between_steps_ms: 900,
            final_check_ms: 3_380,
            final_check_bytes: 32 << 30,
            ..Default::default()
        };
        let (mut written, mut serializing) = (0usize, Duration::ZERO);
        for cycle in 1..=CYCLES {
            result.add_cycle(cycle, 75, (1..=STEPS).map(step).collect(), Some(seal.clone()), false);
            result.finalize(Duration::from_secs(75 * cycle as u64));
            result.overall_stats.in_progress = true;
            let start = Instant::now();
            written += serde_json::to_string_pretty(&result).unwrap().len();
            serializing += start.elapsed();
        }
        result.overall_stats.in_progress = false;
        let start = Instant::now();
        let pretty = serde_json::to_string_pretty(&result).unwrap();
        let pretty_time = start.elapsed();
        let compact = serde_json::to_string(&result).unwrap();
        let mut without_threads = result.clone();
        without_threads.cycles.iter_mut().flat_map(|c| &mut c.tests).for_each(|t| t.threads.clear());
        let without_threads = serde_json::to_string_pretty(&without_threads).unwrap().len();
        let dir = TempDir::new("long-run");
        let path = dir.file("result.json");
        let start = Instant::now();
        result.save_atomic(&path).unwrap();
        let save_time = start.elapsed();
        let records = THREADS * STEPS * CYCLES as usize;
        let mb = |bytes: usize| bytes as f64 / 1e6;
        println!("{THREADS} threads x {STEPS} steps x {CYCLES} cycles = {records} thread records");
        println!(
            "final file: pretty {:.2} MB ({:.2} MB without the thread records, {} B a record), compact {:.2} MB",
            mb(pretty.len()),
            mb(without_threads),
            (pretty.len() - without_threads) / records,
            mb(compact.len())
        );
        println!(
            "final save: to_string_pretty {:.1} ms, save_atomic {:.1} ms",
            pretty_time.as_secs_f64() * 1e3,
            save_time.as_secs_f64() * 1e3
        );
        println!("over the run: {:.1} MB written by {CYCLES} saves, {:.0} ms serializing", mb(written), serializing.as_secs_f64() * 1e3);
    }
}
