use crate::{ErrorMode, EnhancedMemoryLayout, ProgressTracker, BlockInfo};
use crate::constants::{HUGE_PAGE_SIZE, LARGE_PAGE_SIZE, REGULAR_PAGE_SIZE, BYTES_PER_GIB_F64};
use crate::tests::{WindowMode, ChunkMode};
use crate::MemoryAllocationConfig;
use crate::tests::{TestStats, TestMemoryConfig, TestTiming};
use crate::tests::{
    mirror_move, mirror_move_128, mirror_move_256, mirror_move_512, mirror_move_auto,
    stuck_bit_test, stuck_bit_test_128, stuck_bit_test_256, stuck_bit_test_512,
    simple_test, refresh_stable, refresh_stable_128, refresh_stable_256, refresh_stable_512,
    cache_busting_write_test, random_access_torture_test,
    stride_access_test, bandwidth_saturation_test, block_move_test
};
use crate::progress::progress_reporter;
use crate::results::TestRunResult;
use crate::memory::{MemoryBuffer, MemoryAllocator, AllocationConfig, PageSizePreference, BackendType};
use crate::memory::allocation_strategy::SystemMemoryInfo;
use crate::driver::MemoryType;
use crate::memory::buffer::MemoryType as BufferMemoryType;
use crate::config::CpuPinningConfig;
use crate::{MemoryBackend, RuntimeConfig};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;
use crate::cpu_topology::get_numa_node_for_cpu;

use std::sync::mpsc::Receiver;

// Use the proper ThreadPool from thread_pool.rs
use crate::thread_pool::{ThreadPool, WorkResult};
use crate::table::{TableBuilder, Alignment};

// Global flags for shutdown handling
pub static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

// OS memory allocation per thread (renamed from AllocationBlock for clarity)
// Represents: Physical memory allocated from the OS for testing
#[derive(Debug)]
pub struct AllocationBlock {
    pub buffer: MemoryBuffer,
    pub block_info: BlockInfo,
    pub memory_type: MemoryType,
    pub numa_node: u32,
}

// Test definition with display name support
#[derive(Debug, Clone)]
pub struct TestDefinition {
    pub actual_name: &'static str,    // Used for function resolution and stats tracking
    pub display_name: String,         // Used for UI display and logging
    pub function: TestFunction,
    pub config: TestMemoryConfig,
}

// Work item for thread pool


type TestStatsTuple = (usize, usize, u64, u128, u64, u64);	 // thread_id, cpu_id, bytes, elapsed, errors, operations

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
            global_cycles: Some(1),
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

// Test function wrapper enum
#[derive(Debug, Clone)]
pub enum TestFunction {
    Simple(TestFunctionSimple),
    WithStreams(TestFunctionWithStreams),
    WithConfig(TestFunctionWithConfig),
}

pub fn run_tests_with_layout(layout: EnhancedMemoryLayout, error_mode: ErrorMode) -> bool {
    let alloc_config = MemoryAllocationConfig::default();
    let runtime_config = detect_runtime_capabilities(&alloc_config);
    run_tests_with_layout_and_timing(layout, error_mode, TestSuiteTiming::default(), runtime_config)
}

pub fn run_tests_with_layout_and_timing(
    layout: EnhancedMemoryLayout, 
    error_mode: ErrorMode, 
    suite_timing: TestSuiteTiming, 
    runtime_config: RuntimeConfig
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

    let allocated_blocks = match allocate_all_blocks_new(&thread_blocks, &runtime_config) {
        Ok(blocks) => blocks,
        Err(e) => {
            println!("❌ Failed to allocate memory blocks: {}\n", e);
            return false;
        }
    };

    print_allocation_summary(&allocated_blocks);
    
    // Add requested vs allocated reporting
    print_allocation_details(&allocated_blocks, &thread_blocks, &runtime_config);

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
    let test_definitions = create_test_definitions();
    let tests_per_cycle = test_definitions.len() as u64;
    
    progress.set_cycle_info(1, suite_timing.global_cycles, tests_per_cycle);

    // Print test configuration summary with resolved auto-dispatch names
    print_test_configuration(&test_definitions, &suite_timing);

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
    let success = Arc::new(AtomicBool::new(true));
    for (test_idx, test_def) in test_definitions.iter().enumerate() {
        let test_name = test_def.actual_name; // Use actual name for function calls and stats
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
        
        log::info!("Test {} completed: {} bytes, {} errors, {} operations in {:?}", 
                  test_name, total_bytes_for_test, total_errors_for_test, 
                  total_operations_for_test, test_duration);

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
        };
        progress.complete_test(&test_summary);

        // Generate thread timing deviation report
        print_thread_timing_deviation(test_name, &test_stats);

		// Store aggregated stats for this test  
		{
			let mut stats_map = all_test_cpu_stats.lock().unwrap();
			stats_map.entry(test_name.to_string()).or_insert_with(Vec::new).extend(test_stats);
		}

        // Early exit on errors if required
        if !success.load(Ordering::Relaxed) && matches!(error_mode, ErrorMode::Halt) {
            log::error!("Halting test suite due to memory errors");
            SHUTDOWN_REQUESTED.store(true, Ordering::Relaxed);
            return;
        }
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
            "MirrorMove" => TestFunction::WithConfig(|ptr, size, tid, em, timing, config| unsafe { 
                mirror_move_512(ptr, size, tid, em, timing, config) 
            }),
            "StuckBitTest" => TestFunction::WithConfig(|ptr, size, tid, em, timing, config| unsafe { 
                stuck_bit_test_512(ptr, size, tid, em, timing, config) 
            }),
            "RefreshStable" => TestFunction::WithConfig(|ptr, size, tid, em, timing, config| unsafe { 
                refresh_stable_512(ptr, size, tid, em, timing, config) 
            }),
            _ => return None,
        })
    } else if is_x86_feature_detected!("avx2") {
        ("256", match base_name {
            "MirrorMove" => TestFunction::WithConfig(|ptr, size, tid, em, timing, config| unsafe { 
                mirror_move_256(ptr, size, tid, em, timing, config) 
            }),
            "StuckBitTest" => TestFunction::WithConfig(|ptr, size, tid, em, timing, config| unsafe { 
                stuck_bit_test_256(ptr, size, tid, em, timing, config) 
            }),
            "RefreshStable" => TestFunction::WithConfig(|ptr, size, tid, em, timing, config| unsafe { 
                refresh_stable_256(ptr, size, tid, em, timing, config) 
            }),
            _ => return None,
        })
    } else if is_x86_feature_detected!("sse2") {
        ("128", match base_name {
            "MirrorMove" => TestFunction::WithConfig(|ptr, size, tid, em, timing, config| unsafe { 
                mirror_move_128(ptr, size, tid, em, timing, config) 
            }),
            "StuckBitTest" => TestFunction::WithConfig(|ptr, size, tid, em, timing, config| unsafe { 
                stuck_bit_test_128(ptr, size, tid, em, timing, config) 
            }),
            "RefreshStable" => TestFunction::WithConfig(|ptr, size, tid, em, timing, config| unsafe { 
                refresh_stable_128(ptr, size, tid, em, timing, config) 
            }),
            _ => return None,
        })
    } else {
        // Fallback to scalar version
        ("", match base_name {
            "MirrorMove" => TestFunction::WithConfig(|ptr, size, tid, em, timing, config| unsafe { 
                mirror_move(ptr, size, tid, em, timing, config) 
            }),
            "StuckBitTest" => TestFunction::WithConfig(|ptr, size, tid, em, timing, config| unsafe { 
                stuck_bit_test(ptr, size, tid, em, timing, config) 
            }),
            "RefreshStable" => TestFunction::WithConfig(|ptr, size, tid, em, timing, config| unsafe { 
                refresh_stable(ptr, size, tid, em, timing, config) 
            }),
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
            TestFunction::WithConfig(|ptr, size, tid, em, timing, config| unsafe { stuck_bit_test(ptr, size, tid, em, timing, config) }),
            validate_streams(TestMemoryConfig::new(
                WindowMode::FullAllocation,
                ChunkMode::WindowFraction { fraction: 0.0625 },
                false,
                false
            ).with_timing(TestTiming::cycles_only(1))
             .with_streams(1), "StuckBitTest")
             .with_memory_type(None)
        ),
        
        // === Base mirror move test (scalar implementation with detailed error reporting) ===
        (
            "MirrorMove", 
            TestFunction::WithConfig(|ptr, size, tid, em, timing, config| unsafe {
                mirror_move(ptr, size, tid, em, timing, config)
            }),
            validate_streams(TestMemoryConfig::new(
                WindowMode::FixedSize { size_mb: 64 },
                ChunkMode::FixedSize { size_mb: 4 },
                false,
                true
            ).with_timing(TestTiming::duration_only(10))
             .with_streams(1), "MirrorMove")
             .with_memory_type(None)
        ),
        
        // === Auto-dispatch SIMD test (resolved at runtime) ===
        // This will be resolved by resolve_auto_dispatch_test() to the best SIMD variant
        (
            "MirrorMoveAuto",
            TestFunction::WithConfig(|ptr, size, tid, em, timing, config| unsafe {
                // This placeholder will be replaced by the resolver
                mirror_move_auto(ptr, size, tid, em, timing, config)
            }),
            validate_streams(TestMemoryConfig::new(
                WindowMode::FixedSize { size_mb: 64 },
                ChunkMode::FixedSize { size_mb: 8 },
                false,
                true
            ).with_timing(TestTiming::duration_only(10))
             .with_streams(1), "MirrorMoveAuto")
             .with_memory_type(None)
        ),
        
        // === SIMD variants with different vector sizes ===
        (
            "MirrorMove128",
            TestFunction::WithConfig(|ptr, size, tid, em, timing, config| unsafe {
                mirror_move_128(ptr, size, tid, em, timing, config)
            }),
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
            TestFunction::WithConfig(|ptr, size, tid, em, timing, config| unsafe {
                mirror_move_256(ptr, size, tid, em, timing, config)
            }),
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
            TestFunction::WithConfig(|ptr, size, tid, em, timing, config| unsafe {
                mirror_move_512(ptr, size, tid, em, timing, config)
            }),
            validate_streams(TestMemoryConfig::new(
                WindowMode::FixedSize { size_mb: 256 },
                ChunkMode::FixedSize { size_mb: 8 },
                false,
                true
            ).with_timing(TestTiming::duration_only(10))
             .with_streams(4), "MirrorMove512")
             .with_memory_type(None)
        ),
        
        // === Simple test with configurable patterns ===
        (
            "SimpleTest",
            TestFunction::WithConfig(|ptr, size, tid, em, timing, config| unsafe {
                simple_test(ptr, size, tid, em, timing, config)
            }),
            validate_streams(TestMemoryConfig::new(
                WindowMode::FullAllocation,
                ChunkMode::FixedSize { size_mb: 4 },
                false,
                false
            ).with_timing(TestTiming::hybrid(100, 30))
             .with_streams(1), "SimpleTest")
             .with_memory_type(None)
        ),
        
        // === Refresh stability test ===
        (
            "RefreshStable",
            TestFunction::WithConfig(|ptr, size, tid, em, timing, config| unsafe {
                refresh_stable(ptr, size, tid, em, timing, config)
            }),
            validate_streams(TestMemoryConfig::new(
                WindowMode::CacheRelative { multiplier: 2.0 },
                ChunkMode::FixedSize { size_mb: 2048 },
                false,
                true
            ).with_timing(TestTiming::duration_only(15))
             .with_streams(1), "RefreshStable")
             .with_memory_type(None)
        ),
        
        // === Performance stress tests ===
        (
            "CacheBusting",
            TestFunction::WithConfig(|ptr, size, tid, em, timing, config| unsafe {
                cache_busting_write_test(ptr, size, tid, em, timing, config)
            }),
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
            TestFunction::WithConfig(|ptr, size, tid, em, timing, config| unsafe {
                random_access_torture_test(ptr, size, tid, em, timing, config)
            }),
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
            TestFunction::WithConfig(|ptr, size, tid, em, timing, config| unsafe {
                stride_access_test(ptr, size, tid, em, timing, config)
            }),
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
            TestFunction::WithConfig(|ptr, size, tid, em, timing, config| unsafe {
                bandwidth_saturation_test(ptr, size, tid, em, timing, config)
            }),
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
            TestFunction::WithConfig(|ptr, size, tid, em, timing, config| unsafe {
                block_move_test(ptr, size, tid, em, timing, config)
            }),
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
            });
        } else {
            resolved_tests.push(TestDefinition {
                actual_name: test_name,
                display_name: test_name.to_string(),
                function: test_function,
                config,
            });
        }
    }
    
    resolved_tests
}

fn print_thread_timing_deviation(test_name: &str, stats: &[(usize, usize, u64, u128, u64, u64)]) {
    use crate::table::{TableBuilder, Alignment};
    use crate::cpu_topology::get_numa_node_for_cpu;
    
    if stats.is_empty() {
        return;
    }
    
    // Calculate average elapsed time
    let total_elapsed: u128 = stats.iter().map(|(_, _, _, elapsed, _, _)| *elapsed).sum();
    let avg_elapsed = total_elapsed / stats.len() as u128;
    
    // Calculate deviations and collect timing data
    let mut deviations = Vec::new();
    for &(thread_id, cpu_id, bytes, elapsed, errors, _operations) in stats.iter() {
        let deviation = elapsed as i128 - avg_elapsed as i128;
        deviations.push((thread_id, cpu_id, bytes, elapsed, errors, deviation));
    }
    
    // Print timing deviation report
    println!("📊 Thread timing deviation for {} - Avg: {:.1}s", 
             test_name, avg_elapsed as f64 / 1000.0);
    
    let mut table = TableBuilder::new()
        .add_header("Thread", Alignment::Right)
        .add_header("Logical CPU", Alignment::Right)
        .add_header("Physical Core", Alignment::Right)
        .add_header("NUMA Node", Alignment::Center)
        .add_header("Runtime", Alignment::Right)
        .add_header("Deviation", Alignment::Right)
        .add_header("Data", Alignment::Right)
        .add_header("Throughput", Alignment::Right)
        .add_header("Errors", Alignment::Right);
    
    // Sort by deviation descending (highest deviation first)
    deviations.sort_by(|a, b| b.5.cmp(&a.5));
    
    for (thread_id, cpu_id, bytes, elapsed, errors, deviation) in deviations {
        // Get physical core and NUMA node info
        let physical_core = cpu_id / 2; // Simple approximation - may need better mapping
        let numa_node = get_numa_node_for_cpu(cpu_id);
        
        // Calculate throughput
        let throughput_mib_s = if elapsed > 0 {
            (bytes as f64 / (1024.0 * 1024.0)) / (elapsed as f64 / 1000.0)
        } else {
            0.0
        };
        
        // Format runtime and deviation
        let runtime_str = format!("{:.1}s", elapsed as f64 / 1000.0);
        let deviation_str = if deviation >= 0 {
            format!("+{:.1}s", deviation as f64 / 1000.0)
        } else {
            format!("{:.1}s", deviation as f64 / 1000.0)
        };
        
        // Format data size
        let data_str = if bytes >= 1024 * 1024 * 1024 {
            format!("{:.2} GiB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
        } else {
            format!("{:.0} MiB", bytes as f64 / (1024.0 * 1024.0))
        };
        
        // Format errors
        let errors_str = if errors > 0 {
            errors.to_string()
        } else {
            "✅".to_string()
        };
        
        table = table.add_row(vec![
            thread_id.to_string(),
            cpu_id.to_string(),
            physical_core.to_string(),
            numa_node.to_string(),
            runtime_str,
            deviation_str,
            data_str,
            format!("{:.1} MiB/s", throughput_mib_s),
            errors_str,
        ]);
    }
    
    table.print();
    println!();
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
    test_name: &str,
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
    use crate::memory::MemoryAllocator;
    use crate::memory::allocator::AllocationStrategy;
    
    // Determine backend type based on runtime config
    let backend_type = match runtime_config.memory_backend {
        MemoryBackend::KernelDriver => crate::memory::BackendType::Driver,
        MemoryBackend::NativeLargePages => crate::memory::BackendType::Windows { large_pages: true },
        MemoryBackend::NativeRegular => crate::memory::BackendType::Windows { large_pages: false },
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

fn print_allocation_summary(allocated_blocks: &HashMap<usize, Vec<AllocationBlock>>) {
    
    let mut total_allocated = 0usize;
    let mut huge_page_count = 0;
    let mut large_page_count = 0;
    
    for blocks in allocated_blocks.values() {
        for block in blocks {
            total_allocated += block.buffer.size();
            if block.buffer.uses_huge_pages() {
                huge_page_count += 1;
            } else if block.buffer.uses_large_pages() {
                large_page_count += 1;
            }
        }
    }
    
    println!("Stage 1 Memory Allocation Summary:");
    println!("  {} × {:.2} GiB per thread = {:.2} GiB total",
             allocated_blocks.len(),
             total_allocated as f64 / allocated_blocks.len() as f64 / BYTES_PER_GIB_F64,
             total_allocated as f64 / BYTES_PER_GIB_F64);
    
    if huge_page_count > 0 || large_page_count > 0 {
        println!("  ✅ {} blocks with 1GB huge pages, {} blocks with 2MB+ large pages", 
                 huge_page_count, large_page_count);
    }
}

fn print_allocation_details(allocated_blocks: &HashMap<usize, Vec<AllocationBlock>>, _thread_blocks: &HashMap<usize, Vec<BlockInfo>>, _runtime_config: &RuntimeConfig) {
    use crate::reporting::models::{TableData, TableHeader};
    use crate::reporting::create_console_reporter;
    
    // Calculate totals by page type
    let mut total_huge = 0usize;
    let mut total_large = 0usize;
    let mut total_regular = 0usize;
    
    for blocks in allocated_blocks.values() {
        for block in blocks {
            let size = block.buffer.size();
            if block.buffer.uses_huge_pages() {
                total_huge += size;
            } else if block.buffer.uses_large_pages() {
                total_large += size;
            } else {
                total_regular += size;
            }
        }
    }
    
    // Build table using TableData
    let mut rows = Vec::new();
    
    if total_huge > 0 {
        rows.push(vec![
            "Huge (1GB)".to_string(),
            "✅ Allowed".to_string(),
            "N/A".to_string(),
            format!("{:.2} GiB", total_huge as f64 / BYTES_PER_GIB_F64),
            format!("✅ {:.2} GiB", total_huge as f64 / BYTES_PER_GIB_F64)
        ]);
    }
    if total_large > 0 {
        rows.push(vec![
            "Large (2MB)".to_string(),
            "✅ Allowed".to_string(),
            "N/A".to_string(),
            format!("{:.2} GiB", total_large as f64 / BYTES_PER_GIB_F64),
            format!("✅ {:.2} GiB", total_large as f64 / BYTES_PER_GIB_F64)
        ]);
    }
    if total_regular > 0 {
        rows.push(vec![
            "Regular (4KB)".to_string(),
            "✅ Allowed".to_string(),
            "N/A".to_string(),
            format!("{:.2} GiB", total_regular as f64 / BYTES_PER_GIB_F64),
            format!("✅ {:.2} GiB", total_regular as f64 / BYTES_PER_GIB_F64)
        ]);
    }
    
    let mut table = TableBuilder::new()
        .add_header("Page Type", Alignment::Left)
        .add_header("User Constraints", Alignment::Center)
        .add_header("Requested", Alignment::Right)
        .add_header("Allocated", Alignment::Right)
        .add_header("Result", Alignment::Center);
    
    for row in rows {
        table = table.add_row(row);
    }
    
    table.print();
}

fn print_test_configuration(test_definitions: &[TestDefinition], suite_timing: &TestSuiteTiming) {
    
    let mut rows = Vec::new();
    
    for (idx, test_def) in test_definitions.iter().enumerate() {
        let test_name = &test_def.display_name; // Use display name for UI
        let config = &test_def.config;
        let timing_str = if let Some(cycles) = config.timing.cycles {
            if let Some(duration) = config.timing.duration_secs {
                format!("{}cycles/{}s", cycles, duration)
            } else {
                format!("{}cycles", cycles)
            }
        } else if let Some(duration) = config.timing.duration_secs {
            format!("{}s", duration)
        } else {
            "default".to_string()
        };
        
        let window_str = match config.window_mode {
            WindowMode::FullAllocation => "FullAllocation".to_string(),
            WindowMode::FixedSize { size_mb } => format!("FixedSize ({} MB)", size_mb),
            WindowMode::CacheRelative { multiplier } => format!("CacheRelative ({:.1}x)", multiplier),
        };
        
        let chunk_str = match config.chunk_mode {
            ChunkMode::WindowFraction { fraction } => format!("WindowFraction ({:.1}%)", fraction * 100.0),
            ChunkMode::FixedSize { size_mb } => format!("FixedSize ({} MB)", size_mb),
            ChunkMode::AutoOptimal => "AutoOptimal".to_string(),
        };
        
        let mut flags = Vec::new();
        if config.requires_locality {
            flags.push("Locality");
        }
        if config.allow_misaligned {
            flags.push("Misaligned");
        }
        let flags_str = if flags.is_empty() { String::new() } else { flags.join(", ") };
        
        rows.push(vec![
            format!("{}", idx + 1),
            test_name.to_string(),
            timing_str,
            config.streams.to_string(),
            window_str,
            chunk_str,
            flags_str
        ]);
    }
    
    let mut table = TableBuilder::new()
        .add_header("#", Alignment::Right)
        .add_header("Test Name", Alignment::Left)
        .add_header("Timing", Alignment::Right)
        .add_header("Streams", Alignment::Right)
        .add_header("Window Mode", Alignment::Left)
        .add_header("Block Mode", Alignment::Left)
        .add_header("Flags", Alignment::Left);
    
    for row in rows {
        table = table.add_row(row);
    }
    
    table.print();
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
    
    // Create test results summary table using reporting system
    
    let table = TableBuilder::new()
        .add_header("Metric", Alignment::Left)
        .add_header("Value", Alignment::Right)
        .add_row(vec!["Duration".to_string(), format!("{:.2}s", result.overall_stats.total_runtime_secs)])
        .add_row(vec!["Cycles Completed".to_string(), format!("{}", result.overall_stats.cycles_completed)])
        .add_row(vec!["Data Processed".to_string(), format!("{:.2} GiB", result.overall_stats.total_data_processed_gib)])
        .add_row(vec!["Overall Throughput".to_string(), format!("{:.2} GiB/s", result.overall_stats.overall_throughput_gib_s)])
        .add_row(vec!["Total Errors".to_string(), format!("{}", result.overall_stats.total_errors)]);
    
    table.print();
    
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