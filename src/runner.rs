use crate::{ErrorMode, MemoryLayout, ProgressTracker, BlockInfo, TestBuffer};
use crate::layout::{WindowMode, BlockMode};
use crate::tests::{TestStats, TestMemoryConfig, TestTiming, stuck_bit_test};
use crate::tests::{
    mirror_move_128_non_temporal, mirror_move_256_non_temporal, mirror_move_512_non_temporal,
    simple_test, refresh_stable, cache_busting_write_test, random_access_torture_test,
    stride_access_test, bandwidth_saturation_test
};
use crate::progress::{progress_reporter, TestSummary};
use crate::results::{TestRunResult, save_test_result}; // New results module
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::thread;
use std::time::Instant;

// Global flag for graceful shutdown
static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

// Pre-allocated memory block management
pub struct AllocatedBlock {
    pub buffer: TestBuffer,
    pub block_info: BlockInfo,
}

// Global test suite timing configuration
#[derive(Debug, Clone)]
pub struct TestSuiteTiming {
    pub global_cycles: Option<u32>,        // Number of full test suite runs
    pub global_duration_secs: Option<u32>, // Maximum total time for all cycles
}

impl Default for TestSuiteTiming {
    fn default() -> Self {
        Self {
            global_cycles: Some(3), // Default 3 cycles like TM5
            global_duration_secs: None,
        }
    }
}

impl TestSuiteTiming {
    pub fn cycles_only(cycles: u32) -> Self {
        Self {
            global_cycles: Some(cycles),
            global_duration_secs: None,
        }
    }
    
    pub fn duration_only(duration_secs: u32) -> Self {
        Self {
            global_cycles: None,
            global_duration_secs: Some(duration_secs),
        }
    }
    
    pub fn hybrid(cycles: u32, max_duration_secs: u32) -> Self {
        Self {
            global_cycles: Some(cycles),
            global_duration_secs: Some(max_duration_secs),
        }
    }
    
    pub fn unlimited() -> Self {
        Self {
            global_cycles: None,
            global_duration_secs: None,
        }
    }
    
    pub fn should_continue_suite(&self, current_cycle: u32, elapsed_secs: u32) -> bool {
        // Check if shutdown was requested
        if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
            return false;
        }
        
        // Check maximum duration first
        if let Some(max_secs) = self.global_duration_secs {
            if elapsed_secs >= max_secs {
                return false;
            }
        }
        
        // Check cycle limit
        if let Some(max_cycles) = self.global_cycles {
            if current_cycle >= max_cycles {
                return false;
            }
        }
        
        // If no limits set, run forever
        true
    }
}

// Test function with timing passed down
type TestFunction = unsafe fn(*mut u8, usize, usize, ErrorMode, &TestTiming) -> TestStats;

pub fn run_tests_with_layout(layout: MemoryLayout, error_mode: ErrorMode) -> bool {
    run_tests_with_layout_and_timing(layout, error_mode, TestSuiteTiming::default())
}

pub fn run_tests_with_layout_and_timing(layout: MemoryLayout, error_mode: ErrorMode, suite_timing: TestSuiteTiming) -> bool {
    // Set up CTRL+C handler
    setup_signal_handler();
    
    layout.print_layout();

    let progress = Arc::new(ProgressTracker::new());
    let success = Arc::new(AtomicBool::new(true));

    let mut thread_blocks: HashMap<usize, Vec<BlockInfo>> = HashMap::new();
    for block in layout.blocks {
        thread_blocks.entry(block.thread_id).or_default().push(block);
    }

    // Stage 1: Pre-allocate all memory blocks
    progress.set_phase("Stage 1: Allocating Memory");
    println!("Stage 1: Pre-allocating memory blocks...");

    let mut allocated_blocks = match allocate_all_blocks(&thread_blocks) {
        Ok(blocks) => blocks,
        Err(e) => {
            println!("❌ Failed to allocate memory blocks: {}", e);
            return false;
        }
    };

    print_allocation_summary(&allocated_blocks);

    // Calculate progress tracking information
    let test_definitions = create_test_definitions();
    let tests_per_cycle = test_definitions.len() as u64; // One test completion = all threads finishing that test
    
    // Set up progress tracking with proper cycle information
    progress.set_cycle_info(1, suite_timing.global_cycles, tests_per_cycle);

    // Print test configuration summary
    print_test_configuration(&test_definitions, &suite_timing);

    // Start progress reporter thread
    let progress_clone = Arc::clone(&progress);
    let progress_handle = thread::spawn(move || {
        progress_reporter(progress_clone);
    });

    // Initialize test run result
    let test_run_result = Arc::new(Mutex::new(TestRunResult::new()));

    // Run test suite with timing control
    let suite_start = Instant::now();
    let mut current_cycle = 0u32;
    
    println!("\n=== Starting Test Suite ===");
    
    loop {
        current_cycle += 1;
        let cycle_start = Instant::now();
        
        println!("\n=== Test Suite Cycle {} ===", current_cycle);
        progress.start_new_cycle(current_cycle);
        progress.set_phase(&format!("Cycle {}", current_cycle));
        
        // Collect cycle test summaries
        let cycle_summaries = Arc::new(Mutex::new(Vec::new()));
        
        // Run one complete cycle of all tests
        let cycle_success = run_single_test_cycle(
            &thread_blocks,
            &mut allocated_blocks,
            &test_definitions,
            error_mode,
            Arc::clone(&progress),
            Arc::clone(&cycle_summaries),
            current_cycle,
        );
        
        if !cycle_success || SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
            success.store(false, Ordering::Relaxed);
            
            // Print cycle report before exiting
            let cycle_elapsed = cycle_start.elapsed().as_secs() as u32;
            if let Ok(summaries) = cycle_summaries.lock() {
                if !summaries.is_empty() {
                    println!("\n=== Interrupted Cycle {} Report ===", current_cycle);
                    print_cycle_report(current_cycle, cycle_elapsed, &summaries, &test_definitions);
                }
            }
            break;
        }
        
        let cycle_elapsed = cycle_start.elapsed().as_secs() as u32;
        let total_elapsed = suite_start.elapsed().as_secs() as u32;
        
        // Print cycle report and save to result
        if let Ok(summaries) = cycle_summaries.lock() {
            print_cycle_report(current_cycle, cycle_elapsed, &summaries, &test_definitions);
            progress.complete_cycle(current_cycle, summaries.clone());
            
            // Add cycle to test run result
            if let Ok(mut result) = test_run_result.lock() {
                result.add_cycle(current_cycle, cycle_elapsed, summaries.clone());
            }
        }
        
        // Check if we should continue to next cycle
        if !suite_timing.should_continue_suite(current_cycle, total_elapsed) {
            println!("\nTest suite timing limits reached - stopping");
            break;
        }
    }

    progress.set_phase("Completed");
    thread::sleep(std::time::Duration::from_millis(100));

    if let Err(_) = progress_handle.join() {
        log::warn!("Progress reporter thread failed to join cleanly");
    }

    // Print final summary
    let total_time = suite_start.elapsed();
    if let Ok(result) = test_run_result.lock() {
        print_final_summary(&progress, total_time, &result, &test_definitions);
        
        // Save test result to file
        if let Err(e) = save_test_result(&result) {
            log::warn!("Failed to save test result: {}", e);
        } else {
            log::info!("Test result saved to file");
        }
    }

    log::info!("Releasing all allocated memory blocks");
    success.load(Ordering::Relaxed)
}

fn setup_signal_handler() {
    ctrlc::set_handler(move || {
        println!("\n\n🛑 CTRL+C detected - initiating graceful shutdown...");
        SHUTDOWN_REQUESTED.store(true, Ordering::Relaxed);
    }).expect("Error setting CTRL+C handler");
}

fn create_test_definitions() -> Vec<(&'static str, TestFunction, TestMemoryConfig)> {
    vec![
        // === CRITICAL: Full Memory Stuck Bit Test ===
        (
            "StuckBitTest", 
            |ptr, size, tid, em, timing| unsafe { stuck_bit_test(ptr, size, tid, em, timing) },
            TestMemoryConfig::new(
                WindowMode::FullAllocation,           // Use entire allocation 
                BlockMode::WindowFraction { fraction: 0.0625 }, // 1/16th of window per block for efficiency
                false,  // Require alignment
                false   // Doesn't need locality - needs to test ALL memory
            ).with_timing(TestTiming::cycles_only(1)) // Run once per cycle - it's thorough
        ),
        
        // === SIMD Tests with optimal window/block sizing ===
        (
            "MirrorMove128NonTemporal", 
            |ptr, size, tid, em, timing| unsafe { mirror_move_128_non_temporal(ptr, size, tid, em, timing) },
            TestMemoryConfig::new(
                WindowMode::FixedSize { size_mb: 64 },   // 64MB window for SIMD locality
                BlockMode::FixedSize { size_mb: 16 },    // 16MB blocks for 128-bit alignment
                false,  // Require alignment
                true    // Needs locality for SIMD efficiency
            ).with_timing(TestTiming::duration_only(10)) // 10 seconds of continuous SIMD testing
        ),
        (
            "MirrorMove256NonTemporal", 
            |ptr, size, tid, em, timing| unsafe { mirror_move_256_non_temporal(ptr, size, tid, em, timing) },
            TestMemoryConfig::new(
                WindowMode::FixedSize { size_mb: 128 },  // 128MB window
                BlockMode::FixedSize { size_mb: 32 },    // 32MB blocks for 256-bit alignment
                false,
                true
            ).with_timing(TestTiming::duration_only(10))
        ),
        (
            "MirrorMove512NonTemporal", 
            |ptr, size, tid, em, timing| unsafe { mirror_move_512_non_temporal(ptr, size, tid, em, timing) },
            TestMemoryConfig::new(
                WindowMode::FixedSize { size_mb: 256 },  // 256MB window
                BlockMode::FixedSize { size_mb: 64 },    // 64MB blocks for 512-bit alignment
                false,
                true
            ).with_timing(TestTiming::duration_only(10))
        ),
        
        // === Memory Pattern Tests ===
        (
            "SimpleTest", 
            |ptr, size, tid, em, timing| unsafe { simple_test(ptr, size, tid, em, timing) },
            TestMemoryConfig::new(
                WindowMode::FullAllocation,              // Test large portions of memory
                BlockMode::FixedSize { size_mb: 4 },     // 4MB blocks
                false,
                false   // Doesn't need locality
            ).with_timing(TestTiming::hybrid(100, 30)) // 100 cycles or 30 seconds, whichever first
        ),
        
        // === Refresh/Retention Tests ===
        (
            "RefreshStable", 
            |ptr, size, tid, em, timing| unsafe { refresh_stable(ptr, size, tid, em, timing) },
            TestMemoryConfig::new(
                WindowMode::CacheRelative { multiplier: 2.0 }, // 2x cache size for refresh testing
                BlockMode::FixedSize { size_mb: 1 },           // Small 1MB blocks
                false,
                true    // Needs locality for refresh timing
            ).with_timing(TestTiming::duration_only(15)) // 15 seconds of refresh testing
        ),
        
        // === Cache Tests ===
        (
            "CacheBusting", 
            |ptr, size, tid, em, timing| unsafe { cache_busting_write_test(ptr, size, tid, em, timing) },
            TestMemoryConfig::new(
                WindowMode::CacheRelative { multiplier: 0.5 }, // Half cache size
                BlockMode::FixedSize { size_mb: 1 },           // 1MB blocks for cache busting
                false,
                true    // Specifically targets cache behavior
            ).with_timing(TestTiming::duration_only(20)) // 20 seconds of cache busting
        ),
        
        // === Stress Tests ===
        (
            "RandomTorture", 
            |ptr, size, tid, em, timing| unsafe { random_access_torture_test(ptr, size, tid, em, timing) },
            TestMemoryConfig::new(
                WindowMode::FullAllocation,              // Random access across full allocation
                BlockMode::FixedSize { size_mb: 8 },     // 8MB blocks
                true,   // Allow misaligned for maximum stress
                false   // Random access - locality not needed
            ).with_timing(TestTiming::duration_only(25)) // 25 seconds of random torture
        ),
        (
            "StrideAccess", 
            |ptr, size, tid, em, timing| unsafe { stride_access_test(ptr, size, tid, em, timing) },
            TestMemoryConfig::new(
                WindowMode::FullAllocation,              // Test stride patterns across full memory
                BlockMode::FixedSize { size_mb: 2 },     // 2MB blocks
                false,
                false
            ).with_timing(TestTiming::cycles_only(1)) // 1 cycle of stride patterns
        ),
        
        // === Bandwidth Test ===
        (
            "BandwidthSat", 
            |ptr, size, tid, em, timing| unsafe { bandwidth_saturation_test(ptr, size, tid, em, timing) },
            TestMemoryConfig::new(
                WindowMode::FullAllocation,              // Maximum bandwidth requires full allocation
                BlockMode::FixedSize { size_mb: 32 },    // Large 32MB blocks for bandwidth
                false,
                false
            ).with_timing(TestTiming::duration_only(15)) // 15 seconds of bandwidth saturation
        ),
    ]
}

fn run_single_test_cycle(
    thread_blocks: &HashMap<usize, Vec<BlockInfo>>,
    allocated_blocks: &mut HashMap<usize, Vec<AllocatedBlock>>,
    test_definitions: &[(&'static str, TestFunction, TestMemoryConfig)],
    error_mode: ErrorMode,
    progress: Arc<ProgressTracker>,
    cycle_summaries: Arc<Mutex<Vec<TestSummary>>>,
    _: u32, // cycle_number - not used
) -> bool {
    let thread_count = thread_blocks.len();
    let success = Arc::new(AtomicBool::new(true));

    // Run each test type across all threads before moving to next test
    for (test_idx, (test_name, test_func, test_config)) in test_definitions.iter().enumerate() {
        // Check for shutdown before starting each test
        if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
            return false;
        }
        
        progress.set_phase(&format!("{} ({}/{})", test_name, test_idx + 1, test_definitions.len()));
        
        let test_start = Instant::now();
        let barrier = Arc::new(Barrier::new(thread_count));
        let test_stats = Arc::new(Mutex::new(Vec::new()));
        
        // Extract all allocated blocks before threading
        let mut thread_allocated_data = Vec::new();
        for (thread_id, _) in thread_blocks {
            let blocks = allocated_blocks.remove(thread_id).unwrap();
            thread_allocated_data.push((*thread_id, blocks));
        }

        let mut handles = vec![];
        for (thread_id, blocks) in thread_allocated_data {
            let success_clone = Arc::clone(&success);
            let barrier_clone = Arc::clone(&barrier);
            let test_stats_clone = Arc::clone(&test_stats);
            let test_name = test_name.to_string();
            let test_func = *test_func;
            let test_config = test_config.clone();

            let handle = thread::spawn(move || {
                // Wait for all threads to start this test
                barrier_clone.wait();
                
                let thread_start = Instant::now();
                let mut total_bytes = 0u64;
                let mut total_errors = 0u64;
                
                // Run test on all blocks for this thread
                for allocated_block in &blocks {
                    // Check for shutdown during test execution
                    if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
                        success_clone.store(false, Ordering::Relaxed);
                        break;
                    }
                    
                    match run_test_with_memory_stages(
                        &test_name,
                        test_func,
                        allocated_block,
                        &test_config,
                        thread_id,
                        error_mode,
                    ) {
                        Ok(stats) => {
                            total_bytes += stats.bytes_processed as u64;
                            total_errors += stats.error_count;
                            
                            if stats.error_count > 0 {
                                if let Err(e) = handle_test_errors(&stats, error_mode, &test_name) {
                                    log::error!("[Thread {}] {}", thread_id, e);
                                    success_clone.store(false, Ordering::Relaxed);
                                    break;
                                }
                            }
                        }
                        Err(e) => {
                            log::error!("[Thread {}] Test {} failed: {}", thread_id, test_name, e);
                            success_clone.store(false, Ordering::Relaxed);
                            break;
                        }
                    }
                }
                
                let thread_elapsed = thread_start.elapsed().as_millis();
                if let Ok(mut stats) = test_stats_clone.lock() {
                    stats.push((thread_id, total_bytes, thread_elapsed, total_errors));
                }
                
                // Return blocks for reuse
                (thread_id, blocks)
            });
            handles.push(handle);
        }

        // Collect results and restore blocks
        for handle in handles {
            if let Ok((thread_id, blocks)) = handle.join() {
                allocated_blocks.insert(thread_id, blocks);
            } else {
                success.store(false, Ordering::Relaxed);
            }
        }
        
        // Calculate test summary
        let test_elapsed = test_start.elapsed();
        if let Ok(stats) = test_stats.lock() {
            let total_bytes: u64 = stats.iter().map(|(_, bytes, _, _)| bytes).sum();
            let total_errors: u64 = stats.iter().map(|(_, _, _, errors)| errors).sum();
            let throughput_mib_s = if test_elapsed.as_millis() > 0 {
                (total_bytes as f64 / (1024.0 * 1024.0)) / (test_elapsed.as_millis() as f64 / 1000.0)
            } else {
                0.0
            };
            
            let summary = TestSummary {
                name: test_name.to_string(),
                duration_ms: test_elapsed.as_millis(),
                bytes_processed: total_bytes,
                throughput_mib_s,
                errors: total_errors,
            };
            
            if let Ok(mut summaries) = cycle_summaries.lock() {
                summaries.push(summary);
            }
            
            // Mark test as completed in progress tracker
            progress.complete_test(&TestStats {
                name: test_name,
                action: crate::tests::TestAction::Read,
                bytes_processed: total_bytes as usize,
                elapsed_ms: test_elapsed.as_millis(),
                thread_id: 0,
                error_count: total_errors,
            });
        }
        
        if !success.load(Ordering::Relaxed) || SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
            return false;
        }
    }

    success.load(Ordering::Relaxed)
}

// Updated to pass timing to test function
fn run_test_with_memory_stages(
    test_name: &str,
    test_func: TestFunction,
    allocated_block: &AllocatedBlock,
    test_config: &TestMemoryConfig,
    thread_id: usize,
    error_mode: ErrorMode,
) -> Result<TestStats, String> {
    let allocated_size = allocated_block.buffer.size();
    let allocated_ptr = allocated_block.buffer.as_mut_ptr();
    
    // Stage 2: Calculate window size
    let window_size = test_config.calculate_window_size(test_name, allocated_size);
    
    // Stage 3: Calculate block size with alignment
    let (block_size, block_adjusted) = test_config.calculate_block_size(test_name, window_size);
    
    // Ensure window is multiple of block size
    let (final_window_size, window_aligned) = test_config.align_window_to_blocks(window_size, block_size);
    
    // Log configuration once at start
    let allocated_mb = allocated_size as f64 / (1024.0 * 1024.0);
    let window_mb = final_window_size as f64 / (1024.0 * 1024.0);
    let block_mb = block_size as f64 / (1024.0 * 1024.0);
    let window_percent = (final_window_size as f64 / allocated_size as f64) * 100.0;
    
    // Log configuration once per test (only for thread 0 to avoid spam)
    if thread_id == 0 {
        log::info!(
            "{} - Window: {:.1}MB ({:.1}%), Block: {:.1}MB{}",
            test_name, window_mb, window_percent, block_mb,
            if test_config.allow_misaligned { " (misaligned)" } else { "" }
        );
    }
    
    // Debug log for all threads if needed
    log::debug!(
        "[Thread {}] {} - Configuration: Window {:.1}MB of {:.1}MB ({:.1}%), Block {:.1}MB{}",
        thread_id, test_name, window_mb, allocated_mb, window_percent, block_mb,
        if test_config.allow_misaligned { " (misaligned)" } else { "" }
    );
    
    // Run the test function with timing configuration
    let stats = unsafe {
        test_func(allocated_ptr, final_window_size, thread_id, error_mode, &test_config.timing)
    };
    
    Ok(stats)
}

fn allocate_all_blocks(thread_blocks: &HashMap<usize, Vec<BlockInfo>>) -> Result<HashMap<usize, Vec<AllocatedBlock>>, String> {
    let mut allocated_blocks = HashMap::new();
    let mut total_allocated = 0usize;
    let mut total_blocks = 0usize;
    let mut large_page_blocks = 0usize;

    for (thread_id, blocks) in thread_blocks {
        let mut thread_allocated = Vec::new();

        for block in blocks {
            let buffer = TestBuffer::new_large_pages(block.size_bytes)
                .or_else(|| TestBuffer::new_aligned(block.size_bytes))
                .ok_or_else(|| format!("Failed to allocate buffer of {} bytes for thread {}", block.size_bytes, thread_id))?;

            if buffer.uses_large_pages() {
                large_page_blocks += 1;
            }

            total_allocated += buffer.size();
            total_blocks += 1;

            thread_allocated.push(AllocatedBlock {
                buffer,
                block_info: block.clone(),
            });
        }

        allocated_blocks.insert(*thread_id, thread_allocated);
    }

    log::info!(
        "Stage 1 allocation completed: {} blocks, {:.2} GiB total, {} using large pages",
        total_blocks,
        total_allocated as f64 / (1024.0 * 1024.0 * 1024.0),
        large_page_blocks
    );

    Ok(allocated_blocks)
}

fn print_allocation_summary(allocated_blocks: &HashMap<usize, Vec<AllocatedBlock>>) {
    let mut total_size = 0usize;
    let mut large_page_blocks = 0;
    let mut size_per_thread = 0usize;

    for (thread_id, blocks) in allocated_blocks {
        let thread_total_size: usize = blocks.iter().map(|b| b.buffer.size()).sum();
        let thread_large_pages = blocks.iter().filter(|b| b.buffer.uses_large_pages()).count();

        total_size += thread_total_size;
        large_page_blocks += thread_large_pages;

        if *thread_id == 0 {
            size_per_thread = thread_total_size;
        }
    }

    let thread_count = allocated_blocks.len();
    let size_per_thread_gib = size_per_thread as f64 / (1024.0 * 1024.0 * 1024.0);

    println!("Stage 1 Memory Allocation Summary:");
    println!(
        "  {} × {:.2} GiB per thread = {:.2} GiB total (TM5-style maximum allocation)",
        thread_count, size_per_thread_gib,
        total_size as f64 / (1024.0 * 1024.0 * 1024.0)
    );

    if large_page_blocks > 0 {
        println!("  ✅ {} blocks using large pages (2MB pages)", large_page_blocks);
    } else {
        println!("  ⚠️  Using standard 4KB pages (large pages not available)");
    }
    println!();
}

fn print_test_configuration(test_definitions: &[(&'static str, TestFunction, TestMemoryConfig)], suite_timing: &TestSuiteTiming) {
    println!("Test Configuration:");
    
    // Suite timing
    print!("  Suite Timing: ");
    match (suite_timing.global_cycles, suite_timing.global_duration_secs) {
        (Some(cycles), Some(duration)) => println!("{} cycles or {}s max", cycles, duration),
        (Some(cycles), None) => println!("{} cycles", cycles),
        (None, Some(duration)) => println!("{}s duration", duration),
        (None, None) => println!("Unlimited"),
    }
    
    // Test sequence
    println!("  Test Sequence: {} tests", test_definitions.len());
    
    // Individual tests
    for (i, (name, _, config)) in test_definitions.iter().enumerate() {
        print!("    {}. {} - ", i + 1, name);
        
        // Timing
        match (&config.timing.cycles, &config.timing.duration_secs) {
            (Some(c), Some(d)) => print!("{}cycles/{}s, ", c, d),
            (Some(c), None) => print!("{}cycles, ", c),
            (None, Some(d)) => print!("{}s, ", d),
            (None, None) => print!("unlimited, "),
        }
        
        // Window mode
        match &config.window_mode {
            WindowMode::FullAllocation => print!("FullWindow"),
            WindowMode::FixedSize { size_mb } => print!("Window:{}MB", size_mb),
            WindowMode::CacheRelative { multiplier } => print!("Window:{}xCache", multiplier),
        }
        
        // Block mode
        match &config.block_mode {
            BlockMode::AutoOptimal => print!(", AutoBlock"),
            BlockMode::FixedSize { size_mb } => print!(", Block:{}MB", size_mb),
            BlockMode::WindowFraction { fraction } => print!(", Block:{:.1}%", fraction * 100.0),
        }
        
        if config.allow_misaligned {
            print!(", Misaligned");
        }
        if config.requires_locality {
            print!(", Locality");
        }
        
        println!();
    }
}

fn print_cycle_report(cycle: u32, duration_secs: u32, summaries: &[TestSummary], test_definitions: &[(&'static str, TestFunction, TestMemoryConfig)]) {
    println!("\n--- Cycle {} Report ---", cycle);
    println!("Duration: {}s", duration_secs);
    
    let total_bytes: u64 = summaries.iter().map(|s| s.bytes_processed).sum();
    let total_errors: u64 = summaries.iter().map(|s| s.errors).sum();
    let avg_throughput_mib = if duration_secs > 0 {
        (total_bytes as f64 / (1024.0 * 1024.0)) / duration_secs as f64
    } else {
        0.0
    };
    let avg_throughput_gib = avg_throughput_mib / 1024.0;
    
    println!("Tests completed: {}", summaries.len());
    println!("Total data processed: {:.2} GiB", total_bytes as f64 / (1024.0 * 1024.0 * 1024.0));
    println!("Average throughput: {:.1} MiB/s ({:.2} GiB/s)", avg_throughput_mib, avg_throughput_gib);
    println!("Total errors: {}", total_errors);
    
    if !summaries.is_empty() {
        println!("\nTest Performance:");
        for (i, summary) in summaries.iter().enumerate() {
            let test_number = i + 1;
            let throughput_gib_s = summary.throughput_mib_s / 1024.0;
            
            println!("  {}. {} - {:.1}s, {:.2} GiB @ {:.1} MiB/s ({:.2} GiB/s){}",
                test_number,
                summary.name,
                summary.duration_ms as f64 / 1000.0,
                summary.bytes_processed as f64 / (1024.0 * 1024.0 * 1024.0),
                summary.throughput_mib_s,
                throughput_gib_s,
                if summary.errors > 0 { 
                    format!(" [⚠️ ERRORS: {}]", summary.errors) 
                } else { 
                    String::new() 
                }
            );
        }
    }
}

fn print_final_summary(progress: &ProgressTracker, total_time: std::time::Duration, test_result: &TestRunResult, test_definitions: &[(&'static str, TestFunction, TestMemoryConfig)]) {
    let cycle_stats = progress.get_cycle_stats();
    let total_bytes = progress.total_bytes_processed.load(Ordering::Relaxed);
    let total_errors = progress.total_errors.load(Ordering::Relaxed);
    
    println!("\n=== Final Test Summary ===");
    println!("Total runtime: {}", format_duration(total_time));
    println!("Cycles completed: {}", cycle_stats.len());
    println!("Total data processed: {:.2} GiB", total_bytes as f64 / (1024.0 * 1024.0 * 1024.0));
    
    // Calculate overall throughput
    let overall_throughput_mib = if total_time.as_secs() > 0 {
        (total_bytes as f64 / (1024.0 * 1024.0)) / total_time.as_secs() as f64
    } else {
        0.0
    };
    let overall_throughput_gib = overall_throughput_mib / 1024.0;
    
    println!("Overall throughput: {:.1} MiB/s ({:.2} GiB/s)", overall_throughput_mib, overall_throughput_gib);
    println!("Total errors detected: {}", total_errors);
    
    // Print per-test breakdown
    if !cycle_stats.is_empty() {
        println!("\nPer-Test Performance Summary (averaged across {} cycles):", cycle_stats.len());
        
        // Create test aggregates
        let mut test_aggregates: HashMap<String, (u64, u128, u64)> = HashMap::new(); // bytes, duration_ms, errors
        
        for cycle in &cycle_stats {
            for test_summary in &cycle.test_stats {
                let entry = test_aggregates.entry(test_summary.name.clone()).or_insert((0, 0, 0));
                entry.0 += test_summary.bytes_processed;
                entry.1 += test_summary.duration_ms;
                entry.2 += test_summary.errors;
            }
        }
        
        for (i, (test_name, _, _)) in test_definitions.iter().enumerate() {
            if let Some((total_bytes, total_duration_ms, total_errors)) = test_aggregates.get(*test_name) {
                let test_number = i + 1;
                let cycle_count = cycle_stats.len() as u64;
                let avg_bytes = *total_bytes / cycle_count;
                let avg_duration_ms = *total_duration_ms / cycle_count as u128;
                let avg_throughput_mib = if avg_duration_ms > 0 {
                    (avg_bytes as f64 / (1024.0 * 1024.0)) / (avg_duration_ms as f64 / 1000.0)
                } else {
                    0.0
                };
                let avg_throughput_gib = avg_throughput_mib / 1024.0;
                
                println!("  {}. {} - {:.1}s avg, {:.2} GiB avg @ {:.1} MiB/s ({:.2} GiB/s){}",
                    test_number,
                    test_name,
                    avg_duration_ms as f64 / 1000.0,
                    avg_bytes as f64 / (1024.0 * 1024.0 * 1024.0),
                    avg_throughput_mib,
                    avg_throughput_gib,
                    if *total_errors > 0 { 
                        format!(" [⚠️ TOTAL ERRORS: {}]", total_errors) 
                    } else { 
                        String::new() 
                    }
                );
            }
        }
    }
    
    println!("\nResult saved to: .\\{}", test_result.get_filename());
}

fn handle_test_errors(stats: &TestStats, error_mode: ErrorMode, test_name: &str) -> Result<(), String> {
    match error_mode {
        ErrorMode::Panic => panic!("Memory error detected in {} (see logs)", test_name),
        ErrorMode::Halt => return Err(format!("Test halted due to {} errors in {}", stats.error_count, test_name)),
        ErrorMode::Log => {
            log::error!(
                "[Thread {}] {} errors detected in {} (continuing)",
                stats.thread_id,
                stats.error_count,
                test_name
            );
        }
    }
    Ok(())
}

fn format_duration(duration: std::time::Duration) -> String {
    let total_seconds = duration.as_secs();
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;
    format!("{:02}:{:02}:{:02}", hours, minutes, seconds)
}