use crate::{ErrorMode, EnhancedMemoryLayout, ProgressTracker, BlockInfo};
use crate::constants::{HUGE_PAGE_SIZE, LARGE_PAGE_SIZE, REGULAR_PAGE_SIZE};
use crate::tests::{WindowMode, ChunkMode};
use crate::MemoryAllocationConfig;
use crate::tests::{TestStats, TestMemoryConfig, TestTiming, TestProgress};
use crate::tests::{
    mirror_move_multi,
    mirror_move_128_multi, mirror_move_256_multi, mirror_move_512_multi, mirror_move_auto_multi,
    stuck_bit_test_multi, stuck_bit_test_128_multi, stuck_bit_test_256_multi, stuck_bit_test_512_multi,
    stuck_bit_test_auto_multi,
    refresh_stable_multi, refresh_stable_128_multi, refresh_stable_256_multi, refresh_stable_512_multi,
    refresh_stable_auto_multi,
    simple_test_multi,
    cache_busting_multi,
    random_torture_multi,
    stride_access_multi, bandwidth_saturation_multi, block_move_multi
};
use crate::progress::progress_reporter;
use crate::results::TestRunResult;
use crate::memory::{MemoryBuffer, MemoryAllocator, BackendType};
use crate::memory::allocation_strategy::SystemMemoryInfo;
use crate::config::CpuPinningConfig;
use crate::{MemoryBackend, RuntimeConfig};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

use std::sync::mpsc::Receiver;

// Use the proper ThreadPool from thread_pool.rs
use crate::thread_pool::{ThreadPool, WorkResult};

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
#[derive(Debug, Clone)]
pub struct TestDefinition {
    pub actual_name: &'static str,    // Used for function resolution and stats tracking
    pub display_name: String,         // Used for UI display and logging
    pub function: TestFunction,
    pub config: TestMemoryConfig,
    pub original_name: Option<&'static str>, // Preserves original name for Auto variants (e.g., "StuckBitTestAuto")
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
    
    pub fn with_global_cycles(cycles: u32) -> Self {
        Self::cycles_only(cycles)
    }
}

// Test function signatures
type TestFunctionSimple = unsafe fn(*mut u8, usize, usize, ErrorMode, &TestTiming) -> TestStats;
type TestFunctionWithStreams = unsafe fn(*mut u8, usize, usize, ErrorMode, &TestTiming, u32) -> TestStats;
type TestFunctionWithConfig = unsafe fn(*mut u8, usize, usize, ErrorMode, &TestTiming, &TestMemoryConfig) -> TestStats;
type TestFunctionMultiBlock = unsafe fn(&[AllocationBlock], usize, ErrorMode, &TestTiming, &TestMemoryConfig, Option<&TestProgress>) -> TestStats;

// Test function wrapper enum
#[derive(Debug, Clone)]
pub enum TestFunction {
    Simple(TestFunctionSimple),
    WithStreams(TestFunctionWithStreams),
    WithConfig(TestFunctionWithConfig),
    MultiBlock(TestFunctionMultiBlock),
}

pub fn run_tests_with_layout(layout: EnhancedMemoryLayout, error_mode: ErrorMode) -> bool {
    let alloc_config = MemoryAllocationConfig::default();
    let runtime_config = detect_runtime_capabilities(&alloc_config);
    run_tests_with_layout_and_timing(layout, error_mode, TestSuiteTiming::default(), runtime_config, None)
}

pub fn run_tests_with_layout_and_timing(
    layout: EnhancedMemoryLayout,
    error_mode: ErrorMode,
    suite_timing: TestSuiteTiming,
    runtime_config: RuntimeConfig,
    config: Option<&crate::config::ModernConfig>,
) -> bool {
    run_tests_with_layout_and_timing_filtered(layout, error_mode, suite_timing, runtime_config, config, None, None)
}

pub fn run_tests_with_layout_and_timing_filtered(
    layout: EnhancedMemoryLayout,
    error_mode: ErrorMode,
    suite_timing: TestSuiteTiming,
    runtime_config: RuntimeConfig,
    config: Option<&crate::config::ModernConfig>,
    single_test_filter: Option<&str>,
    streams_override: Option<usize>,
) -> bool {
    setup_signal_handler();
    
    layout.print_layout();

    let progress = Arc::new(ProgressTracker::new());
    let success = Arc::new(AtomicBool::new(true));
	let all_test_cpu_stats: Arc<Mutex<HashMap<String, Vec<(usize, usize, u64, u128, u64, u64)>>>> = Arc::new(Mutex::new(HashMap::new()));

    let mut thread_blocks: HashMap<usize, Vec<BlockInfo>> = HashMap::new();
    for block in layout.blocks {
        thread_blocks.entry(block.thread_id).or_default().push(block);
    }

    // Stage 1: Pre-allocate all memory blocks
    progress.set_phase("Stage 1: Allocating Memory");
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

        // Determine which converter to use based on backend
        let report = if matches!(runtime_config.memory_backend, MemoryBackend::KernelDriver) {
            converters::create_block_allocation_report_from_driver(&allocated_blocks)
        } else {
            converters::create_block_allocation_report_from_windows(&allocated_blocks)
        };

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
        
        let report = ThreadAllocationReport { 
            total_threads: allocations.len(),
            allocations 
        };
        let mut reporter = create_console_reporter();
        if let Err(e) = reporter.report_thread_allocations(&report) {
            log::error!("Failed to display thread allocation table: {}", e);
        }
    }

    // Calculate progress tracking information and resolve auto-dispatch tests
    // Use config-driven tests if config is provided, otherwise use hard-coded defaults
    let mut test_definitions = if let Some(cfg) = config {
        match create_test_definitions_from_config(cfg) {
            Ok(tests) => {
                log::info!("Using config-driven test sequence with {} tests", tests.len());
                tests
            }
            Err(e) => {
                println!("❌ Failed to load tests from config: {}", e);
                println!("   Falling back to default test suite");
                log::error!("Config test loading failed: {}", e);
                create_test_definitions()
            }
        }
    } else {
        log::info!("Using default hard-coded test suite");
        create_test_definitions()
    };

    // Apply single test filter if provided
    if let Some(test_name_filter) = single_test_filter {
        test_definitions.retain(|def| {
            def.display_name == test_name_filter ||
            def.original_name == Some(test_name_filter)
        });

        if test_definitions.is_empty() {
            println!("❌ Test '{}' not found. Available tests:", test_name_filter);
            for def in create_test_definitions() {
                println!("  - {}", def.actual_name);
            }
            return false;
        }

        log::info!("🎯 Running single test: {}", test_definitions[0].display_name);
    }

    // Apply streams override if provided (CLI parameter overrides hard-coded config)
    if let Some(streams) = streams_override {
        log::info!("🔧 Applying CLI streams override: {} stream(s)", streams);
        for def in test_definitions.iter_mut() {
            def.config.streams = streams as u32;
        }
    }

    let tests_per_cycle = test_definitions.len() as u64;
    
    progress.set_cycle_info(1, suite_timing.global_cycles, tests_per_cycle);

    // Print test configuration summary with resolved auto-dispatch names
    {
        use crate::reporting::{create_console_reporter, converters};
        let report = converters::create_test_configuration_report_v2(&test_definitions, &suite_timing);
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

    // Initialize test run result
    let test_run_result = Arc::new(Mutex::new(TestRunResult::new()));

    // Create thread pool with pre-allocated blocks
    let thread_count = allocated_blocks.len();
    let pinning_config = CpuPinningConfig::default();
    
    println!("\nCreating thread pool with {} persistent workers...", thread_count);
    let (thread_pool, result_receiver) = ThreadPool::new(
        allocated_blocks, 
        &pinning_config, 
        thread_count,
        runtime_config.cpu_list.as_deref()  // Pass the CPU list
    );
	
    // Run test suite with timing control
    let suite_start = Instant::now();
    
    // Execute the main test cycles
    let cycles = suite_timing.global_cycles.unwrap_or(1);
    for cycle in 1..=cycles {
        if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
            println!("\n🛑 Shutdown requested, ending test suite early");
            break;
        }
        
        progress.set_phase(&format!("Cycle {} of {}", cycle, cycles));
        println!("\n🔄 Starting test cycle {} of {}", cycle, cycles);
        
        // Execute all tests in sequence for this cycle
        execute_test_cycle(
            &test_definitions,
            &thread_pool,
            &result_receiver,
            thread_count,
            error_mode,
            &progress,
            &test_run_result,
            &all_test_cpu_stats,
            cycle as u64
        );
        
        if !success.load(Ordering::Relaxed) {
            break;
        }
    }

    let suite_duration = suite_start.elapsed();

    // Signal completion to progress reporter
    progress.set_phase("Completed");

    // Wait for progress reporter to finish
    if let Err(e) = progress_handle.join() {
        log::warn!("Progress reporter thread panicked: {:?}", e);
    }

    // Shutdown thread pool
    thread_pool.shutdown();

    // Create final performance summary with detailed per-CPU stats
    let final_stats = all_test_cpu_stats.lock().unwrap();
    if !final_stats.is_empty() {
        print_detailed_cpu_performance_summary(&final_stats, suite_duration);
    }

    let final_success = success.load(Ordering::Relaxed);
    if final_success {
        println!("\n✅ All test cycles completed successfully!");
        
        // Display and save final results
        display_and_save_results(&test_run_result, suite_duration);
    } else {
        println!("\n❌ Test suite failed or was interrupted");
    }

    final_success
}

fn execute_test_cycle(
    test_definitions: &[TestDefinition],
    thread_pool: &ThreadPool,
    result_receiver: &Receiver<WorkResult>,
    thread_count: usize,
    error_mode: ErrorMode,
    progress: &Arc<ProgressTracker>,
    test_run_result: &Arc<Mutex<TestRunResult>>,
    all_test_cpu_stats: &Arc<Mutex<HashMap<String, Vec<(usize, usize, u64, u128, u64, u64)>>>>,
    cycle: u64,
) {
    use crate::progress::TestSummary;
    use crate::constants::MB_F64;

    // Track cycle timing and collect test summaries for this cycle
    let cycle_start = Instant::now();
    let mut cycle_test_summaries = Vec::new();

    let success = Arc::new(AtomicBool::new(true));
    for (test_idx, test_def) in test_definitions.iter().enumerate() {
        let test_name = test_def.actual_name; // Use actual_name for thread pool (requires 'static)
        let test_display_name = &test_def.display_name; // Use display_name for results to preserve _A suffix
        let test_func = &test_def.function;
        let test_config = &test_def.config;
        if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
            return;
        }
        
        progress.set_phase(&format!("{} ({}/{})", test_name, test_idx + 1, test_definitions.len()));
        
        let test_start = Instant::now();
        
        // Execute test on all threads
        thread_pool.execute_test(test_name, test_func, test_config, error_mode);
        
		// Collect results from all threads
		let mut test_stats = Vec::new();
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
						// Store simplified stats: (thread_id, cpu_id, bytes, elapsed, errors, operations)
						test_stats.push((result.thread_id, cpu_id,
									   result.total_bytes, result.elapsed_ms, result.total_errors, result.total_operations));
					}

					total_bytes_for_test += result.total_bytes;
					total_errors_for_test += result.total_errors;
					total_operations_for_test += result.total_operations;

					// Track cycle info from first thread (all should be similar)
					if cycles_completed == 0 {
						cycles_completed = result.cycles_completed;
						cycle_limit = result.cycle_limit;
						stopped_by_time_limit = result.stopped_by_time_limit;
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

        // Pause progress output and clear any existing progress line
        progress.pause_progress_output.store(true, Ordering::Relaxed);
        print!("\r\x1b[K"); // Clear the progress ticker line
        std::io::Write::flush(&mut std::io::stdout()).ok();

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

        // Create test report header with results and indicators
        println!("📊 Test report - Cycle {} - {}: {:.1}s, {} errors, {:.2} GiB @ {:.1} MiB/s, {} ops @ {} ops/s{}",
                 cycle,
                 test_def.display_name,
                 test_duration.as_secs_f64(),
                 total_errors_for_test,
                 total_bytes_for_test as f64 / (1024.0 * 1024.0 * 1024.0),
                 (total_bytes_for_test as f64 / (1024.0 * 1024.0)) / test_duration.as_secs_f64(),
                 format_ops(total_operations_for_test),
                 format_ops(ops_per_sec as u64),
                 cycles_info);

        // Add config line
        println!("   Config: streams={}, window={:?}, chunk={:?}, locality={}",
                 test_def.config.streams,
                 test_def.config.window_mode,
                 test_def.config.chunk_mode,
                 if test_def.config.requires_locality { "yes" } else { "no" });

        // Update progress tracker with test completion
        use crate::tests::{TestStats, TestAction};
        let test_summary = TestStats {
            name: test_name,
            action: TestAction::ReadWrite, // Generic action for multi-purpose tests
            bytes_processed: total_bytes_for_test as usize,
            elapsed_ms: test_duration.as_millis(),
            thread_id: 0, // Not used by progress tracker
            error_count: total_errors_for_test,
            total_operations: total_operations_for_test,
            cycles_completed: 0,  // Aggregated from threads
            cycles_planned: None,
            stopped_by_time_limit: false,
        };
        progress.complete_test(&test_summary);

        // Generate thread timing deviation report
        {
            use crate::reporting::{create_console_reporter, converters};
            let report = converters::create_thread_timing_report(test_name, &test_stats);
            let mut reporter = create_console_reporter();
            if let Err(e) = reporter.report_thread_timing(&report) {
                log::error!("Failed to display thread timing report: {}", e);
            }
        }

        // Resume progress output now that report is complete
        progress.pause_progress_output.store(false, Ordering::Relaxed);

		// Store aggregated stats for this test
		{
			let mut stats_map = all_test_cpu_stats.lock().unwrap();
			stats_map.entry(test_name.to_string()).or_insert_with(Vec::new).extend(test_stats);
		}

        // Create TestSummary for this test and add to cycle collection
        cycle_test_summaries.push(TestSummary {
            name: test_display_name.to_string(), // Use display_name to preserve _A suffix for AUTO tests
            duration_ms: test_duration.as_millis(),
            bytes_processed: total_bytes_for_test,
            throughput_mib_s,
            errors: total_errors_for_test,
        });

        // Early exit on errors if required
        if !success.load(Ordering::Relaxed) && matches!(error_mode, ErrorMode::Halt) {
            log::error!("Halting test suite due to memory errors");
            SHUTDOWN_REQUESTED.store(true, Ordering::Relaxed);
            return;
        }
    }

    // Cycle complete - add results to TestRunResult
    let cycle_duration_secs = cycle_start.elapsed().as_secs() as u32;
    if let Ok(mut result) = test_run_result.lock() {
        result.add_cycle(cycle as u32, cycle_duration_secs, cycle_test_summaries);
    } else {
        log::error!("Failed to lock test_run_result to add cycle {}", cycle);
    }
}

// Generic auto-dispatch resolver - converts "*Auto" test names to best SIMD variant
fn resolve_auto_dispatch_test(test_name: &str) -> Option<(&'static str, TestFunction)> {
    if !test_name.ends_with("Auto") {
        return None;
    }
    
    // Strip "Auto" suffix to get base name
    let base_name = &test_name[..test_name.len() - 4];
    
    // Determine best SIMD variant based on CPU capabilities
    let (variant_suffix, test_function): (&str, TestFunction) = if is_x86_feature_detected!("avx512f") {
        ("512", match base_name {
            "MirrorMove" => TestFunction::MultiBlock(mirror_move_512_multi),
            "StuckBitTest" => TestFunction::MultiBlock(stuck_bit_test_512_multi),
            "RefreshStable" => TestFunction::MultiBlock(refresh_stable_512_multi),
            _ => return None,
        })
    } else if is_x86_feature_detected!("avx2") {
        ("256", match base_name {
            "MirrorMove" => TestFunction::MultiBlock(mirror_move_256_multi),
            "StuckBitTest" => TestFunction::MultiBlock(stuck_bit_test_256_multi),
            "RefreshStable" => TestFunction::MultiBlock(refresh_stable_256_multi),
            _ => return None,
        })
    } else if is_x86_feature_detected!("sse2") {
        ("128", match base_name {
            "MirrorMove" => TestFunction::MultiBlock(mirror_move_128_multi),
            "StuckBitTest" => TestFunction::MultiBlock(stuck_bit_test_128_multi),
            "RefreshStable" => TestFunction::MultiBlock(refresh_stable_128_multi),
            _ => return None,
        })
    } else {
        // Fallback to scalar version
        ("", match base_name {
            "MirrorMove" => TestFunction::MultiBlock(mirror_move_multi),
            "StuckBitTest" => TestFunction::MultiBlock(stuck_bit_test_multi),
            "RefreshStable" => TestFunction::MultiBlock(refresh_stable_multi),
            _ => return None,
        })
    };
    
    // Construct the resolved concrete test name
    let concrete_name: &'static str = match (base_name, variant_suffix) {
        ("MirrorMove", "512") => "MirrorMove512",
        ("MirrorMove", "256") => "MirrorMove256", 
        ("MirrorMove", "128") => "MirrorMove128",
        ("MirrorMove", "") => "MirrorMove",
        ("StuckBitTest", "512") => "StuckBitTest512",
        ("StuckBitTest", "256") => "StuckBitTest256",
        ("StuckBitTest", "128") => "StuckBitTest128",
        ("StuckBitTest", "") => "StuckBitTest",
        ("StuckBitTestAuto", "") => "StuckBitTestAuto",
        ("RefreshStable", "512") => "RefreshStable512",
        ("RefreshStable", "256") => "RefreshStable256",
        ("RefreshStable", "128") => "RefreshStable128",
        ("RefreshStable", "") => "RefreshStable",
        _ => return None,
    };
    
    Some((concrete_name, test_function))
}

fn create_test_definitions() -> Vec<TestDefinition> {
    // Helper to validate and adjust streams at config creation time
    let validate_streams = |mut config: TestMemoryConfig, test_name: &str| -> TestMemoryConfig {
        if !config.streams.is_power_of_two() {
            let original = config.streams;
            config.streams = config.streams.next_power_of_two();
            println!("⚠️  {}: Adjusting stream count {} → {} (power-of-2 required)", 
                     test_name, original, config.streams);
        }
        config
    };
    
    let test_definitions = vec![
        // === CRITICAL: Full Memory Stuck Bit Test ===
        (
            "StuckBitTest",
            TestFunction::MultiBlock(stuck_bit_test_multi),
            validate_streams(TestMemoryConfig::new(
                WindowMode::FullAllocation,
                ChunkMode::WindowFraction { fraction: 0.0625 },
                false,
                false
            ).with_timing(TestTiming::cycles_only(1))
             .with_streams(1), "StuckBitTest")
             .with_memory_type(None)
        ),

        // === StuckBitTest SIMD variants ===
        (
            "StuckBitTest128",
            TestFunction::MultiBlock(stuck_bit_test_128_multi),
            validate_streams(TestMemoryConfig::new(
                WindowMode::FullAllocation,
                ChunkMode::WindowFraction { fraction: 0.0625 },
                false,
                false
            ).with_timing(TestTiming::cycles_only(1))
             .with_streams(1), "StuckBitTest128")
             .with_memory_type(None)
        ),

        (
            "StuckBitTest256",
            TestFunction::MultiBlock(stuck_bit_test_256_multi),
            validate_streams(TestMemoryConfig::new(
                WindowMode::FullAllocation,
                ChunkMode::WindowFraction { fraction: 0.0625 },
                false,
                false
            ).with_timing(TestTiming::cycles_only(1))
             .with_streams(1), "StuckBitTest256")
             .with_memory_type(None)
        ),

        (
            "StuckBitTest512",
            TestFunction::MultiBlock(stuck_bit_test_512_multi),
            validate_streams(TestMemoryConfig::new(
                WindowMode::FullAllocation,
                ChunkMode::WindowFraction { fraction: 0.0625 },
                false,
                false
            ).with_timing(TestTiming::cycles_only(1))
             .with_streams(1), "StuckBitTest512")
             .with_memory_type(None)
        ),

        // === StuckBitTest Auto-dispatch (AVX-512 > AVX2 > SSE2 > scalar) ===
        // Placed last so results match the actual variant performance
        (
            "StuckBitTestAuto",
            TestFunction::MultiBlock(stuck_bit_test_auto_multi),
            validate_streams(TestMemoryConfig::new(
                WindowMode::FullAllocation,
                ChunkMode::WindowFraction { fraction: 0.0625 },
                false,
                false
            ).with_timing(TestTiming::cycles_only(1))
             .with_streams(1), "StuckBitTestAuto")
             .with_memory_type(None)
        ),

        // === Base mirror move test (MultiBlock implementation) ===
        (
            "MirrorMove",
            TestFunction::MultiBlock(mirror_move_multi),
            validate_streams(TestMemoryConfig::new(
                WindowMode::FixedSize { size_mb: 64 },
                ChunkMode::FixedSize { size_mb: 4 },
                false,
                true
            ).with_timing(TestTiming::duration_only(10))
             .with_streams(1), "MirrorMove")
             .with_memory_type(None)
        ),

        // === SIMD variants with different vector sizes ===
        (
            "MirrorMove128",
            TestFunction::MultiBlock(mirror_move_128_multi),
            validate_streams(TestMemoryConfig::new(
                WindowMode::FixedSize { size_mb: 64 },
                ChunkMode::FixedSize { size_mb: 8 },
                false,
                true
            ).with_timing(TestTiming::duration_only(10))
             .with_streams(1), "MirrorMove128")
             .with_memory_type(None)
        ),

        (
            "MirrorMove256",
            TestFunction::MultiBlock(mirror_move_256_multi),
            validate_streams(TestMemoryConfig::new(
                WindowMode::FixedSize { size_mb: 128 },
                ChunkMode::FixedSize { size_mb: 8 },
                false,
                true
            ).with_timing(TestTiming::duration_only(10))
             .with_streams(2), "MirrorMove256")
             .with_memory_type(None)
        ),

        (
            "MirrorMove512",
            TestFunction::MultiBlock(mirror_move_512_multi),
            validate_streams(TestMemoryConfig::new(
                WindowMode::FixedSize { size_mb: 256 },
                ChunkMode::FixedSize { size_mb: 8 },
                false,
                true
            ).with_timing(TestTiming::duration_only(10))
             .with_streams(4), "MirrorMove512")
             .with_memory_type(None)
        ),

        // === Auto-dispatch SIMD test (resolved at runtime) ===
        // Auto-dispatches to best SIMD variant at runtime (AVX-512 > AVX2 > SSE2 > scalar)
        // Placed last so results match the actual variant performance
        (
            "MirrorMoveAuto",
            TestFunction::MultiBlock(mirror_move_auto_multi),
            validate_streams(TestMemoryConfig::new(
                WindowMode::FixedSize { size_mb: 64 },
                ChunkMode::FixedSize { size_mb: 8 },
                false,
                true
            ).with_timing(TestTiming::duration_only(10))
             .with_streams(1), "MirrorMoveAuto")
             .with_memory_type(None)
        ),
        
        // === Simple test with configurable patterns ===
        (
            "SimpleTest",
            TestFunction::MultiBlock(simple_test_multi),
            validate_streams(TestMemoryConfig::new(
                WindowMode::FullAllocation,
                ChunkMode::FixedSize { size_mb: 4 },
                false,
                false
            ).with_timing(TestTiming::hybrid(100, 30))
             .with_streams(1), "SimpleTest")
             .with_memory_type(None)
        ),

        // === Refresh stability tests (MultiBlock implementations) ===
        (
            "RefreshStable",
            TestFunction::MultiBlock(refresh_stable_multi),
            validate_streams(TestMemoryConfig::new(
                WindowMode::CacheRelative { multiplier: 2.0 },
                ChunkMode::FixedSize { size_mb: 2048 },
                false,
                true
            ).with_timing(TestTiming::duration_only(15))
             .with_streams(1), "RefreshStable")
             .with_memory_type(None)
        ),

        (
            "RefreshStable128",
            TestFunction::MultiBlock(refresh_stable_128_multi),
            validate_streams(TestMemoryConfig::new(
                WindowMode::CacheRelative { multiplier: 2.0 },
                ChunkMode::FixedSize { size_mb: 2048 },
                false,
                true
            ).with_timing(TestTiming::duration_only(15))
             .with_streams(1), "RefreshStable128")
             .with_memory_type(None)
        ),

        (
            "RefreshStable256",
            TestFunction::MultiBlock(refresh_stable_256_multi),
            validate_streams(TestMemoryConfig::new(
                WindowMode::CacheRelative { multiplier: 2.0 },
                ChunkMode::FixedSize { size_mb: 2048 },
                false,
                true
            ).with_timing(TestTiming::duration_only(15))
             .with_streams(1), "RefreshStable256")
             .with_memory_type(None)
        ),

        (
            "RefreshStable512",
            TestFunction::MultiBlock(refresh_stable_512_multi),
            validate_streams(TestMemoryConfig::new(
                WindowMode::CacheRelative { multiplier: 2.0 },
                ChunkMode::FixedSize { size_mb: 2048 },
                false,
                true
            ).with_timing(TestTiming::duration_only(15))
             .with_streams(1), "RefreshStable512")
             .with_memory_type(None)
        ),

        // === RefreshStable Auto-dispatch (AVX-512 > AVX2 > SSE2 > scalar) ===
        // Placed last so results match the actual variant performance
        (
            "RefreshStableAuto",
            TestFunction::MultiBlock(refresh_stable_auto_multi),
            validate_streams(TestMemoryConfig::new(
                WindowMode::CacheRelative { multiplier: 2.0 },
                ChunkMode::FixedSize { size_mb: 2048 },
                false,
                true
            ).with_timing(TestTiming::duration_only(15))
             .with_streams(1), "RefreshStableAuto")
             .with_memory_type(None)
        ),

        // === Performance stress tests ===
        (
            "CacheBusting",
            TestFunction::MultiBlock(cache_busting_multi),
            validate_streams(TestMemoryConfig::new(
                WindowMode::CacheRelative { multiplier: 0.5 },
                ChunkMode::FixedSize { size_mb: 1 },
                true,
                true
            ).with_timing(TestTiming::duration_only(20))
             .with_streams(4), "CacheBusting")
             .with_memory_type(None)
        ),
        
        (
            "RandomTorture",
            TestFunction::MultiBlock(random_torture_multi),
            validate_streams(TestMemoryConfig::new(
                WindowMode::FullAllocation,
                ChunkMode::FixedSize { size_mb: 8 },
                true,
                false
            ).with_timing(TestTiming::duration_only(25))
             .with_streams(8), "RandomTorture")
             .with_memory_type(None)
        ),
        
        (
            "StrideAccess",
            TestFunction::MultiBlock(stride_access_multi),
            validate_streams(TestMemoryConfig::new(
                WindowMode::FullAllocation,
                ChunkMode::FixedSize { size_mb: 2 },
                false,
                false
            ).with_timing(TestTiming::cycles_only(1))
             .with_streams(4), "StrideAccess")
             .with_memory_type(None)
        ),
        
        (
            "BandwidthSat",
            TestFunction::MultiBlock(bandwidth_saturation_multi),
            validate_streams(TestMemoryConfig::new(
                WindowMode::FullAllocation,
                ChunkMode::FixedSize { size_mb: 32 },
                false,
                false
            ).with_timing(TestTiming::duration_only(15))
             .with_streams(1), "BandwidthSat")
             .with_memory_type(None)
        ),
        
        (
            "BlockMove",
            TestFunction::MultiBlock(block_move_multi),
            validate_streams(TestMemoryConfig::new(
                WindowMode::FullAllocation,
                ChunkMode::FixedSize { size_mb: 16 },
                false,
                true
            ).with_timing(TestTiming::duration_only(20))
             .with_streams(1), "BlockMove")
             .with_memory_type(None)
        ),
    ];
    
    // Process test definitions and resolve auto-dispatch tests
    let mut resolved_tests = Vec::new();
    for (test_name, test_function, config) in test_definitions {
        if let Some((resolved_name, resolved_function)) = resolve_auto_dispatch_test(test_name) {
            log::info!("Auto-dispatch: {} → {} (based on CPU capabilities)", test_name, resolved_name);
            resolved_tests.push(TestDefinition {
                actual_name: resolved_name,
                display_name: format!("{}_A", resolved_name), // Add _A suffix for auto-dispatch
                function: resolved_function,
                config,
                original_name: Some(test_name), // Preserve original "StuckBitTestAuto" name
            });
        } else {
            resolved_tests.push(TestDefinition {
                actual_name: test_name,
                display_name: test_name.to_string(),
                function: test_function,
                config,
                original_name: None,
            });
        }
    }
    
    resolved_tests
}

/// Create test definitions from config file (config-driven test execution)
fn create_test_definitions_from_config(config: &crate::config::ModernConfig) -> Result<Vec<TestDefinition>, String> {
    let mut resolved_tests = Vec::new();

    // Helper to validate and adjust streams
    let validate_streams = |mut test_config: TestMemoryConfig, test_name: &str| -> TestMemoryConfig {
        if !test_config.streams.is_power_of_two() {
            let original = test_config.streams;
            test_config.streams = test_config.streams.next_power_of_two();
            println!("⚠️  {}: Adjusting stream count {} → {} (power-of-2 required)",
                     test_name, original, test_config.streams);
        }
        test_config
    };

    // Get enabled tests from config
    let test_configs = config.get_test_configs();

    for (test_name, mut test_config) in test_configs {
        // Validate streams
        test_config = validate_streams(test_config, test_name);

        // Look up function by name using the test registry
        let test_function = crate::tests::get_test_function_by_name(test_name)
            .ok_or_else(|| format!("Unknown test function '{}' in config", test_name))?;

        // Check for auto-dispatch
        if let Some((resolved_name, resolved_function)) = resolve_auto_dispatch_test(test_name) {
            log::info!("Auto-dispatch: {} → {} (based on CPU capabilities)", test_name, resolved_name);
            // Convert to 'static str by leaking (safe for test names, small and finite set)
            let static_original_name: &'static str = Box::leak(test_name.to_string().into_boxed_str());
            resolved_tests.push(TestDefinition {
                actual_name: resolved_name,
                display_name: format!("{}_A", resolved_name),
                function: resolved_function,
                config: test_config,
                original_name: Some(static_original_name), // Preserve original Auto name
            });
        } else {
            // Convert to 'static str by leaking (safe for test names, small and finite set)
            let static_name: &'static str = Box::leak(test_name.to_string().into_boxed_str());
            resolved_tests.push(TestDefinition {
                actual_name: static_name,
                display_name: test_name.to_string(),
                function: test_function,
                config: test_config,
                original_name: None,
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
    // Check if driver is available
    let driver_available = crate::driver::is_driver_connected();
    
    let memory_backend = if let Some(use_driver) = alloc_config.use_driver {
        if use_driver && driver_available {
            MemoryBackend::KernelDriver
        } else {
            MemoryBackend::NativeLargePages
        }
    } else {
        // Auto-detect: prefer driver if available, fall back to native
        if driver_available {
            println!("✅ Kernel driver available - using enhanced memory access");
            MemoryBackend::KernelDriver
        } else {
            println!("⚠️  Kernel driver not available - using Windows native memory");
            MemoryBackend::NativeLargePages
        }
    };

    let large_pages_available = crate::memory::privileges::check_large_page_privilege().is_ok();

    log::info!("detect_runtime_capabilities: driver_available={}, large_pages_available={}", 
               driver_available, large_pages_available);
    log::info!("Runtime capabilities detected: backend={:?}, driver_available={}, large_pages_available={}", 
               memory_backend, driver_available, large_pages_available);

    RuntimeConfig {
        memory_backend,
        driver_available,
        large_pages_available,
        use_driver_chunking: alloc_config.driver_chunking,
        cpu_list: {
            // Use all available CPUs by default
            let total_cpus = num_cpus::get();
            Some((0..total_cpus).collect())
        },
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

// Performance configuration for thread priority
#[derive(Debug, Clone)]
pub struct PerformanceConfig {
    pub thread_priority: ThreadPriority,
}

#[derive(Debug, Clone)]
pub enum ThreadPriority {
    Normal,
    High,
    Realtime,
}

impl Default for PerformanceConfig {
    fn default() -> Self {
        Self {
            thread_priority: ThreadPriority::High,
        }
    }
}

impl PerformanceConfig {
    pub fn apply(&self) -> Result<(), String> {
        use windows::Win32::System::Threading::{SetThreadPriority, GetCurrentThread, THREAD_PRIORITY_NORMAL, THREAD_PRIORITY_HIGHEST, THREAD_PRIORITY_TIME_CRITICAL};
        
        unsafe {
            let priority = match self.thread_priority {
                ThreadPriority::Normal => THREAD_PRIORITY_NORMAL,
                ThreadPriority::High => THREAD_PRIORITY_HIGHEST,
                ThreadPriority::Realtime => THREAD_PRIORITY_TIME_CRITICAL,
            };
            
            if SetThreadPriority(GetCurrentThread(), priority).is_ok() {
                Ok(())
            } else {
                Err("Failed to set thread priority".to_string())
            }
        }
    }
}

// Main test execution function that thread_pool calls
pub fn run_test_with_memory_stages(
    test_func: &TestFunction,
    allocated_block: &AllocationBlock,
    test_config: &TestMemoryConfig,
    thread_id: usize,
    error_mode: ErrorMode,
) -> Result<TestStats, String> {
    let ptr = allocated_block.buffer.as_mut_ptr() as *mut u8;
    let size = allocated_block.buffer.size();
    
    // Execute the test based on the function type
    let stats = unsafe {
        match test_func {
            TestFunction::Simple(f) => f(ptr, size, thread_id, error_mode, &test_config.timing),
            TestFunction::WithStreams(f) => f(ptr, size, thread_id, error_mode, &test_config.timing, test_config.streams),
            TestFunction::WithConfig(f) => f(ptr, size, thread_id, error_mode, &test_config.timing, test_config),
            TestFunction::MultiBlock(_) => {
                // MultiBlock tests need to be called from thread_pool with all blocks
                // This path should not be reached when properly implemented
                return Err("MultiBlock tests must be called with all blocks at once".to_string());
            }
        }
    };
    
    Ok(stats)
}

// CPU performance stats structure needed by reporting
#[derive(Debug, Clone)]
pub struct CpuPerformanceStats {
    pub total_elapsed_ms: u128,
    pub total_bytes: u64,
    pub thread_count: usize,
}

// Stub functions that need to be implemented properly based on the existing modules
fn setup_signal_handler() {
    // TODO: Implement signal handling for graceful shutdown
}

fn allocate_all_blocks_new(thread_blocks: &HashMap<usize, Vec<BlockInfo>>, runtime_config: &RuntimeConfig) -> Result<HashMap<usize, Vec<AllocationBlock>>, String> {
    use crate::memory::allocator::AllocationStrategy;

    // Determine backend type based on runtime config
    let backend_type = match runtime_config.memory_backend {
        MemoryBackend::KernelDriver => BackendType::Driver,
        MemoryBackend::NativeLargePages => BackendType::Windows { large_pages: true },
        MemoryBackend::NativeRegular => BackendType::Windows { large_pages: false },
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

fn print_detailed_cpu_performance_summary(_final_stats: &HashMap<String, Vec<(usize, usize, u64, u128, u64, u64)>>, _suite_duration: std::time::Duration) {
    println!("CPU Performance Summary:");
    for (test_name, stats) in _final_stats {
        println!("  {}: {} thread results", test_name, stats.len());
    }
}

fn display_and_save_results(test_run_result: &Arc<Mutex<TestRunResult>>, suite_duration: std::time::Duration) {
    let mut result = test_run_result.lock().unwrap();
    result.finalize(suite_duration);

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

pub fn print_current_memory_status() -> Option<SystemMemoryInfo> {
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
            
            let system_memory = SystemMemoryInfo {
                total_installed_bytes: mem_status.ullTotalPhys,
                total_physical_bytes: mem_status.ullTotalPhys,
                available_physical_bytes: mem_status.ullAvailPhys,
                used_physical_bytes: mem_status.ullTotalPhys - mem_status.ullAvailPhys,
                total_virtual_bytes: mem_status.ullTotalPageFile,
                available_virtual_bytes: mem_status.ullAvailPageFile,
                memory_load_percent: mem_status.dwMemoryLoad,
                min_start_address: 0x10000000, // 256MB default start address
            };
            
            Some(system_memory)
        } else {
            eprintln!("⚠️ Failed to retrieve system memory status");
            None
        }
    }
}