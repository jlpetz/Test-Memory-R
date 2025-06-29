use crate::{ErrorMode, MemoryLayout, ProgressTracker, BlockInfo, TestBuffer};
use crate::tests::{TestStats, TestMemoryConfig, TestAction};
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

// Pre-allocated memory block management
pub struct AllocatedBlock {
    pub buffer: TestBuffer,
    pub block_info: BlockInfo,
}

pub fn run_tests_with_layout(layout: MemoryLayout, error_mode: ErrorMode) -> bool {
    layout.print_layout();

    let progress = Arc::new(ProgressTracker::new());
    let success = Arc::new(AtomicBool::new(true));

    // Calculate total tests
    let test_types = 9;
    let total_tests = layout.blocks.len() * test_types;
    progress.total_tests.store(total_tests as u64, Ordering::Relaxed);

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

    // Start progress reporter thread
    let progress_clone = Arc::clone(&progress);
    let progress_handle = thread::spawn(move || {
        progress_reporter(progress_clone);
    });

    let thread_count = thread_blocks.len();
    let barrier = Arc::new(Barrier::new(thread_count));
    let mut handles = vec![];

    for (thread_id, _) in thread_blocks {
        let success_clone = Arc::clone(&success);
        let progress_clone = Arc::clone(&progress);
        let barrier_clone = Arc::clone(&barrier);
        let thread_allocated_blocks = allocated_blocks.remove(&thread_id).unwrap();

        let handle = thread::spawn(move || {
            if let Err(e) =
                run_thread_tests_with_allocated_blocks(thread_id, thread_allocated_blocks, error_mode, progress_clone, barrier_clone)
            {
                log::error!("[Thread {}] Error: {}", thread_id, e);
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

    progress.set_phase("Completed");
    thread::sleep(std::time::Duration::from_millis(100));

    if let Err(_) = progress_handle.join() {
        log::warn!("Progress reporter thread failed to join cleanly");
    }

    log::info!("Releasing all allocated memory blocks");
    success.load(Ordering::Relaxed)
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
        "  {} × {:.2} GiB per thread = {:.2} GiB total (TM5-style)",
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

// Test function with three-stage memory management
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
    
    // Stage 2: Calculate optimal window size within allocation
    let optimal_window_size = test_config.calculate_optimal_window_size(test_name, allocated_size);
    let (window_size, window_adjusted) = if test_config.window_size_bytes > 0 {
        (test_config.window_size_bytes.min(allocated_size), false)
    } else {
        (optimal_window_size, true)
    };
    
    // Stage 3: Calculate optimal block size with alignment
    let (block_size, block_adjusted) = test_config.calculate_optimal_block_size(test_name, window_size);
    
    // Ensure window is multiple of block size
    let (final_window_size, window_aligned) = test_config.align_window_to_blocks(window_size, block_size);
    
    // Log Stage 2 & 3 configuration
    let window_mb = final_window_size as f64 / (1024.0 * 1024.0);
    let block_mb = block_size as f64 / (1024.0 * 1024.0);
    let allocated_mb = allocated_size as f64 / (1024.0 * 1024.0);
    
    log::info!(
        "[Thread {}] {} - Stage 2: Window {:.1}MB/{:.1}MB allocated{}{}",
        thread_id, test_name, window_mb, allocated_mb,
        if window_adjusted { " (auto-sized)" } else { "" },
        if window_aligned { " (aligned to blocks)" } else { "" }
    );
    
    log::info!(
        "[Thread {}] {} - Stage 3: Block size {:.1}MB{}",
        thread_id, test_name, block_mb,
        if block_adjusted { " (auto-aligned)" } else { "" }
    );
    
    // Run the test function on the configured window
    let stats = unsafe {
        test_func(allocated_ptr, final_window_size, thread_id, error_mode)
    };
    
    Ok(stats)
}

fn run_thread_tests_with_allocated_blocks(
    thread_id: usize,
    allocated_blocks: Vec<AllocatedBlock>,
    error_mode: ErrorMode,
    progress: Arc<ProgressTracker>,
    barrier: Arc<Barrier>,
) -> Result<(), String> {
    log::info!(
        "[Thread {}] Starting tests with {} pre-allocated blocks",
        thread_id,
        allocated_blocks.len()
    );

    // Define test functions with their configurations
    let test_definitions: Vec<(&str, unsafe fn(*mut u8, usize, usize, ErrorMode) -> TestStats, TestMemoryConfig)> = vec![
        (
            "MirrorMove128NonTemporal", 
            mirror_move_128_non_temporal,
            TestMemoryConfig::new(Some(64), Some(16), false) // 64MB window, 16MB blocks
        ),
        (
            "MirrorMove256NonTemporal", 
            mirror_move_256_non_temporal,
            TestMemoryConfig::new(Some(128), Some(32), false) // 128MB window, 32MB blocks
        ),
        (
            "MirrorMove512NonTemporal", 
            mirror_move_512_non_temporal,
            TestMemoryConfig::new(Some(256), Some(64), false) // 256MB window, 64MB blocks
        ),
        (
            "SimpleTest", 
            simple_test,
            TestMemoryConfig::new(None, Some(4), false) // Auto window, 4MB blocks
        ),
        (
            "RefreshStable", 
            refresh_stable,
            TestMemoryConfig::new(Some(32), Some(1), false) // 32MB window, 1MB blocks
        ),
        (
            "CacheBusting", 
            cache_busting_write_test,
            TestMemoryConfig::new(None, Some(1), false) // Auto cache-sized window, 1MB blocks
        ),
        (
            "RandomTorture", 
            random_access_torture_test,
            TestMemoryConfig::new(None, Some(8), false) // Auto window, 8MB blocks
        ),
        (
            "StrideAccess", 
            stride_access_test,
            TestMemoryConfig::new(None, Some(2), false) // Auto window, 2MB blocks
        ),
        (
            "BandwidthSat", 
            bandwidth_saturation_test,
            TestMemoryConfig::new(None, Some(32), false) // Auto large window, 32MB blocks
        ),
    ];

    // Run each test type across all blocks before moving to next test type
    for (test_name, test_func, test_config) in test_definitions {
        progress.set_phase(&format!("Stage 2&3: {}", test_name));

        // Wait for all threads to reach this test phase
        barrier.wait();

        for (block_idx, allocated_block) in allocated_blocks.iter().enumerate() {
            log::debug!(
                "[Thread {}] Block {}/{} - Running {} with Stage 2&3 configuration",
                thread_id,
                block_idx + 1,
                allocated_blocks.len(),
                test_name
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

        // Wait for all threads to complete this test phase
        barrier.wait();
    }

    Ok(())
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