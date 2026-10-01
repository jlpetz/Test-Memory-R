use crate::{ErrorMode, EnhancedMemoryLayout, ProgressTracker, BlockInfo};
use crate::constants::{HUGE_PAGE_SIZE, LARGE_PAGE_SIZE, REGULAR_PAGE_SIZE, MB};
use crate::tests::{WindowMode, ChunkMode, CacheTarget};
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
    stride_access_multi, block_move_multi,
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
use crate::memory::{MemoryBuffer, MemoryAllocator, BackendType};
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
/// `(thread_id, cpu_id, bytes, elapsed_ms, errors, operations, cycles_completed)`.
type CpuStatRow = (usize, usize, u64, u128, u64, u64, u32);
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
}

/// CLI-derived overrides controlling which tests run and how their parameters
/// are tweaked. Bundled so the run entry point keeps a manageable signature.
/// All fields are optional — `None` means "use the value from the config/defaults".
#[derive(Default, Clone, Copy)]
pub struct TestRunOverrides<'a> {
    /// Run only the test(s) matching this name/glob filter.
    pub single_test_filter: Option<&'a str>,
    /// Override the per-test `parameter` string.
    pub parameter_override: Option<&'a str>,
    /// Override the pattern-generation mode.
    pub pattern_mode_override: Option<u32>,
    /// Override the verify-repetition count.
    pub verify_reps_override: Option<u32>,
    /// Override the test-repetition count.
    pub test_reps_override: Option<u32>,
    /// Override the write-read-cycle count.
    pub wrc_override: Option<u32>,
    /// Override the channel count used by stride formulas.
    pub channels_override: Option<u32>,
}

// Global flags for shutdown handling
pub static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

// OS memory allocation per thread (renamed from AllocationBlock for clarity)
// Represents: Physical memory allocated from the OS for testing
#[derive(Debug)]
pub struct AllocationBlock {
    pub buffer: MemoryBuffer,  // Contains BufferInfo with numa_node, page_type, etc.
    pub block_info: BlockInfo, // Thread assignment and size planning
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
        write!(f, "mode={}", self.mode)
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

/// Derive the memory effect of a test from its name and config.
///
/// Classification:
/// - **Writes**: Tests that write a known, repeatable pattern (SimpleTest, SimpleTestNT, Bench-Init)
/// - **Preserves**: Tests that leave memory intact (MirrorMove round-trip, Bench-Verify read-only)
/// - **Destroys**: Tests that overwrite with non-standard patterns (StuckBit, Refresh, CacheBust, etc.)
///   Latency and bandwidth tests also destroy — they use internal patterns not tracked by PatternId.
fn derive_memory_effect(test_name: &str, config: &crate::tests::TestMemoryConfig) -> MemoryEffect {
    // Tests that write a known, config-derived pattern
    if test_name.starts_with("Mem-SimpleV2")
        || test_name.starts_with("Mem-SimpleNT")
        || test_name.starts_with("Bench-Init")
    {
        return MemoryEffect::Writes(PatternId::from_config(config));
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
/// Walks the test list in order, tracking what pattern is currently in memory.
/// Reports warnings for tests that use `skip_init=true` (dependent mode) but
/// either have no pattern in memory or the wrong pattern.
///
/// Returns a list of (test_index, warning_message) for any issues found.
pub fn validate_test_plan(tests: &[TestDefinition]) -> Vec<(usize, String)> {
    let mut warnings = Vec::new();
    // Track what pattern is currently in memory (None = unknown/destroyed)
    let mut current_pattern: Option<PatternId> = None;

    for (i, test) in tests.iter().enumerate() {
        let is_dependent = test.config.skip_init;

        if is_dependent {
            // Dependent test skips its own init — needs compatible pattern already in memory.
            // Exception: Writes tests (like Bench-Init) with skip_init just skip the init_fn
            // and do their writes in test_fn — they don't depend on prior memory state.
            let needs_existing_pattern = matches!(&test.memory_effect, MemoryEffect::Preserves);

            if needs_existing_pattern {
                let expected = PatternId::from_config(&test.config);
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

        // Update memory state based on what this test does
        match &test.memory_effect {
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

// Test function signatures
type TestFunctionMultiBlock = unsafe fn(&[AllocationBlock], usize, ErrorMode, &TestTiming, &TestMemoryConfig, Option<&TestProgress>) -> TestStats;
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
) -> bool {
    let TestRunOverrides {
        single_test_filter,
        parameter_override,
        pattern_mode_override,
        verify_reps_override,
        test_reps_override,
        wrc_override,
        channels_override,
    } = overrides;

    layout.print_layout();

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

    let mut thread_blocks: HashMap<usize, Vec<BlockInfo>> = HashMap::new();
    for block in layout.blocks {
        thread_blocks.entry(block.thread_id).or_default().push(block);
    }

    // Stage 1: Pre-allocate all memory blocks
    println!("Stage 1: Pre-allocating memory blocks...");

    // Display page size constraints before allocation begins
    {
        let min = &runtime_config.memory_allocation.min_page_size;
        let max = &runtime_config.memory_allocation.max_page_size;

        let format_size = |s: &str| -> &'static str {
            match s {
                "regular" => "Regular (4KB)",
                "large" => "Large (2MB)",
                "huge" => "Huge (1GB)",
                _ => "Unknown",
            }
        };

        if min == max {
            // Restrictive: only one page size allowed
            println!("⚠️  Page Size Restricted: {} only - may limit available memory", format_size(min));
        } else if min == "large" && max == "huge" {
            // Default constraints
            println!("⚙️  Page Size Constraints: {} to {} (default)", format_size(min), format_size(max));
        } else {
            // Custom constraints
            println!("⚙️  Page Size Constraints: {} to {}", format_size(min), format_size(max));
        }
    }

    let allocated_blocks = match allocate_all_blocks_new(&thread_blocks, &runtime_config) {
        Ok(blocks) => blocks,
        Err(e) => {
            println!("❌ Failed to allocate memory blocks: {}\n", e);
            return false;
        }
    };

    // Display enhanced block allocation report using new reporting system
    {
        use crate::reporting::{create_console_reporter, converters};

        let report = converters::create_block_allocation_report_from_windows(&allocated_blocks);

        let mut reporter = create_console_reporter();
        if let Err(e) = reporter.report_block_allocation(&report) {
            log::error!("Failed to display block allocation report: {}", e);
        }
    }

    // Display detailed memory allocation tables using the modern reporting system
    {
        use crate::reporting::{create_console_reporter, models::{ThreadAllocationReport, ThreadAllocation}};
        
        let mut allocations = Vec::new();
        for (thread_id, blocks) in &allocated_blocks {
            let total_size: usize = blocks.iter().map(|b| b.buffer.size()).sum();
            
            // Calculate actual page counts based on memory size and page type
            let mut huge_pages = 0u64;
            let mut large_pages = 0u64;
            let mut regular_pages = 0u64;
            
            for block in blocks {
                let block_size = block.buffer.size() as u64;
                if block.buffer.uses_huge_pages() {
                    // 1GB huge pages
                    huge_pages += block_size.div_ceil(HUGE_PAGE_SIZE);
                } else if block.buffer.uses_large_pages() {
                    // 2MB large pages
                    large_pages += block_size.div_ceil(LARGE_PAGE_SIZE);
                } else {
                    // 4KB regular pages
                    regular_pages += block_size.div_ceil(REGULAR_PAGE_SIZE);
                }
            }
            
            allocations.push(ThreadAllocation {
                thread_id: *thread_id,
                total_size_bytes: total_size as u64,
                huge_pages_count: huge_pages,
                large_pages_count: large_pages,
                regular_pages_count: regular_pages,
            });
        }
        
        let report = ThreadAllocationReport { allocations };
        let mut reporter = create_console_reporter();
        if let Err(e) = reporter.report_thread_allocations(&report) {
            log::error!("Failed to display thread allocation table: {}", e);
        }
    }

    // Calculate progress tracking information and resolve auto-dispatch tests
    // Use config-driven tests if config is provided, otherwise use hard-coded defaults
    let mut test_definitions = if let Some(cfg) = config {
        match create_test_definitions_from_config(cfg, cache_info) {
            Ok(tests) => {
                log::info!("Using config-driven test sequence with {} tests", tests.len());
                tests
            }
            Err(e) => {
                println!("❌ Failed to load tests from config: {}", e);
                println!("   Falling back to default test suite");
                log::error!("Config test loading failed: {}", e);
                create_test_definitions(cache_info)
            }
        }
    } else {
        log::info!("Using default hard-coded test suite");
        create_test_definitions(cache_info)
    };

    // Apply single test filter if provided (supports glob patterns and comma-separated patterns)
    if let Some(test_name_filter) = single_test_filter {
        // Support comma-separated patterns: "Lat-*,Mem-*-Read"
        let patterns: Vec<&str> = test_name_filter.split(',').map(|s| s.trim()).collect();

        // Compile glob matchers for each pattern
        let matchers: Vec<GlobMatcher> = patterns.iter()
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

        if test_definitions.is_empty() {
            println!("❌ No tests match filter '{}'. Available tests:", test_name_filter);
            for def in create_test_definitions(cache_info) {
                println!("  - {}", def.actual_name);
            }
            return false;
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

    // Apply parameter override if provided (CLI parameter overrides subblock/stride config)
    if let Some(param_str) = parameter_override {
        log::info!("🔧 Applying CLI parameter override: {}", param_str);
        for def in test_definitions.iter_mut() {
            match param_str {
                "none" => {
                    def.config.parameter_context = None;
                }
                s if s.starts_with("subblocks:") => {
                    let n: u32 = s.split(':').nth(1).unwrap_or("2").parse().unwrap_or(2);
                    def.config.parameter_context = Some(crate::config::TestParameterContext {
                        raw_parameter: n,
                        subblock_count: Some(n),
                        ..Default::default()
                    });
                }
                s if s.starts_with("stride:") => {
                    let n: u32 = s.split(':').nth(1).unwrap_or("1").parse().unwrap_or(1);
                    def.config.parameter_context = Some(crate::config::TestParameterContext {
                        raw_parameter: n,
                        page_stride_bytes: Some((n as usize + 1) * 128),
                        ..Default::default()
                    });
                }
                _ => {}
            }
        }
    }

    // Apply pattern-mode override if provided
    if let Some(mode) = pattern_mode_override {
        log::info!("Applying CLI pattern-mode override: {}", mode);
        for def in test_definitions.iter_mut() {
            def.config.pattern_mode = Some(mode);
        }
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

    // Validate test parameter values (must be >= 1 and power-of-2 where required)
    for def in &test_definitions {
        if let Some(ref ctx) = def.config.parameter_context {
            let validate_param = |name: &str, val: u32, require_pow2: bool| {
                if val < 1 {
                    panic!("{}: {} must be >= 1, got {}", def.display_name, name, val);
                }
                if require_pow2 && val > 1 && !val.is_power_of_two() {
                    panic!("{}: {} must be power-of-2 when > 1, got {}", def.display_name, name, val);
                }
            };
            if let Some(v) = ctx.stride_patterns { validate_param("stride_patterns", v, true); }
            if let Some(v) = ctx.rng_sequences { validate_param("rng_sequences", v, true); }
            if let Some(v) = ctx.subdivisions { validate_param("subdivisions", v, true); }
            if let Some(v) = ctx.copy_directions { validate_param("copy_directions", v, false); }
        }
    }

    // Validate test plan dependency chain
    for (i, def) in test_definitions.iter().enumerate() {
        log::debug!("Test plan [{}] '{}': effect={}, skip_init={}",
            i + 1, def.display_name, def.memory_effect, def.config.skip_init);
    }
    let plan_warnings = validate_test_plan(&test_definitions);
    if !plan_warnings.is_empty() {
        println!("\n--- Test Plan Dependency Warnings ---");
        for (idx, msg) in &plan_warnings {
            println!("  [{}] {}", idx + 1, msg);
        }
        println!("  Dependent tests with unmet dependencies will run their own init phase.");
        println!("-------------------------------------\n");
        for (idx, msg) in &plan_warnings {
            log::warn!("Plan validation [test {}]: {}", idx + 1, msg);
        }
    }

    let tests_per_cycle = test_definitions.len() as u64;

    // cache_info is passed from main.rs (detected once at startup via get_system_info())
    // Used for calculating window sizes in CacheLevel mode during display and test execution

    // Display channels configuration before test table
    let effective_channels = channels_override
        .or_else(|| config.map(|c| c.system.channels))
        .unwrap_or(2);
    {
        let cl_bytes = cache_info.cache_line_size;
        println!("⚙  Memory Channels: {} (affects SimpleTest stride: [channels × param - 1] × {}B cache line)",
            effective_channels, cl_bytes);
    }

    // Print test configuration summary with resolved auto-dispatch names
    {
        use crate::reporting::{create_console_reporter, converters};
        let thread_count = allocated_blocks.len();
        let report = converters::create_test_configuration_report_v2(&test_definitions, &suite_timing, cache_info, thread_count);
        let mut reporter = create_console_reporter();
        if let Err(e) = reporter.report_test_configuration(&report) {
            log::error!("Failed to display test configuration report: {}", e);
        }
    }

    // Start progress reporter thread
    let progress_clone = Arc::clone(&progress);
    let progress_handle = thread::spawn(move || {
        progress_reporter(progress_clone);
    });

    // Create thread pool with pre-allocated blocks
    let thread_count = allocated_blocks.len();

    // Set thread_count and cache_line_bytes on all test configs
    let cache_info = crate::tests::get_cache_info();
    for test_def in &mut test_definitions {
        test_def.config.thread_count = thread_count;
        test_def.config.cache_line_bytes = cache_info.cache_line_size;
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
    };
    // Run cycles until a suite limit is reached. Both limits are checked *between* cycles, so a
    // cycle is never cut short by the clock; with neither limit set the suite runs until Ctrl+C or
    // an error halt, as the "Unlimited" in the banner says.
    let cycle_limit = suite_timing.global_cycles;
    let time_limit = suite_timing.global_duration_secs.map(|secs| Duration::from_secs(secs.into()));
    let suite_start = Instant::now();
    progress.begin_suite(suite_start, cycle_limit, time_limit, tests_per_cycle, thread_pool.live_progress());

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
    let final_success =
        outcome != RunOutcome::Halted && memory_errors == 0 && whea_totals.total == 0;

    // Display completion status
    if outcome == RunOutcome::Interrupted {
        println!("\n⚠️  Test suite interrupted by user (CTRL+C)");
    } else if final_success {
        println!("\n✅ All test cycles completed successfully!");
    } else {
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

    final_success
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
    } = ctx;

    use crate::progress::TestSummary;
    use crate::constants::MB_F64;

    // Track cycle timing and collect test summaries for this cycle
    let cycle_start = Instant::now();
    let mut cycle_test_summaries = Vec::new();

    let success = Arc::new(AtomicBool::new(true));
    let mut stopped = None;
    for (test_idx, test_def) in test_definitions.iter().enumerate() {
        let test_name = test_def.actual_name; // Use actual_name for thread pool (requires 'static)
        let test_display_name = &test_def.display_name; // Use display_name for results to preserve _A suffix
        let test_func = &test_def.function;
        let test_config = &test_def.config;
        if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
            stopped = Some(RunOutcome::Interrupted);
            break;
        }

        let test_start = Instant::now();
        progress.start_test(
            test_idx + 1,
            test_display_name,
            test_start,
            test_config.timing.duration_secs.map(|secs| Duration::from_secs(secs.into())),
            !matches!(test_func, TestFunction::Latency(_)),
        );

        // Baseline for this test's WHEA attribution. Drain first so anything queued from the
        // previous test is charged there, not here. Attribution is still approximate at a test
        // boundary — the OS logs WHEA asynchronously (kernel → ETW → EventLog service) — but the
        // run-level total below is exact regardless.
        progress.whea.poll();
        let whea_before = progress.whea.counts();

        // Execute test on all threads
        thread_pool.execute_test(test_name, test_func, test_config, error_mode);
        
		// Collect results from all threads
		let mut test_stats = Vec::new();
		let mut latency_results = Vec::new();  // Collect latency stats separately
		let mut total_bytes_for_test = 0u64;
		let mut total_errors_for_test = 0u64;
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
						// Store simplified stats: (thread_id, cpu_id, bytes, elapsed, errors, operations, cycles_completed)
						test_stats.push((result.thread_id, cpu_id,
									   result.total_bytes, result.elapsed_ms, result.total_errors, result.total_operations,
									   result.cycles_completed));

						// Collect latency stats if present (for latency tests)
						if let Some(lat_stats) = result.latency_stats {
							latency_results.push((result.thread_id, cpu_id, lat_stats));
						}
					}

					total_bytes_for_test += result.total_bytes;
					total_errors_for_test += result.total_errors;
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
						log::error!("Thread {} reported {} memory errors in test '{}'",
								  result.thread_id, result.total_errors, test_name);
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
        let throughput_mib_s = if test_duration_secs > 0.0 {
            (total_bytes_for_test as f64 / MB_F64) / test_duration_secs
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
        let ops_per_sec = if test_duration.as_secs_f64() > 0.0 {
            total_operations_for_test as f64 / test_duration.as_secs_f64()
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
        let test_passed = total_errors_for_test == 0 && whea_for_test.total == 0;
        let pass_indicator = if test_passed { "✅" } else { "❌" };

        // WHEA is always shown, even at zero, so a clean line still states that we were watching;
        // the C/UC split only appears when there is something to split. "errors" is the word every
        // table uses for these (the `Errors` column) — "data" would collide with the `Data` column,
        // which is bytes processed.
        println!("📊 Test report - Cycle {} - {}: {} {:.1}s, {} errors + {} WHEA{}, {:.2} GiB @ {:.1} MiB/s ({:.2} GiB/s), {} ops @ {} ops/s{}{}",
                 cycle,
                 test_def.display_name,
                 pass_indicator,
                 test_duration.as_secs_f64(),
                 total_errors_for_test,
                 whea_for_test.total,
                 whea_for_test.split_suffix(),
                 total_bytes_for_test as f64 / (1024.0 * 1024.0 * 1024.0),
                 (total_bytes_for_test as f64 / (1024.0 * 1024.0)) / test_duration.as_secs_f64(),
                 (total_bytes_for_test as f64 / (1024.0 * 1024.0 * 1024.0)) / test_duration.as_secs_f64(),
                 format_ops(total_operations_for_test),
                 format_ops(ops_per_sec as u64),
                 latency_info,
                 cycles_info);

        // Add config line with calculated window size (uses passed cache_info to avoid re-detection)
        {
            use crate::reporting::formatters::{ReportFormatter, DefaultFormatter};
            let formatter = DefaultFormatter::new();
            let window_str = formatter.format_window_mode_with_size(&test_def.config.window_mode, cache_info, thread_count);
            let chunk_str = formatter.format_chunk_mode_with_size(&test_def.config.chunk_mode, cache_info, thread_count);
            let param_str = match &test_def.config.parameter_context {
                Some(c) => {
                    if let Some(stride) = c.page_stride_bytes {
                        if stride > 0 { format!("PageStride({})", c.raw_parameter) }
                        else { "-".to_string() }
                    } else if let Some(sub) = c.subblock_count {
                        if sub >= 2 { format!("Subblocks({})", sub) }
                        else { "-".to_string() }
                    } else if let Some(stride_el) = c.stride_elements {
                        if stride_el > 0 { format!("Stride({})", stride_el) }
                        else { "-".to_string() }
                    } else { "-".to_string() }
                }
                None => "-".to_string(),
            };
            let mode_str = match test_def.config.pattern_mode {
                Some(m) => match m {
                    0 => "TM5-0".to_string(),
                    1 => "TM5-1".to_string(),
                    2 => "TM5-2".to_string(),
                    10 => "TMR-0".to_string(),
                    11 => "TMR-1".to_string(),
                    12 => "TMR-2".to_string(),
                    other => format!("{}", other),
                },
                None => "-".to_string(),
            };
            let reps_str = if test_def.config.test_reps != 1 || test_def.config.verify_reps != 1 || test_def.config.write_read_cycles != 1 {
                format!(", test_reps={}, verify_reps={}, wrc={}", test_def.config.test_reps, test_def.config.verify_reps, test_def.config.write_read_cycles)
            } else {
                String::new()
            };
            println!("   Config: parameter={}, mode={}, window={}, chunk={}, locality={}{}",
                     param_str,
                     mode_str,
                     window_str,
                     chunk_str,
                     if test_def.config.requires_locality { "yes" } else { "no" },
                     reps_str);
        }

        // Update progress tracker with test completion
        progress.complete_test(test_name, total_errors_for_test);

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
            name: test_display_name.to_string(), // Use display_name to preserve _A suffix for AUTO tests
            duration_ms: test_duration.as_millis(),
            bytes_processed: total_bytes_for_test,
            throughput_mib_s,
            errors: total_errors_for_test,
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

    // Add this cycle's results to TestRunResult, including a partial cycle that stopped early
    if !cycle_test_summaries.is_empty() {
        let cycle_duration_secs = cycle_start.elapsed().as_secs() as u32;
        if let Ok(mut result) = test_run_result.lock() {
            result.add_cycle(cycle as u32, cycle_duration_secs, cycle_test_summaries);
        } else {
            log::error!("Failed to lock test_run_result to add cycle {}", cycle);
        }
    }
    stopped
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

fn create_test_definitions(cache_info: &CacheInfo) -> Vec<TestDefinition> {
    // TSC frequency from the passed cache_info (detected once at startup)
    let tsc_freq = cache_info.tsc_frequency_ghz;

    let test_definitions = vec![
        // === CRITICAL: Full Memory Stuck Bit Test ===
        (
            "Mem-StuckBit",
            TestFunction::MultiBlock(stuck_bit_test_multi),
            TestMemoryConfig::new(
                WindowMode::FullAllocation,
                ChunkMode::Fraction { fraction: 0.0625 },
                false,
                false
            ).with_timing(TestTiming::cycles_only(1))
             .with_memory_type(None)
        ),

        // === StuckBitTest SIMD variants ===
        (
            "Mem-StuckBit128",
            TestFunction::MultiBlock(stuck_bit_test_128_multi),
            TestMemoryConfig::new(
                WindowMode::FullAllocation,
                ChunkMode::Fraction { fraction: 0.0625 },
                false,
                false
            ).with_timing(TestTiming::cycles_only(1))
             .with_memory_type(None)
        ),

        (
            "Mem-StuckBit256",
            TestFunction::MultiBlock(stuck_bit_test_256_multi),
            TestMemoryConfig::new(
                WindowMode::FullAllocation,
                ChunkMode::Fraction { fraction: 0.0625 },
                false,
                false
            ).with_timing(TestTiming::cycles_only(1))
             .with_memory_type(None)
        ),

        (
            "Mem-StuckBit512",
            TestFunction::MultiBlock(stuck_bit_test_512_multi),
            TestMemoryConfig::new(
                WindowMode::FullAllocation,
                ChunkMode::Fraction { fraction: 0.0625 },
                false,
                false
            ).with_timing(TestTiming::cycles_only(1))
             .with_memory_type(None)
        ),

        // === StuckBitTest Auto-dispatch (AVX-512 > AVX2 > SSE2 > scalar) ===
        // Placed last so results match the actual variant performance
        (
            "Mem-StuckBitAuto",
            TestFunction::MultiBlock(stuck_bit_test_auto_multi),
            TestMemoryConfig::new(
                WindowMode::FullAllocation,
                ChunkMode::Fraction { fraction: 0.0625 },
                false,
                false
            ).with_timing(TestTiming::cycles_only(1))
             .with_memory_type(None)
        ),

        // === StuckBit CLFLUSHOPT-verify variant (TODO #59) ===
        // Same 3-phase alternating-bit test, but each chunk is flushed out of cache between
        // write and verify, so the verify provably round-trips through DRAM instead of reading
        // the line it just wrote. User-mode replacement for UC driver memory: stops a valid
        // cached line from masking a flipped DRAM bit.
        //
        // These deliberately use an L2-sized chunk, NOT the plain variants' ~6% fraction.
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
        // (see `calculate_window_size_cpuid`), so on this 2 MiB-L2 part 0.5 resolves to 1 MiB at
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
        // its ~6% fraction vs this at Cache{L2} — the guarantee turns out to be free
        // (8T, MiB/s): scalar 43056 → 43763, 128 47681 → 45705, 256 48060 → 46965,
        // 512 46301 → 47886. Two are faster; all four are inside this box's noise. Both configs
        // are DRAM-bandwidth-bound and move the same total traffic, so the flush *reschedules*
        // the writeback rather than adding to it. (Contrast Mem-Refresh-FlushAuto at -22%: there
        // the write already streams to DRAM naturally, so the flush is pure added instruction
        // cost.) Single run — treat the ±4% spread as "no measurable difference".
        //
        // Auto-dispatch (one preset, not four): under flush the widths converge to a 4.8% spread
        // because all are DRAM-bound (see doc/simd_codegen_rules.md Rule 3), so per-width
        // registrations would measure the same number repeatedly. Set `flush_before_verify` in a
        // JSON config to pin a specific width.
        (
            "Mem-StuckBit-FlushAuto",
            TestFunction::MultiBlock(stuck_bit_test_auto_multi),
            TestMemoryConfig::new(
                WindowMode::FullAllocation,
                ChunkMode::Cache { target: CacheTarget::L2 { scale: 1.0 } },
                false,
                false
            ).with_timing(TestTiming::cycles_only(1))
             .with_memory_type(None)
             .with_flush_before_verify(true)
        ),

        // === SimpleTest NT (non-temporal stores) — bandwidth comparison ===
        (
            "Mem-SimpleNT-128",
            TestFunction::MultiBlock(simple_test_nt_128_multi),
            TestMemoryConfig::new(
                WindowMode::FullAllocation,
                ChunkMode::Absolute { size_bytes: 4 * MB },
                false,
                false
            ).with_timing(TestTiming::hybrid(100, 30))
             .with_memory_type(None)
        ),

        (
            "Mem-SimpleNT-256",
            TestFunction::MultiBlock(simple_test_nt_256_multi),
            TestMemoryConfig::new(
                WindowMode::FullAllocation,
                ChunkMode::Absolute { size_bytes: 4 * MB },
                false,
                false
            ).with_timing(TestTiming::hybrid(100, 30))
             .with_memory_type(None)
        ),

        (
            "Mem-SimpleNT-512",
            TestFunction::MultiBlock(simple_test_nt_512_multi),
            TestMemoryConfig::new(
                WindowMode::FullAllocation,
                ChunkMode::Absolute { size_bytes: 4 * MB },
                false,
                false
            ).with_timing(TestTiming::hybrid(100, 30))
             .with_memory_type(None)
        ),

        (
            "Mem-SimpleNT-Auto",
            TestFunction::MultiBlock(simple_test_nt_auto_multi),
            TestMemoryConfig::new(
                WindowMode::FullAllocation,
                ChunkMode::Absolute { size_bytes: 4 * MB },
                false,
                false
            ).with_timing(TestTiming::hybrid(100, 30))
             .with_memory_type(None)
        ),

        // === Refresh stability tests (MultiBlock implementations) ===
        (
            "Mem-Refresh",
            TestFunction::MultiBlock(refresh_stable_multi),
            TestMemoryConfig::new(
                WindowMode::CacheTotal { fraction: 2.0 },
                ChunkMode::Absolute { size_bytes: 2048 * MB },
                false,
                true
            ).with_timing(TestTiming::duration_only(15))
             .with_memory_type(None)
        ),

        (
            "Mem-Refresh128",
            TestFunction::MultiBlock(refresh_stable_128_multi),
            TestMemoryConfig::new(
                WindowMode::CacheTotal { fraction: 2.0 },
                ChunkMode::Absolute { size_bytes: 2048 * MB },
                false,
                true
            ).with_timing(TestTiming::duration_only(15))
             .with_memory_type(None)
        ),

        (
            "Mem-Refresh256",
            TestFunction::MultiBlock(refresh_stable_256_multi),
            TestMemoryConfig::new(
                WindowMode::CacheTotal { fraction: 2.0 },
                ChunkMode::Absolute { size_bytes: 2048 * MB },
                false,
                true
            ).with_timing(TestTiming::duration_only(15))
             .with_memory_type(None)
        ),

        (
            "Mem-Refresh512",
            TestFunction::MultiBlock(refresh_stable_512_multi),
            TestMemoryConfig::new(
                WindowMode::CacheTotal { fraction: 2.0 },
                ChunkMode::Absolute { size_bytes: 2048 * MB },
                false,
                true
            ).with_timing(TestTiming::duration_only(15))
             .with_memory_type(None)
        ),

        // === RefreshStable Auto-dispatch (AVX-512 > AVX2 > SSE2 > scalar) ===
        // Placed after the explicit widths so results match the actual variant performance
        (
            "Mem-RefreshAuto",
            TestFunction::MultiBlock(refresh_stable_auto_multi),
            TestMemoryConfig::new(
                WindowMode::CacheTotal { fraction: 2.0 },
                ChunkMode::Absolute { size_bytes: 2048 * MB },
                false,
                true
            ).with_timing(TestTiming::duration_only(15))
             .with_memory_type(None)
        ),

        // === Refresh CLFLUSHOPT-verify variant (TODO #59, opt-in) ===
        // Same test with the pre-sleep flush enabled, making the DRAM round-trip architecturally
        // guaranteed rather than relying on eviction policy. Not the default: the plain variants'
        // `CacheTotal 2.0x` window, multi-thread shared-L3 pressure, and forward verify order
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
                WindowMode::CacheTotal { fraction: 2.0 },
                ChunkMode::Absolute { size_bytes: 2048 * MB },
                false,
                true
            ).with_timing(TestTiming::duration_only(15))
             .with_memory_type(None)
             .with_flush_before_verify(true)
        ),

        // === Performance stress tests ===
        (
            "Mem-CacheBust",
            TestFunction::MultiBlock(cache_busting_multi),
            TestMemoryConfig::new(
                WindowMode::CacheTotal { fraction: 0.5 },
                ChunkMode::Absolute { size_bytes: MB },
                true,
                true
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
                WindowMode::FullAllocation,
                ChunkMode::Absolute { size_bytes: 8 * MB },
                true,
                false
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
                WindowMode::FullAllocation,
                ChunkMode::Absolute { size_bytes: 2 * MB },
                false,
                false
            ).with_timing(TestTiming::cycles_only(1))
             .with_parameter_context(crate::config::TestParameterContext {
                 raw_parameter: 4,
                 subdivisions: Some(4),
                 ..Default::default()
             })
             .with_memory_type(None)
        ),

        (
            "Mem-BlockMove",
            TestFunction::MultiBlock(block_move_multi),
            TestMemoryConfig::new(
                WindowMode::FullAllocation,
                ChunkMode::Absolute { size_bytes: 16 * MB },
                false,
                true
            ).with_timing(TestTiming::duration_only(20))
             .with_parameter_context(crate::config::TestParameterContext {
                 raw_parameter: 1,
                 copy_directions: Some(1),
                 ..Default::default()
             })
             .with_memory_type(None)
        ),

        // === v2 Tests — fixed PRNG, correct parameter interpretation, u64 MirrorMove ===

        // Mem-SimpleV2: matches v1 Mem-Simple config (FullAllocation, 4MB chunk, 100cycles/30s)
        (
            "Mem-SimpleV2",
            TestFunction::MultiBlock(simple_test_v2_multi),
            TestMemoryConfig::new(
                WindowMode::FullAllocation,
                ChunkMode::Absolute { size_bytes: 4 * MB },
                false,
                false
            ).with_timing(TestTiming::hybrid(100, 30))
             .with_memory_type(None)
        ),

        // Mem-SimpleV2 SIMD: matches scalar config for A/B comparison
        (
            "Mem-SimpleV2-128",
            TestFunction::MultiBlock(simple_test_v2_128_multi),
            TestMemoryConfig::new(
                WindowMode::FullAllocation,
                ChunkMode::Absolute { size_bytes: 4 * MB },
                false,
                false
            ).with_timing(TestTiming::hybrid(100, 30))
             .with_memory_type(None)
        ),

        (
            "Mem-SimpleV2-256",
            TestFunction::MultiBlock(simple_test_v2_256_multi),
            TestMemoryConfig::new(
                WindowMode::FullAllocation,
                ChunkMode::Absolute { size_bytes: 4 * MB },
                false,
                false
            ).with_timing(TestTiming::hybrid(100, 30))
             .with_memory_type(None)
        ),

        (
            "Mem-SimpleV2-512",
            TestFunction::MultiBlock(simple_test_v2_512_multi),
            TestMemoryConfig::new(
                WindowMode::FullAllocation,
                ChunkMode::Absolute { size_bytes: 4 * MB },
                false,
                false
            ).with_timing(TestTiming::hybrid(100, 30))
             .with_memory_type(None)
        ),

        (
            "Mem-SimpleV2-Auto",
            TestFunction::MultiBlock(simple_test_v2_auto_multi),
            TestMemoryConfig::new(
                WindowMode::FullAllocation,
                ChunkMode::Absolute { size_bytes: 4 * MB },
                false,
                false
            ).with_timing(TestTiming::hybrid(100, 30))
             .with_memory_type(None)
        ),

        // Mem-MirrorV2*: matches v1 Mem-Mirror config (FixedSize 64MB window, 4MB chunk, 10s)
        (
            "Mem-MirrorV2",
            TestFunction::MultiBlock(mirror_move_v2_multi),
            TestMemoryConfig::new(
                WindowMode::Absolute { size_bytes: 64 * MB },
                ChunkMode::Absolute { size_bytes: 4 * MB },
                false,
                true
            ).with_timing(TestTiming::duration_only(10))
             .with_memory_type(None)
        ),

        // v2 SIMD configs match v1 counterparts for fair A/B comparison
        (
            "Mem-MirrorV2-128",
            TestFunction::MultiBlock(mirror_move_v2_128_multi),
            TestMemoryConfig::new(
                WindowMode::Absolute { size_bytes: 64 * MB },
                ChunkMode::Absolute { size_bytes: 8 * MB },
                false,
                true
            ).with_timing(TestTiming::duration_only(10))
             .with_memory_type(None)
        ),

        (
            "Mem-MirrorV2-256",
            TestFunction::MultiBlock(mirror_move_v2_256_multi),
            TestMemoryConfig::new(
                WindowMode::Absolute { size_bytes: 128 * MB },
                ChunkMode::Absolute { size_bytes: 8 * MB },
                false,
                true
            ).with_timing(TestTiming::duration_only(10))
             .with_parameter_context(crate::config::TestParameterContext {
                 raw_parameter: 2,
                 subblock_count: Some(2),
                 ..Default::default()
             })
             .with_memory_type(None)
        ),

        (
            "Mem-MirrorV2-512",
            TestFunction::MultiBlock(mirror_move_v2_512_multi),
            TestMemoryConfig::new(
                WindowMode::Absolute { size_bytes: 256 * MB },
                ChunkMode::Absolute { size_bytes: 8 * MB },
                false,
                true
            ).with_timing(TestTiming::duration_only(10))
             .with_parameter_context(crate::config::TestParameterContext {
                 raw_parameter: 4,
                 subblock_count: Some(4),
                 ..Default::default()
             })
             .with_memory_type(None)
        ),

        (
            "Mem-MirrorV2-Auto",
            TestFunction::MultiBlock(mirror_move_v2_auto_multi),
            TestMemoryConfig::new(
                WindowMode::Absolute { size_bytes: 64 * MB },
                ChunkMode::Absolute { size_bytes: 8 * MB },
                false,
                true
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
                WindowMode::FullAllocation,
                ChunkMode::Absolute { size_bytes: 4 * MB },
                false, false
            ).with_timing(TestTiming::cycles_only(10))
             .with_pattern_config(Some(0), None, None)
             .with_skip_init(true)
             .with_memory_type(None)
        ),
        (
            "Bench-Init-TM5-1",
            TestFunction::MultiBlock(bench_init_multi),
            TestMemoryConfig::new(
                WindowMode::FullAllocation,
                ChunkMode::Absolute { size_bytes: 4 * MB },
                false, false
            ).with_timing(TestTiming::cycles_only(10))
             .with_pattern_config(Some(1), None, None)
             .with_skip_init(true)
             .with_memory_type(None)
        ),
        (
            "Bench-Init-TM5-2",
            TestFunction::MultiBlock(bench_init_multi),
            TestMemoryConfig::new(
                WindowMode::FullAllocation,
                ChunkMode::Absolute { size_bytes: 4 * MB },
                false, false
            ).with_timing(TestTiming::cycles_only(10))
             .with_pattern_config(Some(2), Some(0x5DEECE66D), Some(0xB))
             .with_skip_init(true)
             .with_memory_type(None)
        ),
        (
            "Bench-Init-TMR-0",
            TestFunction::MultiBlock(bench_init_multi),
            TestMemoryConfig::new(
                WindowMode::FullAllocation,
                ChunkMode::Absolute { size_bytes: 4 * MB },
                false, false
            ).with_timing(TestTiming::cycles_only(10))
             .with_pattern_config(Some(10), None, None)
             .with_skip_init(true)
             .with_memory_type(None)
        ),
        (
            "Bench-Init-TMR-1",
            TestFunction::MultiBlock(bench_init_multi),
            TestMemoryConfig::new(
                WindowMode::FullAllocation,
                ChunkMode::Absolute { size_bytes: 4 * MB },
                false, false
            ).with_timing(TestTiming::cycles_only(10))
             .with_pattern_config(Some(11), None, None)
             .with_skip_init(true)
             .with_memory_type(None)
        ),
        (
            "Bench-Init-TMR-2",
            TestFunction::MultiBlock(bench_init_multi),
            TestMemoryConfig::new(
                WindowMode::FullAllocation,
                ChunkMode::Absolute { size_bytes: 4 * MB },
                false, false
            ).with_timing(TestTiming::cycles_only(10))
             .with_pattern_config(Some(12), Some(0xDEADBEEFDEADBEEF), Some(0xCAFEBABECAFEBABE))
             .with_skip_init(true)
             .with_memory_type(None)
        ),
        (
            "Bench-Init-TMR-3",
            TestFunction::MultiBlock(bench_init_multi),
            TestMemoryConfig::new(
                WindowMode::FullAllocation,
                ChunkMode::Absolute { size_bytes: 4 * MB },
                false, false
            ).with_timing(TestTiming::cycles_only(10))
             .with_pattern_config(Some(13), Some(0xC0FFEE42C0FFEE42), None)
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
                WindowMode::FullAllocation,
                ChunkMode::Absolute { size_bytes: 4 * MB },
                false, false
            ).with_timing(TestTiming::cycles_only(10))
             .with_pattern_config(Some(0), None, None)
             .with_memory_type(None)
        ),
        (
            "Bench-Verify-TM5-1",
            TestFunction::MultiBlock(bench_verify_multi),
            TestMemoryConfig::new(
                WindowMode::FullAllocation,
                ChunkMode::Absolute { size_bytes: 4 * MB },
                false, false
            ).with_timing(TestTiming::cycles_only(10))
             .with_pattern_config(Some(1), None, None)
             .with_memory_type(None)
        ),
        (
            "Bench-Verify-TM5-2",
            TestFunction::MultiBlock(bench_verify_multi),
            TestMemoryConfig::new(
                WindowMode::FullAllocation,
                ChunkMode::Absolute { size_bytes: 4 * MB },
                false, false
            ).with_timing(TestTiming::cycles_only(10))
             .with_pattern_config(Some(2), Some(0x5DEECE66D), Some(0xB))
             .with_memory_type(None)
        ),
        (
            "Bench-Verify-TMR-0",
            TestFunction::MultiBlock(bench_verify_multi),
            TestMemoryConfig::new(
                WindowMode::FullAllocation,
                ChunkMode::Absolute { size_bytes: 4 * MB },
                false, false
            ).with_timing(TestTiming::cycles_only(10))
             .with_pattern_config(Some(10), None, None)
             .with_memory_type(None)
        ),
        (
            "Bench-Verify-TMR-1",
            TestFunction::MultiBlock(bench_verify_multi),
            TestMemoryConfig::new(
                WindowMode::FullAllocation,
                ChunkMode::Absolute { size_bytes: 4 * MB },
                false, false
            ).with_timing(TestTiming::cycles_only(10))
             .with_pattern_config(Some(11), None, None)
             .with_memory_type(None)
        ),
        (
            "Bench-Verify-TMR-2",
            TestFunction::MultiBlock(bench_verify_multi),
            TestMemoryConfig::new(
                WindowMode::FullAllocation,
                ChunkMode::Absolute { size_bytes: 4 * MB },
                false, false
            ).with_timing(TestTiming::cycles_only(10))
             .with_pattern_config(Some(12), Some(0xDEADBEEFDEADBEEF), Some(0xCAFEBABECAFEBABE))
             .with_memory_type(None)
        ),
        (
            "Bench-Verify-TMR-3",
            TestFunction::MultiBlock(bench_verify_multi),
            TestMemoryConfig::new(
                WindowMode::FullAllocation,
                ChunkMode::Absolute { size_bytes: 4 * MB },
                false, false
            ).with_timing(TestTiming::cycles_only(10))
             .with_pattern_config(Some(13), Some(0xC0FFEE42C0FFEE42), None)
             .with_memory_type(None)
        ),

        // === Cache Hierarchy Latency Tests ===
        // Uses WindowMode::Cache for automatic sizing based on detected cache
        // Naming: Lat-{Level}-{Operation} for easy filtering
        // tests=Lat-* (15 tests: L1/L2/L3/DRAM/DRAMFull × Read/Write/Copy)
        // tests=Lat-L3-* (3 tests: L3 × Read/Write/Copy)

        // L1 Cache - Read, Write, Copy
        (
            "Lat-L1-Read",
            TestFunction::Latency(read_latency_multi),
            TestMemoryConfig::new(
                WindowMode::Cache { target: CacheTarget::L1_DEFAULT },
                ChunkMode::Auto,
                false,
                false
            ).with_timing(TestTiming::duration_only(10))
             .with_tsc(tsc_freq)
             .with_memory_type(None)
        ),

        (
            "Lat-L1-Write",
            TestFunction::Latency(write_latency_multi),
            TestMemoryConfig::new(
                WindowMode::Cache { target: CacheTarget::L1_DEFAULT },
                ChunkMode::Auto,
                false,
                false
            ).with_timing(TestTiming::duration_only(10))
             .with_tsc(tsc_freq)
             .with_memory_type(None)
        ),

        (
            "Lat-L1-Copy",
            TestFunction::Latency(copy_latency_multi),
            TestMemoryConfig::new(
                WindowMode::Cache { target: CacheTarget::L1_DEFAULT },
                ChunkMode::Auto,
                false,
                false
            ).with_timing(TestTiming::duration_only(10))
             .with_tsc(tsc_freq)
             .with_memory_type(None)
        ),

        // L2 Cache - Read, Write, Copy
        (
            "Lat-L2-Read",
            TestFunction::Latency(read_latency_multi),
            TestMemoryConfig::new(
                WindowMode::Cache { target: CacheTarget::L2_DEFAULT },
                ChunkMode::Auto,
                false,
                false
            ).with_timing(TestTiming::duration_only(10))
             .with_tsc(tsc_freq)
             .with_memory_type(None)
        ),

        (
            "Lat-L2-Write",
            TestFunction::Latency(write_latency_multi),
            TestMemoryConfig::new(
                WindowMode::Cache { target: CacheTarget::L2_DEFAULT },
                ChunkMode::Auto,
                false,
                false
            ).with_timing(TestTiming::duration_only(10))
             .with_tsc(tsc_freq)
             .with_memory_type(None)
        ),

        (
            "Lat-L2-Copy",
            TestFunction::Latency(copy_latency_multi),
            TestMemoryConfig::new(
                WindowMode::Cache { target: CacheTarget::L2_DEFAULT },
                ChunkMode::Auto,
                false,
                false
            ).with_timing(TestTiming::duration_only(10))
             .with_tsc(tsc_freq)
             .with_memory_type(None)
        ),

        // L3 Cache - Read, Write, Copy
        (
            "Lat-L3-Read",
            TestFunction::Latency(read_latency_multi),
            TestMemoryConfig::new(
                WindowMode::Cache { target: CacheTarget::L3_DEFAULT },
                ChunkMode::Auto,
                false,
                false
            ).with_timing(TestTiming::duration_only(10))
             .with_tsc(tsc_freq)
             .with_memory_type(None)
        ),

        (
            "Lat-L3-Write",
            TestFunction::Latency(write_latency_multi),
            TestMemoryConfig::new(
                WindowMode::Cache { target: CacheTarget::L3_DEFAULT },
                ChunkMode::Auto,
                false,
                false
            ).with_timing(TestTiming::duration_only(10))
             .with_tsc(tsc_freq)
             .with_memory_type(None)
        ),

        (
            "Lat-L3-Copy",
            TestFunction::Latency(copy_latency_multi),
            TestMemoryConfig::new(
                WindowMode::Cache { target: CacheTarget::L3_DEFAULT },
                ChunkMode::Auto,
                false,
                false
            ).with_timing(TestTiming::duration_only(10))
             .with_tsc(tsc_freq)
             .with_memory_type(None)
        ),

        // DRAM - High TLB hit rate (typical latency) - Read, Write, Copy
        (
            "Lat-DRAM-Read",
            TestFunction::Latency(read_latency_multi),
            TestMemoryConfig::new(
                WindowMode::Cache { target: CacheTarget::DRAM_DEFAULT },
                ChunkMode::Auto,
                false,
                false
            ).with_timing(TestTiming::duration_only(10))
             .with_tsc(tsc_freq)
             .with_memory_type(None)
        ),

        (
            "Lat-DRAM-Write",
            TestFunction::Latency(write_latency_multi),
            TestMemoryConfig::new(
                WindowMode::Cache { target: CacheTarget::DRAM_DEFAULT },
                ChunkMode::Auto,
                false,
                false
            ).with_timing(TestTiming::duration_only(10))
             .with_tsc(tsc_freq)
             .with_memory_type(None)
        ),

        (
            "Lat-DRAM-Copy",
            TestFunction::Latency(copy_latency_multi),
            TestMemoryConfig::new(
                WindowMode::Cache { target: CacheTarget::DRAM_DEFAULT },
                ChunkMode::Auto,
                false,
                false
            ).with_timing(TestTiming::duration_only(10))
             .with_tsc(tsc_freq)
             .with_memory_type(None)
        ),

        // DRAMFull - Low TLB hit rate (stress with page miss overhead) - Read, Write, Copy
        (
            "Lat-DRAMFull-Read",
            TestFunction::Latency(read_latency_multi),
            TestMemoryConfig::new(
                WindowMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT },
                ChunkMode::Auto,
                false,
                false
            ).with_timing(TestTiming::duration_only(10))
             .with_tsc(tsc_freq)
             .with_memory_type(None)
        ),

        (
            "Lat-DRAMFull-Write",
            TestFunction::Latency(write_latency_multi),
            TestMemoryConfig::new(
                WindowMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT },
                ChunkMode::Auto,
                false,
                false
            ).with_timing(TestTiming::duration_only(10))
             .with_tsc(tsc_freq)
             .with_memory_type(None)
        ),

        (
            "Lat-DRAMFull-Copy",
            TestFunction::Latency(copy_latency_multi),
            TestMemoryConfig::new(
                WindowMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT },
                ChunkMode::Auto,
                false,
                false
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
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L1_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-L1-Read-128",
            TestFunction::Latency(lat_v2_read_128_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L1_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-L1-Read-256",
            TestFunction::Latency(lat_v2_read_256_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L1_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-L1-Read-512",
            TestFunction::Latency(lat_v2_read_512_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L1_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-L2-Read-Auto",
            TestFunction::Latency(lat_v2_read_auto_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L2_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-L2-Read-128",
            TestFunction::Latency(lat_v2_read_128_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L2_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-L2-Read-256",
            TestFunction::Latency(lat_v2_read_256_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L2_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-L2-Read-512",
            TestFunction::Latency(lat_v2_read_512_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L2_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-L3-Read-Auto",
            TestFunction::Latency(lat_v2_read_auto_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L3_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-L3-Read-128",
            TestFunction::Latency(lat_v2_read_128_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L3_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-L3-Read-256",
            TestFunction::Latency(lat_v2_read_256_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L3_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-L3-Read-512",
            TestFunction::Latency(lat_v2_read_512_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L3_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-DRAM-Read-Auto",
            TestFunction::Latency(lat_v2_read_auto_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-DRAM-Read-128",
            TestFunction::Latency(lat_v2_read_128_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-DRAM-Read-256",
            TestFunction::Latency(lat_v2_read_256_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-DRAM-Read-512",
            TestFunction::Latency(lat_v2_read_512_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-DRAMFull-Read-Auto",
            TestFunction::Latency(lat_v2_read_auto_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-DRAMFull-Read-128",
            TestFunction::Latency(lat_v2_read_128_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-DRAMFull-Read-256",
            TestFunction::Latency(lat_v2_read_256_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-DRAMFull-Read-512",
            TestFunction::Latency(lat_v2_read_512_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),

        // Layout A Write — Lat-V2-Write (5 tiers × {Auto,128,256,512} = 20 tests)
        // Single SIMD-width cached store per chain step. Baseline: predicted to track
        // Lat-V2-Read at every tier because one in-flight write fits behind any chain
        // miss. Validates that store-buffer pressure requires concurrency density.
        (
            "Lat-V2-L1-Write-Auto",
            TestFunction::Latency(lat_v2_write_auto_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L1_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-L1-Write-128",
            TestFunction::Latency(lat_v2_write_128_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L1_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-L1-Write-256",
            TestFunction::Latency(lat_v2_write_256_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L1_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-L1-Write-512",
            TestFunction::Latency(lat_v2_write_512_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L1_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-L2-Write-Auto",
            TestFunction::Latency(lat_v2_write_auto_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L2_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-L2-Write-128",
            TestFunction::Latency(lat_v2_write_128_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L2_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-L2-Write-256",
            TestFunction::Latency(lat_v2_write_256_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L2_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-L2-Write-512",
            TestFunction::Latency(lat_v2_write_512_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L2_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-L3-Write-Auto",
            TestFunction::Latency(lat_v2_write_auto_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L3_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-L3-Write-128",
            TestFunction::Latency(lat_v2_write_128_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L3_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-L3-Write-256",
            TestFunction::Latency(lat_v2_write_256_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L3_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-L3-Write-512",
            TestFunction::Latency(lat_v2_write_512_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L3_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-DRAM-Write-Auto",
            TestFunction::Latency(lat_v2_write_auto_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-DRAM-Write-128",
            TestFunction::Latency(lat_v2_write_128_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-DRAM-Write-256",
            TestFunction::Latency(lat_v2_write_256_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-DRAM-Write-512",
            TestFunction::Latency(lat_v2_write_512_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-DRAMFull-Write-Auto",
            TestFunction::Latency(lat_v2_write_auto_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-DRAMFull-Write-128",
            TestFunction::Latency(lat_v2_write_128_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-DRAMFull-Write-256",
            TestFunction::Latency(lat_v2_write_256_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-DRAMFull-Write-512",
            TestFunction::Latency(lat_v2_write_512_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),

        // Layout A Copy — Lat-V2-Copy (5 tiers × {Auto,128,256,512} = 20 tests)
        // Single read-modify-write per chain step. Baseline: predicted to track Lat-V2-Read
        // closely because the load+XOR+store dependency chain is short and one in-flight
        // RMW fits behind any chain miss. Validates L1 dependency-chain limit findings.
        (
            "Lat-V2-L1-Copy-Auto",
            TestFunction::Latency(lat_v2_copy_auto_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L1_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-L1-Copy-128",
            TestFunction::Latency(lat_v2_copy_128_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L1_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-L1-Copy-256",
            TestFunction::Latency(lat_v2_copy_256_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L1_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-L1-Copy-512",
            TestFunction::Latency(lat_v2_copy_512_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L1_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-L2-Copy-Auto",
            TestFunction::Latency(lat_v2_copy_auto_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L2_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-L2-Copy-128",
            TestFunction::Latency(lat_v2_copy_128_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L2_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-L2-Copy-256",
            TestFunction::Latency(lat_v2_copy_256_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L2_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-L2-Copy-512",
            TestFunction::Latency(lat_v2_copy_512_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L2_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-L3-Copy-Auto",
            TestFunction::Latency(lat_v2_copy_auto_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L3_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-L3-Copy-128",
            TestFunction::Latency(lat_v2_copy_128_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L3_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-L3-Copy-256",
            TestFunction::Latency(lat_v2_copy_256_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L3_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-L3-Copy-512",
            TestFunction::Latency(lat_v2_copy_512_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L3_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-DRAM-Copy-Auto",
            TestFunction::Latency(lat_v2_copy_auto_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-DRAM-Copy-128",
            TestFunction::Latency(lat_v2_copy_128_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-DRAM-Copy-256",
            TestFunction::Latency(lat_v2_copy_256_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-DRAM-Copy-512",
            TestFunction::Latency(lat_v2_copy_512_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-DRAMFull-Copy-Auto",
            TestFunction::Latency(lat_v2_copy_auto_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-DRAMFull-Copy-128",
            TestFunction::Latency(lat_v2_copy_128_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-DRAMFull-Copy-256",
            TestFunction::Latency(lat_v2_copy_256_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2-DRAMFull-Copy-512",
            TestFunction::Latency(lat_v2_copy_512_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),

        // Layout B Packed — Lat-V2P-Read (5 tiers × {Auto,128,256,512} = 20 tests)
        (
            "Lat-V2P-L1-Read-Auto",
            TestFunction::Latency(lat_v2p_read_auto_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L1_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L1-Read-128",
            TestFunction::Latency(lat_v2p_read_128_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L1_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L1-Read-256",
            TestFunction::Latency(lat_v2p_read_256_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L1_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L1-Read-512",
            TestFunction::Latency(lat_v2p_read_512_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L1_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L2-Read-Auto",
            TestFunction::Latency(lat_v2p_read_auto_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L2_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L2-Read-128",
            TestFunction::Latency(lat_v2p_read_128_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L2_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L2-Read-256",
            TestFunction::Latency(lat_v2p_read_256_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L2_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L2-Read-512",
            TestFunction::Latency(lat_v2p_read_512_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L2_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L3-Read-Auto",
            TestFunction::Latency(lat_v2p_read_auto_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L3_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L3-Read-128",
            TestFunction::Latency(lat_v2p_read_128_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L3_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L3-Read-256",
            TestFunction::Latency(lat_v2p_read_256_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L3_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L3-Read-512",
            TestFunction::Latency(lat_v2p_read_512_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L3_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAM-Read-Auto",
            TestFunction::Latency(lat_v2p_read_auto_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAM-Read-128",
            TestFunction::Latency(lat_v2p_read_128_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAM-Read-256",
            TestFunction::Latency(lat_v2p_read_256_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAM-Read-512",
            TestFunction::Latency(lat_v2p_read_512_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAMFull-Read-Auto",
            TestFunction::Latency(lat_v2p_read_auto_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAMFull-Read-128",
            TestFunction::Latency(lat_v2p_read_128_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAMFull-Read-256",
            TestFunction::Latency(lat_v2p_read_256_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAMFull-Read-512",
            TestFunction::Latency(lat_v2p_read_512_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT }, ChunkMode::Auto, false, false)
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
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L1_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L1-Write-128",
            TestFunction::Latency(lat_v2p_write_128_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L1_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L1-Write-256",
            TestFunction::Latency(lat_v2p_write_256_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L1_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L1-Write-512",
            TestFunction::Latency(lat_v2p_write_512_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L1_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L2-Write-Auto",
            TestFunction::Latency(lat_v2p_write_auto_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L2_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L2-Write-128",
            TestFunction::Latency(lat_v2p_write_128_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L2_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L2-Write-256",
            TestFunction::Latency(lat_v2p_write_256_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L2_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L2-Write-512",
            TestFunction::Latency(lat_v2p_write_512_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L2_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L3-Write-Auto",
            TestFunction::Latency(lat_v2p_write_auto_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L3_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L3-Write-128",
            TestFunction::Latency(lat_v2p_write_128_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L3_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L3-Write-256",
            TestFunction::Latency(lat_v2p_write_256_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L3_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L3-Write-512",
            TestFunction::Latency(lat_v2p_write_512_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L3_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAM-Write-Auto",
            TestFunction::Latency(lat_v2p_write_auto_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAM-Write-128",
            TestFunction::Latency(lat_v2p_write_128_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAM-Write-256",
            TestFunction::Latency(lat_v2p_write_256_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAM-Write-512",
            TestFunction::Latency(lat_v2p_write_512_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAMFull-Write-Auto",
            TestFunction::Latency(lat_v2p_write_auto_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAMFull-Write-128",
            TestFunction::Latency(lat_v2p_write_128_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAMFull-Write-256",
            TestFunction::Latency(lat_v2p_write_256_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAMFull-Write-512",
            TestFunction::Latency(lat_v2p_write_512_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT }, ChunkMode::Auto, false, false)
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
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L1_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L1-Copy-128",
            TestFunction::Latency(lat_v2p_copy_128_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L1_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L1-Copy-256",
            TestFunction::Latency(lat_v2p_copy_256_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L1_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L1-Copy-512",
            TestFunction::Latency(lat_v2p_copy_512_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L1_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L2-Copy-Auto",
            TestFunction::Latency(lat_v2p_copy_auto_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L2_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L2-Copy-128",
            TestFunction::Latency(lat_v2p_copy_128_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L2_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L2-Copy-256",
            TestFunction::Latency(lat_v2p_copy_256_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L2_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L2-Copy-512",
            TestFunction::Latency(lat_v2p_copy_512_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L2_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L3-Copy-Auto",
            TestFunction::Latency(lat_v2p_copy_auto_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L3_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L3-Copy-128",
            TestFunction::Latency(lat_v2p_copy_128_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L3_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L3-Copy-256",
            TestFunction::Latency(lat_v2p_copy_256_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L3_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L3-Copy-512",
            TestFunction::Latency(lat_v2p_copy_512_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L3_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAM-Copy-Auto",
            TestFunction::Latency(lat_v2p_copy_auto_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAM-Copy-128",
            TestFunction::Latency(lat_v2p_copy_128_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAM-Copy-256",
            TestFunction::Latency(lat_v2p_copy_256_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAM-Copy-512",
            TestFunction::Latency(lat_v2p_copy_512_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAMFull-Copy-Auto",
            TestFunction::Latency(lat_v2p_copy_auto_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAMFull-Copy-128",
            TestFunction::Latency(lat_v2p_copy_128_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAMFull-Copy-256",
            TestFunction::Latency(lat_v2p_copy_256_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAMFull-Copy-512",
            TestFunction::Latency(lat_v2p_copy_512_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),

        // Layout B WriteFull — full-cache-line cached writes (5 tiers × {Auto,128,256,512} = 20 tests)
        // Compares to Lat-V2P-Write to isolate the byte-coverage variable. Same chain
        // structure but every width writes the full 64-byte cache line per cell.
        (
            "Lat-V2P-L1-WriteFull-Auto",
            TestFunction::Latency(lat_v2p_write_full_auto_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L1_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L1-WriteFull-128",
            TestFunction::Latency(lat_v2p_write_full_128_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L1_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L1-WriteFull-256",
            TestFunction::Latency(lat_v2p_write_full_256_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L1_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L1-WriteFull-512",
            TestFunction::Latency(lat_v2p_write_full_512_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L1_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L2-WriteFull-Auto",
            TestFunction::Latency(lat_v2p_write_full_auto_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L2_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L2-WriteFull-128",
            TestFunction::Latency(lat_v2p_write_full_128_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L2_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L2-WriteFull-256",
            TestFunction::Latency(lat_v2p_write_full_256_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L2_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L2-WriteFull-512",
            TestFunction::Latency(lat_v2p_write_full_512_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L2_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L3-WriteFull-Auto",
            TestFunction::Latency(lat_v2p_write_full_auto_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L3_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L3-WriteFull-128",
            TestFunction::Latency(lat_v2p_write_full_128_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L3_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L3-WriteFull-256",
            TestFunction::Latency(lat_v2p_write_full_256_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L3_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L3-WriteFull-512",
            TestFunction::Latency(lat_v2p_write_full_512_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L3_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAM-WriteFull-Auto",
            TestFunction::Latency(lat_v2p_write_full_auto_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAM-WriteFull-128",
            TestFunction::Latency(lat_v2p_write_full_128_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAM-WriteFull-256",
            TestFunction::Latency(lat_v2p_write_full_256_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAM-WriteFull-512",
            TestFunction::Latency(lat_v2p_write_full_512_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAMFull-WriteFull-Auto",
            TestFunction::Latency(lat_v2p_write_full_auto_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAMFull-WriteFull-128",
            TestFunction::Latency(lat_v2p_write_full_128_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAMFull-WriteFull-256",
            TestFunction::Latency(lat_v2p_write_full_256_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAMFull-WriteFull-512",
            TestFunction::Latency(lat_v2p_write_full_512_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),

        // Layout B CopyFull — full-cache-line RMW (5 tiers × {Auto,128,256,512} = 20 tests)
        // Compares to Lat-V2P-Copy to isolate the byte-coverage variable. Tests whether
        // the L3 bimodal distribution is byte-coverage driven or SIMD-path driven.
        (
            "Lat-V2P-L1-CopyFull-Auto",
            TestFunction::Latency(lat_v2p_copy_full_auto_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L1_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L1-CopyFull-128",
            TestFunction::Latency(lat_v2p_copy_full_128_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L1_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L1-CopyFull-256",
            TestFunction::Latency(lat_v2p_copy_full_256_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L1_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L1-CopyFull-512",
            TestFunction::Latency(lat_v2p_copy_full_512_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L1_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L2-CopyFull-Auto",
            TestFunction::Latency(lat_v2p_copy_full_auto_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L2_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L2-CopyFull-128",
            TestFunction::Latency(lat_v2p_copy_full_128_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L2_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L2-CopyFull-256",
            TestFunction::Latency(lat_v2p_copy_full_256_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L2_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L2-CopyFull-512",
            TestFunction::Latency(lat_v2p_copy_full_512_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L2_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L3-CopyFull-Auto",
            TestFunction::Latency(lat_v2p_copy_full_auto_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L3_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L3-CopyFull-128",
            TestFunction::Latency(lat_v2p_copy_full_128_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L3_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L3-CopyFull-256",
            TestFunction::Latency(lat_v2p_copy_full_256_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L3_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-L3-CopyFull-512",
            TestFunction::Latency(lat_v2p_copy_full_512_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::L3_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAM-CopyFull-Auto",
            TestFunction::Latency(lat_v2p_copy_full_auto_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAM-CopyFull-128",
            TestFunction::Latency(lat_v2p_copy_full_128_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAM-CopyFull-256",
            TestFunction::Latency(lat_v2p_copy_full_256_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAM-CopyFull-512",
            TestFunction::Latency(lat_v2p_copy_full_512_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAMFull-CopyFull-Auto",
            TestFunction::Latency(lat_v2p_copy_full_auto_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAMFull-CopyFull-128",
            TestFunction::Latency(lat_v2p_copy_full_128_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAMFull-CopyFull-256",
            TestFunction::Latency(lat_v2p_copy_full_256_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-V2P-DRAMFull-CopyFull-512",
            TestFunction::Latency(lat_v2p_copy_full_512_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),

        // NT Write saturation — Lat-NTW-DRAM-Write ({Auto,Scalar,128,256,512} = 5 tests)
        // NT stores bypass cache hierarchy — only the DRAM tier exists. Width variants exposed
        // because per-store commit time differs (wider stores fill cache lines in fewer ops).
        // Scalar variant uses 8-byte MOVNTI to validate the WCB-fill-ratio hypothesis.
        (
            "Lat-NTW-DRAM-Write-Auto",
            TestFunction::Latency(lat_ntw_write_auto_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-NTW-DRAM-Write-Scalar",
            TestFunction::Latency(lat_ntw_write_scalar_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-NTW-DRAM-Write-128",
            TestFunction::Latency(lat_ntw_write_128_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-NTW-DRAM-Write-256",
            TestFunction::Latency(lat_ntw_write_256_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),
        (
            "Lat-NTW-DRAM-Write-512",
            TestFunction::Latency(lat_ntw_write_512_multi),
            TestMemoryConfig::new(WindowMode::Cache { target: CacheTarget::DRAM_DEFAULT }, ChunkMode::Auto, false, false)
                .with_timing(TestTiming::duration_only(10)).with_tsc(tsc_freq).with_memory_type(None)
        ),

        // === Sequential Bandwidth Tests (Auto-dispatch) ===
        // Measures peak sequential memory bandwidth (unlike latency tests which use random access)
        // Naming: Spd-{Level}-{Operation}-Auto → resolves to Spd-{Level}-{Operation}-{128,256,512}_A
        // Uses WindowMode::Cache for automatic sizing based on detected cache
        // tests=Spd-* (all 15 bandwidth tests: L1/L2/L3/DRAMSmall/DRAMFull × Read/Write/Copy)
        // tests=Spd-L3-* (3 tests: L3 × Read/Write/Copy)
        // tests=Spd-DRAM* (6 tests: DRAMSmall + DRAMFull)

        // L1 Cache Bandwidth - Read, Write, Copy
        (
            "Spd-L1-Read-Auto",
            TestFunction::MultiBlock(spd_read_auto_multi),
            TestMemoryConfig::new(
                WindowMode::Cache { target: CacheTarget::L1_DEFAULT },
                ChunkMode::Auto,
                false,
                false
            ).with_timing(TestTiming::duration_only(10))
             .with_memory_type(None)
        ),

        (
            "Spd-L1-Write-Auto",
            TestFunction::MultiBlock(spd_write_auto_multi),
            TestMemoryConfig::new(
                WindowMode::Cache { target: CacheTarget::L1_DEFAULT },
                ChunkMode::Auto,
                false,
                false
            ).with_timing(TestTiming::duration_only(10))
             .with_memory_type(None)
        ),

        (
            "Spd-L1-Copy-Auto",
            TestFunction::MultiBlock(spd_copy_auto_multi),
            TestMemoryConfig::new(
                WindowMode::Cache { target: CacheTarget::L1_DEFAULT },
                ChunkMode::Auto,
                false,
                false
            ).with_timing(TestTiming::duration_only(10))
             .with_memory_type(None)
        ),

        // L2 Cache Bandwidth - Read, Write, Copy
        (
            "Spd-L2-Read-Auto",
            TestFunction::MultiBlock(spd_read_auto_multi),
            TestMemoryConfig::new(
                WindowMode::Cache { target: CacheTarget::L2_DEFAULT },
                ChunkMode::Auto,
                false,
                false
            ).with_timing(TestTiming::duration_only(10))
             .with_memory_type(None)
        ),

        (
            "Spd-L2-Write-Auto",
            TestFunction::MultiBlock(spd_write_auto_multi),
            TestMemoryConfig::new(
                WindowMode::Cache { target: CacheTarget::L2_DEFAULT },
                ChunkMode::Auto,
                false,
                false
            ).with_timing(TestTiming::duration_only(10))
             .with_memory_type(None)
        ),

        (
            "Spd-L2-Copy-Auto",
            TestFunction::MultiBlock(spd_copy_auto_multi),
            TestMemoryConfig::new(
                WindowMode::Cache { target: CacheTarget::L2_DEFAULT },
                ChunkMode::Auto,
                false,
                false
            ).with_timing(TestTiming::duration_only(10))
             .with_memory_type(None)
        ),

        // L3 Cache Bandwidth - Read, Write, Copy
        (
            "Spd-L3-Read-Auto",
            TestFunction::MultiBlock(spd_read_auto_multi),
            TestMemoryConfig::new(
                WindowMode::Cache { target: CacheTarget::L3_DEFAULT },
                ChunkMode::Auto,
                false,
                false
            ).with_timing(TestTiming::duration_only(10))
             .with_memory_type(None)
        ),

        (
            "Spd-L3-Write-Auto",
            TestFunction::MultiBlock(spd_write_auto_multi),
            TestMemoryConfig::new(
                WindowMode::Cache { target: CacheTarget::L3_DEFAULT },
                ChunkMode::Auto,
                false,
                false
            ).with_timing(TestTiming::duration_only(10))
             .with_memory_type(None)
        ),

        (
            "Spd-L3-Copy-Auto",
            TestFunction::MultiBlock(spd_copy_auto_multi),
            TestMemoryConfig::new(
                WindowMode::Cache { target: CacheTarget::L3_DEFAULT },
                ChunkMode::Auto,
                false,
                false
            ).with_timing(TestTiming::duration_only(10))
             .with_memory_type(None)
        ),

        // DRAMSmall Bandwidth - Smaller working set, high TLB hit rate - Read, Write, Copy
        (
            "Spd-DRAMSmall-Read-Auto",
            TestFunction::MultiBlock(spd_read_auto_multi),
            TestMemoryConfig::new(
                WindowMode::Cache { target: CacheTarget::DRAM_DEFAULT },
                ChunkMode::Auto,
                false,
                false
            ).with_timing(TestTiming::duration_only(10))
             .with_memory_type(None)
        ),

        (
            "Spd-DRAMSmall-Write-Auto",
            TestFunction::MultiBlock(spd_write_nt_auto_multi),
            TestMemoryConfig::new(
                WindowMode::Cache { target: CacheTarget::DRAM_DEFAULT },
                ChunkMode::Auto,
                false,
                false
            ).with_timing(TestTiming::duration_only(10))
             .with_memory_type(None)
        ),

        (
            "Spd-DRAMSmall-Copy-Auto",
            TestFunction::MultiBlock(spd_copy_nt_auto_multi),
            TestMemoryConfig::new(
                WindowMode::Cache { target: CacheTarget::DRAM_DEFAULT },
                ChunkMode::Auto,
                false,
                false
            ).with_timing(TestTiming::duration_only(10))
             .with_memory_type(None)
        ),

        // DRAMFull Bandwidth - Full allocation, includes TLB miss overhead - Read, Write, Copy
        (
            "Spd-DRAMFull-Read-Auto",
            TestFunction::MultiBlock(spd_read_auto_multi),
            TestMemoryConfig::new(
                WindowMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT },
                ChunkMode::Auto,
                false,
                false
            ).with_timing(TestTiming::duration_only(10))
             .with_memory_type(None)
        ),

        (
            "Spd-DRAMFull-Write-Auto",
            TestFunction::MultiBlock(spd_write_nt_auto_multi),
            TestMemoryConfig::new(
                WindowMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT },
                ChunkMode::Auto,
                false,
                false
            ).with_timing(TestTiming::duration_only(10))
             .with_memory_type(None)
        ),

        (
            "Spd-DRAMFull-Copy-Auto",
            TestFunction::MultiBlock(spd_copy_nt_auto_multi),
            TestMemoryConfig::new(
                WindowMode::Cache { target: CacheTarget::DRAM_FULL_DEFAULT },
                ChunkMode::Auto,
                false,
                false
            ).with_timing(TestTiming::duration_only(10))
             .with_memory_type(None)
        ),
    ];
    
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
            });
        }
    }

    resolved_tests
}

/// Create test definitions from config file (config-driven test execution)
fn create_test_definitions_from_config(config: &crate::config::ModernConfig, cache_info: &CacheInfo) -> Result<Vec<TestDefinition>, String> {
    let _tsc_freq = cache_info.tsc_frequency_ghz; // Available for latency tests in config
    let mut resolved_tests = Vec::new();

    // Get enabled tests from config
    let test_configs = config.get_test_configs();

    for (test_name, test_config) in test_configs {

        // Look up function by name using the test registry
        let test_function = crate::tests::get_test_function_by_name(test_name)
            .ok_or_else(|| format!("Unknown test function '{}' in config", test_name))?;

        // Check for auto-dispatch
        if let Some((resolved_name, resolved_function)) = resolve_auto_dispatch_test(test_name) {
            log::debug!("Auto-dispatch: {} → {} (based on CPU capabilities)", test_name, resolved_name);
            // Convert to 'static str by leaking (safe for test names, small and finite set)
            let static_original_name: &'static str = Box::leak(test_name.to_string().into_boxed_str());
            let effect = derive_memory_effect(resolved_name, &test_config);
            let display_name = format!("{}_A", resolved_name);
            resolved_tests.push(TestDefinition {
                actual_name: resolved_name,
                // Mirror onto the config so per-thread logs show the resolved+suffixed name
                config: test_config.with_display_name(&display_name),
                display_name,
                function: resolved_function,
                original_name: Some(static_original_name), // Preserve original Auto name
                memory_effect: effect,
            });
        } else {
            // Convert to 'static str by leaking (safe for test names, small and finite set)
            let static_name: &'static str = Box::leak(test_name.to_string().into_boxed_str());
            let effect = derive_memory_effect(test_name, &test_config);
            resolved_tests.push(TestDefinition {
                actual_name: static_name,
                display_name: test_name.to_string(),
                function: test_function,
                config: test_config.with_display_name(test_name),
                original_name: None,
                memory_effect: effect,
            });
        }
    }

    if resolved_tests.is_empty() {
        return Err("No enabled tests found in configuration".to_string());
    }

    Ok(resolved_tests)
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
            // Use all available CPUs by default
            let total_cpus = num_cpus::get();
            Some((0..total_cpus).collect())
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
        SetThreadAffinityMask, SetThreadIdealProcessorEx, GetCurrentThread
    };
    use windows::Win32::System::Kernel::PROCESSOR_NUMBER;
    
    unsafe {
        let thread_handle = GetCurrentThread();
        
        // Method 1: Set thread affinity mask (hard pinning)
        let affinity_mask = 1u64 << cpu_id;
        if SetThreadAffinityMask(thread_handle, affinity_mask as usize) == 0 {
            return Err(format!("SetThreadAffinityMask failed for thread {} CPU {}", thread_id, cpu_id));
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
                log::warn!("SetThreadIdealProcessorEx failed for thread {} CPU {}: {:?}, but affinity mask succeeded", 
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

fn allocate_all_blocks_new(thread_blocks: &HashMap<usize, Vec<BlockInfo>>, runtime_config: &RuntimeConfig) -> Result<HashMap<usize, Vec<AllocationBlock>>, String> {
    use crate::memory::allocator::AllocationStrategy;

    // Determine backend type based on runtime config
    let backend_type = match runtime_config.memory_backend {
        MemoryBackend::VirtualAlloc2 => BackendType::Windows { large_pages: true },
    };
    
    // Create memory allocator
    let mut allocator = MemoryAllocator::new(backend_type)
        .map_err(|e| format!("Failed to create memory allocator: {:?}", e))?;
    
    // Parse allocation strategy from config
    let strategy = runtime_config.memory_allocation.allocation_strategy
        .parse::<AllocationStrategy>()
        .map_err(|e| format!("Invalid allocation strategy '{}': {:?}", 
                            runtime_config.memory_allocation.allocation_strategy, e))?;
    
    log::info!("Using allocation strategy: {}", strategy);

    // Use the plan-based chunk allocation with configured strategy
    allocator.chunk_allocate_planned(thread_blocks, runtime_config, strategy)
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
    let mut thread_aggregates: HashMap<usize, (u64, u128, u64)> = HashMap::new();  // (bytes, time_ms, errors)

    for stats in final_stats.values() {
        for &(thread_id, _cpu_id, bytes, elapsed_ms, errors, _operations, _cycles) in stats {
            let entry = thread_aggregates.entry(thread_id).or_insert((0, 0, 0));
            entry.0 += bytes;
            entry.1 += elapsed_ms;
            entry.2 += errors;
        }
    }

    // Build PerformanceByThreadReport
    let mut threads = Vec::new();
    for &(thread_id, cpu_id, numa_node) in cpu_assignments {
        if let Some(&(total_bytes, total_time_ms, total_errors)) = thread_aggregates.get(&thread_id) {
            // Look up physical core ID from topology
            let physical_core_id = topology.iter()
                .find(|cpu| cpu.logical_id == cpu_id)
                .map(|cpu| cpu.physical_core_id)
                .unwrap_or(cpu_id);  // Fallback to logical ID if not found

            let throughput_mib_s = if total_time_ms > 0 {
                (total_bytes as f64 / (1024.0 * 1024.0)) / (total_time_ms as f64 / 1000.0)
            } else {
                0.0
            };

            threads.push(ThreadPerformance {
                thread_id,
                cpu_id,
                physical_core_id,
                numa_node,
                total_time_ms,
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
        let report = converters::create_overall_stats_summary_report(&result.overall_stats);
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