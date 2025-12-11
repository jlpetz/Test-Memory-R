// Sequential Bandwidth Tests - Measure peak memory bandwidth for different cache levels
//
// Unlike latency tests (random access to defeat prefetchers), these use sequential access
// to maximize throughput and measure peak bandwidth at each cache level.
//
// Tests:
// - Bw-{Level}-Read:  Sequential read bandwidth
// - Bw-{Level}-Write: Sequential write bandwidth
// - Bw-{Level}-Copy:  Sequential copy bandwidth (read + write)
//
// Where {Level} is: L1, L2, L3, DRAM, DRAMFull

use crate::ErrorMode;
use crate::runner::{AllocationBlock, SHUTDOWN_REQUESTED};
use crate::tests::{TestAction, TestMemoryConfig, TestProgress, TestTiming, TestStats, prepare_blocks_for_window};
use std::sync::atomic::Ordering;
use std::time::Instant;

const MB_F64: f64 = 1024.0 * 1024.0;

// ============================================================================
// WRITE Bandwidth Test - Sequential writes
// ============================================================================

/// Sequential Write Bandwidth Test
/// Writes sequential u64 values to memory as fast as possible.
/// Uses a simple incrementing pattern to prevent compiler optimization while
/// keeping overhead minimal.
pub unsafe fn write_bandwidth_multi(
    blocks: &[AllocationBlock],
    thread_id: usize,
    _error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> TestStats {
    let test_name = "WriteBandwidth";
    let start = Instant::now();

    let total_allocated: usize = blocks.iter().map(|b| b.buffer.size()).sum();
    let window_size = config.calculate_window_size(test_name, total_allocated);
    let test_blocks = prepare_blocks_for_window(blocks, window_size, test_name);
    let total_test_size: usize = test_blocks.iter().map(|b| b.test_size).sum();

    if test_blocks.is_empty() {
        return TestStats {
            name: test_name,
            action: TestAction::Write,
            bytes_processed: 0,
            elapsed_ms: 0,
            thread_id,
            error_count: 0,
            total_operations: 0,
            cycles_completed: 0,
            cycles_planned: timing.cycles,
            stopped_by_time_limit: false,
        };
    }

    let working_set_bytes = window_size.min(total_test_size);
    log::info!("[Thread {}] {} - Working set: {:.2} MiB targeting {}",
        thread_id, test_name, working_set_bytes as f64 / MB_F64,
        config.window_mode.target_level_name());

    let mut cycle = 0u32;
    let mut total_bytes_processed = 0u64;
    let mut last_progress_update = Instant::now();

    // Base pattern varies by thread to avoid false sharing detection
    let base_pattern = 0xDEADBEEF_CAFEBABE_u64.wrapping_add(thread_id as u64 * 0x1234567890ABCDEF);

    loop {
        cycle += 1;

        for test_block in test_blocks.iter() {
            let base = test_block.block.buffer.as_mut_ptr() as *mut u64;
            let len = test_block.test_size.min(working_set_bytes) / std::mem::size_of::<u64>();

            // Pattern changes each cycle to prevent any caching optimization
            let pattern = base_pattern.wrapping_add(cycle as u64);

            // Sequential write - simple incrementing pattern
            // The wrapping_add(i) prevents the compiler from optimizing to memset
            for i in 0..len {
                *base.add(i) = pattern.wrapping_add(i as u64);
            }

            total_bytes_processed += (len * std::mem::size_of::<u64>()) as u64;

            // Check for shutdown
            if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
                return TestStats {
                    name: test_name,
                    action: TestAction::Write,
                    bytes_processed: total_bytes_processed as usize,
                    elapsed_ms: start.elapsed().as_millis(),
                    thread_id,
                    error_count: 0,
                    total_operations: total_bytes_processed / 8,
                    cycles_completed: cycle,
                    cycles_planned: timing.cycles,
                    stopped_by_time_limit: false,
                };
            }
        }

        // Update progress
        if let Some(progress) = progress {
            let now = Instant::now();
            if now.duration_since(last_progress_update).as_millis() >= 250 {
                progress.cycles_completed.store(cycle, Ordering::Relaxed);
                progress.bytes_processed.store(total_bytes_processed, Ordering::Relaxed);
                progress.last_update_ms.store(start.elapsed().as_millis() as u64, Ordering::Relaxed);
                last_progress_update = now;
            }
        }

        // Check timing limits
        let elapsed_secs = start.elapsed().as_secs() as u32;
        if !timing.should_continue(cycle, elapsed_secs) {
            break;
        }
    }

    TestStats {
        name: test_name,
        action: TestAction::Write,
        bytes_processed: total_bytes_processed as usize,
        elapsed_ms: start.elapsed().as_millis(),
        thread_id,
        error_count: 0,
        total_operations: total_bytes_processed / 8,
        cycles_completed: cycle,
        cycles_planned: timing.cycles,
        stopped_by_time_limit: timing.cycles.map_or(true, |limit| cycle < limit),
    }
}

// ============================================================================
// READ Bandwidth Test - Sequential reads
// ============================================================================

/// Sequential Read Bandwidth Test
/// Reads sequential u64 values from memory as fast as possible.
/// Accumulates values to prevent compiler from optimizing away reads.
///
/// IMPORTANT: Memory must be initialized first! The test initialization phase
/// writes patterns to ensure OS has actually allocated physical pages.
pub unsafe fn read_bandwidth_multi(
    blocks: &[AllocationBlock],
    thread_id: usize,
    _error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> TestStats {
    let test_name = "ReadBandwidth";
    let start = Instant::now();

    let total_allocated: usize = blocks.iter().map(|b| b.buffer.size()).sum();
    let window_size = config.calculate_window_size(test_name, total_allocated);
    let test_blocks = prepare_blocks_for_window(blocks, window_size, test_name);
    let total_test_size: usize = test_blocks.iter().map(|b| b.test_size).sum();

    if test_blocks.is_empty() {
        return TestStats {
            name: test_name,
            action: TestAction::Read,
            bytes_processed: 0,
            elapsed_ms: 0,
            thread_id,
            error_count: 0,
            total_operations: 0,
            cycles_completed: 0,
            cycles_planned: timing.cycles,
            stopped_by_time_limit: false,
        };
    }

    let working_set_bytes = window_size.min(total_test_size);
    log::info!("[Thread {}] {} - Working set: {:.2} MiB targeting {}",
        thread_id, test_name, working_set_bytes as f64 / MB_F64,
        config.window_mode.target_level_name());

    // INIT PHASE: Write pattern to memory first
    // This ensures physical pages are allocated and prevents zero-page optimization
    let init_start = Instant::now();
    let init_pattern = 0xA5A5A5A5_5A5A5A5A_u64.wrapping_add(thread_id as u64);
    for test_block in test_blocks.iter() {
        let base = test_block.block.buffer.as_mut_ptr() as *mut u64;
        let len = test_block.test_size.min(working_set_bytes) / std::mem::size_of::<u64>();
        for i in 0..len {
            *base.add(i) = init_pattern.wrapping_add(i as u64);
        }
    }
    let init_elapsed = init_start.elapsed();
    log::debug!("[Thread {}] {} - Init phase completed in {:?}", thread_id, test_name, init_elapsed);

    // TEST PHASE: Sequential reads
    let test_start = Instant::now();
    let mut cycle = 0u32;
    let mut total_bytes_processed = 0u64;
    let mut last_progress_update = Instant::now();

    loop {
        cycle += 1;

        for test_block in test_blocks.iter() {
            // Note: as_mut_ptr used even for reads since MemoryBuffer doesn't expose as_ptr
            let base = test_block.block.buffer.as_mut_ptr() as *const u64;
            let len = test_block.test_size.min(working_set_bytes) / std::mem::size_of::<u64>();

            // Sequential read with accumulator to prevent optimization
            let mut accumulator = 0u64;
            for i in 0..len {
                accumulator = accumulator.wrapping_add(*base.add(i));
            }

            // Prevent compiler from optimizing away the reads
            std::hint::black_box(accumulator);

            total_bytes_processed += (len * std::mem::size_of::<u64>()) as u64;

            // Check for shutdown
            if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
                return TestStats {
                    name: test_name,
                    action: TestAction::Read,
                    bytes_processed: total_bytes_processed as usize,
                    elapsed_ms: start.elapsed().as_millis(),
                    thread_id,
                    error_count: 0,
                    total_operations: total_bytes_processed / 8,
                    cycles_completed: cycle,
                    cycles_planned: timing.cycles,
                    stopped_by_time_limit: false,
                };
            }
        }

        // Update progress
        if let Some(progress) = progress {
            let now = Instant::now();
            if now.duration_since(last_progress_update).as_millis() >= 250 {
                progress.cycles_completed.store(cycle, Ordering::Relaxed);
                progress.bytes_processed.store(total_bytes_processed, Ordering::Relaxed);
                progress.last_update_ms.store(start.elapsed().as_millis() as u64, Ordering::Relaxed);
                last_progress_update = now;
            }
        }

        // Check timing limits (applies to TEST phase, but we report end-to-end)
        let elapsed_secs = test_start.elapsed().as_secs() as u32;
        if !timing.should_continue(cycle, elapsed_secs) {
            break;
        }
    }

    TestStats {
        name: test_name,
        action: TestAction::Read,
        bytes_processed: total_bytes_processed as usize,
        elapsed_ms: start.elapsed().as_millis(),  // End-to-end including init
        thread_id,
        error_count: 0,
        total_operations: total_bytes_processed / 8,
        cycles_completed: cycle,
        cycles_planned: timing.cycles,
        stopped_by_time_limit: timing.cycles.map_or(true, |limit| cycle < limit),
    }
}

// ============================================================================
// COPY Bandwidth Test - Sequential read from source, write to destination
// ============================================================================

/// Sequential Copy Bandwidth Test
/// Reads from first half of memory, writes to second half.
/// This measures combined read+write bandwidth (like memcpy).
pub unsafe fn copy_bandwidth_multi(
    blocks: &[AllocationBlock],
    thread_id: usize,
    _error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> TestStats {
    let test_name = "CopyBandwidth";
    let start = Instant::now();

    let total_allocated: usize = blocks.iter().map(|b| b.buffer.size()).sum();
    let window_size = config.calculate_window_size(test_name, total_allocated);
    let test_blocks = prepare_blocks_for_window(blocks, window_size, test_name);
    let total_test_size: usize = test_blocks.iter().map(|b| b.test_size).sum();

    if test_blocks.is_empty() {
        return TestStats {
            name: test_name,
            action: TestAction::Copy,
            bytes_processed: 0,
            elapsed_ms: 0,
            thread_id,
            error_count: 0,
            total_operations: 0,
            cycles_completed: 0,
            cycles_planned: timing.cycles,
            stopped_by_time_limit: false,
        };
    }

    let working_set_bytes = window_size.min(total_test_size);
    log::info!("[Thread {}] {} - Working set: {:.2} MiB targeting {}",
        thread_id, test_name, working_set_bytes as f64 / MB_F64,
        config.window_mode.target_level_name());

    // INIT PHASE: Write pattern to source half
    let init_start = Instant::now();
    let init_pattern = 0xCAFEBABE_DEADBEEF_u64.wrapping_add(thread_id as u64);
    for test_block in test_blocks.iter() {
        let base = test_block.block.buffer.as_mut_ptr() as *mut u64;
        // Source is first half
        let half_len = (test_block.test_size.min(working_set_bytes) / 2) / std::mem::size_of::<u64>();
        for i in 0..half_len {
            *base.add(i) = init_pattern.wrapping_add(i as u64);
        }
    }
    let init_elapsed = init_start.elapsed();
    log::debug!("[Thread {}] {} - Init phase completed in {:?}", thread_id, test_name, init_elapsed);

    // TEST PHASE: Copy from first half to second half
    let test_start = Instant::now();
    let mut cycle = 0u32;
    let mut total_bytes_processed = 0u64;
    let mut last_progress_update = Instant::now();

    loop {
        cycle += 1;

        for test_block in test_blocks.iter() {
            let base = test_block.block.buffer.as_mut_ptr() as *mut u64;
            let block_working_set = test_block.test_size.min(working_set_bytes);
            let half_len = (block_working_set / 2) / std::mem::size_of::<u64>();

            // Source = first half, destination = second half
            let src = base;
            let dst = base.add(half_len);

            // Sequential copy: read from src, write to dst
            for i in 0..half_len {
                let value = *src.add(i);
                *dst.add(i) = value;
            }

            // Bytes processed = read bytes + write bytes
            total_bytes_processed += (half_len * std::mem::size_of::<u64>() * 2) as u64;

            // Check for shutdown
            if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
                return TestStats {
                    name: test_name,
                    action: TestAction::Copy,
                    bytes_processed: total_bytes_processed as usize,
                    elapsed_ms: start.elapsed().as_millis(),
                    thread_id,
                    error_count: 0,
                    total_operations: total_bytes_processed / 8,
                    cycles_completed: cycle,
                    cycles_planned: timing.cycles,
                    stopped_by_time_limit: false,
                };
            }
        }

        // Update progress
        if let Some(progress) = progress {
            let now = Instant::now();
            if now.duration_since(last_progress_update).as_millis() >= 250 {
                progress.cycles_completed.store(cycle, Ordering::Relaxed);
                progress.bytes_processed.store(total_bytes_processed, Ordering::Relaxed);
                progress.last_update_ms.store(start.elapsed().as_millis() as u64, Ordering::Relaxed);
                last_progress_update = now;
            }
        }

        // Check timing limits
        let elapsed_secs = test_start.elapsed().as_secs() as u32;
        if !timing.should_continue(cycle, elapsed_secs) {
            break;
        }
    }

    TestStats {
        name: test_name,
        action: TestAction::Copy,
        bytes_processed: total_bytes_processed as usize,
        elapsed_ms: start.elapsed().as_millis(),  // End-to-end including init
        thread_id,
        error_count: 0,
        total_operations: total_bytes_processed / 8,
        cycles_completed: cycle,
        cycles_planned: timing.cycles,
        stopped_by_time_limit: timing.cycles.map_or(true, |limit| cycle < limit),
    }
}
