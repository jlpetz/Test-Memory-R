use crate::{ErrorMode, EnhancedMemoryLayout, ProgressTracker, BlockInfo};
use crate::constants::MB;
use crate::tests::{ExtentMode, ChunkMode, CacheTarget};
use crate::MemoryAllocationConfig;
use globset::{Glob, GlobMatcher};
use crate::tests::{TestStats, TestMemoryConfig, TestTiming, TestProgress};
use crate::tests::{
    stuck_bit_test_multi, stuck_bit_test_128_multi, stuck_bit_test_256_multi, stuck_bit_test_512_multi,
    stuck_bit_test_auto_multi,
    refresh_stable_multi, refresh_stable_128_multi, refresh_stable_256_multi, refresh_stable_512_multi,
    refresh_stable_auto_multi,
    simple_test_nt_128_multi, simple_test_nt_256_multi, simple_test_nt_512_multi,
    simple_test_nt_auto_multi,
    cache_busting_multi,
    random_torture_multi,
    stride_access_multi, block_move_multi, moving_inversions_multi, modulo_x_multi, sequence_auto_multi,
    // v2 tests
    simple_test_v2_multi,
    simple_test_v2_128_multi, simple_test_v2_256_multi, simple_test_v2_512_multi,
    simple_test_v2_auto_multi,
    mirror_move_v2_multi,
    mirror_move_v2_128_multi, mirror_move_v2_256_multi, mirror_move_v2_512_multi,
    mirror_move_v2_auto_multi,
    bench_init_multi,
    bench_verify_multi,
};
use crate::latency_tests::{
    read_latency_multi, write_latency_multi, copy_latency_multi,  // Full latency tests with percentiles
    LatencyTestStats,
};
use crate::latency_tests_v2::{
    // Layout A read (1 chain step + 1 SIMD-width data load per iteration)
    lat_v2_read_128_multi, lat_v2_read_256_multi, lat_v2_read_512_multi, lat_v2_read_auto_multi,
    // Layout A write (1 chain step + 1 SIMD-width cached store per iteration)
    lat_v2_write_128_multi, lat_v2_write_256_multi, lat_v2_write_512_multi, lat_v2_write_auto_multi,
    // Layout A copy (1 chain step + 1 SIMD-width read-modify-write per iteration)
    lat_v2_copy_128_multi, lat_v2_copy_256_multi, lat_v2_copy_512_multi, lat_v2_copy_auto_multi,
    // Layout B packed read (1 chain step + 6 SIMD-width data loads per iteration)
    lat_v2p_read_128_multi, lat_v2p_read_256_multi, lat_v2p_read_512_multi, lat_v2p_read_auto_multi,
    // Layout B packed write (1 chain step + 6 SIMD-width cached stores per iteration)
    lat_v2p_write_128_multi, lat_v2p_write_256_multi, lat_v2p_write_512_multi, lat_v2p_write_auto_multi,
    // Layout B packed copy (1 chain step + 6 SIMD-width read-modify-write per iteration)
    lat_v2p_copy_128_multi, lat_v2p_copy_256_multi, lat_v2p_copy_512_multi, lat_v2p_copy_auto_multi,
    // Layout B packed write FULL — writes entire 64B cache line per cell at every width
    lat_v2p_write_full_128_multi, lat_v2p_write_full_256_multi, lat_v2p_write_full_512_multi, lat_v2p_write_full_auto_multi,
    // Layout B packed copy FULL — RMW entire 64B cache line per cell at every width
    lat_v2p_copy_full_128_multi, lat_v2p_copy_full_256_multi, lat_v2p_copy_full_512_multi, lat_v2p_copy_full_auto_multi,
    // NT-write saturation PoC (Scalar = 8-byte MOVNTI, validates WCB-fill hypothesis)
    lat_ntw_write_scalar_multi,
    lat_ntw_write_128_multi, lat_ntw_write_256_multi, lat_ntw_write_512_multi, lat_ntw_write_auto_multi,
};
use crate::bandwidth_tests::{
    spd_read_auto_multi, spd_write_auto_multi, spd_write_nt_auto_multi,
    spd_copy_auto_multi, spd_copy_nt_auto_multi,
    // Concrete SIMD variants for auto-dispatch resolution
    spd_read_128_multi, spd_read_256_multi, spd_read_512_multi,
    spd_write_128_multi, spd_write_256_multi, spd_write_512_multi,
    spd_write_nt_128_multi, spd_write_nt_256_multi, spd_write_nt_512_multi,
    spd_copy_128_multi, spd_copy_256_multi, spd_copy_512_multi,
    spd_copy_nt_128_multi, spd_copy_nt_256_multi, spd_copy_nt_512_multi,
};
use crate::cache::CacheInfo;
use crate::reporting::models::{
    LatencyTestSummaryReport, LatencyLevelSummary, LatencyThreadResult, LatencyPercentiles,
};
use crate::progress::{progress_reporter, RunOutcome};
use crate::results::TestRunResult;
use crate::memory::MemoryBuffer;
use crate::{MemoryBackend, RuntimeConfig};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use std::sync::mpsc::Receiver;

// Use the proper ThreadPool from thread_pool.rs
use crate::thread_pool::{ThreadPool, WorkResult, CpuAssignment};

/// Per-thread result row collected during a test run:
/// `(thread_id, cpu_id, bytes, time, errors, operations, cycles_completed)`, the time the test's own
/// (`WorkResult::timed`).
type CpuStatRow = (usize, usize, u64, std::time::Duration, u64, u64, u32);
/// Per-test map from test name to its collected per-thread stat rows.
type TestCpuStats = HashMap<String, Vec<CpuStatRow>>;

/// Stable per-suite execution context shared across every test cycle. Only the
/// cycle number changes between cycles, so it's passed separately; everything
/// else is bundled here to keep `execute_test_cycle`'s signature manageable.
struct CycleContext<'a> {
    test_definitions: &'a [TestDefinition],
    thread_pool: &'a ThreadPool,
    result_receiver: &'a Receiver<WorkResult>,
    thread_count: usize,
    error_mode: ErrorMode,
    progress: &'a Arc<ProgressTracker>,
    test_run_result: &'a Arc<Mutex<TestRunResult>>,
    all_test_cpu_stats: &'a Arc<Mutex<TestCpuStats>>,
    cache_info: &'a CacheInfo,
    /// The seal's kernel when the run is sealed (TODO 74)
    seal: Option<crate::seal::SealKernel>,
    /// A TM5 import: steps are named by the `.cfg`'s test numbers
    tm5: bool,
}

/// Keeps the tests whose display, actual or original name matches one of the filter's
/// comma-separated globs, e.g. `"Lat-*,Mem-*-Read"`.
pub(crate) fn retain_matching(test_definitions: &mut Vec<TestDefinition>, filter: &str) {
    let matchers: Vec<GlobMatcher> = filter.split(',').map(|s| s.trim())
        .filter_map(|pattern| {
            match Glob::new(pattern) {
                Ok(glob) => Some(glob.compile_matcher()),
                Err(e) => {
                    log::warn!("Invalid glob pattern '{}': {}", pattern, e);
                    None
                }
            }
        })
        .collect();

    test_definitions.retain(|def| {
        matchers.iter().any(|matcher| {
            matcher.is_match(&def.display_name) ||
            matcher.is_match(def.actual_name) ||
            def.original_name.map(|n| matcher.is_match(n)).unwrap_or(false)
        })
    });
}

/// CLI-derived overrides controlling which tests run and how their parameters
/// are tweaked. Bundled so the run entry point keeps a manageable signature.
/// All fields are optional — `None` means "use the value from the config/defaults".
#[derive(Default, Clone, Copy)]
pub struct TestRunOverrides<'a> {
    /// Run only the test(s) matching this name/glob filter.
    pub single_test_filter: Option<&'a str>,
    /// The built-in plan that picks the tests, when no config does (TODO 25); a filter narrows it.
    pub plan: Option<&'static crate::plans::Plan>,
    /// Override the seal (`seal=tmr|tm5|off`) and its width (`seal-width=`).
    pub seal_override: Option<&'a str>,
    pub seal_width_override: Option<&'a str>,
    /// Override every mirror test's mode (`mirror=`).
    pub mirror_override: Option<crate::config::MirrorMode>,
    /// Override the pattern-generation mode.
    pub pattern_mode_override: Option<u32>,
    /// Override Mem-Sequence's passes and default order (`sequence=`, `order=`).
    pub sequence_override: Option<&'a str>,
    pub order_override: Option<&'a str>,
    /// Override the verify-repetition count.
    pub verify_reps_override: Option<u32>,
    /// Override the test-repetition count.
    pub test_reps_override: Option<u32>,
    /// Override the write-read-cycle count.
    pub wrc_override: Option<u32>,
    /// Override the channel count used by stride formulas.
    pub channels_override: Option<u32>,
    /// Print the layout and the test plan, then return before allocating (`--startup-debug`).
    pub plan_only: bool,
}

/// How a whole run ended, for the console's last line and the exit status: 0 passed, 1 tests
/// failed or found errors, 2 stopped before testing (a config error, no test matched, allocation
/// failed, or no test ran at all), 3 interrupted (Ctrl+C) with no errors so far. Not
/// `progress::RunOutcome`, which says why the test loop stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    Passed,
    Failed,
    NotRun,
    Interrupted,
}

impl RunStatus {
    pub fn exit_code(self) -> i32 {
        match self {
            RunStatus::Passed => 0,
            RunStatus::Failed => 1,
            RunStatus::NotRun => 2,
            RunStatus::Interrupted => 3,
        }
    }
}

// Global flags for shutdown handling
pub static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

// OS memory allocation per thread (renamed from AllocationBlock for clarity)
// Represents: Physical memory allocated from the OS for testing
#[derive(Debug)]
pub struct AllocationBlock {
    pub buffer: MemoryBuffer,  // Contains BufferInfo with numa_node, page_type, etc.
}

// Test definition with display name support
/// What a test does to the memory patterns after it runs.
/// Used for plan-level validation of dependent test ordering.
#[derive(Debug, Clone, PartialEq)]
pub enum MemoryEffect {
    /// Test writes a known pattern identified by (mode, param0, param1).
    /// After this test, memory contains this pattern.
    /// Examples: Bench-Init-*, SimpleTest (test_fn re-writes each cycle)
    Writes(PatternId),
    /// Test preserves existing memory contents (round-trip operations).
    /// After this test, memory still contains whatever was there before.
    /// Examples: MirrorMove (mirror + unmirror), Bench-Verify (read-only)
    Preserves,
    /// Test overwrites memory with test-specific patterns that don't match any
    /// standard PatternId. Dependent tests cannot follow this.
    /// Examples: StuckBit (writes 0xAAAA/0x5555), RefreshStable (writes fixed pattern)
    Destroys,
}

/// Identifies a specific pattern written to memory.
/// Two tests with the same PatternId write compatible patterns.
#[derive(Debug, Clone, PartialEq)]
pub struct PatternId {
    pub mode: u32,
    pub param0: u64,
    pub param1: u64,
}

impl PatternId {
    /// The pattern a test writes or expects: a Bench-*-Seal-* test's is its seal pattern, modes
    /// 200 (TMR) and 201 (TM5) whatever its width; a native Bench pair's, Init and Verify alike, its
    /// family's pinned image (`NATIVE`, `param0` numbering the family from 1); any other test's is
    /// its config's.
    pub fn for_test(test_name: &str, config: &crate::tests::TestMemoryConfig) -> Self {
        if test_name.starts_with("Bench-Init-Seal-") || test_name.starts_with("Bench-Verify-Seal-") {
            let mode = if test_name.contains("-Seal-TM5") { 201 } else { 200 };
            return Self { mode, param0: 0, param1: 0 };
        }
        if let Some(i) = crate::tests::native_bench_tests().iter().position(|&(init, verify, _, _)| test_name == init || test_name == verify) {
            return Self { mode: Self::NATIVE, param0: 1 + i as u64, param1: 0 };
        }
        Self::from_config(config)
    }

    /// Mode 300: a native program's image (pattern spec v2), keyed by its step and cycle, which
    /// no other step's verify expects.
    pub const NATIVE: u32 = 300;

    /// Create from a TestMemoryConfig's pattern settings.
    pub fn from_config(config: &crate::tests::TestMemoryConfig) -> Self {
        Self {
            mode: config.pattern_mode.unwrap_or(10),
            param0: config.pattern_param0.unwrap_or(0xDEADBEEFDEADBEEF),
            param1: config.pattern_param1.unwrap_or(0xCAFEBABECAFEBABE),
        }
    }
}

impl std::fmt::Display for PatternId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let bench = crate::tests::native_bench_tests();
        match (self.mode, self.param0 as usize) {
            (Self::NATIVE, i @ 1..) if i <= bench.len() => write!(f, "mode={} ({})", self.mode, bench[i - 1].0.trim_start_matches("Bench-Init-")),
            _ => write!(f, "mode={}", self.mode),
        }
    }
}

impl std::fmt::Display for MemoryEffect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MemoryEffect::Writes(pid) => write!(f, "Writes({})", pid),
            MemoryEffect::Preserves => write!(f, "Preserves"),
            MemoryEffect::Destroys => write!(f, "Destroys"),
        }
    }
}

/// The command line's `pattern-mode=`. On a SimpleTest it replaces the native program, which would
/// otherwise win and leave the mode reported but not run. A numbered mode goes on every test but
/// Mem-Sequence, which runs native patterns only and keeps its program. A native family's number
/// runs the family alone on every SimpleTest and Mem-Sequence and leaves the other tests as they
/// were; with neither in the plan it changes nothing, and says so.
fn apply_pattern_mode(defs: &mut [TestDefinition], mode: u32) {
    let Some(family) = crate::pattern::Family::from_number(mode) else {
        for def in defs.iter_mut() {
            if def.actual_name == "Mem-Sequence" {
                log::info!("pattern-mode={mode} is a numbered mode, and Mem-Sequence runs native patterns only: it keeps its program");
                continue;
            }
            def.config.pattern_mode = Some(mode);
            def.config.program = None;
        }
        return;
    };
    let mut native = defs.iter_mut().filter(|d| d.actual_name.starts_with("Mem-SimpleV2") || d.actual_name == "Mem-Sequence")
        .peekable();
    if native.peek().is_none() {
        log::warn!("pattern-mode={mode} is the native `{}` family, which only the SimpleTests and Mem-Sequence run, and \
            this plan has neither: it changes nothing", family.name());
    }
    for def in native {
        def.config.pattern_mode = None;
        def.config.program = Some(crate::pattern::Program::single(family));
    }
}

/// The command line's `sequence=` and `order=`, on every Mem-Sequence step: `sequence=` replaces
/// its passes, `order=` the order its steps take when they name none. Checked as a config's are.
fn apply_sequence(defs: &mut [TestDefinition], sequence: Option<&str>, order: Option<&str>) -> Result<(), String> {
    let order = order.map(|o| crate::sequence::Order::parse(o).ok_or_else(|| format!("order={o}: forward, reverse, random or stride")))
        .transpose()?;
    let mut any = false;
    for def in defs.iter_mut().filter(|d| d.actual_name == "Mem-Sequence") {
        let now = def.config.sequence();
        let order = order.unwrap_or(now.order);
        let next = match sequence {
            Some(text) => crate::sequence::Sequence::parse(text, order).map_err(|e| format!("sequence={text}: {e}"))?,
            None => now.with_order(order),
        };
        crate::config::check_sequence(&next, &def.config).map_err(|e| format!("{}: {e}", def.display_name))?;
        def.config.sequence = Some(Box::new(next));
        any = true;
    }
    if !any {
        log::warn!("sequence= and order= set Mem-Sequence's passes, and this plan has no Mem-Sequence: they change nothing");
    }
    Ok(())
}

/// A native SimpleTest that writes first writes each pattern as a pair, P then its inverse ~P, in
/// a chunk's back-to-back rounds (`Program::phase`): an odd `write_read_cycles` rounds up to even,
/// so no pattern goes without its inverse, whatever the cycles or the time limit. After the CLI's
/// overrides, so it covers theirs too. A config's SimpleTest and a TM5 import take 4 by default,
/// the built-in suite's 2.
fn pair_native_rounds(defs: &mut [TestDefinition]) {
    for def in defs {
        let wrc = def.config.write_read_cycles;
        if def.actual_name.starts_with("Mem-SimpleV2") && def.config.native_program().is_some() && def.config.test_reps > 0
            && wrc % 2 == 1
        {
            log::warn!("{}: write_read_cycles {wrc} is odd, but native patterns are written in pairs (each pattern, then \
                its inverse), so it runs {}", def.display_name, wrc + 1);
            def.config.write_read_cycles = wrc + 1;
        }
        // Mem-Sequence: its program rounds a chunk (`Sequence::rounds`), the same way, where one
        // more run makes them even
        if def.actual_name == "Mem-Sequence" {
            let seq = def.config.sequence();
            let reps = wrc.max(1);
            if seq.rounds(reps) % 2 == 1 {
                if seq.rounds(reps + 1) % 2 == 0 {
                    log::warn!("{}: '{}' {reps} times writes an odd number of rounds, but native patterns are written in \
                        pairs (each pattern, then its inverse), so it runs {} times", def.display_name, seq.name(), reps + 1);
                    def.config.write_read_cycles = reps + 1;
                } else {
                    log::warn!("{}: '{}' writes an odd number of rounds however many times it runs, so the last pattern a \
                        chunk gets each cycle goes without its inverse", def.display_name, seq.name());
                }
            }
        }
    }
}

/// Derive the memory effect of a test from its name and config.
///
/// Classification:
/// - **Writes**: Tests that write a known, repeatable pattern (SimpleTest, SimpleTestNT, Bench-Init)
/// - **Preserves**: Tests that leave memory intact (MirrorMove round-trip, Bench-Verify read-only)
/// - **Destroys**: Tests that overwrite with non-standard patterns (StuckBit, Refresh, CacheBust, etc.)
///   Latency and bandwidth tests also destroy — they use internal patterns not tracked by PatternId.
fn derive_memory_effect(test_name: &str, config: &crate::tests::TestMemoryConfig) -> MemoryEffect {
    // Tests that write a known, config-derived pattern
    if test_name.starts_with("Mem-SimpleV2") && config.native_program().is_some() {
        return MemoryEffect::Writes(PatternId { mode: PatternId::NATIVE, param0: 0, param1: 0 });
    }
    if test_name.starts_with("Mem-SimpleV2")
        || test_name.starts_with("Mem-SimpleNT")
        || test_name.starts_with("Bench-Init")
    {
        return MemoryEffect::Writes(PatternId::for_test(test_name, config));
    }

    // Tests that preserve existing memory contents (round-trip or read-only)
    if test_name.starts_with("Mem-Mirror")
        || test_name.starts_with("Bench-Verify")
    {
        return MemoryEffect::Preserves;
    }

    // Everything else destroys: StuckBit, Refresh, CacheBust, Random, Stride,
    // BlockMove, Saturate, latency tests, bandwidth tests
    MemoryEffect::Destroys
}

/// Validate a test plan's dependency chain.
///
/// Walks the steps in order, tracking what pattern is currently in memory. Reports each step that
/// uses `skip_init=true` (dependent mode) but finds no pattern in memory or the wrong one: a load
/// error, since it would verify data nothing wrote. A step that takes the seal leaves the seal. A
/// native SimpleTest that verifies without writing is refused outright: its image is keyed by its
/// own step, which no other step writes, and only a native Bench pair shares one (TODO 13).
///
/// Returns a list of (step_index, message) for any issues found.
pub fn validate_test_plan(tests: &[TestDefinition]) -> Vec<(usize, String)> {
    let mut warnings = Vec::new();
    // Track what pattern is currently in memory (None = unknown/destroyed)
    let mut current_pattern: Option<PatternId> = None;

    for (i, test) in tests.iter().enumerate() {
        let is_dependent = test.config.skip_init;

        if is_dependent && test.actual_name.starts_with("Mem-SimpleV2") && test.config.native_program().is_some()
            && test.config.test_reps == 0
        {
            warnings.push((i, format!(
                "'{}' (dependent, test_reps 0) would only verify, but a native program's image is keyed by its own \
                 step, which no other step writes; give it test_reps 1 or more, or use a Bench-Init-* and its Bench-Verify-*",
                test.display_name
            )));
        }

        if is_dependent {
            // Dependent test skips its own init — needs compatible pattern already in memory.
            // Exception: Writes tests (like Bench-Init) with skip_init just skip the init_fn
            // and do their writes in test_fn — they don't depend on prior memory state.
            let needs_existing_pattern = matches!(&test.memory_effect, MemoryEffect::Preserves);

            if needs_existing_pattern {
                let expected = PatternId::for_test(test.actual_name, &test.config);
                match &current_pattern {
                    Some(current) if *current == expected => {
                        // Pattern matches — dependency satisfied
                    }
                    Some(current) => {
                        warnings.push((i, format!(
                            "'{}' (dependent) expects pattern {} but memory has {}",
                            test.display_name, expected, current
                        )));
                    }
                    None => {
                        warnings.push((i, format!(
                            "'{}' (dependent) expects pattern {} but no prior test wrote a known pattern",
                            test.display_name, expected
                        )));
                    }
                }
            }
        }

        // Update memory state based on what this test does. A step that takes the seal leaves it
        // in memory, whatever it wrote (TODO 74).
        let sealed = test.config.seal.expects.then_some(MemoryEffect::Writes(PatternId {
            mode: match test.config.seal.kernel.pattern {
                crate::seal::SealPattern::Tmr => 200,
                crate::seal::SealPattern::Tm5 => 201,
            },
            param0: 0,
            param1: 0,
        }));
        match sealed.as_ref().unwrap_or(&test.memory_effect) {
            MemoryEffect::Writes(pattern) => {
                current_pattern = Some(pattern.clone());
            }
            MemoryEffect::Preserves => {
                // Memory unchanged — current_pattern stays as-is
            }
            MemoryEffect::Destroys => {
                current_pattern = None;
            }
        }
    }

    warnings
}

#[derive(Debug, Clone)]
pub struct TestDefinition {
    pub actual_name: &'static str,    // Used for function resolution and stats tracking
    pub display_name: String,         // Used for UI display and logging
    pub function: TestFunction,
    pub config: TestMemoryConfig,
    pub original_name: Option<&'static str>, // Preserves original name for Auto variants (e.g., "StuckBitTestAuto")
    /// What this test does to memory patterns. Used for plan-level dependency validation.
    pub memory_effect: MemoryEffect,
    /// Which of the plan's tests this step runs (TODO 74): the steps of one test share it.
    pub test_index: usize,
    /// The test's config `id`; for a TM5 import, the `.cfg`'s test number.
    pub id: Option<String>,
    /// How the step takes the seal.
    pub seal_use: SealUse,
    /// `seal: false` on the test in the config.
    pub seal_opt_out: bool,
}

impl TestDefinition {
    /// The step's name in the plan, the ticker and error lines: the TM5 test number for an import,
    /// the id for a config that gives one, e.g. `Mem-SimpleV2_A (Test 12)`.
    pub fn step_label(&self, tm5: bool) -> String {
        match (&self.id, tm5) {
            (Some(id), true) => format!("{} (Test {id})", self.display_name),
            (Some(id), false) => format!("{} ({id})", self.display_name),
            (None, _) => self.display_name.clone(),
        }
    }
}

/// How a test takes the seal (TODO 74, `seal.rs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SealUse {
    /// A correctness test: the seal is checked before each of its chunks and resealed after.
    Wrap,
    /// A mirror test: its data is the seal and its verify the seal check (TM5's
    /// `Capable_UseTst0ForGenAndCheck`).
    Data,
    /// TM5's test 0 as a step: the seal check alone.
    Check,
    /// Bandwidth, latency, Bench and Mem-Random tests: never sealed. The worker checks what one will
    /// overwrite before it runs and reseals it before the next step that takes the seal.
    Never,
}

/// A test's seal use, from its name. A new `Mem-*` correctness test takes the seal unless it is
/// listed here; `every_sealed_test_honours_the_seal` checks each one does.
pub fn seal_use(test_name: &str) -> SealUse {
    if test_name == "Seal-Check" {
        SealUse::Check
    } else if test_name.starts_with("Mem-MirrorV2") {
        SealUse::Data
    } else if test_name.starts_with("Mem-") && test_name != "Mem-Random" {
        SealUse::Wrap
    } else {
        SealUse::Never
    }
}

/// The seal each step runs with (TODO 74): `kernel` when the run is sealed (`None` when not, and
/// then the mirror tests still use `pattern`'s kernel for their data).
fn set_step_seals(test_definitions: &mut [TestDefinition], kernel: crate::seal::SealKernel, on: bool) {
    for def in test_definitions {
        let takes = def.seal_use != SealUse::Never && !def.seal_opt_out;
        def.config.seal = crate::seal::SealStep {
            kernel,
            on,
            expects: on && takes,
            wrap: on && takes && def.seal_use == SealUse::Wrap,
        };
    }
}

// Work item for thread pool

// Test Suite Timing Configuration
#[derive(Debug, Clone)]
pub struct TestSuiteTiming {
    pub global_cycles: Option<u32>,
    pub global_duration_secs: Option<u32>,
    pub per_test_cycle_multiplier: f64,
}

impl Default for TestSuiteTiming {
    fn default() -> Self {
        Self {
            global_cycles: Some(3),  // Default 3 cycles to match help text
            global_duration_secs: None,
            per_test_cycle_multiplier: 1.0,
        }
    }
}

impl TestSuiteTiming {
    pub fn cycles_only(cycles: u32) -> Self {
        Self {
            global_cycles: Some(cycles),
            global_duration_secs: None,
            per_test_cycle_multiplier: 1.0,
        }
    }
    
    pub fn duration_only(duration_secs: u32) -> Self {
        Self {
            global_cycles: None,
            global_duration_secs: Some(duration_secs),
            per_test_cycle_multiplier: 1.0,
        }
    }
}

/// The channels line and the test configuration table, shown before a run and by `--startup-debug`.
/// `thread_memory` is each thread's bytes, planned or allocated, so the table shows the extents
/// and chunks the tests will resolve; `tm5` a TM5 import's `.cfg` window and lock granularity.
#[allow(clippy::too_many_arguments)]
fn print_test_plan(test_definitions: &[TestDefinition], suite_timing: &TestSuiteTiming, cache_info: &CacheInfo, channels: u32,
                   thread_memory: &[usize], tm5: Option<(u32, u32)>, sequence: &crate::reporting::converters::PlanSequence) {
    use crate::reporting::{create_console_reporter, converters};
    println!("⚙  Memory Channels: {} (affects SimpleTest stride: [channels × param - 1] × {}B cache line)",
        channels, cache_info.cache_line_size);
    let report = converters::create_test_configuration_report_v2(test_definitions, suite_timing, cache_info, thread_memory, tm5, sequence);
    let mut reporter = create_console_reporter();
    if let Err(e) = reporter.report_test_configuration(&report) {
        log::error!("Failed to display test configuration report: {}", e);
    }
}

// Test function signatures
pub(crate) type TestFunctionMultiBlock = unsafe fn(&[AllocationBlock], usize, ErrorMode, &TestTiming, &TestMemoryConfig, Option<&TestProgress>) -> TestStats;
type TestFunctionLatency = unsafe fn(&[AllocationBlock], usize, ErrorMode, &TestTiming, &TestMemoryConfig, Option<&TestProgress>) -> LatencyTestStats;

// Test function wrapper enum
#[derive(Debug, Clone)]
pub enum TestFunction {
    MultiBlock(TestFunctionMultiBlock),
    /// Latency tests return extended stats with percentiles
    Latency(TestFunctionLatency),
}

pub fn run_tests_with_layout_and_timing_filtered(
    layout: EnhancedMemoryLayout,
    error_mode: ErrorMode,
    suite_timing: TestSuiteTiming,
    runtime_config: RuntimeConfig,
    config: Option<&crate::config::ModernConfig>,
    overrides: TestRunOverrides,
    cache_info: &CacheInfo,
) -> RunStatus {
    let TestRunOverrides {
        single_test_filter,
        plan,
        seal_override,
        seal_width_override,
        mirror_override,
        pattern_mode_override,
        sequence_override,
        order_override,
        verify_reps_override,
        test_reps_override,
        wrc_override,
        channels_override,
        plan_only,
    } = overrides;

    layout.print_layout();

    // Calculate progress tracking information and resolve auto-dispatch tests
    // Use config-driven tests if config is provided, otherwise use hard-coded defaults
    let mut cycle_order = None;
    let mut test_definitions = if let Some(cfg) = config {
        match create_test_definitions_from_config(cfg, cache_info) {
            Ok((tests, order)) => {
                log::info!("Using config-driven test sequence with {} steps", tests.len());
                cycle_order = order;
                tests
            }
            Err(e) => {
                // Stop rather than run a different plan than the config asked for
                println!("❌ Config error: {}", e);
                log::error!(target: crate::console::FILE_ONLY_TARGET, "Config error: {}", e);
                return RunStatus::NotRun;
            }
        }
    } else {
        let plan = plan.unwrap_or(crate::plans::find("all").expect("a built-in plan"));
        log::info!("Using the built-in suite, plan {}", plan.name);
        match crate::plans::select(plan, create_test_definitions(cache_info)) {
            Ok(tests) => tests,
            Err(e) => {
                println!("❌ {e}");
                return RunStatus::NotRun;
            }
        }
    };

    // Apply single test filter if provided (supports glob patterns and comma-separated patterns)
    if let Some(test_name_filter) = single_test_filter {
        retain_matching(&mut test_definitions, test_name_filter);

        if test_definitions.is_empty() {
            match plan.filter(|p| p.tests.is_some()) {
                // A filter narrows a named plan: list what it could have picked from
                Some(plan) => {
                    println!("❌ No test of plan {} matches filter '{}'. Its tests (plan=all for every test):", plan.name, test_name_filter);
                    for def in crate::plans::select(plan, create_test_definitions(cache_info)).unwrap_or_default() {
                        println!("  - {}", def.original_name.unwrap_or(def.actual_name));
                    }
                }
                None => {
                    println!("❌ No tests match filter '{}'. Available tests:", test_name_filter);
                    for def in create_test_definitions(cache_info) {
                        println!("  - {}", def.actual_name);
                    }
                }
            }
            return RunStatus::NotRun;
        }

        if test_definitions.len() == 1 {
            log::info!("🎯 Running single test: {}", test_definitions[0].display_name);
        } else {
            log::info!("🎯 Running {} tests matching filter '{}'", test_definitions.len(), test_name_filter);
            for def in &test_definitions {
                log::debug!("  - {}", def.display_name);
            }
        }
    }

    // Apply the mirror override if provided: it sets the one field on every mirror test, leaving
    // the rest of each test's parameters as they were (TODO 85)
    if let Some(mirror) = mirror_override {
        log::info!("🔧 Applying CLI mirror override: {}", mirror);
        for def in test_definitions.iter_mut().filter(|d| d.actual_name.starts_with("Mem-MirrorV2")) {
            def.config.parameter_context.get_or_insert_with(Default::default).mirror = Some(mirror);
        }
    }

    if let Some(mode) = pattern_mode_override {
        log::info!("Applying CLI pattern-mode override: {}", mode);
        apply_pattern_mode(&mut test_definitions, mode);
    }
    if (sequence_override.is_some() || order_override.is_some())
        && let Err(e) = apply_sequence(&mut test_definitions, sequence_override, order_override)
    {
        println!("❌ Config error: {e}");
        log::error!(target: crate::console::FILE_ONLY_TARGET, "Config error: {e}");
        return RunStatus::NotRun;
    }

    // Apply verify-reps override if provided
    if let Some(reps) = verify_reps_override {
        log::info!("Applying CLI verify-reps override: {}", reps);
        for def in test_definitions.iter_mut() {
            def.config.verify_reps = reps;
        }
    }

    // Apply test-reps override if provided
    if let Some(reps) = test_reps_override {
        log::info!("Applying CLI test-reps override: {}", reps);
        for def in test_definitions.iter_mut() {
            def.config.test_reps = reps;
        }
    }

    // Apply write-read-cycles override if provided
    if let Some(wrc) = wrc_override {
        log::info!("Applying CLI write-read-cycles override: {}", wrc);
        for def in test_definitions.iter_mut() {
            def.config.write_read_cycles = wrc;
        }
    }

    // Apply channels override if provided (affects stride calculation in SimpleTest)
    if let Some(channels) = channels_override {
        log::info!("Applying CLI channels override: {} channel(s)", channels);
        for def in test_definitions.iter_mut() {
            if let Some(ref mut ctx) = def.config.parameter_context {
                // Recompute stride if this test has stride_cachelines (SimpleTest)
                if ctx.stride_cachelines.is_some() && ctx.raw_parameter > 0 {
                    let stride_cl = (channels as usize * ctx.raw_parameter as usize).saturating_sub(1);
                    ctx.stride_cachelines = Some(stride_cl);
                    ctx.stride_elements = Some(stride_cl * (def.config.cache_line_bytes / 8));
                    log::debug!("  {} stride recalculated: {} cache lines ({} channels × {} parameter - 1)",
                        def.display_name, stride_cl, channels, ctx.raw_parameter);
                }
            }
        }
    }

    pair_native_rounds(&mut test_definitions);

    // Validate test parameter values (must be >= 1 and power-of-2 where required). A bad value is a
    // config error: stop before anything is allocated.
    let mut param_errors = Vec::new();
    for def in &test_definitions {
        // After the CLI's overrides too: a verify of the fill with a cycle-keyed TMR mode fails on
        // every word, as the config check refuses (`validate_repetition`)
        if def.actual_name.starts_with("Mem-SimpleV2") && def.config.test_reps == 0
            && matches!(def.config.pattern_mode, Some(0 | 1 | 2 | 20 | 30))
        {
            param_errors.push(format!("{}: test_reps 0 verifies the fill alone, but pattern_mode {} keys its image by the \
                cycle, which the fill and the verifies don't share; use a native program, or mode 10, 11 or 13",
                def.display_name, def.config.pattern_mode.unwrap_or(0)));
        }
        if let Some(ref ctx) = def.config.parameter_context {
            let mut check = |name: &str, val: Option<u32>, require_pow2: bool| {
                let Some(val) = val else { return };
                if val < 1 {
                    param_errors.push(format!("{}: {} must be >= 1, got {}", def.display_name, name, val));
                } else if require_pow2 && val > 1 && !val.is_power_of_two() {
                    param_errors.push(format!("{}: {} must be power-of-2 when > 1, got {}", def.display_name, name, val));
                }
            };
            check("stride_patterns", ctx.stride_patterns, false);
            check("rng_sequences", ctx.rng_sequences, true);
            check("subdivisions", ctx.subdivisions, true);
            check("copy_directions", ctx.copy_directions, false);
            // Mem-Stride splits each chunk by shifting; a chunk is a multiple of 4 KiB (512 u64),
            // so more subdivisions than that would leave part of a chunk untested (TODO 76)
            if let Some(v) = ctx.subdivisions && v > 512 {
                param_errors.push(format!("{}: subdivisions must be at most 512, got {}", def.display_name, v));
            }
        }
    }
    if !param_errors.is_empty() {
        for e in &param_errors {
            println!("❌ Config error: {}", e);
            log::error!(target: crate::console::FILE_ONLY_TARGET, "Config error: {}", e);
        }
        return RunStatus::NotRun;
    }

    // The run's seal (TODO 74): the command line's, else the config's, else tmr at the widest
    let seal_settings = crate::seal::SealSettings::parse(
        seal_override.or(config.and_then(|c| c.system.seal.as_deref())).unwrap_or("tmr"),
        seal_width_override.or(config.and_then(|c| c.system.seal_width.as_deref())).unwrap_or("auto"),
    ).and_then(|s| match s.kernel() {
        Ok(kernel) => Ok((s, kernel)),
        // Unsealed, the width only matters to a mirror's data: take the widest there is
        Err(e) if !s.on => {
            log::warn!("{e}; the mirror tests use the widest width this CPU has");
            Ok((s, crate::seal::SealKernel::new(s.pattern, crate::seal::SealKernel::detect().width)))
        }
        Err(e) => Err(e),
    });
    let (mut seal_settings, seal_kernel) = match seal_settings {
        Ok(s) => s,
        Err(e) => {
            println!("❌ Config error: {}", e);
            log::error!(target: crate::console::FILE_ONLY_TARGET, "Config error: {}", e);
            return RunStatus::NotRun;
        }
    };
    set_step_seals(&mut test_definitions, seal_kernel, seal_settings.on);
    // A run with no step that takes the seal (test=Spd-*, --ram-latency) doesn't seal memory at all
    let mut seal_off_reason = "seal=off";
    if seal_settings.on && !test_definitions.iter().any(|d| d.config.seal.expects) {
        log::info!("No step takes the seal, so the run is unsealed");
        seal_settings.on = false;
        seal_off_reason = "no step in this plan takes it";
        set_step_seals(&mut test_definitions, seal_kernel, false);
    }
    let tm5_run = config.is_some_and(|c| c.legacy_metadata.is_some());

    // Validate test plan dependency chain: a step that verifies the data an earlier one left must
    // directly follow it, with no seal in between (TODO 74, the plan-time half of TODO 13)
    for (i, def) in test_definitions.iter().enumerate() {
        log::debug!("Test plan [{}] '{}': effect={}, skip_init={}",
            i + 1, def.display_name, def.memory_effect, def.config.skip_init);
    }
    let plan_errors = validate_test_plan(&test_definitions);
    if !plan_errors.is_empty() {
        for (idx, msg) in &plan_errors {
            println!("❌ Config error: step {}: {}", idx + 1, msg);
            log::error!(target: crate::console::FILE_ONLY_TARGET, "Config error: step {}: {}", idx + 1, msg);
        }
        return RunStatus::NotRun;
    }

    let mut thread_blocks: HashMap<usize, Vec<BlockInfo>> = HashMap::new();
    for block in layout.blocks {
        thread_blocks.entry(block.thread_id).or_default().push(block);
    }

    let effective_channels = channels_override
        .or_else(|| config.map(|c| c.system.channels))
        .unwrap_or(2);

    let tm5 = config.and_then(|c| c.legacy_metadata.as_ref()).map(|m| (m.tm5_window_mb, m.tm5_lock_mb));
    let plan_sequence = crate::reporting::converters::PlanSequence {
        order: cycle_order.as_deref(),
        seal: seal_settings.on.then_some(seal_kernel),
        seal_off_reason,
        tm5: tm5_run,
    };

    // --startup-debug: show the plan, then stop before WHEA, allocation or any test.
    if plan_only {
        let planned: Vec<usize> = thread_blocks.values().map(|blocks| blocks.iter().map(|b| b.size_bytes).sum()).collect();
        print_test_plan(&test_definitions, &suite_timing, cache_info, effective_channels, &planned, tm5, &plan_sequence);
        return RunStatus::Passed;
    }

    let progress = Arc::new(ProgressTracker::new());

    // Subscribe to WHEA before any memory is touched, so hardware errors during allocation are
    // caught too. Failure is non-fatal — WHEA adds visibility that our verify reads cannot have
    // (DDR5 on-die ECC corrections, faults during non-verifying tests), but testing works without
    // it. See whea.rs for the transport choice.
    match progress.whea.start() {
        Ok(()) => println!("🔎 WHEA hardware-error monitoring: active"),
        Err(reason) => println!("🔎 WHEA hardware-error monitoring: unavailable ({})", reason),
    }

	let all_test_cpu_stats: Arc<Mutex<TestCpuStats>> = Arc::new(Mutex::new(HashMap::new()));

    // Stage 1: Pre-allocate all memory blocks
    println!("Stage 1: Pre-allocating memory blocks...");

    // Display page size constraints before allocation begins
    {
        // Parsed the way the allocator parses them, so "2mb" or "Large" show as what they mean
        use crate::memory::allocator::PageSizeLevel;
        let min = PageSizeLevel::from_config_str(&runtime_config.memory_allocation.min_page_size);
        let max = PageSizeLevel::from_config_str(&runtime_config.memory_allocation.max_page_size);

        let format_size = |s: PageSizeLevel| -> &'static str {
            match s {
                PageSizeLevel::Regular => "Regular (4KB)",
                PageSizeLevel::Large => "Large (2MB)",
                PageSizeLevel::Huge => "Huge (1GB)",
            }
        };

        if min == max {
            // Restrictive: only one page size allowed
            println!("⚠️  Page Size Restricted: {} only - may limit available memory", format_size(min));
        } else if min == PageSizeLevel::Large && max == PageSizeLevel::Huge {
            // Default constraints
            println!("⚙️  Page Size Constraints: {} to {} (default)", format_size(min), format_size(max));
        } else {
            // Custom constraints
            println!("⚙️  Page Size Constraints: {} to {}", format_size(min), format_size(max));
        }
    }

    let crate::memory::stitched::StitchedAllocation { blocks: allocated_blocks, spans, elapsed, numa_nodes } =
        match allocate_all_blocks_new(&thread_blocks, &runtime_config) {
            Ok(allocation) => allocation,
            Err(e) => {
                println!("❌ Failed to allocate memory blocks: {}\n", e);
                return RunStatus::NotRun;
            }
        };

    // The allocation report (TODO 81). The request record goes with it.
    {
        use crate::reporting::{create_console_reporter, converters};

        let report = converters::create_allocation_report(spans, elapsed, numa_nodes);
        if let Err(e) = create_console_reporter().report_allocation(&report) {
            log::error!("Failed to display the allocation report: {}", e);
        }
    }

    let tests_per_cycle = test_definitions.len() as u64;

    // cache_info is passed from main.rs (detected once at startup via get_system_info())
    // Used for calculating extent sizes in CacheLevel mode during display and test execution

    let allocated: Vec<usize> = allocated_blocks.values().map(|blocks| blocks.iter().map(|b| b.buffer.size()).sum()).collect();
    print_test_plan(&test_definitions, &suite_timing, cache_info, effective_channels, &allocated, tm5, &plan_sequence);

    // Start progress reporter thread
    let progress_clone = Arc::clone(&progress);
    let progress_handle = thread::spawn(move || {
        progress_reporter(progress_clone);
    });

    // Create thread pool with pre-allocated blocks
    let thread_count = allocated_blocks.len();

    // Set thread_count, cache_line_bytes and the TSC rate on all test configs (a config's latency
    // tests need the TSC rate as the built-in ones do, TODO 94)
    let cache_info = crate::tests::get_cache_info();
    for test_def in &mut test_definitions {
        test_def.config.thread_count = thread_count;
        test_def.config.cache_line_bytes = cache_info.cache_line_size;
        if test_def.config.tsc_frequency_ghz == 0.0 {
            test_def.config.tsc_frequency_ghz = cache_info.tsc_frequency_ghz;
        }
    }

    let thread_priority = ThreadPriority::default();

    // Tally the page-size mix and the real allocated total while we still own the blocks —
    // `ThreadPool::new` takes them by value below. Both are *outcomes*, not requests: the
    // allocator takes 1 GB pages only when physical memory is contiguous enough, and a partially
    // satisfied request still runs, so neither can be derived from `layout.allocation_result`.
    let page_mix = crate::run_context::PageSizeMix::from_blocks(&allocated_blocks);
    let allocated_bytes: u64 = allocated_blocks
        .values()
        .flat_map(|blocks| blocks.iter())
        .map(|block| block.buffer.size() as u64)
        .sum();

    log::info!(
        "Creating thread pool: {} persistent workers at {} priority ({})",
        thread_count,
        thread_priority,
        thread_priority.win32_name()
    );
    let (thread_pool, result_receiver) = ThreadPool::new(
        allocated_blocks,
        runtime_config.pin_threads,
        thread_priority,
        thread_count,
        runtime_config.cpu_list.as_deref()  // Pass the CPU list
    );

    // Open the result file with the run's resolved configuration, now that both allocation and
    // thread placement are observable facts (TODO #67). Without this, `--compare-results` has no way
    // to know two runs used different working-set sizes and reports the difference as a code change.
    let test_run_result = {
        let snapshot = crate::run_context::RunConfigSnapshot::capture(crate::run_context::RunConfigInputs {
            system_memory_info: &layout.system_memory_info,
            allocation_result: &layout.allocation_result,
            strategy: &layout.strategy,
            runtime_config: &runtime_config,
            page_mix,
            allocated_bytes,
            thread_count,
            cpu_assignments: thread_pool.get_cpu_assignments(),
            pinned: runtime_config.pin_threads,
            thread_priority,
            suite_timing: &suite_timing,
            error_mode,
            test_filter: single_test_filter.map(|s| s.to_string()),
            plan: config.is_none().then(|| plan.map_or("all", |p| p.name).to_string()),
            channels: effective_channels,
            config_name: config.map(|c| c.metadata.name.clone()),
            tests: &test_definitions,
            cache_info,
        });
        Arc::new(Mutex::new(TestRunResult::new(snapshot)))
    };

    let cycle_ctx = CycleContext {
        test_definitions: &test_definitions,
        thread_pool: &thread_pool,
        result_receiver: &result_receiver,
        thread_count,
        error_mode,
        progress: &progress,
        test_run_result: &test_run_result,
        all_test_cpu_stats: &all_test_cpu_stats,
        cache_info,
        seal: seal_settings.on.then_some(seal_kernel),
        tm5: tm5_run,
    };
    // Run cycles until a suite limit is reached. Both limits are checked *between* cycles, so a
    // cycle is never cut short by the clock; with neither limit set the suite runs until Ctrl+C or
    // an error halt, as the "Unlimited" in the banner says.
    let cycle_limit = suite_timing.global_cycles;
    let time_limit = suite_timing.global_duration_secs.map(|secs| Duration::from_secs(secs.into()));
    let suite_start = Instant::now();
    progress.begin_suite(suite_start, cycle_limit, time_limit, tests_per_cycle, thread_pool.live_progress());

    // The native patterns' seed and transfer width, once: with the seed (and the same layout) a
    // run writes the same bytes again
    let (seed, (beat, beat_source)) = (crate::pattern::run_seed(), crate::pattern::transfer_beat());
    log::info!("Pattern seed {seed:#018x} ({}); transfer width {} ({beat_source}); rescan {}",
               if crate::pattern::seed_given() { "seed=" } else { "from the clock" },
               beat.map_or("unknown".to_string(), |b| format!("{} bits", b.bits())),
               if crate::pattern::rescan_enabled() { "on" } else { "off" });
    crate::console::print_above(&format!("🎲 Pattern seed {seed:#018x} (seed= replays it), transfer width {}",
        beat.map_or("unknown: switch alternates 64 and 32 bits".to_string(), |b| format!("{} bits ({beat_source})", b.bits()))));

    let mut cycles_done = 0u32;
    let outcome = loop {
        if cycle_limit.is_some_and(|limit| cycles_done >= limit) {
            break RunOutcome::Completed;
        }
        if time_limit.is_some_and(|limit| suite_start.elapsed() >= limit) {
            break RunOutcome::TimeLimit;
        }
        if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
            break RunOutcome::Interrupted;
        }

        let cycle = cycles_done + 1;
        progress.start_new_cycle(cycle);
        crate::console::print_above(&match cycle_limit {
            Some(limit) => format!("\n🔄 Starting test cycle {} of {}", cycle, limit),
            None => format!("\n🔄 Starting test cycle {}", cycle),
        });

        // Execute all tests in sequence for this cycle
        if let Some(stop) = execute_test_cycle(&cycle_ctx, cycle as u64) {
            break stop;
        }
        cycles_done = cycle;
    };

    let suite_duration = suite_start.elapsed();

    // Signal completion to progress reporter
    progress.finish();

    // Wait for progress reporter to finish
    if let Err(e) = progress_handle.join() {
        log::warn!("Progress reporter thread panicked: {:?}", e);
    }

    // Shutdown thread pool and capture allocations for explicit cleanup
    let (thread_blocks, cpu_assignments) = thread_pool.shutdown();

    // Calculate total memory to be freed for logging
    let total_blocks: usize = thread_blocks.values().map(|v| v.len()).sum();
    let total_bytes: usize = thread_blocks.values()
        .flat_map(|blocks| blocks.iter())
        .map(|block| block.buffer.size())
        .sum();

    log::info!("Freeing {} memory blocks ({:.2} GiB) across {} threads",
        total_blocks,
        total_bytes as f64 / (1024.0 * 1024.0 * 1024.0),
        thread_blocks.len());

    // Explicitly drop allocations - this triggers MemoryBuffer::drop()
    // which calls backend.free() for each allocation
    drop(thread_blocks);

    log::info!("Memory cleanup complete");

    // Final WHEA tally for the run. Taken from the monitor's own cumulative counter, so unlike the
    // per-test deltas this is exact — no event can fall between two snapshots.
    //
    // ORDER MATTERS: `stop()` takes a last drain, and the window it covers is real work — the
    // progress reporter stopped polling at its `join()` above, and freeing tens of GiB of large
    // pages happens between there and here. On an interrupted run that window is the whole tail of
    // the run. So stop (and drain) first, *then* read the counts; reading them first silently
    // discards whatever that final drain found. `is_active()` is the exception — it must be read
    // before `stop()` flips the flag.
    //
    // This also has to precede the performance summary below, which reports the run's WHEA total on
    // its aggregate rows. Nothing between the free above and here touches memory, so the drain has
    // already covered every window that could produce an event.
    let whea_monitored = progress.whea.is_active();
    progress.whea.stop();
    let whea_totals = progress.whea.counts();

    // The reporter thread printed everything it had seen before it exited, so anything that last
    // drain turned up would otherwise be counted but never described. Flush it here.
    for event in progress.whea.take_pending() {
        println!("{}", event);
    }

    // Create final performance summary with detailed per-CPU stats
    let final_stats = all_test_cpu_stats.lock().unwrap();
    if !final_stats.is_empty() {
        print_detailed_cpu_performance_summary(
            &final_stats,
            &cpu_assignments,
            suite_duration,
            whea_totals,
            runtime_config.pin_threads,
        );
    }

    // `progress.total_errors()` is the accurate aggregate (summed from every thread's result in
    // `complete_test`), so the verdict is derived from it directly. A halt fails the run even at
    // zero counts: the one halt trigger that carries no count is a lost worker result.
    let memory_errors = progress.total_errors();
    let failed = outcome == RunOutcome::Halted || memory_errors > 0 || whea_totals.total > 0;
    // `tests_done` counts the current cycle only; a finished cycle means tests ran.
    let any_test_ran = cycles_done > 0 || progress.tests_done() > 0;
    let status = if failed {
        RunStatus::Failed
    } else if !any_test_ran {
        RunStatus::NotRun
    } else if outcome == RunOutcome::Interrupted {
        RunStatus::Interrupted
    } else {
        RunStatus::Passed
    };

    // Display completion status
    if outcome == RunOutcome::Interrupted {
        println!("\n⚠️  Test suite interrupted by user (CTRL+C)");
    } else if status == RunStatus::NotRun {
        println!("\n⚠️  No test ran: the cycle or time limit is 0");
    } else if status == RunStatus::Passed {
        println!("\n✅ All test cycles completed successfully!");
    }
    if failed {
        // Same form as the per-test topline. Either count alone fails the run: a WHEA event with
        // `0 errors` is a fault that ECC corrected, or one raised during a test phase that does not
        // verify — still not a stable configuration.
        println!(
            "\n❌ Test suite failed: {} errors + {} WHEA{}",
            memory_errors, whea_totals.total, whea_totals.split_suffix()
        );
    }

    // Always display and save results (even for partial/interrupted runs)
    display_and_save_results(&test_run_result, suite_duration, whea_totals, whea_monitored);

    status
}

/// Run every test once. Returns `None` when the cycle ran to the end, or the outcome the suite
/// must end with when it stopped early (Ctrl+C, or an `ErrorMode::Halt` trigger). Either way the
/// tests that ran are added to the results as this cycle.
fn execute_test_cycle(ctx: &CycleContext, cycle: u64) -> Option<RunOutcome> {
    let &CycleContext {
        test_definitions,
        thread_pool,
        result_receiver,
        thread_count,
        error_mode,
        progress,
        test_run_result,
        all_test_cpu_stats,
        cache_info,
        seal,
        tm5,
    } = ctx;

    use crate::progress::TestSummary;
    use crate::constants::MB_F64;

    // Track cycle timing and collect test summaries for this cycle
    let cycle_start = Instant::now();
    let mut cycle_test_summaries = Vec::new();

    let success = Arc::new(AtomicBool::new(true));
    let mut stopped = None;
    let mut seal_stages = crate::results::CycleSeal::default();
    // This cycle's seal: a key of its own and every other cycle inverted, from the seal at its
    // start to the final check at its end, so each check expects what the step before it sealed
    let run_seed = crate::pattern::run_seed();
    let seal = seal.map(|kernel| kernel.for_cycle(run_seed, cycle));
    if let Some(kernel) = seal {
        let (bytes, _, _, duration) = run_seal_stage(ctx, crate::thread_pool::SealOp::SealAll, kernel, cycle);
        seal_stages.seal_ms = duration.as_millis();
        seal_stages.seal_bytes = bytes;
    }
    for (test_idx, test_def) in test_definitions.iter().enumerate() {
        let test_name = test_def.actual_name; // Use actual_name for thread pool (requires 'static)
        let test_display_name = &test_def.display_name; // Use display_name for results to preserve _A suffix
        let test_func = &test_def.function;
        // The step as this cycle runs it: the cycle's seal kernel, and where the step is in the
        // suite, which keys its native patterns (crate::pattern), so they change every cycle
        let mut step_config = test_def.config.clone();
        step_config.seal.kernel = step_config.seal.kernel.for_cycle(run_seed, cycle);
        step_config.epoch = crate::pattern::StepEpoch { seed: run_seed, suite_cycle: cycle as u32, step: test_idx as u32 };
        let test_config = &step_config;
        if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
            stopped = Some(RunOutcome::Interrupted);
            break;
        }

        let test_start = Instant::now();
        let step_label = test_def.step_label(tm5);
        progress.start_test(
            test_idx + 1,
            &step_label,
            test_start,
            test_config.timing.duration_secs.map(|secs| Duration::from_secs(secs.into())),
            // Every test publishes through its TestRunner, the latency tests too (TODO 76)
            true,
        );

        // Baseline for this test's WHEA attribution. Drain first so anything queued from the
        // previous test is charged there, not here. Attribution is still approximate at a test
        // boundary — the OS logs WHEA asynchronously (kernel → ETW → EventLog service) — but the
        // run-level total below is exact regardless.
        progress.whea.poll();
        let whea_before = progress.whea.counts();

        // Execute test on all threads
        let step_context = format!("step {} ({}), cycle {}", test_idx + 1, step_label, cycle);
        // The seal's work between steps first, on its own clock (TODO 74)
        let mut seal_errors_for_test = 0u64;
        let mut seal_bits_for_test = crate::failing_bits::FailingBits::default();
        if seal.is_some() {
            let between = Instant::now();
            thread_pool.execute_seal_before_step(test_name, test_config, &step_context);
            for _ in 0..thread_count {
                match result_receiver.recv() {
                    Ok(result) => {
                        seal_errors_for_test += result.seal_errors;
                        seal_bits_for_test.add(&result.seal_failing_bits);
                        if result.seal_errors > 0 {
                            success.store(false, Ordering::Relaxed);
                            log::error!("Thread {}: the seal check before step {} ('{}') found {} errors",
                                        result.thread_id, test_idx + 1, test_name, result.seal_errors);
                        }
                    }
                    Err(e) => {
                        log::error!("Failed to receive seal result: {}", e);
                        success.store(false, Ordering::Relaxed);
                    }
                }
            }
            seal_stages.between_steps_ms += between.elapsed().as_millis();
            // Under errors=halt, a failed check stops here: the step would overwrite the bad words
            if seal_errors_for_test > 0 && matches!(error_mode, ErrorMode::Halt) {
                log::error!("Halting before step {} ('{}'): the seal check before it found {} errors; memory keeps them",
                            test_idx + 1, test_name, seal_errors_for_test);
                progress.add_errors(seal_errors_for_test);
                // The step didn't run; its entry carries the seal check's errors
                cycle_test_summaries.push(TestSummary {
                    step: test_idx + 1,
                    test_index: test_def.test_index,
                    id: test_def.id.clone(),
                    label: step_label.clone(),
                    seal_errors: seal_errors_for_test,
                    name: test_display_name.to_string(),
                    duration_ms: 0,
                    bytes_processed: 0,
                    throughput_mib_s: 0.0,
                    errors: 0,
                    alarms: 0,
                    not_reproduced: 0,
                    failing_bits: Default::default(),
                    seal_failing_bits: seal_bits_for_test,
                    whea_total: 0,
                    whea_corrected: 0,
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
                });
                stopped = Some(RunOutcome::Halted);
                break;
            }
        }
        let test_start = Instant::now();
        thread_pool.execute_test(test_name, test_func, test_config, error_mode, &step_context);
        
		// Collect results from all threads
		let mut test_stats = Vec::new();
		let mut latency_results = Vec::new();  // Collect latency stats separately
		let mut total_bytes_for_test = 0u64;
		let mut total_errors_for_test = 0u64;
		let (mut alarms_for_test, mut not_reproduced_for_test) = (0u64, 0u64);
		let mut bits_for_test = crate::failing_bits::FailingBits::default();
		let mut timed_for_test: Option<std::time::Duration> = None;
		let mut total_operations_for_test = 0u64;
		let mut cycles_completed = 0u32;
		let mut cycle_limit: Option<u32> = None;
		let mut stopped_by_time_limit = false;

		for _ in 0..thread_count {
			match result_receiver.recv() {
				Ok(result) => {
					// Look up CPU assignment for this thread
					if let Some(&(_, cpu_id, _)) = thread_pool.get_cpu_assignments()
						.iter()
						.find(|(tid, _, _)| *tid == result.thread_id)
					{
						// Store simplified stats: (thread_id, cpu_id, bytes, time, errors, operations, cycles_completed)
						// The thread's own timed clock, as the step's speed (TODO 90)
						test_stats.push((result.thread_id, cpu_id,
									   result.total_bytes, result.timed.unwrap_or(result.elapsed), result.total_errors, result.total_operations,
									   result.cycles_completed));

						// Collect latency stats if present (for latency tests)
						if let Some(lat_stats) = result.latency_stats {
							latency_results.push((result.thread_id, cpu_id, lat_stats));
						}
					}

					total_bytes_for_test += result.total_bytes;
					total_errors_for_test += result.total_errors;
					alarms_for_test += result.alarms;
					not_reproduced_for_test += result.not_reproduced;
					bits_for_test.add(&result.failing_bits);
					seal_bits_for_test.add(&result.seal_failing_bits);
					timed_for_test = timed_for_test.max(result.timed);
					seal_errors_for_test += result.seal_errors;
					total_operations_for_test += result.total_operations;

					// Track maximum cycle count (to detect if any thread hit the limit)
					if result.cycles_completed > cycles_completed {
						cycles_completed = result.cycles_completed;
					}
					// Track cycle limit from first thread (same for all)
					if cycle_limit.is_none() {
						cycle_limit = result.cycle_limit;
					}
					// Track if ANY thread was stopped by time limit
					if result.stopped_by_time_limit {
						stopped_by_time_limit = true;
					}

					if result.total_errors > 0 {
						success.store(false, Ordering::Relaxed);
						log::error!("Thread {} reported {} memory errors in test '{}' ({} failed checks, {} not reproduced on rescan)",
								  result.thread_id, result.total_errors, test_name, result.alarms, result.not_reproduced);
					}
					if result.seal_errors > 0 {
						success.store(false, Ordering::Relaxed);
						log::error!("Thread {}: the seal checks in step {} ('{}') found {} errors",
								  result.thread_id, test_idx + 1, test_name, result.seal_errors);
					}
				}
				Err(e) => {
					log::error!("Failed to receive test result: {}", e);
					success.store(false, Ordering::Relaxed);
				}
			}
		}

        // Attribute WHEA events that landed while this test ran. A non-zero count here is a real
        // failure signal even when every thread reported zero errors: the fault was either
        // corrected before our verify read could see it, or it happened during a test that does no
        // verification at all.
        progress.whea.poll();
        let whea_for_test = progress.whea.counts().since(&whea_before);
        if whea_for_test.total > 0 {
            success.store(false, Ordering::Relaxed);
            log::error!(
                "Test '{}' saw {} WHEA{}",
                test_name,
                whea_for_test.total,
                whea_for_test.split_suffix()
            );
        }

        let test_duration = test_start.elapsed();
        let test_duration_secs = test_duration.as_secs_f64();
        // Over the test's own time, its slowest thread's: without the setup a test leaves out of its
        // clock (a Bench-Verify's fill, a bandwidth test's page faults, TODO 90), however short. The
        // step's wall time when no thread ran the test.
        let timed_secs = timed_for_test.map_or(test_duration_secs, |t| t.as_secs_f64());
        let throughput_mib_s = if timed_secs > 0.0 {
            (total_bytes_for_test as f64 / MB_F64) / timed_secs
        } else {
            0.0
        };

        // Format cycle info for logging
        let cycles_info = if cycles_completed > 0 || cycle_limit.is_some() {
            let limit_str = cycle_limit.map_or("∞".to_string(), |l| l.to_string());

            // Cycle limit indicator: RED if cycle limit caused stop, GREEN if didn't reach it
            let stop_indicator_cycles = if let Some(limit) = cycle_limit {
                if cycles_completed >= limit { "🔴" } else { "🟢" }  // RED when hit, GREEN when not
            } else {
                "🟢"  // No limit = always green (can't hit what doesn't exist)
            };

            // Time limit indicator: red if time limit stopped us, green otherwise
            let stop_indicator_time = if stopped_by_time_limit { "🔴" } else { "🟢" };

            format!(", Cycles {} {}/{}, Time Limit {}",
                    stop_indicator_cycles, cycles_completed, limit_str, stop_indicator_time)
        } else {
            String::new()
        };

        // Take the ticker down and keep it down while the report below prints.
        crate::console::hold();

        // Format operations in human-readable form (B/M/K notation)
        let format_ops = |ops: u64| -> String {
            if ops >= 1_000_000_000 {
                format!("{:.2}B", ops as f64 / 1_000_000_000.0)
            } else if ops >= 1_000_000 {
                format!("{:.2}M", ops as f64 / 1_000_000.0)
            } else if ops >= 1_000 {
                format!("{:.2}K", ops as f64 / 1_000.0)
            } else {
                format!("{}", ops)
            }
        };

        // Calculate operations per second
        let ops_per_sec = if timed_secs > 0.0 {
            total_operations_for_test as f64 / timed_secs
        } else {
            0.0
        };

        // Calculate consolidated P50 and spread for latency tests
        let latency_info = if !latency_results.is_empty() {
            // Collect all latencies from all threads
            let all_latencies: Vec<f64> = latency_results.iter()
                .flat_map(|(_, _, stats)| stats.latencies_ns.clone())
                .collect();

            if !all_latencies.is_empty() {
                let mut sorted = all_latencies;
                sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
                let len = sorted.len();
                let percentile = |p: f64| sorted[((len as f64 - 1.0) * p / 100.0) as usize];
                let p50 = percentile(50.0);
                let p5 = percentile(5.0);
                let p95 = percentile(95.0);
                let spread = if p5 > 0.0 { p95 / p5 } else { 0.0 };
                format!(", P50: {:.1}ns, Spread: {:.2}x", p50, spread)
            } else {
                String::new()
            }
        } else {
            String::new()
        };

        // Pass/fail for this test. Both error sources count: a WHEA event during the test means the
        // hardware reported a fault whether or not a verify read caught it, and either way the
        // system is not stable. This is the only per-test verdict in the output — `success` above is
        // the ErrorMode::Halt trigger, not a report.
        //
        // The ✅/❌ here is a verdict; the 🔴/🟢 in `cycles_info` is not — those mark *which limit
        // ended the test*, so the two glyph pairs are deliberately different.
        let test_passed = total_errors_for_test == 0 && seal_errors_for_test == 0 && whea_for_test.total == 0;
        let pass_indicator = if test_passed { "✅" } else { "❌" };
        // The seal's errors, in a sealed run: what the checks before this step's chunks found
        let seal_info = if seal.is_some() { format!(" + {} seal{}", seal_errors_for_test, seal_bits_for_test.brief()) } else { String::new() };

        // WHEA is always shown, even at zero, so a clean line still states that we were watching;
        // the C/UC split only appears when there is something to split. "errors" is the word every
        // table uses for these (the `Errors` column) — "data" would collide with the `Data` column,
        // which is bytes processed.
        // The bits that failed follow each count: ` (bit 31)`
        println!("📊 Test report - Cycle {} - Step {} {}: {} {:.1}s, {} errors{}{} + {} WHEA{}, {:.2} GiB @ {:.1} MiB/s ({:.2} GiB/s), {} ops @ {} ops/s{}{}",
                 cycle,
                 test_idx + 1,
                 step_label,
                 pass_indicator,
                 test_duration.as_secs_f64(),
                 total_errors_for_test,
                 bits_for_test.brief(),
                 seal_info,
                 whea_for_test.total,
                 whea_for_test.split_suffix(),
                 total_bytes_for_test as f64 / (1024.0 * 1024.0 * 1024.0),
                 throughput_mib_s,
                 throughput_mib_s / 1024.0,
                 format_ops(total_operations_for_test),
                 format_ops(ops_per_sec as u64),
                 latency_info,
                 cycles_info);

        // Add config line with calculated extent size (uses passed cache_info to avoid re-detection)
        {
            use crate::reporting::formatters::{ReportFormatter, DefaultFormatter};
            let formatter = DefaultFormatter::new();
            let extent_str = formatter.format_extent_mode_with_size(&test_def.config.extent_mode, cache_info, thread_count);
            let chunk_str = formatter.format_chunk_mode_with_size(&test_def.config.chunk_mode, cache_info, thread_count);
            let param_str = crate::reporting::converters::format_parameter_context(&test_def.config.parameter_context);
            let mode_str = match test_def.config.pattern_mode {
                Some(m @ 0..=2) => format!("TM5-{m}"),
                Some(m) => format!("TMR-{m}"),
                // A native program (pattern spec v2), and Mem-Sequence's passes
                None if test_def.actual_name.starts_with("Mem-SimpleV2") => {
                    test_def.config.native_program().map_or("-".to_string(), |p| p.name())
                }
                None if test_def.actual_name == "Mem-Sequence" => {
                    let seq = test_def.config.sequence();
                    format!("{}, sequence={} (order={})", test_def.config.native_program().map_or("-".to_string(), |p| p.name()),
                            seq.name(), seq.order.name())
                }
                None => "-".to_string(),
            };
            let reps_str = if test_def.config.test_reps != 1 || test_def.config.verify_reps != 1 || test_def.config.write_read_cycles != 1 {
                format!(", test_reps={}, verify_reps={}, wrc={}", test_def.config.test_reps, test_def.config.verify_reps, test_def.config.write_read_cycles)
            } else {
                String::new()
            };
            println!("   Config: parameter={}, mode={}, extent={}, chunk={}{}",
                     param_str,
                     mode_str,
                     extent_str,
                     chunk_str,
                     reps_str);
        }

        // Update progress tracker with test completion
        progress.complete_test(total_errors_for_test + seal_errors_for_test);

        // Generate thread timing deviation report
        {
            use crate::reporting::{create_console_reporter, converters};
            let report =
                converters::create_thread_timing_report(&test_stats, whea_for_test);
            let mut reporter = create_console_reporter();
            if let Err(e) = reporter.report_thread_timing(&report) {
                log::error!("Failed to display thread timing report: {}", e);
            }
        }

        // Display latency-specific report if this was a latency test
        if !latency_results.is_empty() {
            use crate::reporting::create_console_reporter;

            let level_summary = convert_to_latency_level_summary(&latency_results);
            let summary_report = LatencyTestSummaryReport { levels_tested: vec![level_summary] };

            // Use existing reporting infrastructure - just display the single level
            let mut reporter = create_console_reporter();
            if let Err(e) = reporter.report_latency_summary(&summary_report) {
                log::error!("Failed to display latency report: {}", e);
            }
        }

        // Where the suite stood as this test ended. The ticker is down during every report and
        // gone after the run, so this is what stays on screen: one line per test.
        if let Some(stamp) = progress.test_stamp(test_duration) {
            println!("{}", stamp);
        }
        // Blank line to separate from the next test
        println!();

        // Report done: let the ticker back.
        crate::console::release();

		// Store aggregated stats for this test
		{
			let mut stats_map = all_test_cpu_stats.lock().unwrap();
			stats_map.entry(test_name.to_string()).or_default().extend(test_stats);
		}

        // Create TestSummary for this test and add to cycle collection
        // Include latency data if this was a latency test
        let (latency_samples, latency_p5, latency_p10, latency_p25, latency_p50,
             latency_p75, latency_p90, latency_p95, latency_p99, latency_p99_9, latency_spread) =
            if !latency_results.is_empty() {
                // Aggregate latency stats across all threads
                let total_samples: usize = latency_results.iter().map(|(_, _, stats)| stats.sample_count).sum();
                let all_latencies: Vec<f64> = latency_results.iter()
                    .flat_map(|(_, _, stats)| stats.latencies_ns.clone())
                    .collect();

                if !all_latencies.is_empty() {
                    let mut sorted = all_latencies.clone();
                    sorted.sort_by(|a: &f64, b: &f64| a.partial_cmp(b).unwrap());
                    let len = sorted.len();
                    let percentile = |p: f64| sorted[((len as f64 - 1.0) * p / 100.0) as usize];
                    let p5 = percentile(5.0);
                    let p95 = percentile(95.0);
                    let spread = if p5 > 0.0 { p95 / p5 } else { 0.0 };

                    (Some(total_samples as u64), Some(percentile(5.0)), Some(percentile(10.0)),
                     Some(percentile(25.0)), Some(percentile(50.0)), Some(percentile(75.0)),
                     Some(percentile(90.0)), Some(percentile(95.0)), Some(percentile(99.0)),
                     Some(percentile(99.9)), Some(spread))
                } else {
                    (None, None, None, None, None, None, None, None, None, None, None)
                }
            } else {
                (None, None, None, None, None, None, None, None, None, None, None)
            };

        cycle_test_summaries.push(TestSummary {
            step: test_idx + 1,
            test_index: test_def.test_index,
            id: test_def.id.clone(),
            label: step_label.clone(),
            seal_errors: seal_errors_for_test,
            name: test_display_name.to_string(), // Use display_name to preserve _A suffix for AUTO tests
            duration_ms: test_duration.as_millis(),
            bytes_processed: total_bytes_for_test,
            throughput_mib_s,
            errors: total_errors_for_test,
            alarms: alarms_for_test,
            not_reproduced: not_reproduced_for_test,
            failing_bits: bits_for_test,
            seal_failing_bits: seal_bits_for_test,
            whea_total: whea_for_test.total,
            whea_corrected: whea_for_test.corrected,
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
        });

        // Early exit on errors if required. This is reported as a halt, not routed through
        // SHUTDOWN_REQUESTED — that flag means Ctrl+C, and the verdict would report it as one.
        if !success.load(Ordering::Relaxed) && matches!(error_mode, ErrorMode::Halt) {
            log::error!("Halting test suite due to memory errors");
            stopped = Some(RunOutcome::Halted);
            break;
        }
        // Ctrl+C during this test cut it short, so the cycle did not finish even if this was its
        // last test.
        if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
            stopped = Some(RunOutcome::Interrupted);
            break;
        }
    }

    // The cycle ends with a check of all memory (TODO 74), unless it stopped early
    if let Some(kernel) = seal
        && stopped.is_none()
    {
        let (bytes, errors, bits, duration) = run_seal_stage(ctx, crate::thread_pool::SealOp::FinalCheck, kernel, cycle);
        seal_stages.final_check_ms = duration.as_millis();
        seal_stages.final_check_bytes = bytes;
        seal_stages.final_check_errors = errors;
        seal_stages.final_check_failing_bits = bits;
        if errors > 0 {
            progress.add_seal_errors(errors);
            if matches!(error_mode, ErrorMode::Halt) {
                stopped = Some(RunOutcome::Halted);
            }
        }
    }

    // Add this cycle's results to TestRunResult, including a partial cycle that stopped early
    if !cycle_test_summaries.is_empty() {
        let cycle_duration_secs = cycle_start.elapsed().as_secs() as u32;
        if let Ok(mut result) = test_run_result.lock() {
            result.add_cycle(cycle as u32, cycle_duration_secs, cycle_test_summaries, seal.map(|_| seal_stages));
        } else {
            log::error!("Failed to lock test_run_result to add cycle {}", cycle);
        }
    }
    stopped
}

/// Run a seal stage on every worker (TODO 74) and report it: the bytes, the errors and the bits
/// that failed (a final check's), and the wall time.
fn run_seal_stage(ctx: &CycleContext, op: crate::thread_pool::SealOp, kernel: crate::seal::SealKernel,
                  cycle: u64) -> (u64, u64, crate::failing_bits::FailingBits, Duration) {
    use crate::thread_pool::SealOp;
    let (name, lead) = match op {
        SealOp::SealAll => ("Sealing memory", "🔒 Sealing memory"),
        SealOp::FinalCheck => ("Final seal check", "🔒 Final seal check"),
    };
    let start = Instant::now();
    ctx.progress.start_test(0, name, start, None, true);
    ctx.thread_pool.execute_seal(op, kernel, &format!("cycle {cycle}"));
    let (mut bytes, mut errors) = (0u64, 0u64);
    let mut bits = crate::failing_bits::FailingBits::default();
    for _ in 0..ctx.thread_count {
        match ctx.result_receiver.recv() {
            Ok(result) => {
                bytes += result.total_bytes;
                errors += result.seal_errors;
                bits.add(&result.seal_failing_bits);
            }
            Err(e) => log::error!("Failed to receive seal stage result: {}", e),
        }
    }
    let duration = start.elapsed();
    let gib = bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    let errors_info = if op == SealOp::FinalCheck { format!("{errors} errors{}, ", bits.brief()) } else { String::new() };
    crate::console::print_above(&format!("{lead} ({}): {errors_info}{gib:.2} GiB in {:.1}s ({:.2} GiB/s)",
        kernel.name(), duration.as_secs_f64(), gib / duration.as_secs_f64().max(1e-9)));
    if errors > 0 {
        log::error!("The final seal check of cycle {cycle} found {errors} errors");
    }
    (bytes, errors, bits, duration)
}

// Generic auto-dispatch resolver - converts "*-Auto" test names to best SIMD variant
fn resolve_auto_dispatch_test(test_name: &str) -> Option<(&'static str, TestFunction)> {
    if !test_name.ends_with("Auto") {
        return None;
    }

    // Lat-V2/V2P/NTW family — Latency-typed auto-dispatch
    // Test names are "Lat-V2-{tier}-Read-Auto", "Lat-V2P-{tier}-Read-Auto", "Lat-NTW-DRAM-Write-Auto"
    // Resolved names follow the same pattern with -{128,256,512} replacing -Auto
    if test_name.starts_with("Lat-V2-") || test_name.starts_with("Lat-V2P-") || test_name.starts_with("Lat-NTW-") {
        let width: &str = if is_x86_feature_detected!("avx512f") {
            "512"
        } else if is_x86_feature_detected!("avx2") {
            "256"
        } else {
            "128"
        };

        // Build resolved name: replace trailing "Auto" with width
        let resolved: &'static str = match (test_name, width) {
            // Layout A — Lat-V2-*-Read
            ("Lat-V2-L1-Read-Auto", "512") => "Lat-V2-L1-Read-512",
            ("Lat-V2-L1-Read-Auto", "256") => "Lat-V2-L1-Read-256",
            ("Lat-V2-L1-Read-Auto", "128") => "Lat-V2-L1-Read-128",
            ("Lat-V2-L2-Read-Auto", "512") => "Lat-V2-L2-Read-512",
            ("Lat-V2-L2-Read-Auto", "256") => "Lat-V2-L2-Read-256",
            ("Lat-V2-L2-Read-Auto", "128") => "Lat-V2-L2-Read-128",
            ("Lat-V2-L3-Read-Auto", "512") => "Lat-V2-L3-Read-512",
            ("Lat-V2-L3-Read-Auto", "256") => "Lat-V2-L3-Read-256",
            ("Lat-V2-L3-Read-Auto", "128") => "Lat-V2-L3-Read-128",
            ("Lat-V2-DRAM-Read-Auto", "512") => "Lat-V2-DRAM-Read-512",
            ("Lat-V2-DRAM-Read-Auto", "256") => "Lat-V2-DRAM-Read-256",
            ("Lat-V2-DRAM-Read-Auto", "128") => "Lat-V2-DRAM-Read-128",
            ("Lat-V2-DRAMFull-Read-Auto", "512") => "Lat-V2-DRAMFull-Read-512",
            ("Lat-V2-DRAMFull-Read-Auto", "256") => "Lat-V2-DRAMFull-Read-256",
            ("Lat-V2-DRAMFull-Read-Auto", "128") => "Lat-V2-DRAMFull-Read-128",
            // Layout A Write — single cached store per chain step (baseline)
            ("Lat-V2-L1-Write-Auto", "512") => "Lat-V2-L1-Write-512",
            ("Lat-V2-L1-Write-Auto", "256") => "Lat-V2-L1-Write-256",
            ("Lat-V2-L1-Write-Auto", "128") => "Lat-V2-L1-Write-128",
            ("Lat-V2-L2-Write-Auto", "512") => "Lat-V2-L2-Write-512",
            ("Lat-V2-L2-Write-Auto", "256") => "Lat-V2-L2-Write-256",
            ("Lat-V2-L2-Write-Auto", "128") => "Lat-V2-L2-Write-128",
            ("Lat-V2-L3-Write-Auto", "512") => "Lat-V2-L3-Write-512",
            ("Lat-V2-L3-Write-Auto", "256") => "Lat-V2-L3-Write-256",
            ("Lat-V2-L3-Write-Auto", "128") => "Lat-V2-L3-Write-128",
            ("Lat-V2-DRAM-Write-Auto", "512") => "Lat-V2-DRAM-Write-512",
            ("Lat-V2-DRAM-Write-Auto", "256") => "Lat-V2-DRAM-Write-256",
            ("Lat-V2-DRAM-Write-Auto", "128") => "Lat-V2-DRAM-Write-128",
            ("Lat-V2-DRAMFull-Write-Auto", "512") => "Lat-V2-DRAMFull-Write-512",
            ("Lat-V2-DRAMFull-Write-Auto", "256") => "Lat-V2-DRAMFull-Write-256",
            ("Lat-V2-DRAMFull-Write-Auto", "128") => "Lat-V2-DRAMFull-Write-128",
            // Layout A Copy — single read-modify-write per chain step (baseline)
            ("Lat-V2-L1-Copy-Auto", "512") => "Lat-V2-L1-Copy-512",
            ("Lat-V2-L1-Copy-Auto", "256") => "Lat-V2-L1-Copy-256",
            ("Lat-V2-L1-Copy-Auto", "128") => "Lat-V2-L1-Copy-128",
            ("Lat-V2-L2-Copy-Auto", "512") => "Lat-V2-L2-Copy-512",
            ("Lat-V2-L2-Copy-Auto", "256") => "Lat-V2-L2-Copy-256",
            ("Lat-V2-L2-Copy-Auto", "128") => "Lat-V2-L2-Copy-128",
            ("Lat-V2-L3-Copy-Auto", "512") => "Lat-V2-L3-Copy-512",
            ("Lat-V2-L3-Copy-Auto", "256") => "Lat-V2-L3-Copy-256",
            ("Lat-V2-L3-Copy-Auto", "128") => "Lat-V2-L3-Copy-128",
            ("Lat-V2-DRAM-Copy-Auto", "512") => "Lat-V2-DRAM-Copy-512",
            ("Lat-V2-DRAM-Copy-Auto", "256") => "Lat-V2-DRAM-Copy-256",
            ("Lat-V2-DRAM-Copy-Auto", "128") => "Lat-V2-DRAM-Copy-128",
            ("Lat-V2-DRAMFull-Copy-Auto", "512") => "Lat-V2-DRAMFull-Copy-512",
            ("Lat-V2-DRAMFull-Copy-Auto", "256") => "Lat-V2-DRAMFull-Copy-256",
            ("Lat-V2-DRAMFull-Copy-Auto", "128") => "Lat-V2-DRAMFull-Copy-128",
            // Layout B — Lat-V2P-*-Read
            ("Lat-V2P-L1-Read-Auto", "512") => "Lat-V2P-L1-Read-512",
            ("Lat-V2P-L1-Read-Auto", "256") => "Lat-V2P-L1-Read-256",
            ("Lat-V2P-L1-Read-Auto", "128") => "Lat-V2P-L1-Read-128",
            ("Lat-V2P-L2-Read-Auto", "512") => "Lat-V2P-L2-Read-512",
            ("Lat-V2P-L2-Read-Auto", "256") => "Lat-V2P-L2-Read-256",
            ("Lat-V2P-L2-Read-Auto", "128") => "Lat-V2P-L2-Read-128",
            ("Lat-V2P-L3-Read-Auto", "512") => "Lat-V2P-L3-Read-512",
            ("Lat-V2P-L3-Read-Auto", "256") => "Lat-V2P-L3-Read-256",
            ("Lat-V2P-L3-Read-Auto", "128") => "Lat-V2P-L3-Read-128",
            ("Lat-V2P-DRAM-Read-Auto", "512") => "Lat-V2P-DRAM-Read-512",
            ("Lat-V2P-DRAM-Read-Auto", "256") => "Lat-V2P-DRAM-Read-256",
            ("Lat-V2P-DRAM-Read-Auto", "128") => "Lat-V2P-DRAM-Read-128",
            ("Lat-V2P-DRAMFull-Read-Auto", "512") => "Lat-V2P-DRAMFull-Read-512",
            ("Lat-V2P-DRAMFull-Read-Auto", "256") => "Lat-V2P-DRAMFull-Read-256",
            ("Lat-V2P-DRAMFull-Read-Auto", "128") => "Lat-V2P-DRAMFull-Read-128",
            // Layout B Write — packed cached writes (Path B store-buffer pressure PoC)
            ("Lat-V2P-L1-Write-Auto", "512") => "Lat-V2P-L1-Write-512",
            ("Lat-V2P-L1-Write-Auto", "256") => "Lat-V2P-L1-Write-256",
            ("Lat-V2P-L1-Write-Auto", "128") => "Lat-V2P-L1-Write-128",
            ("Lat-V2P-L2-Write-Auto", "512") => "Lat-V2P-L2-Write-512",
            ("Lat-V2P-L2-Write-Auto", "256") => "Lat-V2P-L2-Write-256",
            ("Lat-V2P-L2-Write-Auto", "128") => "Lat-V2P-L2-Write-128",
            ("Lat-V2P-L3-Write-Auto", "512") => "Lat-V2P-L3-Write-512",
            ("Lat-V2P-L3-Write-Auto", "256") => "Lat-V2P-L3-Write-256",
            ("Lat-V2P-L3-Write-Auto", "128") => "Lat-V2P-L3-Write-128",
            ("Lat-V2P-DRAM-Write-Auto", "512") => "Lat-V2P-DRAM-Write-512",
            ("Lat-V2P-DRAM-Write-Auto", "256") => "Lat-V2P-DRAM-Write-256",
            ("Lat-V2P-DRAM-Write-Auto", "128") => "Lat-V2P-DRAM-Write-128",
            ("Lat-V2P-DRAMFull-Write-Auto", "512") => "Lat-V2P-DRAMFull-Write-512",
            ("Lat-V2P-DRAMFull-Write-Auto", "256") => "Lat-V2P-DRAMFull-Write-256",
            ("Lat-V2P-DRAMFull-Write-Auto", "128") => "Lat-V2P-DRAMFull-Write-128",
            // Layout B Copy — packed read-modify-write (warms lines into L1 before stores)
            ("Lat-V2P-L1-Copy-Auto", "512") => "Lat-V2P-L1-Copy-512",
            ("Lat-V2P-L1-Copy-Auto", "256") => "Lat-V2P-L1-Copy-256",
            ("Lat-V2P-L1-Copy-Auto", "128") => "Lat-V2P-L1-Copy-128",
            ("Lat-V2P-L2-Copy-Auto", "512") => "Lat-V2P-L2-Copy-512",
            ("Lat-V2P-L2-Copy-Auto", "256") => "Lat-V2P-L2-Copy-256",
            ("Lat-V2P-L2-Copy-Auto", "128") => "Lat-V2P-L2-Copy-128",
            ("Lat-V2P-L3-Copy-Auto", "512") => "Lat-V2P-L3-Copy-512",
            ("Lat-V2P-L3-Copy-Auto", "256") => "Lat-V2P-L3-Copy-256",
            ("Lat-V2P-L3-Copy-Auto", "128") => "Lat-V2P-L3-Copy-128",
            ("Lat-V2P-DRAM-Copy-Auto", "512") => "Lat-V2P-DRAM-Copy-512",
            ("Lat-V2P-DRAM-Copy-Auto", "256") => "Lat-V2P-DRAM-Copy-256",
            ("Lat-V2P-DRAM-Copy-Auto", "128") => "Lat-V2P-DRAM-Copy-128",
            ("Lat-V2P-DRAMFull-Copy-Auto", "512") => "Lat-V2P-DRAMFull-Copy-512",
            ("Lat-V2P-DRAMFull-Copy-Auto", "256") => "Lat-V2P-DRAMFull-Copy-256",
            ("Lat-V2P-DRAMFull-Copy-Auto", "128") => "Lat-V2P-DRAMFull-Copy-128",
            // Layout B WriteFull — full-cache-line cached writes (byte-coverage isolation)
            ("Lat-V2P-L1-WriteFull-Auto", "512") => "Lat-V2P-L1-WriteFull-512",
            ("Lat-V2P-L1-WriteFull-Auto", "256") => "Lat-V2P-L1-WriteFull-256",
            ("Lat-V2P-L1-WriteFull-Auto", "128") => "Lat-V2P-L1-WriteFull-128",
            ("Lat-V2P-L2-WriteFull-Auto", "512") => "Lat-V2P-L2-WriteFull-512",
            ("Lat-V2P-L2-WriteFull-Auto", "256") => "Lat-V2P-L2-WriteFull-256",
            ("Lat-V2P-L2-WriteFull-Auto", "128") => "Lat-V2P-L2-WriteFull-128",
            ("Lat-V2P-L3-WriteFull-Auto", "512") => "Lat-V2P-L3-WriteFull-512",
            ("Lat-V2P-L3-WriteFull-Auto", "256") => "Lat-V2P-L3-WriteFull-256",
            ("Lat-V2P-L3-WriteFull-Auto", "128") => "Lat-V2P-L3-WriteFull-128",
            ("Lat-V2P-DRAM-WriteFull-Auto", "512") => "Lat-V2P-DRAM-WriteFull-512",
            ("Lat-V2P-DRAM-WriteFull-Auto", "256") => "Lat-V2P-DRAM-WriteFull-256",
            ("Lat-V2P-DRAM-WriteFull-Auto", "128") => "Lat-V2P-DRAM-WriteFull-128",
            ("Lat-V2P-DRAMFull-WriteFull-Auto", "512") => "Lat-V2P-DRAMFull-WriteFull-512",
            ("Lat-V2P-DRAMFull-WriteFull-Auto", "256") => "Lat-V2P-DRAMFull-WriteFull-256",
            ("Lat-V2P-DRAMFull-WriteFull-Auto", "128") => "Lat-V2P-DRAMFull-WriteFull-128",
            // Layout B CopyFull — full-cache-line RMW (byte-coverage isolation for Copy)
            ("Lat-V2P-L1-CopyFull-Auto", "512") => "Lat-V2P-L1-CopyFull-512",
            ("Lat-V2P-L1-CopyFull-Auto", "256") => "Lat-V2P-L1-CopyFull-256",
            ("Lat-V2P-L1-CopyFull-Auto", "128") => "Lat-V2P-L1-CopyFull-128",
            ("Lat-V2P-L2-CopyFull-Auto", "512") => "Lat-V2P-L2-CopyFull-512",
            ("Lat-V2P-L2-CopyFull-Auto", "256") => "Lat-V2P-L2-CopyFull-256",
            ("Lat-V2P-L2-CopyFull-Auto", "128") => "Lat-V2P-L2-CopyFull-128",
            ("Lat-V2P-L3-CopyFull-Auto", "512") => "Lat-V2P-L3-CopyFull-512",
            ("Lat-V2P-L3-CopyFull-Auto", "256") => "Lat-V2P-L3-CopyFull-256",
            ("Lat-V2P-L3-CopyFull-Auto", "128") => "Lat-V2P-L3-CopyFull-128",
            ("Lat-V2P-DRAM-CopyFull-Auto", "512") => "Lat-V2P-DRAM-CopyFull-512",
            ("Lat-V2P-DRAM-CopyFull-Auto", "256") => "Lat-V2P-DRAM-CopyFull-256",
            ("Lat-V2P-DRAM-CopyFull-Auto", "128") => "Lat-V2P-DRAM-CopyFull-128",
            ("Lat-V2P-DRAMFull-CopyFull-Auto", "512") => "Lat-V2P-DRAMFull-CopyFull-512",
            ("Lat-V2P-DRAMFull-CopyFull-Auto", "256") => "Lat-V2P-DRAMFull-CopyFull-256",
            ("Lat-V2P-DRAMFull-CopyFull-Auto", "128") => "Lat-V2P-DRAMFull-CopyFull-128",
            // NT-Write saturation
            ("Lat-NTW-DRAM-Write-Auto", "512") => "Lat-NTW-DRAM-Write-512",
            ("Lat-NTW-DRAM-Write-Auto", "256") => "Lat-NTW-DRAM-Write-256",
            ("Lat-NTW-DRAM-Write-Auto", "128") => "Lat-NTW-DRAM-Write-128",
            _ => return None,
        };

        // Pick the function family based on the test prefix and operation
        let is_write_full = test_name.contains("-WriteFull-");
        let is_copy_full = test_name.contains("-CopyFull-");
        let is_write = !is_write_full && test_name.contains("-Write-");
        let is_copy = !is_copy_full && test_name.contains("-Copy-");
        let func: TestFunction = if test_name.starts_with("Lat-V2P-") {
            if is_copy_full {
                TestFunction::Latency(match width {
                    "512" => lat_v2p_copy_full_512_multi,
                    "256" => lat_v2p_copy_full_256_multi,
                    _ => lat_v2p_copy_full_128_multi,
                })
            } else if is_write_full {
                TestFunction::Latency(match width {
                    "512" => lat_v2p_write_full_512_multi,
                    "256" => lat_v2p_write_full_256_multi,
                    _ => lat_v2p_write_full_128_multi,
                })
            } else if is_copy {
                TestFunction::Latency(match width {
                    "512" => lat_v2p_copy_512_multi,
                    "256" => lat_v2p_copy_256_multi,
                    _ => lat_v2p_copy_128_multi,
                })
            } else if is_write {
                TestFunction::Latency(match width {
                    "512" => lat_v2p_write_512_multi,
                    "256" => lat_v2p_write_256_multi,
                    _ => lat_v2p_write_128_multi,
                })
            } else {
                TestFunction::Latency(match width {
                    "512" => lat_v2p_read_512_multi,
                    "256" => lat_v2p_read_256_multi,
                    _ => lat_v2p_read_128_multi,
                })
            }
        } else if test_name.starts_with("Lat-V2-") {
            if is_copy {
                TestFunction::Latency(match width {
                    "512" => lat_v2_copy_512_multi,
                    "256" => lat_v2_copy_256_multi,
                    _ => lat_v2_copy_128_multi,
                })
            } else if is_write {
                TestFunction::Latency(match width {
                    "512" => lat_v2_write_512_multi,
                    "256" => lat_v2_write_256_multi,
                    _ => lat_v2_write_128_multi,
                })
            } else {
                TestFunction::Latency(match width {
                    "512" => lat_v2_read_512_multi,
                    "256" => lat_v2_read_256_multi,
                    _ => lat_v2_read_128_multi,
                })
            }
        } else {
            // Lat-NTW-*
            TestFunction::Latency(match width {
                "512" => lat_ntw_write_512_multi,
                "256" => lat_ntw_write_256_multi,
                _ => lat_ntw_write_128_multi,
            })
        };
        return Some((resolved, func));
    }

    // Strip "Auto" suffix to get base name (e.g., "Mem-MirrorV2-Auto" -> "Mem-MirrorV2-")
    let base_name = &test_name[..test_name.len() - 4];

    // Helper: classify Spd-* base_name into operation type
    // base_name after stripping "Auto" has trailing dash: "Spd-L1-Read-", "Spd-DRAMFull-Write-"
    let spd_op = if base_name.starts_with("Spd-") {
        if base_name.ends_with("-Read-") {
            Some("read")
        } else if base_name.ends_with("-Write-") {
            let is_nt = base_name.contains("DRAM");
            Some(if is_nt { "write_nt" } else { "write" })
        } else if base_name.ends_with("-Copy-") {
            let is_nt = base_name.contains("DRAM");
            Some(if is_nt { "copy_nt" } else { "copy" })
        } else {
            None
        }
    } else {
        None
    };

    // Determine best SIMD variant based on CPU capabilities
    let (variant_suffix, test_function): (&str, TestFunction) = if is_x86_feature_detected!("avx512f") {
        ("512", match base_name {
            "Mem-MirrorV2-" => TestFunction::MultiBlock(mirror_move_v2_512_multi),
            "Mem-SimpleV2-" => TestFunction::MultiBlock(simple_test_v2_512_multi),
            "Mem-SimpleNT-" => TestFunction::MultiBlock(simple_test_nt_512_multi),
            "Mem-StuckBit" => TestFunction::MultiBlock(stuck_bit_test_512_multi),
            "Mem-StuckBit-Flush" => TestFunction::MultiBlock(stuck_bit_test_512_multi),
            "Mem-Refresh" => TestFunction::MultiBlock(refresh_stable_512_multi),
            "Mem-Refresh-Flush" => TestFunction::MultiBlock(refresh_stable_512_multi),
            _ => match spd_op {
                Some("read") => TestFunction::MultiBlock(spd_read_512_multi),
                Some("write") => TestFunction::MultiBlock(spd_write_512_multi),
                Some("write_nt") => TestFunction::MultiBlock(spd_write_nt_512_multi),
                Some("copy") => TestFunction::MultiBlock(spd_copy_512_multi),
                Some("copy_nt") => TestFunction::MultiBlock(spd_copy_nt_512_multi),
                _ => return None,
            },
        })
    } else if is_x86_feature_detected!("avx2") {
        ("256", match base_name {
            "Mem-MirrorV2-" => TestFunction::MultiBlock(mirror_move_v2_256_multi),
            "Mem-SimpleV2-" => TestFunction::MultiBlock(simple_test_v2_256_multi),
            "Mem-SimpleNT-" => TestFunction::MultiBlock(simple_test_nt_256_multi),
            "Mem-StuckBit" => TestFunction::MultiBlock(stuck_bit_test_256_multi),
            "Mem-StuckBit-Flush" => TestFunction::MultiBlock(stuck_bit_test_256_multi),
            "Mem-Refresh" => TestFunction::MultiBlock(refresh_stable_256_multi),
            "Mem-Refresh-Flush" => TestFunction::MultiBlock(refresh_stable_256_multi),
            _ => match spd_op {
                Some("read") => TestFunction::MultiBlock(spd_read_256_multi),
                Some("write") => TestFunction::MultiBlock(spd_write_256_multi),
                Some("write_nt") => TestFunction::MultiBlock(spd_write_nt_256_multi),
                Some("copy") => TestFunction::MultiBlock(spd_copy_256_multi),
                Some("copy_nt") => TestFunction::MultiBlock(spd_copy_nt_256_multi),
                _ => return None,
            },
        })
    } else if is_x86_feature_detected!("sse2") {
        ("128", match base_name {
            "Mem-MirrorV2-" => TestFunction::MultiBlock(mirror_move_v2_128_multi),
            "Mem-SimpleV2-" => TestFunction::MultiBlock(simple_test_v2_128_multi),
            "Mem-SimpleNT-" => TestFunction::MultiBlock(simple_test_nt_128_multi),
            "Mem-StuckBit" => TestFunction::MultiBlock(stuck_bit_test_128_multi),
            "Mem-StuckBit-Flush" => TestFunction::MultiBlock(stuck_bit_test_128_multi),
            "Mem-Refresh" => TestFunction::MultiBlock(refresh_stable_128_multi),
            "Mem-Refresh-Flush" => TestFunction::MultiBlock(refresh_stable_128_multi),
            _ => match spd_op {
                Some("read") => TestFunction::MultiBlock(spd_read_128_multi),
                Some("write") => TestFunction::MultiBlock(spd_write_128_multi),
                Some("write_nt") => TestFunction::MultiBlock(spd_write_nt_128_multi),
                Some("copy") => TestFunction::MultiBlock(spd_copy_128_multi),
                Some("copy_nt") => TestFunction::MultiBlock(spd_copy_nt_128_multi),
                _ => return None,
            },
        })
    } else {
        // Fallback to scalar: no scalar Spd-* variants exist, use 128 (SSE2 always available on x86-64)
        ("128", match base_name {
            "Mem-MirrorV2-" => { return Some(("Mem-MirrorV2", TestFunction::MultiBlock(mirror_move_v2_multi))); },
            "Mem-SimpleV2-" => { return Some(("Mem-SimpleV2", TestFunction::MultiBlock(simple_test_v2_multi))); },
            "Mem-SimpleNT-" => TestFunction::MultiBlock(simple_test_nt_128_multi),
            "Mem-StuckBit" => { return Some(("Mem-StuckBit", TestFunction::MultiBlock(stuck_bit_test_multi))); },
            "Mem-StuckBit-Flush" => { return Some(("Mem-StuckBit-Flush", TestFunction::MultiBlock(stuck_bit_test_multi))); },
            "Mem-Refresh" => { return Some(("Mem-Refresh", TestFunction::MultiBlock(refresh_stable_multi))); },
            "Mem-Refresh-Flush" => { return Some(("Mem-Refresh-Flush", TestFunction::MultiBlock(refresh_stable_multi))); },
            _ => match spd_op {
                Some("read") => TestFunction::MultiBlock(spd_read_128_multi),
                Some("write") => TestFunction::MultiBlock(spd_write_128_multi),
                Some("write_nt") => TestFunction::MultiBlock(spd_write_nt_128_multi),
                Some("copy") => TestFunction::MultiBlock(spd_copy_128_multi),
                Some("copy_nt") => TestFunction::MultiBlock(spd_copy_nt_128_multi),
                _ => return None,
            },
        })
    };

    // Construct the resolved concrete test name
    // For Spd-* tests: strip trailing dash from base, append variant (e.g. "Spd-L1-Read-" -> "Spd-L1-Read-512")
    let concrete_name: &'static str = if base_name.starts_with("Spd-") {
        // base_name is e.g. "Spd-L1-Read-" — construct "Spd-L1-Read-{128,256,512}"
        match (base_name, variant_suffix) {
            ("Spd-L1-Read-", "512") => "Spd-L1-Read-512",
            ("Spd-L1-Read-", "256") => "Spd-L1-Read-256",
            ("Spd-L1-Read-", "128") => "Spd-L1-Read-128",
            ("Spd-L1-Write-", "512") => "Spd-L1-Write-512",
            ("Spd-L1-Write-", "256") => "Spd-L1-Write-256",
            ("Spd-L1-Write-", "128") => "Spd-L1-Write-128",
            ("Spd-L1-Copy-", "512") => "Spd-L1-Copy-512",
            ("Spd-L1-Copy-", "256") => "Spd-L1-Copy-256",
            ("Spd-L1-Copy-", "128") => "Spd-L1-Copy-128",
            ("Spd-L2-Read-", "512") => "Spd-L2-Read-512",
            ("Spd-L2-Read-", "256") => "Spd-L2-Read-256",
            ("Spd-L2-Read-", "128") => "Spd-L2-Read-128",
            ("Spd-L2-Write-", "512") => "Spd-L2-Write-512",
            ("Spd-L2-Write-", "256") => "Spd-L2-Write-256",
            ("Spd-L2-Write-", "128") => "Spd-L2-Write-128",
            ("Spd-L2-Copy-", "512") => "Spd-L2-Copy-512",
            ("Spd-L2-Copy-", "256") => "Spd-L2-Copy-256",
            ("Spd-L2-Copy-", "128") => "Spd-L2-Copy-128",
            ("Spd-L3-Read-", "512") => "Spd-L3-Read-512",
            ("Spd-L3-Read-", "256") => "Spd-L3-Read-256",
            ("Spd-L3-Read-", "128") => "Spd-L3-Read-128",
            ("Spd-L3-Write-", "512") => "Spd-L3-Write-512",
            ("Spd-L3-Write-", "256") => "Spd-L3-Write-256",
            ("Spd-L3-Write-", "128") => "Spd-L3-Write-128",
            ("Spd-L3-Copy-", "512") => "Spd-L3-Copy-512",
            ("Spd-L3-Copy-", "256") => "Spd-L3-Copy-256",
            ("Spd-L3-Copy-", "128") => "Spd-L3-Copy-128",
            ("Spd-DRAMSmall-Read-", "512") => "Spd-DRAMSmall-Read-512",
            ("Spd-DRAMSmall-Read-", "256") => "Spd-DRAMSmall-Read-256",
            ("Spd-DRAMSmall-Read-", "128") => "Spd-DRAMSmall-Read-128",
            ("Spd-DRAMSmall-Write-", "512") => "Spd-DRAMSmall-Write-512",
            ("Spd-DRAMSmall-Write-", "256") => "Spd-DRAMSmall-Write-256",
            ("Spd-DRAMSmall-Write-", "128") => "Spd-DRAMSmall-Write-128",
            ("Spd-DRAMSmall-Copy-", "512") => "Spd-DRAMSmall-Copy-512",
            ("Spd-DRAMSmall-Copy-", "256") => "Spd-DRAMSmall-Copy-256",
            ("Spd-DRAMSmall-Copy-", "128") => "Spd-DRAMSmall-Copy-128",
            ("Spd-DRAMFull-Read-", "512") => "Spd-DRAMFull-Read-512",
            ("Spd-DRAMFull-Read-", "256") => "Spd-DRAMFull-Read-256",
            ("Spd-DRAMFull-Read-", "128") => "Spd-DRAMFull-Read-128",
            ("Spd-DRAMFull-Write-", "512") => "Spd-DRAMFull-Write-512",
            ("Spd-DRAMFull-Write-", "256") => "Spd-DRAMFull-Write-256",
            ("Spd-DRAMFull-Write-", "128") => "Spd-DRAMFull-Write-128",
            ("Spd-DRAMFull-Copy-", "512") => "Spd-DRAMFull-Copy-512",
            ("Spd-DRAMFull-Copy-", "256") => "Spd-DRAMFull-Copy-256",
            ("Spd-DRAMFull-Copy-", "128") => "Spd-DRAMFull-Copy-128",
            _ => return None,
        }
    } else {
        match (base_name, variant_suffix) {
            ("Mem-MirrorV2-", "512") => "Mem-MirrorV2-512",
            ("Mem-MirrorV2-", "256") => "Mem-MirrorV2-256",
            ("Mem-MirrorV2-", "128") => "Mem-MirrorV2-128",
            ("Mem-SimpleV2-", "512") => "Mem-SimpleV2-512",
            ("Mem-SimpleV2-", "256") => "Mem-SimpleV2-256",
            ("Mem-SimpleV2-", "128") => "Mem-SimpleV2-128",
            ("Mem-SimpleNT-", "512") => "Mem-SimpleNT-512",
            ("Mem-SimpleNT-", "256") => "Mem-SimpleNT-256",
            ("Mem-SimpleNT-", "128") => "Mem-SimpleNT-128",
            ("Mem-StuckBit", "512") => "Mem-StuckBit512",
            ("Mem-StuckBit", "256") => "Mem-StuckBit256",
            ("Mem-StuckBit", "128") => "Mem-StuckBit128",
            ("Mem-StuckBit-Flush", "512") => "Mem-StuckBit-Flush512",
            ("Mem-StuckBit-Flush", "256") => "Mem-StuckBit-Flush256",
            ("Mem-StuckBit-Flush", "128") => "Mem-StuckBit-Flush128",
            ("Mem-Refresh", "512") => "Mem-Refresh512",
            ("Mem-Refresh", "256") => "Mem-Refresh256",
            ("Mem-Refresh", "128") => "Mem-Refresh128",
            ("Mem-Refresh-Flush", "512") => "Mem-Refresh-Flush512",
            ("Mem-Refresh-Flush", "256") => "Mem-Refresh-Flush256",
            ("Mem-Refresh-Flush", "128") => "Mem-Refresh-Flush128",
            _ => return None,
        }
    };

    Some((concrete_name, test_function))
}

pub(crate) fn create_test_definitions(cache_info: &CacheInfo) -> Vec<TestDefinition> {
    // TSC frequency from the passed cache_info (detected once at startup)
    let tsc_freq = cache_info.tsc_frequency_ghz;

    let test_definitions = vec![
        // === CRITICAL: Full Memory Stuck Bit Test ===
        (
            "Mem-StuckBit",
            TestFunction::MultiBlock(stuck_bit_test_multi),
            TestMemoryConfig::new(
                ExtentMode::FullAllocation,
                ChunkMode::Absolute { size_bytes: 512 * MB }
            ).with_timing(TestTiming::cycles_only(1))
             .with_memory_type(None)
        ),

        // === StuckBitTest SIMD variants ===
        (
            "Mem-StuckBit128",
            TestFunction::MultiBlock(stuck_bit_test_128_multi),
            TestMemoryConfig::new(
                ExtentMode::FullAllocation,
                ChunkMode::Absolute { size_bytes: 512 * MB }
            ).with_timing(TestTiming::cycles_only(1))
             .with_memory_type(None)
        ),

        (
            "Mem-StuckBit256",
            TestFunction::MultiBlock(stuck_bit_test_256_multi),
            TestMemoryConfig::new(
                ExtentMode::FullAllocation,
                ChunkMode::Absolute { size_bytes: 512 * MB }
            ).with_timing(TestTiming::cycles_only(1))
             .with_memory_type(None)
        ),

        (
            "Mem-StuckBit512",
            TestFunction::MultiBlock(stuck_bit_test_512_multi),
            TestMemoryConfig::new(
                ExtentMode::FullAllocation,
                ChunkMode::Absolute { size_bytes: 512 * MB }
            ).with_timing(TestTiming::cycles_only(1))
             .with_memory_type(None)
        ),

        // === StuckBitTest Auto-dispatch (AVX-512 > AVX2 > SSE2 > scalar) ===
        // Placed last so results match the actual variant performance
        (
            "Mem-StuckBitAuto",
            TestFunction::MultiBlock(stuck_bit_test_auto_multi),
            TestMemoryConfig::new(
                ExtentMode::FullAllocation,
                ChunkMode::Absolute { size_bytes: 512 * MB }
            ).with_timing(TestTiming::cycles_only(1))
             .with_memory_type(None)
        ),

        // === StuckBit CLFLUSHOPT-verify variant (TODO #59) ===
        // Same 3-phase alternating-bit test, but each chunk is flushed out of cache between
        // write and verify, so the verify provably round-trips through DRAM instead of reading
        // the line it just wrote. User-mode replacement for UC driver memory: stops a valid
        // cached line from masking a flipped DRAM bit.
        //
        // These deliberately use an L2-sized chunk, NOT the plain variants' large one (512 MiB).
        // Measured on Intel Granite Rapids, 4T (1/core) and 8T (SMT), MiB/s, flush off → on:
        //     chunk    64 KiB   1 MiB   16 MiB   256 MiB
        //     4T off   134500  120763    55725     42944
        //     4T on     23061   36285    36629     36064
        //     8T off   150104  134165    61726     56338
        //     8T on     31680   48574    47846     45250
        // Two things to read off that: the penalty grows as chunks shrink (1.19× → 5.8×), and
        // — the actual point — *flush=off varies ~3× with chunk size while flush=on is flat
        // within 1.6% for chunks ≥ 1 MiB*. Without the flush, what the test measures depends on
        // chunk-vs-cache: at small chunks the "verify" reads SRAM written microseconds ago and
        // never touches DRAM. So the flush belongs precisely where the chunk is cache-resident;
        // at the plain variants' large chunks natural eviction already forces DRAM reads and
        // flushing only buys ~20% less bandwidth for no change in what is tested.
        //
        // Why `scale: 1.0` and not 0.5: `CacheTarget::L2` divides by *active threads per core*
        // (see `size_bytes_cpuid`), so on this 2 MiB-L2 part 0.5 resolves to 1 MiB at
        // 1 thread/core but only 512 KiB under SMT — and flush-on throughput *falls* below
        // ~1 MiB (8T: 48574 @ 1 MiB → 31680 @ 64 KiB) because the per-chunk fence + call +
        // trailing MFENCE is paid far more often per GiB with too little work to overlap the
        // drain against. There is no minimum-chunk floor here beyond SIMD alignment, so nothing
        // would clamp that. 1.0 lands at 2 MiB / 1 MiB respectively — both in the measured flat
        // zone, still cache-resident by construction.
        //
        // Those numbers come from a fixed-absolute-chunk sweep of Mem-StuckBit128
        // (test_configs/flush_chunk_sweep.json), i.e. they isolate "does the verify reach DRAM"
        // by holding chunk size constant. Compared instead at *operational defaults* — plain at
        // its then ~6% fraction (512 MiB since 2026-10-03) vs this at Cache{L2} — the
        // guarantee turns out to be free (8T, MiB/s): scalar 43056 → 43763, 128 47681 → 45705,
        // 256 48060 → 46965, 512 46301 → 47886. Two are faster; all four are inside this box's
        // noise. Both configs are DRAM-bandwidth-bound and move the same total traffic, so the
        // flush *reschedules* the writeback rather than adding to it. (Contrast
        // Mem-Refresh-FlushAuto at -22%: there the write already streams to DRAM naturally, so the
        // flush is pure added instruction cost.) Single run — treat the ±4% spread as "no
        // measurable difference".
        //
        // Auto-dispatch (one preset, not four): under flush the widths converge to a 4.8% spread
        // because all are DRAM-bound (see doc/simd_codegen_rules.md Rule 3), so per-width
        // registrations would measure the same number repeatedly. Set `flush_before_verify` in a
        // JSON config to pin a specific width.
        (
            "Mem-StuckBit-FlushAuto",
            TestFunction::MultiBlock(stuck_bit_test_auto_multi),
            TestMemoryConfig::new(
                ExtentMode::FullAllocation,
                ChunkMode::Cache { target: CacheTarget::L2 { scale: 1.0 } }
            ).with_timing(TestTiming::cycles_only(1))
             .with_memory_type(None)
             .with_flush_before_verify(true)
        ),

        // === SimpleTest NT (non-temporal stores) — bandwidth comparison ===
        (
            "Mem-SimpleNT-128",
            TestFunction::MultiBlock(simple_test_nt_128_multi),
            TestMemoryConfig::new(
                ExtentMode::FullAllocation,
                ChunkMode::Absolute { size_bytes: 4 * MB }
            ).with_timing(TestTiming::hybrid(100, 30))
             .with_memory_type(None)
        ),

        (
            "Mem-SimpleNT-256",
            TestFunction::MultiBlock(simple_test_nt_256_multi),
            TestMemoryConfig::new(
                ExtentMode::FullAllocation,
                ChunkMode::Absolute { size_bytes: 4 * MB }
            ).with_timing(TestTiming::hybrid(100, 30))
             .with_memory_type(None)
        ),

        (
            "Mem-SimpleNT-512",
            TestFunction::MultiBlock(simple_test_nt_512_multi),
            TestMemoryConfig::new(
                ExtentMode::FullAllocation,
                ChunkMode::Absolute { size_bytes: 4 * MB }
            ).with_timing(TestTiming::hybrid(100, 30))
             .with_memory_type(None)
        ),

        (
            "Mem-SimpleNT-Auto",
            TestFunction::MultiBlock(simple_test_nt_auto_multi),
            TestMemoryConfig::new(
                ExtentMode::FullAllocation,
                ChunkMode::Absolute { size_bytes: 4 * MB }
            ).with_timing(TestTiming::hybrid(100, 30))
             .with_memory_type(None)
        ),

        // === Refresh stability tests (MultiBlock implementations) ===
        (
            "Mem-Refresh",
            TestFunction::MultiBlock(refresh_stable_multi),
            TestMemoryConfig::new(
                ExtentMode::CacheTotal { fraction: 2.0 },
                ChunkMode::Absolute { size_bytes: 2048 * MB }
            ).with_timing(TestTiming::duration_only(15))
             .with_memory_type(None)
        ),

        (
            "Mem-Refresh128",
            TestFunction::MultiBlock(refresh_stable_128_multi),
            TestMemoryConfig::new(
                ExtentMode::CacheTotal { fraction: 2.0 },
                ChunkMode::Absolute { size_bytes: 2048 * MB }
            ).with_timing(TestTiming::duration_only(15))
             .with_memory_type(None)
        ),

        (
            "Mem-Refresh256",
            TestFunction::MultiBlock(refresh_stable_256_multi),
            TestMemoryConfig::new(
                ExtentMode::CacheTotal { fraction: 2.0 },
                ChunkMode::Absolute { size_bytes: 2048 * MB }
            ).with_timing(TestTiming::duration_only(15))
             .with_memory_type(None)
        ),

        (
            "Mem-Refresh512",
            TestFunction::MultiBlock(refresh_stable_512_multi),
            TestMemoryConfig::new(
                ExtentMode::CacheTotal { fraction: 2.0 },
                ChunkMode::Absolute { size_bytes: 2048 * MB }
            ).with_timing(TestTiming::duration_only(15))
             .with_memory_type(None)
        ),

        // === RefreshStable Auto-dispatch (AVX-512 > AVX2 > SSE2 > scalar) ===
        // Placed after the explicit widths so results match the actual variant performance
        (
            "Mem-RefreshAuto",
            TestFunction::MultiBlock(refresh_stable_auto_multi),
            TestMemoryConfig::new(
                ExtentMode::CacheTotal { fraction: 2.0 },
                ChunkMode::Absolute { size_bytes: 2048 * MB }
            ).with_timing(TestTiming::duration_only(15))
             .with_memory_type(None)
        ),

        // === Refresh CLFLUSHOPT-verify variant (TODO #59, opt-in) ===
        // Same test with the pre-sleep flush enabled, making the DRAM round-trip architecturally
        // guaranteed rather than relying on eviction policy. Not the default: the plain variants'
        // `CacheTotal 2.0x` extent, multi-thread shared-L3 pressure, and forward verify order
        // already evict all but a small, least-likely-resident tail (see refresh_impl!).
        //
        // Measured cost (Intel Granite Rapids, 8T, MiB/s): Mem-Refresh512 45,797 and
        // Mem-Refresh512_A 45,457 vs Mem-Refresh-Flush 35,293 — **-22%** for the guarantee.
        // Worth it when you want certainty rather than probability; not worth it for routine
        // stability runs, where throughput is coverage per unit time.
        //
        // Auto-dispatch (one preset, not four): under flush every width is DRAM-bound, so the
        // widths converge and separate per-width registrations would measure the same number
        // repeatedly. Set `flush_before_verify` in a JSON config to pin a specific width.
        // Listed last so it reads as the variant of the plain suite above it.
        (
            "Mem-Refresh-FlushAuto",
            TestFunction::MultiBlock(refresh_stable_auto_multi),
            TestMemoryConfig::new(
                ExtentMode::CacheTotal { fraction: 2.0 },
                ChunkMode::Absolute { size_bytes: 2048 * MB }
            ).with_timing(TestTiming::duration_only(15))
             .with_memory_type(None)
             .with_flush_before_verify(true)
        ),

        // === Performance stress tests ===
        (
            "Mem-CacheBust",
            TestFunction::MultiBlock(cache_busting_multi),
            TestMemoryConfig::new(
                ExtentMode::CacheTotal { fraction: 0.5 },
                ChunkMode::Absolute { size_bytes: MB }
            ).with_timing(TestTiming::duration_only(20))
             .with_parameter_context(crate::config::TestParameterContext {
                 raw_parameter: 4,
                 stride_patterns: Some(4),
                 ..Default::default()
             })
             .with_memory_type(None)
        ),

        (
            "Mem-Random",
            TestFunction::MultiBlock(random_torture_multi),
            TestMemoryConfig::new(
                ExtentMode::FullAllocation,
                ChunkMode::Absolute { size_bytes: 8 * MB }
            ).with_timing(TestTiming::duration_only(25))
             .with_parameter_context(crate::config::TestParameterContext {
                 raw_parameter: 8,
                 rng_sequences: Some(8),
                 ..Default::default()
             })
             .with_memory_type(None)
        ),

        (
            "Mem-Stride",
            TestFunction::MultiBlock(stride_access_multi),
            TestMemoryConfig::new(
                ExtentMode::FullAllocation,
                ChunkMode::Absolute { size_bytes: 2 * MB }
            ).with_timing(TestTiming::cycles_only(1))
             .with_parameter_context(crate::config::TestParameterContext {
                 raw_parameter: 4,
                 subdivisions: Some(4),
                 ..Default::default()
             })
             .with_memory_type(None)
        ),

        // Moving inversions, memtest86's (TODO 97): chunks twice the L3, so every pass reaches DRAM
        (
            "Mem-MovingInv",
            TestFunction::MultiBlock(moving_inversions_multi),
            TestMemoryConfig::new(
                ExtentMode::FullAllocation,
                ChunkMode::Cache { target: CacheTarget::Dram { scale: 2.0 } }
            ).with_timing(TestTiming::duration_only(30))
             .with_memory_type(None)
        ),

        // Protected victims, memtest86's modulo-20 (TODO 97): chunks twice the L3, as the moving
        // inversions'
        (
            "Mem-ModuloX",
            TestFunction::MultiBlock(modulo_x_multi),
            TestMemoryConfig::new(
                ExtentMode::FullAllocation,
                ChunkMode::Cache { target: CacheTarget::Dram { scale: 2.0 } }
            ).with_timing(TestTiming::duration_only(30))
             .with_memory_type(None)
        ),

        // A configurable sequence of writes, reads and read-then-writes in an order (TODO 97):
        // SimpleTest's write and reads, a pass that reads and writes line by line, and reads in a
        // random order, with chunks twice the L3, so each pass reaches DRAM
        (
            "Mem-Sequence",
            TestFunction::MultiBlock(sequence_auto_multi),
            TestMemoryConfig::new(
                ExtentMode::FullAllocation,
                ChunkMode::Cache { target: CacheTarget::Dram { scale: 2.0 } }
            ).with_timing(TestTiming::duration_only(30))
             .with_memory_type(None)
             .with_write_read_cycles(2)
             .with_sequence(crate::sequence::Sequence::parse("W 2R RW 2R@random", crate::sequence::Order::Forward)
                 .expect("a valid sequence"))
        ),

        (
            "Mem-BlockMove",
            TestFunction::MultiBlock(block_move_multi),
            TestMemoryConfig::new(
                // Twice the caches, so its copies stream through DRAM (TODO 91)
                ExtentMode::CacheTotal { fraction: 2.0 },
                ChunkMode::Absolute { size_bytes: 16 * MB }
            ).with_timing(TestTiming::duration_only(20))
             .with_parameter_context(crate::config::TestParameterContext {
                 raw_parameter: 1,
                 copy_directions: Some(1),
                 ..Default::default()
             })
             .with_memory_type(None)
        ),

        // === v2 Tests — fixed PRNG, correct parameter interpretation, u64 MirrorMove ===

        // Mem-SimpleV2: matches v1 Mem-Simple config (FullAllocation, 4MB chunk, 100cycles/30s),
        // with one pair of rounds a cycle: a native program writes P, then ~P (`pair_native_rounds`)
        (
            "Mem-SimpleV2",
            TestFunction::MultiBlock(simple_test_v2_multi),
            TestMemoryConfig::new(
                ExtentMode::FullAllocation,
                ChunkMode::Absolute { size_bytes: 4 * MB }
            ).with_timing(TestTiming::hybrid(100, 30))
             .with_memory_type(None)
             .with_write_read_cycles(2)
        ),

        // Mem-SimpleV2 SIMD: matches scalar config for A/B comparison
        (
            "Mem-SimpleV2-128",
            TestFunction::MultiBlock(simple_test_v2_128_multi),
            TestMemoryConfig::new(
                ExtentMode::FullAllocation,
                ChunkMode::Absolute { size_bytes: 4 * MB }
            ).with_timing(TestTiming::hybrid(100, 30))
             .with_memory_type(None)
             .with_write_read_cycles(2)
        ),

        (
            "Mem-SimpleV2-256",
            TestFunction::MultiBlock(simple_test_v2_256_multi),
            TestMemoryConfig::new(
                ExtentMode::FullAllocation,
                ChunkMode::Absolute { size_bytes: 4 * MB }
            ).with_timing(TestTiming::hybrid(100, 30))
             .with_memory_type(None)
             .with_write_read_cycles(2)
        ),

        (
            "Mem-SimpleV2-512",
            TestFunction::MultiBlock(simple_test_v2_512_multi),
            TestMemoryConfig::new(
                ExtentMode::FullAllocation,
                ChunkMode::Absolute { size_bytes: 4 * MB }
            ).with_timing(TestTiming::hybrid(100, 30))
             .with_memory_type(None)
             .with_write_read_cycles(2)
        ),

        (
            "Mem-SimpleV2-Auto",
            TestFunction::MultiBlock(simple_test_v2_auto_multi),
            TestMemoryConfig::new(
                ExtentMode::FullAllocation,
                ChunkMode::Absolute { size_bytes: 4 * MB }
            ).with_timing(TestTiming::hybrid(100, 30))
             .with_memory_type(None)
             .with_write_read_cycles(2)
        ),

        // Mem-MirrorV2*: matches v1 Mem-Mirror config (FixedSize 64MB extent, 4MB chunk, 10s)
        (
            "Mem-MirrorV2",
            TestFunction::MultiBlock(mirror_move_v2_multi),
            TestMemoryConfig::new(
                ExtentMode::Absolute { size_bytes: 64 * MB },
                ChunkMode::Absolute { size_bytes: 4 * MB }
            ).with_timing(TestTiming::duration_only(10))
             .with_memory_type(None)
        ),

        // v2 SIMD configs match v1 counterparts for fair A/B comparison
        (
            "Mem-MirrorV2-128",
            TestFunction::MultiBlock(mirror_move_v2_128_multi),
            TestMemoryConfig::new(
                ExtentMode::Absolute { size_bytes: 64 * MB },
                ChunkMode::Absolute { size_bytes: 8 * MB }
            ).with_timing(TestTiming::duration_only(10))
             .with_memory_type(None)
        ),

        (
            "Mem-MirrorV2-256",
            TestFunction::MultiBlock(mirror_move_v2_256_multi),
            TestMemoryConfig::new(
                ExtentMode::Absolute { size_bytes: 128 * MB },
                ChunkMode::Absolute { size_bytes: 8 * MB }
            ).with_timing(TestTiming::duration_only(10))
             .with_parameter_context(crate::config::TestParameterContext {
                 mirror: Some(crate::config::MirrorMode::Subblocks(2)),
                 ..Default::default()
             })
             .with_memory_type(None)
        ),

        (
            "Mem-MirrorV2-512",
            TestFunction::MultiBlock(mirror_move_v2_512_multi),
            TestMemoryConfig::new(
                ExtentMode::Absolute { size_bytes: 256 * MB },
                ChunkMode::Absolute { size_bytes: 8 * MB }
            ).with_timing(TestTiming::duration_only(10))
             .with_parameter_context(crate::config::TestParameterContext {
                 mirror: Some(crate::config::MirrorMode::Subblocks(4)),
                 ..Default::default()
             })
             .with_memory_type(None)
        ),

        (
            "Mem-MirrorV2-Auto",
            TestFunction::MultiBlock(mirror_move_v2_auto_multi),
            TestMemoryConfig::new(
                ExtentMode::Absolute { size_bytes: 64 * MB },
                ChunkMode::Absolute { size_bytes: 8 * MB }
            ).with_timing(TestTiming::duration_only(10))
             .with_memory_type(None)
        ),

        // === Bench-Init: Pattern Generation Throughput Benchmarks ===
        // test=Bench-Init-* runs all 6, test=Bench-Init-TM5-* runs TM5-faithful only
        // skip_init=true: init_fn not called at startup, test_fn does the pattern write.
        // This avoids double-writing (init + test_fn) — only the test_fn writes are measured.
        (
            "Bench-Init-TM5-0",
            TestFunction::MultiBlock(bench_init_multi),
            TestMemoryConfig::new(
                ExtentMode::FullAllocation,
                ChunkMode::Absolute { size_bytes: 4 * MB }
            ).with_timing(TestTiming::cycles_only(10))
             .with_pattern_config(Some(0), None, None)
             .with_skip_init(true)
             .with_memory_type(None)
        ),
        (
            "Bench-Init-TM5-1",
            TestFunction::MultiBlock(bench_init_multi),
            TestMemoryConfig::new(
                ExtentMode::FullAllocation,
                ChunkMode::Absolute { size_bytes: 4 * MB }
            ).with_timing(TestTiming::cycles_only(10))
             .with_pattern_config(Some(1), None, None)
             .with_skip_init(true)
             .with_memory_type(None)
        ),
        (
            "Bench-Init-TM5-2",
            TestFunction::MultiBlock(bench_init_multi),
            TestMemoryConfig::new(
                ExtentMode::FullAllocation,
                ChunkMode::Absolute { size_bytes: 4 * MB }
            ).with_timing(TestTiming::cycles_only(10))
             .with_pattern_config(Some(2), Some(0x5DEECE66D), Some(0xB))
             .with_skip_init(true)
             .with_memory_type(None)
        ),
        (
            "Bench-Init-TMR-10",
            TestFunction::MultiBlock(bench_init_multi),
            TestMemoryConfig::new(
                ExtentMode::FullAllocation,
                ChunkMode::Absolute { size_bytes: 4 * MB }
            ).with_timing(TestTiming::cycles_only(10))
             .with_pattern_config(Some(10), None, None)
             .with_skip_init(true)
             .with_memory_type(None)
        ),
        (
            "Bench-Init-TMR-11",
            TestFunction::MultiBlock(bench_init_multi),
            TestMemoryConfig::new(
                ExtentMode::FullAllocation,
                ChunkMode::Absolute { size_bytes: 4 * MB }
            ).with_timing(TestTiming::cycles_only(10))
             .with_pattern_config(Some(11), None, None)
             .with_skip_init(true)
             .with_memory_type(None)
        ),
        (
            "Bench-Init-TMR-13",
            TestFunction::MultiBlock(bench_init_multi),
            TestMemoryConfig::new(
                ExtentMode::FullAllocation,
                ChunkMode::Absolute { size_bytes: 4 * MB }
            ).with_timing(TestTiming::cycles_only(10))
             .with_pattern_config(Some(13), Some(0xC0FFEE42C0FFEE42), None)
             .with_skip_init(true)
             .with_memory_type(None)
        ),
        (
            "Bench-Init-TMR-20",
            TestFunction::MultiBlock(bench_init_multi),
            TestMemoryConfig::new(
                ExtentMode::FullAllocation,
                ChunkMode::Absolute { size_bytes: 4 * MB }
            ).with_timing(TestTiming::cycles_only(10))
             .with_pattern_config(Some(20), Some(0xDEADBEEFDEADBEEF), Some(0xCAFEBABECAFEBABE))
             .with_skip_init(true)
             .with_memory_type(None)
        ),
        (
            "Bench-Init-TMR-30",
            TestFunction::MultiBlock(bench_init_multi),
            TestMemoryConfig::new(
                ExtentMode::FullAllocation,
                ChunkMode::Absolute { size_bytes: 4 * MB }
            ).with_timing(TestTiming::cycles_only(10))
             .with_pattern_config(Some(30), Some(0xDEADBEEFDEADBEEF), Some(0xCAFEBABECAFEBABE))
             .with_skip_init(true)
             .with_memory_type(None)
        ),

        // === Bench-Verify: Pattern Verification Throughput Benchmarks ===
        // Independent mode (default): writes patterns then measures verify throughput
        // Dependent mode: set skip_init=true in config, requires matching Bench-Init-* earlier in plan
        (
            "Bench-Verify-TM5-0",
            TestFunction::MultiBlock(bench_verify_multi),
            TestMemoryConfig::new(
                ExtentMode::FullAllocation,
                ChunkMode::Absolute { size_bytes: 4 * MB }
            ).with_timing(TestTiming::cycles_only(10))
             .with_pattern_config(Some(0), None, None)
             .with_memory_type(None)
        ),
        (
            "Bench-Verify-TM5-1",
            TestFunction::MultiBlock(bench_verify_multi),
            TestMemoryConfig::new(
                ExtentMode::FullAllocation,
                ChunkMode::Absolute { size_bytes: 4 * MB }
            ).with_timing(TestTiming::cycles_only(10))
             .with_pattern_config(Some(1), None, None)
             .with_memory_type(None)
        ),
        (
            "Bench-Verify-TM5-2",
            TestFunction::MultiBlock(bench_verify_multi),
            TestMemoryConfig::new(
                ExtentMode::FullAllocation,
                ChunkMode::Absolute { size_bytes: 4 * MB }
            ).with_timing(TestTiming::cycles_only(10))
             .with_pattern_config(Some(2), Some(0x5DEECE66D), Some(0xB))
             .with_memory_type(None)
        ),
        (
            "Bench-Verify-TMR-10",
            TestFunction::MultiBlock(bench_verify_multi),
            TestMemoryConfig::new(
                ExtentMode::FullAllocation,
                ChunkMode::Absolute { size_bytes: 4 * MB }
            ).with_timing(TestTiming::cycles_only(10))
             .with_pattern_config(Some(10), None, None)
             .with_memory_type(None)
        ),
        (
            "Bench-Verify-TMR-11",
            TestFunction::MultiBlock(bench_verify_multi),
            TestMemoryConfig::new(
                ExtentMode::FullAllocation,
                ChunkMode::Absolute { size_bytes: 4 * MB }
            ).with_timing(TestTiming::cycles_only(10))
             .with_pattern_config(Some(11), None, None)
             .with_memory_type(None)
        ),
        (
            "Bench-Verify-TMR-13",
            TestFunction::MultiBlock(bench_verify_multi),
            TestMemoryConfig::new(
                ExtentMode::FullAllocation,
                ChunkMode::Absolute { size_bytes: 4 * MB }
            ).with_timing(TestTiming::cycles_only(10))
             .with_pattern_config(Some(13), Some(0xC0FFEE42C0FFEE42), None)
             .with_memory_type(None)
        ),
        (
            "Bench-Verify-TMR-20",
            TestFunction::MultiBlock(bench_verify_multi),
            TestMemoryConfig::new(
                ExtentMode::FullAllocation,
                ChunkMode::Absolute { size_bytes: 4 * MB }
            ).with_timing(TestTiming::cycles_only(10))
             .with_pattern_config(Some(20), Some(0xDEADBEEFDEADBEEF), Some(0xCAFEBABECAFEBABE))
             .with_memory_type(None)
        ),
        (
            "Bench-Verify-TMR-30",
            TestFunction::MultiBlock(bench_verify_multi),
            TestMemoryConfig::new(
                ExtentMode::FullAllocation,
                ChunkMode::Absolute { size_bytes: 4 * MB }
            ).with_timing(TestTiming::cycles_only(10))
             .with_pattern_config(Some(30), Some(0xDEADBEEFDEADBEEF), Some(0xCAFEBABECAFEBABE))
             .with_memory_type(None)
        ),

        // === Cache Hierarchy Latency Tests ===
        // Uses ExtentMode::Cache for automatic sizing based on detected cache
        // Naming: Lat-{Level}-{Operation} for easy filtering
        // tests=Lat-* (15 tests: L1/L2/L3/DRAM/DRAMFull × Read/Write/Copy)
        // tests=Lat-L3-* (3 tests: L3 × Read/Write/Copy)

        // L1 Cache - Read, Write, Copy
        (
            "Lat-L1-Read",
            TestFunction::Latency(read_latency_multi),
            TestMemoryConfig::new(
                ExtentMode::Cache { target: CacheTarget::L1_DEFAULT },
                ChunkMode::Whole
            ).with_timing(TestTiming::duration_only(10))
             .with_tsc(tsc_freq)
             .with_memory_type(None)
        ),

        (
            "Lat-L1-Write",
            TestFunction::Latency(write_latency_multi),
            TestMemoryConfig::new(
                ExtentMode::Cache { target: CacheTarget::L1_DEFAULT },
                ChunkMode::Whole
            ).with_timing(TestTiming::duration_only(10))
             .with_tsc(tsc_freq)
             .with_memory_type(None)
        ),

        (
            "Lat-L1-Copy",
            TestFunction::Latency(copy_latency_multi),
            TestMemoryConfig::new(
                ExtentMode::Cache { target: CacheTarget::L1_DEFAULT },
                ChunkMode::Whole
            ).with_timing(TestTiming::duration_only(10))
             .with_tsc(tsc_freq)
             .with_memory_type(None)
        ),

        // L2 Cache - Read, Write, Copy
        (
            "Lat-L2-Read",
            TestFunction::Latency(read_latency_multi),
            TestMemoryConfig::new(
                ExtentMode::Cache { target: CacheTarget::L2_DEFAULT },
                ChunkMode::Whole
            ).with_timing(TestTiming::duration_only(10))
             .with_tsc(tsc_freq)
             .with_memory_type(None)
        ),

        (
            "Lat-L2-Write",
            TestFunction::Latency(write_latency_multi),
            TestMemoryConfig::new(
                ExtentMode::Cache { target: CacheTarget::L2_DEFAULT },
                ChunkMode::Whole
            ).with_timing(TestTiming::duration_only(10))
             .with_tsc(tsc_freq)
             .with_memory_type(None)
        ),

        (
            "Lat-L2-Copy",
            TestFunction::Latency(copy_latency_multi),
            TestMemoryConfig::new(
                ExtentMode::Cache { target: CacheTarget::L2_DEFAULT },
                ChunkMode::Whole
            ).with_timing(TestTiming::duration_only(10))
             .with_tsc(tsc_freq)
             .with_memory_type(None)
        ),

        // L3 Cache - Read, Write, Copy
        (
            "Lat-L3-Read",
            TestFunction::Latency(read_latency_multi),
            TestMemoryConfig::new(
                ExtentMode::Cache { target: CacheTarget::L3_DEFAULT },
                ChunkMode::Whole
            ).with_timing(TestTiming::duration_only(10))
             .with_tsc(tsc_freq)
             .with_memory_type(None)
        ),

        (
            "Lat-L3-Write",
            TestFunction::Latency(write_latency_multi),
            TestMemoryConfig::new(
                ExtentMode::Cache { target: CacheTarget::L3_DEFAULT },
                ChunkMode::Whole
            ).with_timing(TestTiming::duration_only(10))
             .with_tsc(tsc_freq)
             .with_memory_type(None)
        ),

        (
            "Lat-L3-Copy",
            TestFunction::Latency(copy_latency_multi),
            TestMemoryConfig::new(
                ExtentMode::Cache { target: CacheTarget::L3_DEFAULT },
                ChunkMode::Whole
            ).with_timing(TestTiming::duration_only(10))
             .with_tsc(tsc_freq)
             .with_memory_type(None)
        ),

        // DRAM - High TLB hit rate (typical latency) - Read, Write, Copy
        (
            "Lat-DRAM-Read",
            TestFunction::Latency(read_latency_multi),
            TestMemoryConfig::new(
                ExtentMode::Cache { target: CacheTarget::DRAM_DEFAULT },
                ChunkMode::Whole
            ).with_timing(TestTiming::duration_only(10))
             .with_tsc(tsc_freq)
             .with_memory_type(None)
        ),

        (
            "Lat-DRAM-Write",
            TestFunction::Latency(write_latency_multi),
            TestMemoryConfig::new(
                ExtentMode::Cache { target: CacheTarget::DRAM_DEFAULT },
                ChunkMode::Whole
            ).with_timing(TestTiming::duration_only(10))
             .with_tsc(tsc_freq)
             .with_memory_type(None)
        ),

        (
            "Lat-DRAM-Copy",
            TestFunction::Latency(copy_latency_multi),
            TestMemoryConfig::new(
                ExtentMode::Cache { target: CacheTarget::DRAM_DEFAULT },
                ChunkMode::Whole
            ).with_timing(TestTiming::duration_only(10))
             .with_tsc(tsc_freq)
             .with_memory_type(None)
        ),

        // DRAMFull - Low TLB hit rate (stress with page miss overhead) - Read, Write, Copy
        (
            "Lat-DRAMFull-Read",
            TestFunction::Latency(read_latency_multi),
            TestMemoryConfig::new(
                ExtentMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT },
                ChunkMode::Whole
            ).with_timing(TestTiming::duration_only(10))
             .with_tsc(tsc_freq)
             .with_memory_type(None)
        ),

        (
            "Lat-DRAMFull-Write",
            TestFunction::Latency(write_latency_multi),
            TestMemoryConfig::new(
                ExtentMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT },
                ChunkMode::Whole
            ).with_timing(TestTiming::duration_only(10))
             .with_tsc(tsc_freq)
             .with_memory_type(None)
        ),

        (
            "Lat-DRAMFull-Copy",
            TestFunction::Latency(copy_latency_multi),
            TestMemoryConfig::new(
                ExtentMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT },
                ChunkMode::Whole
            ).with_timing(TestTiming::duration_only(10))
             .with_tsc(tsc_freq)
             .with_memory_type(None)
        ),

        // === V2 Latency Tests (PoC for #24) ===
        // Three new test families exploring better measurement strategies:
        //   Lat-V2-*-Read    — Layout A: 1 chain cell + 1 data cell pair (1 read per chain step)
        //   Lat-V2P-*-Read   — Layout B packed: 1 chain cell + 6 data cells (6 reads per step, MLP)
        //   Lat-NTW-DRAM-Write — NT streaming write saturation (single tier — NT bypasses cache)
        //
        // Each variant registered with -Auto (CPU-best dispatch) plus -128/-256/-512 explicit
        // widths so users can compare widths side-by-side via test= filter, matching the
        // Mem-MirrorV2 / Mem-SimpleV2 convention.

        // Layout A — Lat-V2-Read (5 tiers × {Auto,128,256,512} = 20 tests)
        (
            "Lat-V2-L1-Read-Auto",
            TestFunction::Latency(lat_v2_read_auto_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L1_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-L1-Read-128",
            TestFunction::Latency(lat_v2_read_128_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L1_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-L1-Read-256",
            TestFunction::Latency(lat_v2_read_256_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L1_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-L1-Read-512",
            TestFunction::Latency(lat_v2_read_512_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L1_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-L2-Read-Auto",
            TestFunction::Latency(lat_v2_read_auto_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L2_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-L2-Read-128",
            TestFunction::Latency(lat_v2_read_128_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L2_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-L2-Read-256",
            TestFunction::Latency(lat_v2_read_256_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L2_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-L2-Read-512",
            TestFunction::Latency(lat_v2_read_512_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L2_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-L3-Read-Auto",
            TestFunction::Latency(lat_v2_read_auto_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L3_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-L3-Read-128",
            TestFunction::Latency(lat_v2_read_128_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L3_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-L3-Read-256",
            TestFunction::Latency(lat_v2_read_256_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L3_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-L3-Read-512",
            TestFunction::Latency(lat_v2_read_512_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L3_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-DRAM-Read-Auto",
            TestFunction::Latency(lat_v2_read_auto_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-DRAM-Read-128",
            TestFunction::Latency(lat_v2_read_128_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-DRAM-Read-256",
            TestFunction::Latency(lat_v2_read_256_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-DRAM-Read-512",
            TestFunction::Latency(lat_v2_read_512_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-DRAMFull-Read-Auto",
            TestFunction::Latency(lat_v2_read_auto_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-DRAMFull-Read-128",
            TestFunction::Latency(lat_v2_read_128_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-DRAMFull-Read-256",
            TestFunction::Latency(lat_v2_read_256_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-DRAMFull-Read-512",
            TestFunction::Latency(lat_v2_read_512_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),

        // Layout A Write — Lat-V2-Write (5 tiers × {Auto,128,256,512} = 20 tests)
        // Single SIMD-width cached store per chain step. Baseline: predicted to track
        // Lat-V2-Read at every tier because one in-flight write fits behind any chain
        // miss. Validates that store-buffer pressure requires concurrency density.
        (
            "Lat-V2-L1-Write-Auto",
            TestFunction::Latency(lat_v2_write_auto_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L1_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-L1-Write-128",
            TestFunction::Latency(lat_v2_write_128_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L1_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-L1-Write-256",
            TestFunction::Latency(lat_v2_write_256_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L1_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-L1-Write-512",
            TestFunction::Latency(lat_v2_write_512_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L1_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-L2-Write-Auto",
            TestFunction::Latency(lat_v2_write_auto_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L2_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-L2-Write-128",
            TestFunction::Latency(lat_v2_write_128_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L2_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-L2-Write-256",
            TestFunction::Latency(lat_v2_write_256_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L2_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-L2-Write-512",
            TestFunction::Latency(lat_v2_write_512_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L2_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-L3-Write-Auto",
            TestFunction::Latency(lat_v2_write_auto_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L3_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-L3-Write-128",
            TestFunction::Latency(lat_v2_write_128_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L3_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-L3-Write-256",
            TestFunction::Latency(lat_v2_write_256_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L3_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-L3-Write-512",
            TestFunction::Latency(lat_v2_write_512_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L3_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-DRAM-Write-Auto",
            TestFunction::Latency(lat_v2_write_auto_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-DRAM-Write-128",
            TestFunction::Latency(lat_v2_write_128_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-DRAM-Write-256",
            TestFunction::Latency(lat_v2_write_256_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-DRAM-Write-512",
            TestFunction::Latency(lat_v2_write_512_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-DRAMFull-Write-Auto",
            TestFunction::Latency(lat_v2_write_auto_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-DRAMFull-Write-128",
            TestFunction::Latency(lat_v2_write_128_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-DRAMFull-Write-256",
            TestFunction::Latency(lat_v2_write_256_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-DRAMFull-Write-512",
            TestFunction::Latency(lat_v2_write_512_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),

        // Layout A Copy — Lat-V2-Copy (5 tiers × {Auto,128,256,512} = 20 tests)
        // Single read-modify-write per chain step. Baseline: predicted to track Lat-V2-Read
        // closely because the load+XOR+store dependency chain is short and one in-flight
        // RMW fits behind any chain miss. Validates L1 dependency-chain limit findings.
        (
            "Lat-V2-L1-Copy-Auto",
            TestFunction::Latency(lat_v2_copy_auto_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L1_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-L1-Copy-128",
            TestFunction::Latency(lat_v2_copy_128_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L1_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-L1-Copy-256",
            TestFunction::Latency(lat_v2_copy_256_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L1_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-L1-Copy-512",
            TestFunction::Latency(lat_v2_copy_512_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L1_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-L2-Copy-Auto",
            TestFunction::Latency(lat_v2_copy_auto_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L2_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-L2-Copy-128",
            TestFunction::Latency(lat_v2_copy_128_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L2_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-L2-Copy-256",
            TestFunction::Latency(lat_v2_copy_256_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L2_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-L2-Copy-512",
            TestFunction::Latency(lat_v2_copy_512_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L2_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-L3-Copy-Auto",
            TestFunction::Latency(lat_v2_copy_auto_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L3_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-L3-Copy-128",
            TestFunction::Latency(lat_v2_copy_128_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L3_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-L3-Copy-256",
            TestFunction::Latency(lat_v2_copy_256_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L3_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-L3-Copy-512",
            TestFunction::Latency(lat_v2_copy_512_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L3_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-DRAM-Copy-Auto",
            TestFunction::Latency(lat_v2_copy_auto_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-DRAM-Copy-128",
            TestFunction::Latency(lat_v2_copy_128_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-DRAM-Copy-256",
            TestFunction::Latency(lat_v2_copy_256_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-DRAM-Copy-512",
            TestFunction::Latency(lat_v2_copy_512_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-DRAMFull-Copy-Auto",
            TestFunction::Latency(lat_v2_copy_auto_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-DRAMFull-Copy-128",
            TestFunction::Latency(lat_v2_copy_128_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-DRAMFull-Copy-256",
            TestFunction::Latency(lat_v2_copy_256_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-DRAMFull-Copy-512",
            TestFunction::Latency(lat_v2_copy_512_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),

        // Layout B Packed — Lat-V2P-Read (5 tiers × {Auto,128,256,512} = 20 tests)
        (
            "Lat-V2P-L1-Read-Auto",
            TestFunction::Latency(lat_v2p_read_auto_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L1_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L1-Read-128",
            TestFunction::Latency(lat_v2p_read_128_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L1_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L1-Read-256",
            TestFunction::Latency(lat_v2p_read_256_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L1_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L1-Read-512",
            TestFunction::Latency(lat_v2p_read_512_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L1_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L2-Read-Auto",
            TestFunction::Latency(lat_v2p_read_auto_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L2_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L2-Read-128",
            TestFunction::Latency(lat_v2p_read_128_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L2_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L2-Read-256",
            TestFunction::Latency(lat_v2p_read_256_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L2_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L2-Read-512",
            TestFunction::Latency(lat_v2p_read_512_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L2_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L3-Read-Auto",
            TestFunction::Latency(lat_v2p_read_auto_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L3_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L3-Read-128",
            TestFunction::Latency(lat_v2p_read_128_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L3_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L3-Read-256",
            TestFunction::Latency(lat_v2p_read_256_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L3_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L3-Read-512",
            TestFunction::Latency(lat_v2p_read_512_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L3_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAM-Read-Auto",
            TestFunction::Latency(lat_v2p_read_auto_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAM-Read-128",
            TestFunction::Latency(lat_v2p_read_128_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAM-Read-256",
            TestFunction::Latency(lat_v2p_read_256_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAM-Read-512",
            TestFunction::Latency(lat_v2p_read_512_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAMFull-Read-Auto",
            TestFunction::Latency(lat_v2p_read_auto_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAMFull-Read-128",
            TestFunction::Latency(lat_v2p_read_128_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAMFull-Read-256",
            TestFunction::Latency(lat_v2p_read_256_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAMFull-Read-512",
            TestFunction::Latency(lat_v2p_read_512_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),

        // Layout B Write — Lat-V2P-Write (5 tiers × {Auto,128,256,512} = 20 tests)
        // Path B PoC: 6 SIMD-width cached stores per chain step. Tests whether the store
        // buffer becomes a measurable bottleneck under sustained random write pressure
        // at DRAM tier. If results match Lat-V2P-Read closely, cached writes remain
        // hidden by the store buffer (expected on x86). If DRAM tier rises notably above
        // the Read baseline, store-buffer pressure is visible.
        (
            "Lat-V2P-L1-Write-Auto",
            TestFunction::Latency(lat_v2p_write_auto_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L1_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L1-Write-128",
            TestFunction::Latency(lat_v2p_write_128_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L1_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L1-Write-256",
            TestFunction::Latency(lat_v2p_write_256_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L1_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L1-Write-512",
            TestFunction::Latency(lat_v2p_write_512_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L1_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L2-Write-Auto",
            TestFunction::Latency(lat_v2p_write_auto_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L2_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L2-Write-128",
            TestFunction::Latency(lat_v2p_write_128_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L2_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L2-Write-256",
            TestFunction::Latency(lat_v2p_write_256_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L2_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L2-Write-512",
            TestFunction::Latency(lat_v2p_write_512_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L2_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L3-Write-Auto",
            TestFunction::Latency(lat_v2p_write_auto_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L3_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L3-Write-128",
            TestFunction::Latency(lat_v2p_write_128_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L3_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L3-Write-256",
            TestFunction::Latency(lat_v2p_write_256_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L3_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L3-Write-512",
            TestFunction::Latency(lat_v2p_write_512_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L3_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAM-Write-Auto",
            TestFunction::Latency(lat_v2p_write_auto_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAM-Write-128",
            TestFunction::Latency(lat_v2p_write_128_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAM-Write-256",
            TestFunction::Latency(lat_v2p_write_256_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAM-Write-512",
            TestFunction::Latency(lat_v2p_write_512_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAMFull-Write-Auto",
            TestFunction::Latency(lat_v2p_write_auto_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAMFull-Write-128",
            TestFunction::Latency(lat_v2p_write_128_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAMFull-Write-256",
            TestFunction::Latency(lat_v2p_write_256_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAMFull-Write-512",
            TestFunction::Latency(lat_v2p_write_512_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),

        // Layout B Copy — Lat-V2P-Copy (5 tiers × {Auto,128,256,512} = 20 tests)
        // Read-modify-write: each chain step loads 6 random data cells, XORs them, stores
        // back. Loads warm lines into L1 first, so stores hit cached lines without RFO.
        // Tests realistic "copy traffic" — predicted to fall between Read and Write since
        // the load+store-on-same-line path avoids the store-buffer pressure that pure
        // random writes hit at L2/L3.
        (
            "Lat-V2P-L1-Copy-Auto",
            TestFunction::Latency(lat_v2p_copy_auto_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L1_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L1-Copy-128",
            TestFunction::Latency(lat_v2p_copy_128_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L1_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L1-Copy-256",
            TestFunction::Latency(lat_v2p_copy_256_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L1_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L1-Copy-512",
            TestFunction::Latency(lat_v2p_copy_512_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L1_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L2-Copy-Auto",
            TestFunction::Latency(lat_v2p_copy_auto_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L2_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L2-Copy-128",
            TestFunction::Latency(lat_v2p_copy_128_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L2_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L2-Copy-256",
            TestFunction::Latency(lat_v2p_copy_256_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L2_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L2-Copy-512",
            TestFunction::Latency(lat_v2p_copy_512_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L2_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L3-Copy-Auto",
            TestFunction::Latency(lat_v2p_copy_auto_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L3_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L3-Copy-128",
            TestFunction::Latency(lat_v2p_copy_128_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L3_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L3-Copy-256",
            TestFunction::Latency(lat_v2p_copy_256_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L3_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L3-Copy-512",
            TestFunction::Latency(lat_v2p_copy_512_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L3_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAM-Copy-Auto",
            TestFunction::Latency(lat_v2p_copy_auto_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAM-Copy-128",
            TestFunction::Latency(lat_v2p_copy_128_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAM-Copy-256",
            TestFunction::Latency(lat_v2p_copy_256_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAM-Copy-512",
            TestFunction::Latency(lat_v2p_copy_512_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAMFull-Copy-Auto",
            TestFunction::Latency(lat_v2p_copy_auto_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAMFull-Copy-128",
            TestFunction::Latency(lat_v2p_copy_128_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAMFull-Copy-256",
            TestFunction::Latency(lat_v2p_copy_256_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAMFull-Copy-512",
            TestFunction::Latency(lat_v2p_copy_512_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),

        // Layout B WriteFull — full-cache-line cached writes (5 tiers × {Auto,128,256,512} = 20 tests)
        // Compares to Lat-V2P-Write to isolate the byte-coverage variable. Same chain
        // structure but every width writes the full 64-byte cache line per cell.
        (
            "Lat-V2P-L1-WriteFull-Auto",
            TestFunction::Latency(lat_v2p_write_full_auto_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L1_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L1-WriteFull-128",
            TestFunction::Latency(lat_v2p_write_full_128_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L1_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L1-WriteFull-256",
            TestFunction::Latency(lat_v2p_write_full_256_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L1_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L1-WriteFull-512",
            TestFunction::Latency(lat_v2p_write_full_512_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L1_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L2-WriteFull-Auto",
            TestFunction::Latency(lat_v2p_write_full_auto_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L2_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L2-WriteFull-128",
            TestFunction::Latency(lat_v2p_write_full_128_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L2_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L2-WriteFull-256",
            TestFunction::Latency(lat_v2p_write_full_256_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L2_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L2-WriteFull-512",
            TestFunction::Latency(lat_v2p_write_full_512_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L2_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L3-WriteFull-Auto",
            TestFunction::Latency(lat_v2p_write_full_auto_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L3_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L3-WriteFull-128",
            TestFunction::Latency(lat_v2p_write_full_128_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L3_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L3-WriteFull-256",
            TestFunction::Latency(lat_v2p_write_full_256_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L3_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L3-WriteFull-512",
            TestFunction::Latency(lat_v2p_write_full_512_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L3_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAM-WriteFull-Auto",
            TestFunction::Latency(lat_v2p_write_full_auto_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAM-WriteFull-128",
            TestFunction::Latency(lat_v2p_write_full_128_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAM-WriteFull-256",
            TestFunction::Latency(lat_v2p_write_full_256_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAM-WriteFull-512",
            TestFunction::Latency(lat_v2p_write_full_512_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAMFull-WriteFull-Auto",
            TestFunction::Latency(lat_v2p_write_full_auto_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAMFull-WriteFull-128",
            TestFunction::Latency(lat_v2p_write_full_128_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAMFull-WriteFull-256",
            TestFunction::Latency(lat_v2p_write_full_256_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAMFull-WriteFull-512",
            TestFunction::Latency(lat_v2p_write_full_512_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),

        // Layout B CopyFull — full-cache-line RMW (5 tiers × {Auto,128,256,512} = 20 tests)
        // Compares to Lat-V2P-Copy to isolate the byte-coverage variable. Tests whether
        // the L3 bimodal distribution is byte-coverage driven or SIMD-path driven.
        (
            "Lat-V2P-L1-CopyFull-Auto",
            TestFunction::Latency(lat_v2p_copy_full_auto_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L1_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L1-CopyFull-128",
            TestFunction::Latency(lat_v2p_copy_full_128_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L1_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L1-CopyFull-256",
            TestFunction::Latency(lat_v2p_copy_full_256_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L1_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L1-CopyFull-512",
            TestFunction::Latency(lat_v2p_copy_full_512_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L1_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L2-CopyFull-Auto",
            TestFunction::Latency(lat_v2p_copy_full_auto_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L2_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L2-CopyFull-128",
            TestFunction::Latency(lat_v2p_copy_full_128_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L2_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L2-CopyFull-256",
            TestFunction::Latency(lat_v2p_copy_full_256_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L2_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L2-CopyFull-512",
            TestFunction::Latency(lat_v2p_copy_full_512_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L2_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L3-CopyFull-Auto",
            TestFunction::Latency(lat_v2p_copy_full_auto_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L3_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L3-CopyFull-128",
            TestFunction::Latency(lat_v2p_copy_full_128_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L3_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L3-CopyFull-256",
            TestFunction::Latency(lat_v2p_copy_full_256_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L3_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L3-CopyFull-512",
            TestFunction::Latency(lat_v2p_copy_full_512_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::L3_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAM-CopyFull-Auto",
            TestFunction::Latency(lat_v2p_copy_full_auto_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAM-CopyFull-128",
            TestFunction::Latency(lat_v2p_copy_full_128_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAM-CopyFull-256",
            TestFunction::Latency(lat_v2p_copy_full_256_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAM-CopyFull-512",
            TestFunction::Latency(lat_v2p_copy_full_512_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAMFull-CopyFull-Auto",
            TestFunction::Latency(lat_v2p_copy_full_auto_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAMFull-CopyFull-128",
            TestFunction::Latency(lat_v2p_copy_full_128_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAMFull-CopyFull-256",
            TestFunction::Latency(lat_v2p_copy_full_256_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAMFull-CopyFull-512",
            TestFunction::Latency(lat_v2p_copy_full_512_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),

        // NT Write saturation — Lat-NTW-DRAM-Write ({Auto,Scalar,128,256,512} = 5 tests)
        // NT stores bypass cache hierarchy — only the DRAM tier exists. Width variants exposed
        // because per-store commit time differs (wider stores fill cache lines in fewer ops).
        // Scalar variant uses 8-byte MOVNTI to validate the WCB-fill-ratio hypothesis.
        (
            "Lat-NTW-DRAM-Write-Auto",
            TestFunction::Latency(lat_ntw_write_auto_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-NTW-DRAM-Write-Scalar",
            TestFunction::Latency(lat_ntw_write_scalar_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-NTW-DRAM-Write-128",
            TestFunction::Latency(lat_ntw_write_128_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-NTW-DRAM-Write-256",
            TestFunction::Latency(lat_ntw_write_256_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-NTW-DRAM-Write-512",
            TestFunction::Latency(lat_ntw_write_512_multi),
            TestMemoryConfig::new(ExtentMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Whole)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),

        // === Sequential Bandwidth Tests (Auto-dispatch) ===
        // Measures peak sequential memory bandwidth (unlike latency tests which use random access)
        // Naming: Spd-{Level}-{Operation}-Auto → resolves to Spd-{Level}-{Operation}-{128,256,512}_A
        // Uses ExtentMode::Cache for automatic sizing based on detected cache
        // tests=Spd-* (all 15 bandwidth tests: L1/L2/L3/DRAMSmall/DRAMFull × Read/Write/Copy)
        // tests=Spd-L3-* (3 tests: L3 × Read/Write/Copy)
        // tests=Spd-DRAM* (6 tests: DRAMSmall + DRAMFull)

        // L1 Cache Bandwidth - Read, Write, Copy
        (
            "Spd-L1-Read-Auto",
            TestFunction::MultiBlock(spd_read_auto_multi),
            TestMemoryConfig::new(
                ExtentMode::Cache { target: CacheTarget::L1_DEFAULT },
                ChunkMode::Whole
            ).with_timing(TestTiming::duration_only(10))
             .with_memory_type(None)
        ),

        (
            "Spd-L1-Write-Auto",
            TestFunction::MultiBlock(spd_write_auto_multi),
            TestMemoryConfig::new(
                ExtentMode::Cache { target: CacheTarget::L1_DEFAULT },
                ChunkMode::Whole
            ).with_timing(TestTiming::duration_only(10))
             .with_memory_type(None)
        ),

        (
            "Spd-L1-Copy-Auto",
            TestFunction::MultiBlock(spd_copy_auto_multi),
            TestMemoryConfig::new(
                ExtentMode::Cache { target: CacheTarget::L1_DEFAULT },
                ChunkMode::Whole
            ).with_timing(TestTiming::duration_only(10))
             .with_memory_type(None)
        ),

        // L2 Cache Bandwidth - Read, Write, Copy
        (
            "Spd-L2-Read-Auto",
            TestFunction::MultiBlock(spd_read_auto_multi),
            TestMemoryConfig::new(
                ExtentMode::Cache { target: CacheTarget::L2_DEFAULT },
                ChunkMode::Whole
            ).with_timing(TestTiming::duration_only(10))
             .with_memory_type(None)
        ),

        (
            "Spd-L2-Write-Auto",
            TestFunction::MultiBlock(spd_write_auto_multi),
            TestMemoryConfig::new(
                ExtentMode::Cache { target: CacheTarget::L2_DEFAULT },
                ChunkMode::Whole
            ).with_timing(TestTiming::duration_only(10))
             .with_memory_type(None)
        ),

        (
            "Spd-L2-Copy-Auto",
            TestFunction::MultiBlock(spd_copy_auto_multi),
            TestMemoryConfig::new(
                ExtentMode::Cache { target: CacheTarget::L2_DEFAULT },
                ChunkMode::Whole
            ).with_timing(TestTiming::duration_only(10))
             .with_memory_type(None)
        ),

        // L3 Cache Bandwidth - Read, Write, Copy
        (
            "Spd-L3-Read-Auto",
            TestFunction::MultiBlock(spd_read_auto_multi),
            TestMemoryConfig::new(
                ExtentMode::Cache { target: CacheTarget::L3_DEFAULT },
                ChunkMode::Whole
            ).with_timing(TestTiming::duration_only(10))
             .with_memory_type(None)
        ),

        (
            "Spd-L3-Write-Auto",
            TestFunction::MultiBlock(spd_write_auto_multi),
            TestMemoryConfig::new(
                ExtentMode::Cache { target: CacheTarget::L3_DEFAULT },
                ChunkMode::Whole
            ).with_timing(TestTiming::duration_only(10))
             .with_memory_type(None)
        ),

        (
            "Spd-L3-Copy-Auto",
            TestFunction::MultiBlock(spd_copy_auto_multi),
            TestMemoryConfig::new(
                ExtentMode::Cache { target: CacheTarget::L3_DEFAULT },
                ChunkMode::Whole
            ).with_timing(TestTiming::duration_only(10))
             .with_memory_type(None)
        ),

        // DRAMSmall Bandwidth - Smaller working set, high TLB hit rate - Read, Write, Copy
        (
            "Spd-DRAMSmall-Read-Auto",
            TestFunction::MultiBlock(spd_read_auto_multi),
            TestMemoryConfig::new(
                ExtentMode::Cache { target: CacheTarget::DRAM_DEFAULT },
                ChunkMode::Whole
            ).with_timing(TestTiming::duration_only(10))
             .with_memory_type(None)
        ),

        (
            "Spd-DRAMSmall-Write-Auto",
            TestFunction::MultiBlock(spd_write_nt_auto_multi),
            TestMemoryConfig::new(
                ExtentMode::Cache { target: CacheTarget::DRAM_DEFAULT },
                ChunkMode::Whole
            ).with_timing(TestTiming::duration_only(10))
             .with_memory_type(None)
        ),

        (
            "Spd-DRAMSmall-Copy-Auto",
            TestFunction::MultiBlock(spd_copy_nt_auto_multi),
            TestMemoryConfig::new(
                ExtentMode::Cache { target: CacheTarget::DRAM_DEFAULT },
                ChunkMode::Whole
            ).with_timing(TestTiming::duration_only(10))
             .with_memory_type(None)
        ),

        // DRAMFull Bandwidth - Full allocation, includes TLB miss overhead - Read, Write, Copy
        (
            "Spd-DRAMFull-Read-Auto",
            TestFunction::MultiBlock(spd_read_auto_multi),
            TestMemoryConfig::new(
                ExtentMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT },
                ChunkMode::Whole
            ).with_timing(TestTiming::duration_only(10))
             .with_memory_type(None)
        ),

        (
            "Spd-DRAMFull-Write-Auto",
            TestFunction::MultiBlock(spd_write_nt_auto_multi),
            TestMemoryConfig::new(
                ExtentMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT },
                ChunkMode::Whole
            ).with_timing(TestTiming::duration_only(10))
             .with_memory_type(None)
        ),

        (
            "Spd-DRAMFull-Copy-Auto",
            TestFunction::MultiBlock(spd_copy_nt_auto_multi),
            TestMemoryConfig::new(
                ExtentMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT },
                ChunkMode::Whole
            ).with_timing(TestTiming::duration_only(10))
             .with_memory_type(None)
        ),
    ];
    
    // The seal's fill and check, per pattern and width (TODO 74), set up as the Bench pairs above
    let bench_at = test_definitions.iter().rposition(|(name, _, _)| name.starts_with("Bench-Verify-")).map_or(test_definitions.len(), |i| i + 1);
    let mut test_definitions = test_definitions;
    let seal_benches = crate::tests::bench_seal_tests().map(|(name, function)| {
        let config = TestMemoryConfig::new(ExtentMode::FullAllocation, ChunkMode::Absolute { size_bytes: 4 * MB })
            .with_timing(TestTiming::cycles_only(10))
            .with_skip_init(name.starts_with("Bench-Init-"))
            .with_memory_type(None);
        (name, function, config)
    });
    test_definitions.splice(bench_at..bench_at, seal_benches);

    // The native families' write and verify alone (pattern spec v2), after the seal's: each family's
    // Init then its Verify, which fills itself and times only its verifies (TODO 90)
    let bench_at = test_definitions.iter().rposition(|(name, _, _)| name.starts_with("Bench-Verify-")).map_or(test_definitions.len(), |i| i + 1);
    let native_benches = crate::tests::native_bench_tests().into_iter().flat_map(|(init, verify, fi, fv)| {
        let config = |skip_init| TestMemoryConfig::new(ExtentMode::FullAllocation, ChunkMode::Absolute { size_bytes: 4 * MB })
            .with_timing(TestTiming::cycles_only(10))
            .with_skip_init(skip_init)
            .with_memory_type(None);
        [(init, TestFunction::MultiBlock(fi), config(true)), (verify, TestFunction::MultiBlock(fv), config(false))]
    }).collect::<Vec<_>>();
    test_definitions.splice(bench_at..bench_at, native_benches);

    // Process test definitions and resolve auto-dispatch tests
    let mut resolved_tests = Vec::new();
    for (test_name, test_function, config) in test_definitions {
        if let Some((resolved_name, resolved_function)) = resolve_auto_dispatch_test(test_name) {
            log::debug!("Auto-dispatch: {} → {} (based on CPU capabilities)", test_name, resolved_name);
            let effect = derive_memory_effect(resolved_name, &config);
            let display_name = format!("{}_A", resolved_name); // Add _A suffix for auto-dispatch
            resolved_tests.push(TestDefinition {
                actual_name: resolved_name,
                // Mirror onto the config so per-thread logs show the resolved+suffixed name
                config: config.with_display_name(&display_name),
                display_name,
                function: resolved_function,
                original_name: Some(test_name), // Preserve original "StuckBitTestAuto" name
                memory_effect: effect,
                test_index: resolved_tests.len(),
                id: None,
                seal_use: seal_use(resolved_name),
                seal_opt_out: false,
            });
        } else {
            let effect = derive_memory_effect(test_name, &config);
            resolved_tests.push(TestDefinition {
                actual_name: test_name,
                display_name: test_name.to_string(),
                function: test_function,
                config: config.with_display_name(test_name),
                original_name: None,
                memory_effect: effect,
                test_index: resolved_tests.len(),
                id: None,
                seal_use: seal_use(test_name),
                seal_opt_out: false,
            });
        }
    }

    resolved_tests
}

/// `cycle_order` as written: each id, with whether it runs (a disabled test's doesn't)
type CycleOrder = Vec<(String, bool)>;

/// A config's steps (TODO 74): its tests in `cycle_order`, or each enabled test once in file
/// order. Returns them with `cycle_order` as written (each id with whether it runs), if it has one.
///
/// A config can name any test the built-in suite has, latency and bandwidth tests included
/// (TODO 94), by its name or its `-Auto` name, plus the aliases `get_test_function_by_name` knows.
fn create_test_definitions_from_config(config: &crate::config::ModernConfig, cache_info: &CacheInfo) -> Result<(Vec<TestDefinition>, Option<CycleOrder>), String> {
    let plan = config.plan()?;
    let built_in = create_test_definitions(cache_info);
    let mut tests = Vec::new();
    for (test_index, test) in plan.tests.into_iter().enumerate() {
        let test_name = test.function;
        // Look up function by name: the aliases first, then the built-in suite
        let test_function = crate::tests::get_test_function_by_name(test_name)
            .or_else(|| built_in.iter()
                .find(|d| d.actual_name == test_name || d.original_name == Some(test_name))
                .map(|d| d.function.clone()))
            .ok_or_else(|| format!("Unknown test function '{}' in config", test_name))?;
        let id = test.id.map(str::to_string);

        // Check for auto-dispatch
        let def = if let Some((resolved_name, resolved_function)) = resolve_auto_dispatch_test(test_name) {
            log::debug!("Auto-dispatch: {} → {} (based on CPU capabilities)", test_name, resolved_name);
            // Convert to 'static str by leaking (safe for test names, small and finite set)
            let static_original_name: &'static str = Box::leak(test_name.to_string().into_boxed_str());
            let effect = derive_memory_effect(resolved_name, &test.config);
            let display_name = format!("{}_A", resolved_name);
            TestDefinition {
                actual_name: resolved_name,
                // Mirror onto the config so per-thread logs show the resolved+suffixed name
                config: test.config.with_display_name(&display_name),
                display_name,
                function: resolved_function,
                original_name: Some(static_original_name), // Preserve original Auto name
                memory_effect: effect,
                test_index,
                id,
                seal_use: seal_use(resolved_name),
                seal_opt_out: test.seal_opt_out,
            }
        } else {
            // Convert to 'static str by leaking (safe for test names, small and finite set)
            let static_name: &'static str = Box::leak(test_name.to_string().into_boxed_str());
            let effect = derive_memory_effect(test_name, &test.config);
            TestDefinition {
                actual_name: static_name,
                display_name: test_name.to_string(),
                function: test_function,
                config: test.config.with_display_name(test_name),
                original_name: None,
                memory_effect: effect,
                test_index,
                id,
                seal_use: seal_use(test_name),
                seal_opt_out: test.seal_opt_out,
            }
        };
        tests.push(def);
    }

    let steps: Vec<TestDefinition> = plan.steps.iter().map(|&t| tests[t].clone()).collect();
    if steps.is_empty() {
        return Err("No enabled tests found in configuration".to_string());
    }
    Ok((steps, plan.order))
}

// Re-export RuntimeConfig from lib.rs instead of redefining
// RuntimeConfig is already defined in lib.rs

// Detect runtime capabilities
pub fn detect_runtime_capabilities(alloc_config: &MemoryAllocationConfig) -> RuntimeConfig {
    // Always large-page capable, even without the privilege: `WindowsBackend` itself
    // decides per-allocation whether large pages are usable and falls back, whereas building it
    // with `large_pages: false` would turn that soft fallback into a hard "backend doesn't
    // support large pages" error. `large_pages_available` is reported separately for display.
    let memory_backend = MemoryBackend::VirtualAlloc2;

    let large_pages_available = crate::memory::privileges::check_large_page_privilege().is_ok();

    log::info!("Runtime capabilities detected: backend={:?}, large_pages_available={}",
               memory_backend, large_pages_available);

    RuntimeConfig {
        memory_backend,
        large_pages_available,
        cpu_list: {
            // Every CPU by default, in TMR's numbering (group * 64 + bit: not 0..n once a
            // processor group is short of 64)
            Some(crate::cpu_topology::logical_cpu_ids())
        },
        pin_threads: true,
        enhanced_memory_strategy: crate::memory::allocation_strategy::EnhancedMemoryStrategy::default(),
        memory_allocation: alloc_config.clone(),
    }
}

// CPU pinning functions needed by thread_pool
pub fn pin_thread_to_cpu_with_config(thread_id: usize, cpu_id: usize, enable: bool) -> Result<usize, String> {
    if !enable {
        return Ok(cpu_id);
    }
    
    use windows::Win32::System::Threading::{
        SetThreadGroupAffinity, SetThreadIdealProcessorEx, GetCurrentThread, GetCurrentProcessorNumberEx
    };
    use windows::Win32::System::Kernel::PROCESSOR_NUMBER;
    use windows::Win32::System::SystemInformation::GROUP_AFFINITY;

    unsafe {
        let thread_handle = GetCurrentThread();

        // Method 1: hard pinning, in the CPU's own processor group. TMR numbers CPUs
        // group * 64 + bit (cpu_topology), and SetThreadAffinityMask only reaches the thread's
        // current group: above CPU 63 its 1 << cpu wrapped, stacking workers on group 0's CPUs.
        let target = GROUP_AFFINITY { Mask: 1usize << (cpu_id % 64), Group: (cpu_id / 64) as u16, Reserved: [0; 3] };
        if !SetThreadGroupAffinity(thread_handle, &target, None).as_bool() {
            return Err(format!("SetThreadGroupAffinity failed for thread {} CPU {} (group {}, bit {})",
                               thread_id, cpu_id, cpu_id / 64, cpu_id % 64));
        }
        // Check where the thread now runs: once per worker, and the only proof the pinning held
        std::thread::yield_now();
        let now = GetCurrentProcessorNumberEx();
        let running_on = now.Group as usize * 64 + now.Number as usize;
        if running_on != cpu_id {
            log::warn!("Thread {} pinned to CPU {} but running on CPU {} (group {}, number {})",
                       thread_id, cpu_id, running_on, now.Group, now.Number);
        }
        
        // Method 2: Set ideal processor (soft preference)
        let processor = PROCESSOR_NUMBER {
            Group: (cpu_id / 64) as u16,  // Processor group (for systems with >64 CPUs)
            Number: (cpu_id % 64) as u8,  // Processor number within group
            Reserved: 0,
        };
        
        match SetThreadIdealProcessorEx(thread_handle, &processor, None) {
            Ok(_) => {
                log::debug!("Thread {} successfully pinned to CPU {} (Group: {}, Number: {})", 
                    thread_id, cpu_id, processor.Group, processor.Number);
                Ok(cpu_id)
            },
            Err(e) => {
                log::warn!("SetThreadIdealProcessorEx failed for thread {} CPU {}: {:?}, but the group affinity succeeded",
                    thread_id, cpu_id, e);
                Ok(cpu_id) // Affinity mask succeeded, so still return success
            }
        }
    }
}

pub fn set_thread_ideal_processor_ex(_thread_handle: windows::Win32::Foundation::HANDLE, cpu_id: u32) -> Result<(), String> {
    // Simplified implementation - SetThreadIdealProcessorEx requires PROCESSOR_NUMBER which isn't easily available
    // For now, just log and return success since SetThreadAffinityMask already does the pinning
    log::debug!("Ideal processor set to CPU {}", cpu_id);
    Ok(())
}

/// Scheduling priority of the worker threads, set by each worker on itself at pool creation.
///
/// Relative to TMR's process class, which it leaves at `NORMAL_PRIORITY_CLASS`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ThreadPriority {
    #[expect(dead_code, reason = "no config key selects it yet; see TODO #77 A")]
    Normal,
    #[default]
    High,
    /// `THREAD_PRIORITY_TIME_CRITICAL`: base priority 15, the top of the normal range. Not the
    /// realtime priority *class* (16-31). With a worker on every logical CPU it can still starve
    /// the console and input, so it suits runs that leave cores free.
    #[expect(dead_code, reason = "no config key selects it yet; see TODO #77 A")]
    Realtime,
}

impl ThreadPriority {
    fn win32(self) -> (windows::Win32::System::Threading::THREAD_PRIORITY, &'static str) {
        use windows::Win32::System::Threading::{
            THREAD_PRIORITY_HIGHEST, THREAD_PRIORITY_NORMAL, THREAD_PRIORITY_TIME_CRITICAL,
        };
        match self {
            Self::Normal => (THREAD_PRIORITY_NORMAL, "THREAD_PRIORITY_NORMAL"),
            Self::High => (THREAD_PRIORITY_HIGHEST, "THREAD_PRIORITY_HIGHEST"),
            Self::Realtime => (THREAD_PRIORITY_TIME_CRITICAL, "THREAD_PRIORITY_TIME_CRITICAL"),
        }
    }

    /// The Win32 level this maps to, for the log.
    pub fn win32_name(self) -> &'static str {
        self.win32().1
    }

    /// Apply to the calling thread.
    pub fn apply(self) -> Result<(), String> {
        use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority};

        unsafe { SetThreadPriority(GetCurrentThread(), self.win32().0) }
            .map_err(|e| format!("SetThreadPriority({}) failed: {e}", self.win32_name()))
    }
}

impl std::fmt::Display for ThreadPriority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Normal => "normal",
            Self::High => "high",
            Self::Realtime => "realtime",
        })
    }
}

/// Convert latency test results to LatencyLevelSummary for reporting
fn convert_to_latency_level_summary(
    latency_results: &[(usize, usize, LatencyTestStats)],
) -> LatencyLevelSummary {
    // Collect per-thread results
    let mut per_thread_results = Vec::new();
    let mut all_latencies = Vec::new();
    let mut total_samples = 0;

    for (thread_id, cpu_id, lat_stats) in latency_results {
        per_thread_results.push(LatencyThreadResult {
            thread_id: *thread_id,
            cpu_id: *cpu_id,
            sample_count: lat_stats.sample_count,
            percentiles: LatencyPercentiles {
                min_ns: lat_stats.latencies_ns.iter().min_by(|a, b| a.partial_cmp(b).unwrap()).copied().unwrap_or(0.0),
                p5_ns: lat_stats.p5_ns,
                p10_ns: lat_stats.p10_ns,
                p25_ns: lat_stats.p25_ns,
                p50_ns: lat_stats.p50_ns,
                p75_ns: lat_stats.p75_ns,
                p90_ns: lat_stats.p90_ns,
                p95_ns: lat_stats.p95_ns,
                p99_ns: lat_stats.p99_ns,
                p99_9_ns: lat_stats.p99_9_ns,
                spread_ratio: lat_stats.spread_ratio,
            },
        });

        all_latencies.extend(&lat_stats.latencies_ns);
        total_samples += lat_stats.sample_count;
    }

    // Calculate consolidated percentiles
    let consolidated = if !all_latencies.is_empty() {
        // Sort latencies (safe to unwrap - timing values are never NaN)
        all_latencies.sort_by(|a: &f64, b: &f64| a.partial_cmp(b).unwrap());
        let len = all_latencies.len();

        let percentile = |p: f64| -> f64 {
            let idx = ((len as f64 - 1.0) * p / 100.0) as usize;
            all_latencies[idx.min(len - 1)]
        };

        LatencyPercentiles {
            min_ns: all_latencies[0],
            p5_ns: percentile(5.0),
            p10_ns: percentile(10.0),
            p25_ns: percentile(25.0),
            p50_ns: percentile(50.0),
            p75_ns: percentile(75.0),
            p90_ns: percentile(90.0),
            p95_ns: percentile(95.0),
            p99_ns: percentile(99.0),
            p99_9_ns: percentile(99.9),
            spread_ratio: {
                let p5 = percentile(5.0);
                let p95 = percentile(95.0);
                if p5 > 0.0 { p95 / p5 } else { 0.0 }
            },
        }
    } else {
        LatencyPercentiles {
            min_ns: 0.0, p5_ns: 0.0, p10_ns: 0.0, p25_ns: 0.0,
            p50_ns: 0.0, p75_ns: 0.0, p90_ns: 0.0, p95_ns: 0.0, p99_ns: 0.0,
            p99_9_ns: 0.0, spread_ratio: 0.0,
        }
    };

    LatencyLevelSummary {
        total_samples,
        per_thread_results,
        consolidated,
    }
}

/// Every thread's share, from the stitched allocator: one contiguous VA span per thread.
fn allocate_all_blocks_new(thread_blocks: &HashMap<usize, Vec<BlockInfo>>, runtime_config: &RuntimeConfig) -> Result<crate::memory::stitched::StitchedAllocation, String> {
    match runtime_config.memory_backend {
        MemoryBackend::VirtualAlloc2 => crate::memory::stitched::chunk_allocate_stitched(thread_blocks, runtime_config),
    }
}

fn print_detailed_cpu_performance_summary(
    final_stats: &TestCpuStats,
    cpu_assignments: &[CpuAssignment],  // (thread_id, logical_cpu, numa_node)
    _suite_duration: std::time::Duration,
    whea: crate::whea::WheaCounts,
    pinned: bool,
) {
    use crate::reporting::{Reporter, models::*, formatters::DefaultFormatter, renderers::ConsoleRenderer};
    use crate::cpu_topology::get_cpu_topology;
    use std::collections::HashMap;

    println!("\n=== CPU Performance Summary ===");

    // Get CPU topology for physical core ID lookup
    let topology = get_cpu_topology();

    // Aggregate stats across ALL tests by thread_id
    let mut thread_aggregates: HashMap<usize, (u64, std::time::Duration, u64)> = HashMap::new();  // (bytes, time, errors)

    for stats in final_stats.values() {
        for &(thread_id, _cpu_id, bytes, time, errors, _operations, _cycles) in stats {
            let entry = thread_aggregates.entry(thread_id).or_default();
            entry.0 += bytes;
            entry.1 += time;
            entry.2 += errors;
        }
    }

    // Build PerformanceByThreadReport
    let mut threads = Vec::new();
    for &(thread_id, cpu_id, numa_node) in cpu_assignments {
        if let Some(&(total_bytes, total_time, total_errors)) = thread_aggregates.get(&thread_id) {
            // Look up physical core ID from topology
            let physical_core_id = topology.iter()
                .find(|cpu| cpu.logical_id == cpu_id)
                .map(|cpu| cpu.physical_core_id)
                .unwrap_or(cpu_id);  // Fallback to logical ID if not found

            let throughput_mib_s = if !total_time.is_zero() {
                (total_bytes as f64 / (1024.0 * 1024.0)) / total_time.as_secs_f64()
            } else {
                0.0
            };

            threads.push(ThreadPerformance {
                thread_id,
                cpu_id,
                physical_core_id,
                numa_node,
                total_time_ms: total_time.as_millis(),
                total_bytes,
                throughput_mib_s,
                total_errors,
            });
        }
    }
    threads.sort_by_key(|t| t.thread_id);

    // Build PerformanceByCpuReport (aggregate by logical CPU)
    let mut cpu_aggregates: HashMap<usize, (usize, u32, u64, u128, u64)> = HashMap::new();  // (physical_core, numa, bytes, time, errors)
    for thread in &threads {
        let entry = cpu_aggregates.entry(thread.cpu_id).or_insert((thread.physical_core_id, thread.numa_node, 0, 0, 0));
        entry.2 += thread.total_bytes;
        entry.3 += thread.total_time_ms;
        entry.4 += thread.total_errors;
    }

    let mut cpus: Vec<CpuPerformance> = cpu_aggregates.iter().map(|(&cpu_id, &(physical_core_id, numa_node, total_bytes, total_time_ms, total_errors))| {
        let throughput_mib_s = if total_time_ms > 0 {
            (total_bytes as f64 / (1024.0 * 1024.0)) / (total_time_ms as f64 / 1000.0)
        } else {
            0.0
        };
        CpuPerformance {
            cpu_id,
            physical_core_id,
            numa_node,
            total_time_ms,
            total_bytes,
            throughput_mib_s,
            total_errors,
        }
    }).collect();
    cpus.sort_by_key(|c| c.cpu_id);

    // Build PerformanceByPhysicalCoreReport (aggregate by physical core)
    let mut core_aggregates: HashMap<usize, (Vec<usize>, u64, u128, u64)> = HashMap::new();  // (logical_cpus, bytes, time, errors)
    for cpu in &cpus {
        let entry = core_aggregates.entry(cpu.physical_core_id).or_insert((Vec::new(), 0, 0, 0));
        if !entry.0.contains(&cpu.cpu_id) {
            entry.0.push(cpu.cpu_id);
        }
        entry.1 += cpu.total_bytes;
        entry.2 += cpu.total_time_ms;
        entry.3 += cpu.total_errors;
    }

    let mut cores: Vec<PhysicalCorePerformance> = core_aggregates.iter().map(|(&core_id, (logical_cpus, total_bytes, total_time_ms, total_errors))| {
        let throughput_mib_s = if *total_time_ms > 0 {
            (*total_bytes as f64 / (1024.0 * 1024.0)) / (*total_time_ms as f64 / 1000.0)
        } else {
            0.0
        };
        let mut logical_cpus_sorted = logical_cpus.clone();
        logical_cpus_sorted.sort();
        PhysicalCorePerformance {
            core_id,
            logical_cpus: logical_cpus_sorted,
            total_time_ms: *total_time_ms,
            total_bytes: *total_bytes,
            throughput_mib_s,
            total_errors: *total_errors,
        }
    }).collect();
    cores.sort_by_key(|c| c.core_id);

    // Display all reports using the reporting system (with variance columns built-in)
    let mut reporter = Reporter::new(Box::new(DefaultFormatter::new()), ConsoleRenderer::new());

    // All three breakdowns carry the same run-wide WHEA figure: the events cannot be attributed to a
    // thread, CPU or core, so each view reports the run total on its aggregate row rather than
    // dividing something indivisible.
    if !threads.is_empty() {
        let report = PerformanceByThreadReport {
            threads,
            whea,
            pinned,
        };
        let _ = reporter.report_performance_by_thread(&report);
    }

    if !cpus.is_empty() {
        let report = PerformanceByCpuReport {
            cpus,
            whea,
        };
        let _ = reporter.report_performance_by_cpu(&report);
    }

    if !cores.is_empty() {
        let report = PerformanceByPhysicalCoreReport {
            cores,
            whea,
        };
        let _ = reporter.report_performance_by_physical_core(&report);
    }
}

fn display_and_save_results(
    test_run_result: &Arc<Mutex<TestRunResult>>,
    suite_duration: std::time::Duration,
    whea_totals: crate::whea::WheaCounts,
    whea_monitored: bool,
) {
    let mut result = test_run_result.lock().unwrap();
    result.finalize(suite_duration);
    // Run-level WHEA figures come from the monitor's cumulative counters, not from summing the
    // per-test deltas: a test that never started (early exit) or an event logged between tests
    // would otherwise be dropped from the total.
    result.set_whea_totals(whea_totals, whea_monitored);

    // Display final summary using reporting layer
    {
        use crate::reporting::{create_console_reporter, converters};
        let report = converters::create_overall_stats_summary_report(&result);
        let mut reporter = create_console_reporter();
        if let Err(e) = reporter.report_final_summary(&report) {
            log::error!("Failed to display final summary: {}", e);
        }
    }

    // Save results to file
    let filename = result.get_filename();
    match result.save_to_file(filename) {
        Ok(_) => println!("💾 Results saved: {}", filename),
        Err(e) => log::error!("Failed to save results: {}", e),
    }
}

pub fn print_current_memory_status() {
    use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    use crate::table::{TableBuilder, Alignment};
    
    unsafe {
        let mut mem_status = MEMORYSTATUSEX {
            dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
            ..Default::default()
        };
        
        if GlobalMemoryStatusEx(&mut mem_status).is_ok() {
            println!("📊 Current System Memory Status:");
            
            let total_phys_gib = mem_status.ullTotalPhys as f64 / (1024.0 * 1024.0 * 1024.0);
            let avail_phys_gib = mem_status.ullAvailPhys as f64 / (1024.0 * 1024.0 * 1024.0);
            let total_page_gib = mem_status.ullTotalPageFile as f64 / (1024.0 * 1024.0 * 1024.0);
            let avail_page_gib = mem_status.ullAvailPageFile as f64 / (1024.0 * 1024.0 * 1024.0);
            let phys_percent = (mem_status.ullAvailPhys as f64 / mem_status.ullTotalPhys as f64) * 100.0;
            
            let table = TableBuilder::new()
                .add_header("Memory Type", Alignment::Left)
                .add_header("Total", Alignment::Right)
                .add_header("Available", Alignment::Right)
                .add_header("Used", Alignment::Right)
                .add_header("% Available", Alignment::Right)
                .add_row(vec![
                    "Physical".to_string(),
                    format!("{:.2} GiB", total_phys_gib),
                    format!("{:.2} GiB", avail_phys_gib),
                    format!("{:.2} GiB", total_phys_gib - avail_phys_gib),
                    format!("{:.1}%", phys_percent),
                ])
                .add_row(vec![
                    "Page File".to_string(),
                    format!("{:.2} GiB", total_page_gib),
                    format!("{:.2} GiB", avail_page_gib),
                    format!("{:.2} GiB", total_page_gib - avail_page_gib),
                    format!("{:.1}%", (avail_page_gib / total_page_gib) * 100.0),
                ]);
            
            table.print();
            println!();
        } else {
            eprintln!("⚠️ Failed to retrieve system memory status");
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::allocator::AllocationConfig;
    use crate::memory::backend::{AllocError, Backend, BackendAllocation};
    use crate::memory::buffer::{BufferInfo, PageType};

    /// Blocks over a heap buffer the test owns: nothing to allocate or free.
    #[derive(Debug)]
    struct HeapView;
    impl Backend for HeapView {
        fn allocate(&self, _: &AllocationConfig) -> Result<BackendAllocation, AllocError> {
            unreachable!("test blocks are built directly")
        }
        fn free(&self, _: BackendAllocation) -> Result<(), String> { Ok(()) }
        fn name(&self) -> &'static str { "heap" }
    }

    const KIB: usize = 1024;
    const GUARD: usize = 64 * KIB;
    const FILL: u8 = 0xA5;
    /// A word no test pattern writes, to find extent words a test never touched
    const UNTOUCHED: u64 = 0x5AF0_0FA5_C3D2_1E87;

    /// A heap buffer with guard zones on both sides, cut into adjacent blocks of `sizes`: one span.
    struct Region {
        mem: *mut u8,
        layout: std::alloc::Layout,
        len: usize,
        blocks: Vec<AllocationBlock>,
    }

    impl Region {
        fn new(sizes: &[usize]) -> Self {
            let len: usize = sizes.iter().sum();
            let layout = std::alloc::Layout::from_size_align(GUARD + len + GUARD, 4096).unwrap();
            let mem = unsafe { std::alloc::alloc(layout) };
            assert!(!mem.is_null());
            unsafe { std::ptr::write_bytes(mem, FILL, layout.size()) };
            let backend: Arc<dyn Backend> = Arc::new(HeapView);
            let mut at = GUARD;
            let blocks = sizes.iter().map(|&size| {
                let block = AllocationBlock {
                    buffer: MemoryBuffer::new(
                        BackendAllocation {
                            ptr: unsafe { mem.add(at) },
                            size,
                            info: BufferInfo { numa_node: 0, page_type: PageType::Large(size) },
                        },
                        backend.clone(),
                    ),
                };
                at += size;
                block
            }).collect();
            Region { mem, layout, len, blocks }
        }

        fn words(&mut self) -> &mut [u64] {
            unsafe { std::slice::from_raw_parts_mut(self.mem.add(GUARD) as *mut u64, self.len / 8) }
        }

        fn guards_intact(&self) -> bool {
            let bytes = unsafe { std::slice::from_raw_parts(self.mem, self.layout.size()) };
            bytes[..GUARD].iter().chain(&bytes[GUARD + self.len..]).all(|&b| b == FILL)
        }
    }

    impl Drop for Region {
        fn drop(&mut self) {
            self.blocks.clear();
            unsafe { std::alloc::dealloc(self.mem, self.layout) };
        }
    }

    /// The built-in suite, set up to run once on a test region (one thread, 64 B lines)
    fn suite() -> Vec<(TestDefinition, TestFunctionMultiBlock)> {
        let cache_info = crate::tests::get_cache_info();
        let avx512 = is_x86_feature_detected!("avx512f");
        create_test_definitions(cache_info).into_iter().filter_map(|mut def| {
            // The latency tests return other stats: `latency_tests_run_in_place_on_the_span`
            let TestFunction::MultiBlock(test_fn) = def.function else { return None };
            if def.actual_name.contains("512") && !avx512 {
                return None;
            }
            def.config.timing = TestTiming::cycles_only(1);
            def.config.thread_count = 1;
            def.config.cache_line_bytes = cache_info.cache_line_size;
            Some((def, test_fn))
        }).collect()
    }

    /// TODO 76: every built-in correctness and bandwidth test runs clean on one region of three
    /// joined blocks (640 KiB, not a power of two). Tests with an absolute chunk get 68 KiB, which
    /// divides neither the region nor a block, so its 10 chunks overlap by 4 or 8 KiB. Nothing is
    /// written outside the region (guard zones), and the correctness tests leave no extent word
    /// untouched, so nothing is skipped. That also checks that a test the harness doesn't fill,
    /// because it writes first, really writes every word. Every chunk is exactly 68 KiB: a
    /// Bench-Init writes first, so its bytes are 10 x 68 KiB per write pass. (The Spd-* bandwidth
    /// tests still take power-of-two blocks.)
    #[test]
    fn built_in_tests_run_clean_on_joined_blocks() {
        let mut region = Region::new(&[256 * KIB, 256 * KIB, 128 * KIB]);
        assert_eq!(crate::test_memory::extent(&region.blocks, usize::MAX).test_size, 640 * KIB);

        let mut ran = Vec::new();
        for (mut def, test_fn) in suite() {
            if matches!(def.config.chunk_mode, ChunkMode::Absolute { .. }) {
                def.config.chunk_mode = ChunkMode::Absolute { size_bytes: 68 * KIB };
            }
            // Writers cover the whole extent; read-only bandwidth tests and dependent verifies don't
            let writes_all = def.actual_name.starts_with("Mem-") || def.actual_name.starts_with("Bench-Init-");
            if writes_all {
                region.words().fill(UNTOUCHED);
            }
            let stats = unsafe { test_fn(&region.blocks, 0, ErrorMode::Log, &def.config.timing, &def.config, None) };
            assert_eq!(stats.error_count, 0, "{} found errors", def.display_name);
            assert!(stats.bytes_processed > 0, "{} tested nothing", def.display_name);
            if def.actual_name.starts_with("Bench-Init-") {
                let passes = def.config.write_read_cycles as usize;
                assert_eq!(stats.bytes_processed, passes * 10 * 68 * KIB, "{}: a chunk wasn't 68 KiB, or it got a fill", def.display_name);
            }
            if writes_all {
                let left = region.words().iter().filter(|&&w| w == UNTOUCHED).count();
                assert_eq!(left, 0, "{} left {} words of its extent untouched", def.display_name, left);
            }
            assert!(region.guards_intact(), "{} wrote outside the region", def.display_name);
            ran.push(def.display_name.clone());
        }
        assert!(ran.len() > 30, "only {} tests ran: {:?}", ran.len(), ran);
    }

    /// TODO 76: chunks overlap where they don't tile, so a word's value may depend only on its
    /// position. Each writer whose last pass covers every word leaves the same image at 68 KiB
    /// and 100 KiB chunks (Mem-Stride's last pass doesn't), and each Bench-Verify finds its
    /// Bench-Init's image clean at a third size, 132 KiB.
    #[test]
    fn writers_leave_the_same_image_at_any_chunk_size() {
        let mut region = Region::new(&[256 * KIB, 256 * KIB, 128 * KIB]);
        let suite = suite();
        let with_chunk = |def: &TestDefinition, chunk: usize| {
            let mut def = def.clone();
            def.config.chunk_mode = ChunkMode::Absolute { size_bytes: chunk };
            def
        };
        let mut compared = 0;
        for (def, test_fn) in &suite {
            let name = def.actual_name;
            if !(name.starts_with("Mem-") || name.starts_with("Bench-Init-")) || name.starts_with("Mem-Stride") {
                continue;
            }
            let mut images = Vec::new();
            for chunk in [68 * KIB, 100 * KIB] {
                let def = with_chunk(def, chunk);
                region.words().fill(UNTOUCHED);
                let stats = unsafe { test_fn(&region.blocks, 0, ErrorMode::Log, &def.config.timing, &def.config, None) };
                assert_eq!(stats.error_count, 0, "{} found errors", def.display_name);
                images.push(region.words().to_vec());
            }
            assert!(images[0] == images[1], "{} wrote a different image at 68 KiB and 100 KiB chunks", def.display_name);
            compared += 1;
        }
        assert!(compared > 20, "only {compared} writers compared");

        let mut verified = 0;
        for (init, init_fn) in suite.iter().filter(|(d, _)| d.display_name.starts_with("Bench-Init-")) {
            let verify_name = init.display_name.replace("Bench-Init-", "Bench-Verify-");
            let (verify, verify_fn) = suite.iter().find(|(d, _)| d.display_name == verify_name).unwrap();
            let init = with_chunk(init, 68 * KIB);
            let mut verify = with_chunk(verify, 132 * KIB);
            verify.config.skip_init = true;
            unsafe { init_fn(&region.blocks, 0, ErrorMode::Log, &init.config.timing, &init.config, None) };
            let stats = unsafe { verify_fn(&region.blocks, 0, ErrorMode::Log, &verify.config.timing, &verify.config, None) };
            assert_eq!(stats.error_count, 0, "{} at 132 KiB disagrees with {} at 68 KiB", verify.display_name, init.display_name);
            verified += 1;
        }
        assert!(verified >= 7, "only {verified} Bench pairs verified");
        assert!(region.guards_intact());
    }

    /// TODO 74: every test that takes the seal honours it. Sealed, on memory that holds the seal,
    /// it finds no errors of its own or the seal's and leaves every word of the span sealed again.
    /// A new `Mem-*` test that skips the wrap fails here.
    #[test]
    fn every_sealed_test_honours_the_seal() {
        let mut region = Region::new(&[256 * KIB, 256 * KIB, 128 * KIB]);
        let words = 640 * KIB / 8;
        let base = region.words().as_mut_ptr();
        let mut ran = Vec::new();
        // Two cycles under tm5: a step that runs past its first cycle reseals its extent at the end
        for (pattern, cycles) in [(crate::seal::SealPattern::Tmr, 1), (crate::seal::SealPattern::Tm5, 2)] {
            let kernel = crate::seal::SealKernel { pattern, ..crate::seal::SealKernel::detect() };
            for (mut def, test_fn) in suite() {
                if def.seal_use == SealUse::Never {
                    continue;
                }
                if matches!(def.config.chunk_mode, ChunkMode::Absolute { .. }) {
                    def.config.chunk_mode = ChunkMode::Absolute { size_bytes: 68 * KIB };
                }
                def.config.timing = TestTiming::cycles_only(cycles);
                set_step_seals(std::slice::from_mut(&mut def), kernel, true);
                unsafe { kernel.fill(base, 0, words) };
                let stats = unsafe { test_fn(&region.blocks, 0, ErrorMode::Log, &def.config.timing, &def.config, None) };
                assert_eq!((stats.error_count, stats.seal_errors), (0, 0), "{} ({})", def.display_name, kernel.name());
                assert_eq!(unsafe { kernel.check(base, 0, words, "test") }, 0, "{} left words unsealed", def.display_name);
                ran.push(def.actual_name);
            }
        }
        assert!(ran.len() > 40, "only {} sealed runs", ran.len());
        assert!(ran.iter().any(|n| n.starts_with("Mem-MirrorV2")) && ran.iter().any(|n| n.starts_with("Mem-BlockMove")));
        assert!(region.guards_intact());
    }

    /// TODO 93: the strided SimpleTest, at every width and in every mode, runs clean and leaves the
    /// image the sequential one leaves (the scalar test's strided mode 2 writes mode 20's lines),
    /// over a 15-line stride (TM5 Parameter 8 on 2 channels) and one longer than a chunk; sealed,
    /// it leaves the span sealed.
    #[test]
    fn strided_simple_tests_write_the_sequential_image() {
        let mut region = Region::new(&[256 * KIB, 256 * KIB, 128 * KIB]);
        let words = 640 * KIB / 8;
        let kernel = crate::seal::SealKernel::detect();
        let mut ran = 0;
        for (def, test_fn) in suite().into_iter().filter(|(d, _)| d.actual_name.starts_with("Mem-SimpleV2")) {
            for mode in crate::pattern_gen::MODES {
                let run = |region: &mut Region, stride_cl: usize, sealed: bool| {
                    let mut def = def.clone();
                    def.config.chunk_mode = ChunkMode::Absolute { size_bytes: 68 * KIB };
                    def.config.pattern_mode = Some(mode);
                    def.config.parameter_context = (stride_cl > 0).then(|| crate::config::TestParameterContext {
                        stride_cachelines: Some(stride_cl),
                        stride_elements: Some(stride_cl * 8),
                        ..Default::default()
                    });
                    set_step_seals(std::slice::from_mut(&mut def), kernel, sealed);
                    if sealed {
                        unsafe { kernel.fill(region.words().as_mut_ptr(), 0, words) };
                    } else {
                        region.words().fill(UNTOUCHED);
                    }
                    let stats = unsafe { test_fn(&region.blocks, 0, ErrorMode::Log, &def.config.timing, &def.config, None) };
                    assert_eq!((stats.error_count, stats.seal_errors), (0, 0), "{} mode {mode} stride {stride_cl}", def.display_name);
                    region.words().to_vec()
                };
                // Every width runs mode 2 on the scalar build, whose strided walk writes mode 20's lines
                let same_as = if mode == 2 { 20 } else { mode };
                let sequential = {
                    let mut seq = def.clone();
                    seq.config.chunk_mode = ChunkMode::Absolute { size_bytes: 68 * KIB };
                    seq.config.pattern_mode = Some(same_as);
                    seq.config.parameter_context = None;
                    set_step_seals(std::slice::from_mut(&mut seq), kernel, false);
                    region.words().fill(UNTOUCHED);
                    unsafe { test_fn(&region.blocks, 0, ErrorMode::Log, &seq.config.timing, &seq.config, None) };
                    region.words().to_vec()
                };
                for stride_cl in [15, 4095] {
                    let image = run(&mut region, stride_cl, false);
                    assert!(image == sequential, "{} mode {mode} stride {stride_cl}: not the sequential image", def.display_name);
                    run(&mut region, stride_cl, true);
                    assert_eq!(unsafe { kernel.check(region.words().as_ptr(), 0, words, "test") }, 0,
                               "{} mode {mode} stride {stride_cl} left words unsealed", def.display_name);
                    ran += 1;
                }
            }
        }
        assert!(ran >= 7 * 2 * 4, "only {ran} strided runs");
        assert!(region.guards_intact());
    }

    /// TODO 74: a bit flipped in sealed memory is the seal's error before a correctness test's
    /// chunk (TM5 numbers it 0), but the mirror's own after a mirror (TM5's step number), and
    /// either way the chunk ends sealed again.
    #[test]
    fn the_seal_attributes_a_flipped_bit_as_tm5_does() {
        let mut region = Region::new(&[256 * KIB, 256 * KIB, 128 * KIB]);
        let words = 640 * KIB / 8;
        let kernel = crate::seal::SealKernel::detect();
        let suite = suite();
        let find = |name: &str| suite.iter().find(|(d, _)| d.display_name == name).cloned().unwrap();
        for (name, own, seal) in [("Mem-StuckBit", 0, 1), ("Mem-SimpleV2", 0, 1), ("Mem-MirrorV2", 1, 0)] {
            let (mut def, test_fn) = find(name);
            def.config.chunk_mode = ChunkMode::Absolute { size_bytes: 68 * KIB };
            set_step_seals(std::slice::from_mut(&mut def), kernel, true);
            let base = region.words().as_mut_ptr();
            unsafe { kernel.fill(base, 0, words) };
            // In the second chunk only: [68, 128) KiB of the ten 68 KiB chunks over 640 KiB
            region.words()[(70 * KIB) / 8] ^= 1 << 9;
            let stats = unsafe { test_fn(&region.blocks, 0, ErrorMode::Log, &def.config.timing, &def.config, None) };
            assert_eq!((stats.error_count, stats.seal_errors), (own, seal), "{name}");
            assert_eq!(unsafe { kernel.check(region.words().as_ptr(), 0, words, "test") }, 0, "{name} left it unsealed");
        }
    }

    /// TODO 74: the worker's seal bookkeeping between steps. A step that never takes the seal has
    /// the part of its extent still sealed checked first, and leaves it unsealed; the next step
    /// that takes the seal gets it back first. A bad word found there is a seal error.
    #[test]
    fn steps_that_skip_the_seal_are_checked_first_and_resealed_after() {
        let mut region = Region::new(&[256 * KIB, 256 * KIB, 128 * KIB]);
        let words = 640 * KIB / 8;
        let kernel = crate::seal::SealKernel::detect();
        let suite = suite();
        let find = |name: &str| suite.iter().find(|(d, _)| d.display_name == name).cloned().unwrap();
        let progress = crate::tests::TestProgress::new();
        let (mut never, _) = find("Bench-Init-TMR-10");
        let (mut sealed, _) = find("Mem-StuckBit");
        set_step_seals(std::slice::from_mut(&mut never), kernel, true);
        set_step_seals(std::slice::from_mut(&mut sealed), kernel, true);
        assert!(!never.config.seal.expects && sealed.config.seal.expects);
        let base = region.words().as_mut_ptr();
        unsafe { kernel.fill(base, 0, words) };
        let mut unsealed = 0;
        region.words()[100] ^= 1;
        let errors = crate::thread_pool::seal_before_step(&region.blocks, &never.config, never.actual_name, &progress, &mut unsealed);
        assert_eq!((errors, unsealed), (1, 640 * KIB), "the whole span is Bench-Init's extent");
        // The seal's bits, not the test's, taken with the work item (TODO 97)
        let bits = crate::tests::take_failing_bits();
        assert!(bits.test.is_empty() && !bits.seal.is_empty() && bits.seal.detail().starts_with("0: 1 / 1"), "{bits:?}");
        assert!(crate::tests::take_failing_bits().seal.is_empty());
        region.words().fill(UNTOUCHED);
        let errors = crate::thread_pool::seal_before_step(&region.blocks, &sealed.config, sealed.actual_name, &progress, &mut unsealed);
        assert_eq!((errors, unsealed), (0, 0));
        assert_eq!(unsafe { kernel.check(region.words().as_ptr(), 0, words, "test") }, 0);
    }

    /// TODO 94: a config can name every built-in test, latency and bandwidth tests included, by
    /// its name or its `-Auto` name.
    #[test]
    fn a_config_can_name_every_built_in_test() {
        let cache_info = crate::tests::get_cache_info();
        let mut config = crate::config::ModernConfig::create_demo_config();
        let mut names: Vec<&str> = Vec::new();
        for def in create_test_definitions(cache_info) {
            names.push(def.actual_name);
            names.extend(def.original_name);
        }
        names.sort();
        names.dedup();
        config.test_sequence = names.iter()
            .map(|name| crate::config::TestConfig { enabled: true, function: name.to_string(), ..Default::default() })
            .collect();
        let (steps, _) = create_test_definitions_from_config(&config, cache_info).unwrap();
        assert_eq!(steps.len(), names.len());
        assert!(names.contains(&"Lat-L1-Copy") && names.contains(&"Spd-L1-Read-Auto"));
    }

    /// TODO 92: each latency shortcut runs the tests its help text names, no more.
    #[test]
    fn latency_shortcuts_select_the_tests_they_name() {
        let cache_info = crate::tests::get_cache_info();
        let selected = |filter: &str| {
            let mut defs = create_test_definitions(cache_info);
            retain_matching(&mut defs, filter);
            let mut names: Vec<&str> = defs.iter().map(|d| d.actual_name).collect();
            names.sort();
            names
        };
        let tests = |tiers: &[&str]| {
            let mut names: Vec<String> = tiers.iter()
                .flat_map(|t| ["Copy", "Read", "Write"].map(|op| format!("Lat-{t}-{op}")))
                .collect();
            names.sort();
            names
        };
        assert_eq!(selected(crate::cli::RAM_LATENCY_FILTER), tests(&["DRAM", "DRAMFull"]));
        assert_eq!(selected(crate::cli::CACHE_LATENCY_FILTER), tests(&["DRAM", "L1", "L2", "L3"]));
    }

    /// TODO 85: every mirror test runs clean in every mode, twice per chunk, on joined blocks
    /// with overlapping 68 KiB chunks, and writes nothing outside the region.
    #[test]
    fn mirror_tests_run_clean_in_every_mode() {
        let mut region = Region::new(&[256 * KIB, 256 * KIB, 128 * KIB]);
        let mut ran = 0;
        for (mut def, test_fn) in suite().into_iter().filter(|(d, _)| d.actual_name.starts_with("Mem-MirrorV2")) {
            def.config.chunk_mode = ChunkMode::Absolute { size_bytes: 68 * KIB };
            def.config.test_reps = 2;
            for mirror in ["whole", "subblocks:2", "subblocks:4", "jump:0", "jump:2", "jump:510"] {
                def.config.parameter_context.get_or_insert_with(Default::default).mirror = Some(mirror.parse().unwrap());
                region.words().fill(UNTOUCHED);
                let stats = unsafe { test_fn(&region.blocks, 0, ErrorMode::Log, &def.config.timing, &def.config, None) };
                assert_eq!(stats.error_count, 0, "{} {mirror} found errors", def.display_name);
                ran += 1;
            }
        }
        assert!(ran >= 4 * 6, "only {ran} mirror runs");
        assert!(region.guards_intact());
    }

    /// TODO 76: every latency test builds its chain in place on the span (no heap), takes its
    /// sample, and writes nothing outside the span.
    #[test]
    fn latency_tests_run_in_place_on_the_span() {
        let region = Region::new(&[256 * KIB, 256 * KIB, 128 * KIB]);
        let cache_info = crate::tests::get_cache_info();
        let avx512 = is_x86_feature_detected!("avx512f");
        let mut ran = 0;
        for mut def in create_test_definitions(cache_info) {
            let TestFunction::Latency(test_fn) = def.function else { continue };
            if def.actual_name.contains("512") && !avx512 {
                continue;
            }
            def.config.timing = TestTiming::cycles_only(1);
            def.config.thread_count = 1;
            def.config.tsc_frequency_ghz = 1.0;
            let stats = unsafe { test_fn(&region.blocks, 0, ErrorMode::Log, &def.config.timing, &def.config, None) };
            assert_eq!(stats.sample_count, 1, "{} took no sample", def.display_name);
            assert!(stats.basic_stats.bytes_processed > 0, "{} counted nothing", def.display_name);
            assert!(region.guards_intact(), "{} wrote outside the region", def.display_name);
            ran += 1;
        }
        assert!(ran >= 100, "only {ran} latency tests ran");
    }

    /// Why `subdivisions` is capped at 512: past it, Mem-Stride's shift no longer divides a 4 KiB
    /// multiple of a chunk, and the tail of each chunk goes untested. Also shows the coverage check
    /// in `built_in_tests_run_clean_on_joined_blocks` catches a skipped tail.
    #[test]
    fn stride_with_too_many_subdivisions_skips_tails() {
        let mut region = Region::new(&[256 * KIB, 256 * KIB, 128 * KIB]);
        let (mut stride, stride_fn) = suite().into_iter().find(|(d, _)| d.display_name == "Mem-Stride").unwrap();
        stride.config.chunk_mode = ChunkMode::Absolute { size_bytes: 68 * KIB };
        let mut untouched_with = |subdivisions: u32, region: &mut Region| {
            stride.config.parameter_context.as_mut().unwrap().subdivisions = Some(subdivisions);
            region.words().fill(UNTOUCHED);
            unsafe { stride_fn(&region.blocks, 0, ErrorMode::Log, &stride.config.timing, &stride.config, None) };
            region.words().iter().filter(|&&w| w == UNTOUCHED).count()
        };
        assert_eq!(untouched_with(512, &mut region), 0);
        assert!(untouched_with(1024, &mut region) > 0);
    }

    /// TODO 76: a fault between a write and its verify is counted once per chunk that verifies
    /// it: once in one chunk, twice in the overlap of two (as `verify_reps` counts each detection),
    /// and once under halt, which stops at the first. A halted test still reports the bytes it got
    /// through (accounting is per chunk, not per piece).
    #[test]
    fn an_injected_fault_is_counted_and_halt_reports_progress() {
        let mut region = Region::new(&[256 * KIB, 256 * KIB, 128 * KIB]);
        let suite = suite();
        let find = |name: &str| suite.iter().find(|(d, _)| d.display_name == name).cloned().unwrap();
        let (init, init_fn) = find("Bench-Init-TMR-10");
        let (mut verify, verify_fn) = find("Bench-Verify-TMR-10");
        verify.config.skip_init = true;
        verify.config.chunk_mode = ChunkMode::Absolute { size_bytes: 68 * KIB };

        // Chunks [0, 68) and [60, 128) KiB: 68 KiB + 40 B is in the second only, 62 KiB in both
        for (word, mode, errors, check) in [
            ((68 * KIB) / 8 + 5, ErrorMode::Log, 1, "log"),
            ((68 * KIB) / 8 + 5, ErrorMode::Halt, 1, "halt"),
            ((62 * KIB) / 8, ErrorMode::Log, 2, "log, overlap"),
            ((62 * KIB) / 8, ErrorMode::Halt, 1, "halt, overlap"),
        ] {
            unsafe { init_fn(&region.blocks, 0, ErrorMode::Log, &init.config.timing, &init.config, None) };
            region.words()[word] ^= 1 << 17;
            let stats = unsafe { verify_fn(&region.blocks, 0, mode, &verify.config.timing, &verify.config, None) };
            assert_eq!(stats.error_count, errors, "{check}: one flipped bit");
            assert!(stats.bytes_processed >= 68 * KIB, "{check}: the chunks it verified are counted ({})", stats.bytes_processed);
        }
        assert!(region.guards_intact());
    }

    // ─── Native patterns (pattern spec v2) ─────────────────────────────────────────────────────

    /// Every registered test resolves its chunk by its own name, as the plan table and the seal's
    /// work between steps do: a name the sizing doesn't know panics there, on the first live run
    /// (it did for the native Bench pairs). TODO 19's rule: audit new names against name-keyed
    /// lookups, not just registration.
    #[test]
    fn every_registered_test_sizes_its_chunk_by_name() {
        for def in create_test_definitions(crate::tests::get_cache_info()) {
            let _ = def.config.calculate_chunk_size(def.actual_name, 64 * MB);
            let _ = def.config.calculate_extent_size(def.actual_name, 64 * MB);
        }
    }

    /// The image a native SimpleTest of thread 0 leaves after write round `g` of `program` under
    /// `schedule`: every word from `Phase::line`, whose formulas `pattern::tests` checks against
    /// the spec's.
    fn native_image(program: crate::pattern::Program, epoch: crate::pattern::StepEpoch, wrc: u32, g: u64,
                    schedule: crate::pattern::Schedule, words: usize) -> Vec<u64> {
        let phase = program.phase(&epoch, wrc, g, schedule, crate::pattern::transfer_beat().0);
        (0..words).map(|i| phase.line((i / 8) as u64)[i % 8]).collect()
    }

    /// The Mem-SimpleV2 tests at every width this CPU has, as the built-in suite registers them:
    /// no pattern_mode, so a native program.
    fn native_tests() -> Vec<(TestDefinition, TestFunctionMultiBlock)> {
        suite().into_iter().filter(|(d, _)| d.actual_name.starts_with("Mem-SimpleV2")).map(|(mut def, f)| {
            assert!(def.config.native_program().is_some(), "{} runs a TMR mode", def.display_name);
            def.config.chunk_mode = ChunkMode::Absolute { size_bytes: 68 * KIB };
            def.config.epoch = crate::pattern::StepEpoch { seed: 7, suite_cycle: 2, step: 3 };
            (def, f)
        }).collect()
    }

    /// `switch` at every transfer width, LPDDR6's 12 and DDR6's 24 among them: every kernel width
    /// writes the spec's image, sequential and strided, verifies it clean, and finds one flipped
    /// bit, localised to its word.
    #[test]
    fn switch_writes_one_image_at_every_beat() {
        use crate::pattern_gen::BusBeat;
        let mut region = Region::new(&[256 * KIB, 256 * KIB, 128 * KIB]);
        let words = 640 * KIB / 8;
        let program = crate::pattern::Program::single(crate::pattern::Family::Switch);
        let mut ran = 0;
        for beat in [BusBeat::Bits12, BusBeat::Bits16, BusBeat::Bits24, BusBeat::Bits32, BusBeat::Bits64] {
            let _beat = crate::pattern::TestBeat::set(Some(beat));
            for (def, test_fn) in native_tests() {
                for stride_cl in [0, 15] {
                    let mut def = def.clone();
                    def.config = def.config.with_program(program);
                    (def.config.write_read_cycles, def.config.verify_reps, def.config.test_reps) = (2, 1, 1);
                    def.config.parameter_context = (stride_cl > 0).then(|| crate::config::TestParameterContext {
                        stride_cachelines: Some(stride_cl),
                        stride_elements: Some(stride_cl * 8),
                        ..Default::default()
                    });
                    let what = format!("{} at {} bits, stride {stride_cl}", def.display_name, beat.bits());
                    region.words().fill(UNTOUCHED);
                    let stats = unsafe { test_fn(&region.blocks, 0, ErrorMode::Log, &def.config.timing, &def.config, None) };
                    assert_eq!((stats.error_count, stats.alarms), (0, 0), "{what}");
                    let want = native_image(program, def.config.epoch, 2, 1, crate::pattern::Schedule::Rounds, words);
                    assert!(region.words().iter().zip(&want).all(|(g, w)| g == w), "{what}: not the spec's image");
                    // A verify of that image finds one flipped bit, and each check that saw it
                    // finds its word on the rescan
                    let mut verify = def.clone();
                    (verify.config.test_reps, verify.config.skip_init, verify.config.write_read_cycles) = (0, true, 1);
                    let image = native_image(program, verify.config.epoch, 2, 0, crate::pattern::Schedule::OncePerRun, words);
                    region.words().copy_from_slice(&image);
                    region.words()[40_001] ^= 1 << 37;
                    let stats = unsafe { test_fn(&region.blocks, 0, ErrorMode::Log, &verify.config.timing, &verify.config, None) };
                    assert!(stats.error_count >= 1, "{what}: the flip went unseen");
                    assert_eq!((stats.alarms, stats.not_reproduced), (stats.error_count, 0), "{what}");
                    ran += 1;
                }
            }
        }
        assert!(ran >= 5 * 2 * 2, "only {ran} runs");
        assert!(region.guards_intact());
    }

    /// Every width writes the one image, sequential and strided (TM5's walk), over overlapping
    /// chunks, for each family alone and the default program, and verifies it clean: the last
    /// round of two (pair 0's ~P) is what memory holds.
    #[test]
    fn native_programs_write_one_image_at_every_width() {
        let mut region = Region::new(&[256 * KIB, 256 * KIB, 128 * KIB]);
        let words = 640 * KIB / 8;
        let programs = crate::pattern::Family::ALL.iter().map(|&f| crate::pattern::Program::single(f))
            .chain([crate::pattern::Program::default_simple()]);
        let mut ran = 0;
        for program in programs {
            for (def, test_fn) in native_tests() {
                for stride_cl in [0, 15, 4095] {
                    let mut def = def.clone();
                    def.config = def.config.with_program(program);
                    (def.config.write_read_cycles, def.config.verify_reps, def.config.test_reps) = (2, 2, 1);
                    def.config.parameter_context = (stride_cl > 0).then(|| crate::config::TestParameterContext {
                        stride_cachelines: Some(stride_cl),
                        stride_elements: Some(stride_cl * 8),
                        ..Default::default()
                    });
                    region.words().fill(UNTOUCHED);
                    let stats = unsafe { test_fn(&region.blocks, 0, ErrorMode::Log, &def.config.timing, &def.config, None) };
                    let what = format!("{} {} stride {stride_cl}", def.display_name, program.name());
                    assert_eq!((stats.error_count, stats.alarms), (0, 0), "{what}");
                    let want = native_image(program, def.config.epoch, 2, 1, crate::pattern::Schedule::Rounds, words);
                    let got = region.words().to_vec();
                    let bad = got.iter().zip(&want).position(|(g, w)| g != w);
                    assert!(bad.is_none(), "{what}: word {} is {:#x}, not {:#x}", bad.unwrap(), got[bad.unwrap()], want[bad.unwrap()]);
                    ran += 1;
                }
            }
        }
        assert!(ran >= 6 * 4 * 3);
        assert!(region.guards_intact());
    }

    /// A verify of the fill (test_reps 0, skip_init) on an image with flipped bits counts each bad
    /// word once per chunk that covers it, as a failed check its rescan localised; under halt it
    /// stops at the first and leaves the bad word as it found it.
    #[test]
    fn native_verify_counts_each_bad_word_and_halts_with_the_evidence() {
        let mut region = Region::new(&[256 * KIB, 256 * KIB, 128 * KIB]);
        let words = 640 * KIB / 8;
        for (def, test_fn) in native_tests() {
            let mut def = def.clone();
            def.config.test_reps = 0;
            def.config.skip_init = true;
            // One verify a chunk: the counts below are per verify
            (def.config.verify_reps, def.config.write_read_cycles) = (1, 1);
            // Pair 0's P: what a verify-only step expects
            let image = native_image(def.config.native_program().unwrap(), def.config.epoch, def.config.write_read_cycles, 0, crate::pattern::Schedule::OncePerRun, words);
            // Chunks of 68 KiB spread over 640: word 5 is in the first alone, word 34000 in one chunk
            // (34000 * 8 = 265 KiB), and three bits of word 71000 in another
            for (mode, flips, errors) in [(ErrorMode::Log, vec![5usize, 34_000, 71_000], 3u64), (ErrorMode::Halt, vec![34_000, 71_000], 1)] {
                region.words().copy_from_slice(&image);
                for &w in &flips {
                    region.words()[w] ^= if w == 71_000 { 0b111 << 40 } else { 1 << 3 };
                }
                let stats = unsafe { test_fn(&region.blocks, 0, mode, &def.config.timing, &def.config, None) };
                assert_eq!(stats.error_count, errors, "{} {mode:?}", def.display_name);
                assert_eq!((stats.alarms, stats.not_reproduced), (errors, 0), "{} {mode:?}", def.display_name);
                if mode == ErrorMode::Log {
                    // Each check's bits and each bad word's, by the way they went (TODO 97)
                    let mut want = crate::failing_bits::FailingBits::default();
                    for &w in &flips {
                        want.add_check(region.words()[w] ^ image[w]);
                        want.add_word(image[w], region.words()[w]);
                    }
                    assert_eq!(crate::tests::take_failing_bits().test, want, "{}", def.display_name);
                }
                if mode == ErrorMode::Halt {
                    // It stopped at the first, so the second was never checked; both are still bad
                    assert_eq!(region.words()[34_000], image[34_000] ^ (1 << 3), "{}", def.display_name);
                    assert_eq!(region.words()[71_000], image[71_000] ^ (0b111 << 40));
                }
            }
        }
    }

    /// A flipped bit in any word of a group of 4 lines is found, at every width, sequential and
    /// strided: the group covers every accumulator of every width's verify (the second review: the
    /// other corruption test reaches only some of them).
    #[test]
    fn native_verify_sees_every_word_of_a_group() {
        let mut region = Region::new(&[256 * KIB, 256 * KIB, 128 * KIB]);
        let words = 640 * KIB / 8;
        for (def, test_fn) in native_tests() {
            for stride_cl in [0, 15] {
                let mut def = def.clone();
                // One verify a chunk: the counts below are per verify
                (def.config.test_reps, def.config.skip_init, def.config.verify_reps, def.config.write_read_cycles) = (0, true, 1, 1);
                def.config.parameter_context = (stride_cl > 0).then(|| crate::config::TestParameterContext {
                    stride_cachelines: Some(stride_cl),
                    stride_elements: Some(stride_cl * 8),
                    ..Default::default()
                });
                let image = native_image(def.config.native_program().unwrap(), def.config.epoch, 1, 0, crate::pattern::Schedule::OncePerRun, words);
                // A 256 B group, line-aligned, inside the first chunk alone
                let group = 4096 + 32;
                for k in 0..32 {
                    region.words().copy_from_slice(&image);
                    region.words()[group + k] ^= 1 << (k * 2);
                    let stats = unsafe { test_fn(&region.blocks, 0, ErrorMode::Log, &def.config.timing, &def.config, None) };
                    assert_eq!(stats.error_count, 1, "{} stride {stride_cl}: word {k} of the group", def.display_name);
                }
            }
        }
    }

    /// Under errors=halt a bad word the seal check before a chunk finds stays in memory: the test
    /// stops before writing that chunk, and doesn't reseal.
    #[test]
    fn halt_keeps_what_the_seal_check_found() {
        let mut region = Region::new(&[256 * KIB, 256 * KIB, 128 * KIB]);
        let words = 640 * KIB / 8;
        let kernel = crate::seal::SealKernel::detect().for_cycle(1, 2);
        let (mut def, test_fn) = native_tests().into_iter().next().unwrap();
        def.config.seal.kernel = kernel;
        set_step_seals(std::slice::from_mut(&mut def), kernel, true);
        def.config.seal.kernel = kernel;
        unsafe { kernel.fill(region.words().as_mut_ptr(), 0, words) };
        let bad = 50_000;
        region.words()[bad] ^= 1;
        let stats = unsafe { test_fn(&region.blocks, 0, ErrorMode::Halt, &def.config.timing, &def.config, None) };
        assert_eq!((stats.error_count, stats.seal_errors), (0, 1));
        // The seal check before a chunk counts in the seal's bits (TODO 97)
        let bits = crate::tests::take_failing_bits();
        assert!(bits.test.is_empty() && bits.seal.detail().starts_with("0: 1 / 1"), "{bits:?}");
        let want = kernel.word(region.words().as_ptr().wrapping_add(bad) as usize) ^ 1;
        assert_eq!(region.words()[bad], want, "the bad seal word was overwritten");
    }

    /// The suite cycle and the step change every word; the same epoch writes the same bytes again
    /// (seed= replays a run), on any thread count.
    #[test]
    fn native_images_change_with_cycle_and_step_and_replay() {
        let mut region = Region::new(&[256 * KIB, 256 * KIB, 128 * KIB]);
        let (def, test_fn) = native_tests().into_iter().next().unwrap();
        let mut image = |epoch: crate::pattern::StepEpoch| {
            let mut def = def.clone();
            def.config.epoch = epoch;
            unsafe { test_fn(&region.blocks, 0, ErrorMode::Log, &def.config.timing, &def.config, None) };
            region.words().to_vec()
        };
        let e = |suite_cycle, step| crate::pattern::StepEpoch { seed: 11, suite_cycle, step };
        let (a, b, c, again) = (image(e(1, 0)), image(e(2, 0)), image(e(1, 1)), image(e(1, 0)));
        assert_eq!(a, again, "the same epoch wrote different bytes");
        let same = |x: &[u64], y: &[u64]| x.iter().zip(y).filter(|(p, q)| p == q).count();
        assert!(same(&a, &b) < a.len() / 1000 && same(&a, &c) < a.len() / 1000, "{} {}", same(&a, &b), same(&a, &c));
    }

    /// A SimpleTest that verifies its fill (test_reps 0) writes one image a run, so its pairs span
    /// two suite cycles (P, then ~P under the same key), and it visits every place in the program
    /// (the second review: with its wrc's rotation the default program never ran switch there).
    #[test]
    fn a_verify_only_native_step_visits_every_family() {
        let mut region = Region::new(&[256 * KIB, 256 * KIB, 128 * KIB]);
        let words = 640 * KIB / 8;
        let (mut def, test_fn) = native_tests().into_iter().next().unwrap();
        def.config.test_reps = 0;
        let program = def.config.native_program().unwrap();
        let mut phases = Vec::new();
        for suite_cycle in 1..=8 {
            def.config.epoch.suite_cycle = suite_cycle;
            let stats = unsafe { test_fn(&region.blocks, 0, ErrorMode::Log, &def.config.timing, &def.config, None) };
            assert_eq!(stats.error_count, 0, "suite cycle {suite_cycle}");
            assert_eq!(region.words().to_vec(), native_image(program, def.config.epoch, 1, 0, crate::pattern::Schedule::OncePerRun, words), "suite cycle {suite_cycle}");
            phases.push(program.phase(&def.config.epoch, 1, 0, crate::pattern::Schedule::OncePerRun, None));
        }
        let families: Vec<_> = phases.iter().step_by(2).map(|p| p.family).collect();
        let laps: Vec<_> = program.families().iter().copied().cycle().take(families.len()).collect();
        assert_eq!(families, laps, "one place in the program each two suite cycles");
        assert!(phases.chunks(2).all(|p| p[0].key == p[1].key && (p[0].mask, p[1].mask) == (0, !0)), "P then ~P, one key");
    }

    /// An odd write_read_cycles rounds up to even for a native SimpleTest that writes first, so
    /// its rounds are whole P/~P pairs; not for a TMR mode, a verify-only step or another test.
    #[test]
    fn odd_native_rounds_round_up_to_pairs() {
        let base = native_tests().into_iter().next().unwrap().0;
        let with = |wrc: u32, edit: &dyn Fn(&mut TestDefinition)| {
            let mut def = base.clone();
            def.config.write_read_cycles = wrc;
            edit(&mut def);
            def
        };
        let sequence = |text: &'static str| move |d: &mut TestDefinition| {
            d.actual_name = "Mem-Sequence";
            d.config = d.config.clone().with_sequence(crate::sequence::Sequence::parse(text, crate::sequence::Order::Forward).unwrap());
        };
        let mut defs = vec![
            with(1, &|_| {}),
            with(3, &|_| {}),
            with(4, &|_| {}),
            with(3, &|d| d.config.pattern_mode = Some(30)),
            with(3, &|d| d.config.test_reps = 0),
            with(3, &|d| d.actual_name = "Mem-MirrorV2-Auto"),
            // Mem-Sequence: one W a run, an odd 3 runs, so 4; W and RW, 2 rounds a run, stay;
            // a fill and two Ws a run are odd however many runs, so they stay too
            with(3, &sequence("W 5R")),
            with(3, &sequence("W 2R RW 2R")),
            with(1, &sequence("R 2W R")),
        ];
        pair_native_rounds(&mut defs);
        let rounds: Vec<u32> = defs.iter().map(|d| d.config.write_read_cycles).collect();
        assert_eq!(rounds, [2, 4, 4, 3, 3, 3, 4, 3, 1]);
        // The built-in suite names whole pairs, so a run without a config warns of none (review A)
        let mut builtin = create_test_definitions(crate::tests::get_cache_info());
        let before: Vec<u32> = builtin.iter().map(|d| d.config.write_read_cycles).collect();
        pair_native_rounds(&mut builtin);
        assert_eq!(before, builtin.iter().map(|d| d.config.write_read_cycles).collect::<Vec<_>>());
    }

    /// Three rounds asked of a native SimpleTest run as four, two whole pairs: the last image is the
    /// second pair's ~P.
    #[test]
    fn three_native_rounds_run_two_pairs() {
        let mut region = Region::new(&[256 * KIB, 256 * KIB, 128 * KIB]);
        let words = 640 * KIB / 8;
        let (mut def, test_fn) = native_tests().into_iter().next().unwrap();
        (def.config.write_read_cycles, def.config.test_reps, def.config.verify_reps) = (3, 1, 1);
        def.config.timing = TestTiming::cycles_only(1);
        let mut defs = [def];
        pair_native_rounds(&mut defs);
        let [def] = defs;
        assert_eq!(def.config.write_read_cycles, 4);
        let program = def.config.native_program().unwrap();
        let stats = unsafe { test_fn(&region.blocks, 0, ErrorMode::Log, &def.config.timing, &def.config, None) };
        assert_eq!(stats.error_count, 0);
        let last = native_image(program, def.config.epoch, 4, 3, crate::pattern::Schedule::Rounds, words);
        let first = native_image(program, def.config.epoch, 4, 2, crate::pattern::Schedule::Rounds, words);
        assert_eq!(region.words().to_vec(), last);
        assert!(last.iter().zip(&first).all(|(l, f)| *l == !*f), "the last round is the pair's ~P");
    }

    /// A native SimpleTest that verifies without writing (dependent, test_reps 0) is refused
    /// whatever precedes it: no other step writes its step-keyed image (TODO 13). With its own
    /// writes, or its own fill, it stands alone.
    #[test]
    fn a_dependent_verify_only_native_step_is_refused() {
        let (writer, _) = native_tests().into_iter().next().unwrap();
        let step = |skip_init, test_reps| {
            let mut def = writer.clone();
            (def.config.skip_init, def.config.test_reps) = (skip_init, test_reps);
            def
        };
        let bench = suite().into_iter().find(|(d, _)| d.display_name == "Bench-Init-Switch").unwrap().0;
        for before in [vec![], vec![writer.clone()], vec![bench]] {
            let mut plan = before.clone();
            plan.push(step(true, 0));
            let refused = validate_test_plan(&plan);
            assert!(refused.len() == 1 && refused[0].0 == before.len() && refused[0].1.contains("keyed by its own step"), "{refused:?}");
        }
        for (skip_init, test_reps) in [(true, 1), (false, 0)] {
            let plan = validate_test_plan(&[writer.clone(), step(skip_init, test_reps)]);
            assert!(plan.is_empty(), "skip_init {skip_init}, test_reps {test_reps}: {plan:?}");
        }
    }

    /// Mem-Random, CacheBust, Stride and BlockMove write each word's id into it, so two threads'
    /// images share no value (TODO 97: they shared the index, or were one apart).
    #[test]
    fn other_tests_values_differ_between_threads() {
        let mut region = Region::new(&[256 * KIB, 256 * KIB, 128 * KIB]);
        let kernel = crate::seal::SealKernel::detect();
        let suite = suite();
        for name in ["Mem-Random", "Mem-CacheBust", "Mem-Stride", "Mem-BlockMove", "Mem-MovingInv", "Mem-ModuloX"] {
            let (mut def, test_fn) = suite.iter().find(|(d, _)| d.display_name == name).cloned().unwrap();
            set_step_seals(std::slice::from_mut(&mut def), kernel, false);
            def.config.chunk_mode = ChunkMode::Absolute { size_bytes: 68 * KIB };
            let mut image = |thread_id| {
                region.words().fill(UNTOUCHED);
                let stats = unsafe { test_fn(&region.blocks, thread_id, ErrorMode::Log, &def.config.timing, &def.config, None) };
                assert_eq!(stats.error_count, 0, "{name} thread {thread_id}");
                region.words().iter().copied().filter(|&w| w != UNTOUCHED).collect::<std::collections::HashSet<u64>>()
            };
            let (a, b) = (image(0), image(1));
            assert!(a.len() > 1000 && b.len() > 1000, "{name} wrote {} and {} values", a.len(), b.len());
            assert_eq!(a.intersection(&b).count(), 0, "{name}: threads 0 and 1 wrote values in common");
        }
    }

    /// After a cycle Mem-MovingInv leaves P everywhere, and Mem-ModuloX P in its victim lines and ~P
    /// in the rest: `ident`'s word of each word's id under the cycle's key (TODO 97).
    #[test]
    fn moving_inversions_and_modulo_x_leave_their_images() {
        let mut region = Region::new(&[256 * KIB, 256 * KIB, 128 * KIB]);
        let kernel = crate::seal::SealKernel::detect();
        let suite = suite();
        let thread = 3;
        for name in ["Mem-MovingInv", "Mem-ModuloX"] {
            let (mut def, test_fn) = suite.iter().find(|(d, _)| d.display_name == name).cloned().unwrap();
            set_step_seals(std::slice::from_mut(&mut def), kernel, false);
            def.config.epoch = crate::pattern::StepEpoch { seed: 5, suite_cycle: 2, step: 1 };
            region.words().fill(UNTOUCHED);
            let stats = unsafe { test_fn(&region.blocks, thread, ErrorMode::Log, &def.config.timing, &def.config, None) };
            assert_eq!((stats.error_count, stats.cycles_completed), (0, 1), "{name}");
            let key = crate::pattern::fmix(def.config.epoch.key().wrapping_add(1));
            let offset = (crate::pattern::fmix(def.config.epoch.key()) % 20) as usize;
            let ids = crate::pattern::word_base(thread);
            for (i, &w) in region.words().iter().enumerate() {
                let p = crate::pattern::ident_word(ids + i as u64, key);
                let victim = name == "Mem-MovingInv" || (i / 8) % 20 == offset;
                assert_eq!(w, if victim { p } else { !p }, "{name} word {i}");
            }
        }
    }

    /// The phase a Mem-Sequence chunk ends a cycle on: its last write's, inverted by each
    /// read-then-write since.
    fn sequence_end_phase(def: &TestDefinition, cycle: u32) -> crate::pattern::Phase {
        let seq = def.config.sequence();
        let reps = def.config.write_read_cycles.max(1);
        let rounds = seq.rounds(reps);
        let program = def.config.native_program().unwrap();
        let beat = crate::pattern::transfer_beat().0;
        let first = u64::from(cycle - 1) * u64::from(rounds);
        let (mut current, mut next) = (first, first);
        if seq.reads_first() {
            (current, next) = crate::sequence::round_after(crate::sequence::Op::Write, current, next);
        }
        for _ in 0..reps {
            for step in seq.steps() {
                for _ in 0..step.count {
                    (current, next) = crate::sequence::round_after(step.op, current, next);
                }
            }
        }
        program.phase(&def.config.epoch, rounds, current, crate::pattern::Schedule::Rounds, beat)
    }

    /// Mem-Sequence in every order and with every step: each pass visits every line (an
    /// untouched word would fail a read), every read finds what the last write left, inverted by
    /// each read-then-write, and the chunk ends on that phase's image. Sealed and not; over
    /// overlapping chunks, and a stride (TODO 97).
    #[test]
    fn mem_sequence_runs_every_order_and_step() {
        let mut region = Region::new(&[256 * KIB, 256 * KIB, 128 * KIB]);
        let words = 640 * KIB / 8;
        let kernel = crate::seal::SealKernel::detect();
        let (base_def, test_fn) = suite().into_iter().find(|(d, _)| d.display_name == "Mem-Sequence").unwrap();
        let stride = crate::config::TestParameterContext { stride_cachelines: Some(15), stride_elements: Some(120), ..Default::default() };
        let mut ran = 0;
        for (text, order) in [("W R", "forward"), ("W R", "reverse"), ("W R", "random"), ("W R", "stride"),
                              ("W@random R RW R@reverse", "forward"), ("2R W RW@random 2R@random", "forward"),
                              ("W 2R RW 2R@random", "forward"), ("W RW@stride R@random RW R", "reverse")] {
            for sealed in [false, true] {
                let mut def = base_def.clone();
                def.config.chunk_mode = ChunkMode::Absolute { size_bytes: 68 * KIB };
                def.config.parameter_context = Some(stride.clone());
                def.config.epoch = crate::pattern::StepEpoch { seed: 9, suite_cycle: 3, step: 2 };
                let order = crate::sequence::Order::parse(order).unwrap();
                def.config = def.config.with_sequence(crate::sequence::Sequence::parse(text, order).unwrap());
                set_step_seals(std::slice::from_mut(&mut def), kernel, sealed);
                if sealed {
                    unsafe { kernel.fill(region.words().as_mut_ptr(), 0, words) };
                } else {
                    region.words().fill(UNTOUCHED);
                }
                let what = format!("'{text}' order {order:?}, sealed {sealed}");
                let stats = unsafe { test_fn(&region.blocks, 0, ErrorMode::Log, &def.config.timing, &def.config, None) };
                assert_eq!((stats.error_count, stats.seal_errors, stats.alarms), (0, 0, 0), "{what}");
                if sealed {
                    assert_eq!(unsafe { kernel.check(region.words().as_ptr(), 0, words, "test") }, 0, "{what}: left unsealed");
                } else {
                    let phase = sequence_end_phase(&def, 1);
                    let want: Vec<u64> = (0..words).map(|i| phase.line((i / 8) as u64)[i % 8]).collect();
                    let bad = region.words().iter().zip(&want).position(|(g, w)| g != w);
                    assert!(bad.is_none(), "{what}: word {} isn't the end phase's", bad.unwrap());
                }
                ran += 1;
            }
        }
        assert_eq!(ran, 16);
        assert!(region.guards_intact());
    }

    /// `sequence=` and `order=` reach every Mem-Sequence step and no other, checked as a config's
    /// are (TODO 97).
    #[test]
    fn the_command_lines_sequence_reaches_mem_sequence() {
        let suite = suite();
        let find = |name: &str| suite.iter().find(|(d, _)| d.display_name == name).map(|(d, _)| d.clone()).unwrap();
        let mut defs = vec![find("Mem-Sequence"), find("Mem-StuckBit")];
        apply_sequence(&mut defs, Some("W,4R,RW@random,R"), None).unwrap();
        let seq = defs[0].config.sequence();
        assert_eq!((seq.name(), seq.order), ("W 4R RW@random R".to_string(), crate::sequence::Order::Forward));
        assert!(defs[1].config.sequence.is_none());
        apply_sequence(&mut defs, None, Some("reverse")).unwrap();
        assert_eq!(defs[0].config.sequence().name(), "W 4R RW@random R", "order= keeps the steps");
        assert_eq!(defs[0].config.sequence().order, crate::sequence::Order::Reverse);
        assert!(apply_sequence(&mut defs, Some("W R@stride"), None).unwrap_err().contains("parameter"), "no stride");
        // A stride of 0 (parameter 1 on one channel) is TM5's jump 0, in order
        defs[0].config.parameter_context = Some(crate::config::interpret_tm5_parameter_with_channels("Mem-Sequence", 1, 1));
        assert!(apply_sequence(&mut defs, Some("W R@stride"), None).is_ok());
        assert!(apply_sequence(&mut defs, None, Some("sideways")).is_err());
    }

    /// `pattern-mode=` sets a numbered mode on every test, replacing a SimpleTest's program; a
    /// native family's number runs that family alone on the SimpleTests and leaves the rest (the
    /// second review).
    #[test]
    fn the_command_lines_pattern_mode_reaches_the_tests_it_names() {
        let suite = suite();
        let find = |name: &str| suite.iter().find(|(d, _)| d.display_name == name).map(|(d, _)| d.clone()).unwrap();
        let (simple, bench, sequence) = (native_tests().into_iter().next().unwrap().0, find("Bench-Init-TMR-30"), find("Mem-Sequence"));
        let modes = |defs: &[TestDefinition]| defs.iter().map(|d| (d.config.pattern_mode, d.config.native_program())).collect::<Vec<_>>();
        let mut defs = vec![simple.clone(), bench.clone(), sequence];
        apply_pattern_mode(&mut defs, 31);
        let switch = crate::pattern::Program::single(crate::pattern::Family::Switch);
        assert_eq!(modes(&defs), [(None, Some(switch)), (Some(30), None), (None, Some(switch))]);
        // A numbered mode: not on Mem-Sequence, which keeps its native program
        apply_pattern_mode(&mut defs, 20);
        assert_eq!(modes(&defs), [(Some(20), None), (Some(20), None), (None, Some(switch))]);
        // No SimpleTest: it changes nothing (and warns)
        let mut defs = vec![bench];
        apply_pattern_mode(&mut defs, 31);
        assert_eq!(modes(&defs), [(Some(30), None)]);
    }

    /// A dependent native Bench-Verify follows its own family's Bench-Init and no other (Astra's
    /// review: the Verify's identity was its config's, mode 10, so even the matching pair failed).
    #[test]
    fn native_bench_dependencies_match_by_family() {
        let suite = suite();
        let find = |name: &str| suite.iter().find(|(d, _)| d.display_name == name).map(|(d, _)| d.clone()).unwrap();
        let mut verify = find("Bench-Verify-Ident");
        verify.config.skip_init = true;
        let same = validate_test_plan(&[find("Bench-Init-Ident"), verify.clone()]);
        assert!(same.is_empty(), "{same:?}");
        let other = validate_test_plan(&[find("Bench-Init-Random"), verify]);
        assert!(other.len() == 1 && other[0].1.contains("(Ident)") && other[0].1.contains("(Random)"), "{other:?}");
    }

    /// A native Bench-Init pins pair 0's P, so after any number of its cycles a dependent
    /// Bench-Verify of the same family finds its image (the review: unpinned, the Init's last cycle
    /// wrote another phase).
    #[test]
    fn native_bench_pairs_agree_over_cycles() {
        let region = Region::new(&[256 * KIB, 256 * KIB, 128 * KIB]);
        let suite = suite();
        for (init_name, verify_name, _, _) in crate::tests::native_bench_tests() {
            let find = |name: &str| suite.iter().find(|(d, _)| d.display_name == name).cloned().unwrap();
            let (mut init, init_fn) = find(init_name);
            let (mut verify, verify_fn) = find(verify_name);
            init.config.timing = TestTiming::cycles_only(3);
            init.config.chunk_mode = ChunkMode::Absolute { size_bytes: 68 * KIB };
            verify.config.skip_init = true;
            verify.config.chunk_mode = ChunkMode::Absolute { size_bytes: 68 * KIB };
            unsafe { init_fn(&region.blocks, 0, ErrorMode::Log, &init.config.timing, &init.config, None) };
            let stats = unsafe { verify_fn(&region.blocks, 0, ErrorMode::Log, &verify.config.timing, &verify.config, None) };
            assert_eq!(stats.error_count, 0, "{verify_name} after 3 cycles of {init_name}");
        }
    }

    /// The SIMD widths run the TMR modes they have no kernel for (0, 1, 2, 13, 30) on the scalar
    /// build, so they write the mode's own image, not mode 10's (TODO 3).
    #[test]
    fn simd_widths_write_the_tmr_modes_own_image() {
        let mut region = Region::new(&[256 * KIB, 256 * KIB, 128 * KIB]);
        let tests = suite().into_iter().filter(|(d, _)| d.actual_name.starts_with("Mem-SimpleV2")).collect::<Vec<_>>();
        for mode in crate::pattern_gen::MODES {
            let mut images = Vec::new();
            for (def, test_fn) in &tests {
                let mut def = def.clone();
                def.config.chunk_mode = ChunkMode::Absolute { size_bytes: 68 * KIB };
                def.config.pattern_mode = Some(mode);
                region.words().fill(UNTOUCHED);
                let stats = unsafe { test_fn(&region.blocks, 0, ErrorMode::Log, &def.config.timing, &def.config, None) };
                assert_eq!(stats.error_count, 0, "{} mode {mode}", def.display_name);
                images.push((def.display_name.clone(), region.words().to_vec()));
            }
            for (name, image) in &images[1..] {
                // Mode 10's SIMD base is param0 whatever the width; the scalar's too
                assert!(*image == images[0].1, "{name} mode {mode}: not {}'s image", images[0].0);
            }
        }
    }
}
