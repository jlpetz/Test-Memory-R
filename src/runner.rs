use crate::{ErrorMode, MemoryLayout, ProgressTracker, BlockInfo, TestBuffer};
use crate::layout::{WindowMode, BlockMode};
use crate::tests::{TestStats, TestMemoryConfig, TestAction, TestTiming, stuck_bit_test};
use crate::tests::{
    mirror_move_128_non_temporal, mirror_move_256_non_temporal, mirror_move_512_non_temporal,
    simple_test, refresh_stable, cache_busting_write_test, random_access_torture_test,
    stride_access_test, bandwidth_saturation_test
};
use crate::progress::progress_reporter;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Instant;

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

pub fn run_tests_with_layout(layout: MemoryLayout, error_mode: ErrorMode) -> bool {
    run_tests_with_layout_and_timing(layout, error_mode, TestSuiteTiming::default())
}

pub fn run_tests_with_layout_and_timing(layout: MemoryLayout, error_mode: ErrorMode, suite_timing: TestSuiteTiming) -> bool {
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

    // Calculate total tests for progress tracking
    let test_definitions = create_test_definitions();
    let max_cycles = suite_timing.global_cycles.unwrap_or(1);
    let total_tests = thread_blocks.len() * test_definitions.len() * max_cycles as usize;
    progress.total_tests.store(total_tests as u64, Ordering::Relaxed);

    // Start progress reporter thread
    let progress_clone = Arc::clone(&progress);
    let progress_handle = thread::spawn(move || {
        progress_reporter(progress_clone);
    });

    // Run test suite with timing control
    let suite_start = Instant::now();
    let mut current_cycle = 0u32;
    
    println!("Starting test suite with timing: {:?}", suite_timing);
    
    loop {
        current_cycle += 1;
        let cycle_start = Instant::now();
        
        println!("\n=== Test Suite Cycle {} ===", current_cycle);
        progress.set_phase(&format!("Cycle {}", current_cycle));
        
        // Run one complete cycle of all tests
        let cycle_success = run_single_test_cycle(
            &thread_blocks,
            &mut allocated_blocks,
            &test_definitions,
            error_mode,
            Arc::clone(&progress),
            current_cycle,
        );
        
        if !cycle_success {
            success.store(false, Ordering::Relaxed);
            break;
        }
        
        let cycle_elapsed = cycle_start.elapsed().as_secs() as u32;
        let total_elapsed = suite_start.elapsed().as_secs() as u32;
        
        println!("Cycle {} completed in {}s (total: {}s)", current_cycle, cycle_elapsed, total_elapsed);
        
        // Check if we should continue to next cycle
        if !suite_timing.should_continue_suite(current_cycle, total_elapsed) {
            println!("Test suite timing limits reached - stopping");
            break;
        }
    }

    progress.set_phase("Completed");
    thread::sleep(std::time::Duration::from_millis(100));

    if let Err(_) = progress_handle.join() {
        log::warn!("Progress reporter thread failed to join cleanly");
    }

    log::info!("Releasing all allocated memory blocks");
    success.load(Ordering::Relaxed)
}

fn create_test_definitions() -> Vec<(&'static str, unsafe fn(*mut u8, usize, usize, ErrorMode) -> TestStats, TestMemoryConfig)> {
    vec![
        // === CRITICAL: Full Memory Stuck Bit Test ===
        (
            "StuckBitTest", 
            stuck_bit_test,
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
            mirror_move_128_non_temporal,
            TestMemoryConfig::new(
                WindowMode::FixedSize { size_mb: 64 },   // 64MB window for SIMD locality
                BlockMode::FixedSize { size_mb: 16 },    // 16MB blocks for 128-bit alignment
                false,  // Require alignment
                true    // Needs locality for SIMD efficiency
            ).with_timing(TestTiming::duration_only(10)) // 10 seconds of continuous SIMD testing
        ),
        (
            "MirrorMove256NonTemporal", 
            mirror_move_256_non_temporal,
            TestMemoryConfig::new(
                WindowMode::FixedSize { size_mb: 128 },  // 128MB window
                BlockMode::FixedSize { size_mb: 32 },    // 32MB blocks for 256-bit alignment
                false,
                true
            ).with_timing(TestTiming::duration_only(10))
        ),
        (
            "MirrorMove512NonTemporal", 
            mirror_move_512_non_temporal,
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
            simple_test,
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
            refresh_stable,
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
            cache_busting_write_test,
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
            random_access_torture_test,
            TestMemoryConfig::new(
                WindowMode::FullAllocation,              // Random access across full allocation
                BlockMode::FixedSize { size_mb: 8 },     // 8MB blocks
                true,   // Allow misaligned for maximum stress
                false   // Random access - locality not needed
            ).with_timing(TestTiming::duration_only(25)) // 25 seconds of random torture
        ),
        (
            "StrideAccess", 
            stride_access_test,
            TestMemoryConfig::new(
                WindowMode::FullAllocation,              // Test stride patterns across full memory
                BlockMode::FixedSize { size_mb: 2 },     // 2MB blocks
                false,
                false
            ).with_timing(TestTiming::cycles_only(50)) // 50 cycles of stride patterns
        ),
        
        // === Bandwidth Test ===
        (
            "BandwidthSat", 
            bandwidth_saturation_test,
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
    test_definitions: &[(&'static str, unsafe fn(*mut u8, usize, usize, ErrorMode) -> TestStats, TestMemoryConfig)],
    error_mode: ErrorMode,
    progress: Arc<ProgressTracker>,
    cycle_number: u32,
) -> bool {
    let thread_count = thread_blocks.len();
    let barrier = Arc::new(Barrier::new(thread_count));
    let success = Arc::new(AtomicBool::new(true));
    let mut handles = vec![];

    // Extract all allocated blocks before threading
    let mut thread_allocated_data = Vec::new();
    for (thread_id, _) in thread_blocks {
        let blocks = allocated_blocks.remove(thread_id).unwrap();
        thread_allocated_data.push((*thread_id, blocks));
    }

    for (thread_id, blocks) in thread_allocated_data {
        let success_clone = Arc::clone(&success);
        let progress_clone = Arc::clone(&progress);
        let barrier_clone = Arc::clone(&barrier);
        let test_definitions_clone = test_definitions.to_vec();

        let handle = thread::spawn(move || {
            if let Err(e) = run_thread_test_cycle(
                thread_id,
                &blocks,
                test_definitions_clone,
                error_mode,
                progress_clone,
                barrier_clone,
                cycle_number,
            ) {
                log::error!("[Thread {}] Cycle {} Error: {}", thread_id, cycle_number, e);
                success_clone.store(false, Ordering::Relaxed);
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        if handle.join().is_err() {
            success.store(false, Ordering::Relaxed);
        }
    }

    success.load(Ordering::Relaxed)
}

fn run_thread_test_cycle(
    thread_id: usize,
    allocated_blocks: &[AllocatedBlock],  // Changed from Vec<AllocatedBlock>
    test_definitions: Vec<(&'static str, unsafe fn(*mut u8, usize, usize, ErrorMode) -> TestStats, TestMemoryConfig)>,
    error_mode: ErrorMode,
    progress: Arc<ProgressTracker>,
    barrier: Arc<Barrier>,
    cycle_number: u32,
) -> Result<(), String> {
    
	log::info!(
		"[Thread {}] Cycle {} starting with {} pre-allocated blocks",
		thread_id, cycle_number, allocated_blocks.len()
	);

    // Run each test type across all blocks before moving to next test type
    for (test_name, test_func, test_config) in test_definitions {
        progress.set_phase(&format!("Cycle {} - {}", cycle_number, test_name));

        // Wait for all threads to reach this test phase
        barrier.wait();

        // Run test with timing control
        let test_start = Instant::now();
        let mut test_cycle = 0u32;
        
        loop {
            test_cycle += 1;
            
            // Run test on all blocks for this thread
            for (block_idx, allocated_block) in allocated_blocks.iter().enumerate() {
                log::debug!(
                    "[Thread {}] Cycle {} Block {}/{} - Running {} (test cycle {})",
                    thread_id, cycle_number, block_idx + 1, allocated_blocks.len(), test_name, test_cycle
                );

                match run_test_with_memory_stages(
                    test_name,
                    test_func,
                    allocated_block,
                    &test_config,
                    thread_id,
                    error_mode,
                ) {
                    Ok(stats) => {
                        log_stats(&stats);
                        progress.complete_test(&stats);

                        if stats.error_count > 0 {
                            handle_test_errors(&stats, error_mode, test_name)?;
                        }
                    }
                    Err(e) => {
                        log::error!("[Thread {}] Test {} failed: {}", thread_id, test_name, e);
                        return Err(e);
                    }
                }
            }
            
            // Check if we should continue this test based on timing configuration
            let test_elapsed_secs = test_start.elapsed().as_secs() as u32;
            if !test_config.timing.should_continue(test_cycle, test_elapsed_secs) {
                log::debug!("[Thread {}] {} completed: {} cycles in {}s", 
                           thread_id, test_name, test_cycle, test_elapsed_secs);
                break;
            }
        }

        // Wait for all threads to complete this test phase
        barrier.wait();
    }

    Ok(())
}

// Test function with three-stage memory management (unchanged logic, enhanced logging)
fn run_test_with_memory_stages(
    test_name: &str,
    test_func: unsafe fn(*mut u8, usize, usize, ErrorMode) -> TestStats,
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
    
    // Enhanced logging for Stage 2 & 3 configuration
    let window_mb = final_window_size as f64 / (1024.0 * 1024.0);
    let block_mb = block_size as f64 / (1024.0 * 1024.0);
    let allocated_mb = allocated_size as f64 / (1024.0 * 1024.0);
    let window_percent = (final_window_size as f64 / allocated_size as f64) * 100.0;
    
    if final_window_size == allocated_size {
        log::debug!(
            "[Thread {}] {} - Stage 2: Full allocation window ({:.1}MB = 100%)",
            thread_id, test_name, window_mb
        );
    } else {
        log::debug!(
            "[Thread {}] {} - Stage 2: Window {:.1}MB of {:.1}MB allocated ({:.1}%){}",
            thread_id, test_name, window_mb, allocated_mb, window_percent,
            if window_aligned { " (aligned to blocks)" } else { "" }
        );
    }
    
    log::debug!(
        "[Thread {}] {} - Stage 3: Block size {:.1}MB{}{}",
        thread_id, test_name, block_mb,
        if block_adjusted { " (auto-aligned)" } else { "" },
        if test_config.allow_misaligned { " (misaligned allowed)" } else { "" }
    );
    
    // Run the test function on the configured window
    let stats = unsafe {
        test_func(allocated_ptr, final_window_size, thread_id, error_mode)
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
    let mut total_blocks = 0;
    let mut total_size = 0usize;
    let mut large_page_blocks = 0;
    let mut size_per_thread = 0usize;

    for (thread_id, blocks) in allocated_blocks {
        let thread_total_size: usize = blocks.iter().map(|b| b.buffer.size()).sum();
        let thread_large_pages = blocks.iter().filter(|b| b.buffer.uses_large_pages()).count();

        total_blocks += blocks.len();
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

fn log_stats(stats: &TestStats) {
    let gib = stats.bytes_processed as f64 / (1024.0 * 1024.0 * 1024.0);
    let secs = stats.elapsed_ms as f64 / 1000.0;
    let throughput = if secs > 0.0 { gib / secs } else { 0.0 };
    log::debug!(
        "[T{}] [{}] ({}) completed in {} ms — {:.2} GiB/s — {} errors",
        stats.thread_id,
        stats.name,
        stats.action.label(),
        stats.elapsed_ms,
        throughput,
        stats.error_count
    );
}
