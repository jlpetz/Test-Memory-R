//! Zero-cost test orchestration harness for TMR v2 tests.
//!
//! This replaces the duplicated boilerplate in each test function with a single
//! generic function that monomorphizes per call site. Unlike the failed
//! `test_framework.rs` approach which used `dyn TestPattern` (vtable dispatch in
//! hot loop → no inlining → no SIMD), this uses concrete generic types that the
//! compiler fully specializes.
//!
//! # Why this works where traits didn't
//!
//! - `Init`/`Test`/`Verify` are concrete generic types → compiler monomorphizes
//! - `#[inline(always)]` on the harness → orchestration inlines into caller
//! - Closures capture SIMD constants by value → no indirection
//! - No vtable, no boxing, no `dyn`

use crate::runner::{AllocationBlock, SHUTDOWN_REQUESTED};
use crate::tests::{
    TestAction, TestMemoryConfig, TestProgress, TestStats, TestTiming,
    calculate_ideal_chunk_size, get_safe_chunk_size, prepare_blocks_for_window,
};
use crate::ErrorMode;
use std::sync::atomic::Ordering;
use std::time::Instant;

/// Chunk-level context passed to closures.
/// Contains pre-computed values so closures don't need to recompute them.
pub struct ChunkCtx {
    /// Pointer to the start of this block's memory (as u64 elements).
    pub ptr: *mut u64,
    /// Start element index within this block (0-based).
    pub chunk_start: usize,
    /// End element index (exclusive) within this block.
    pub chunk_end: usize,
    /// Current cycle number (1-based).
    pub cycle: u32,
    /// Thread ID running this test.
    pub thread_id: usize,
    /// Error check interval mask from config (None = PER_CHUNK, Some(mask) = check every N elements).
    /// Verify closures should use this for intermediate error batch reporting.
    pub check_mask: Option<u32>,
}

impl ChunkCtx {
    /// Number of u64 elements in this chunk.
    #[inline(always)]
    pub fn len(&self) -> usize {
        self.chunk_end - self.chunk_start
    }
}

/// Run a v2 test with monomorphized closures for init, test, and verify phases.
///
/// This handles ALL orchestration boilerplate:
/// - Block preparation with window limits
/// - Chunked iteration for shutdown responsiveness
/// - Cycle/timing loop
/// - Error accumulation and mode handling
/// - Progress reporting
/// - Stats collection
///
/// # Type Parameters
///
/// - `Init`: Called once per block at the start to write initial patterns.
///   Signature: `fn(ctx: &ChunkCtx)` — write patterns to `ctx.ptr[ctx.chunk_start..ctx.chunk_end]`
/// - `Test`: Called per chunk per cycle for the "shake" operation (mirror swap, etc).
///   May be a no-op closure `|_| {}` for tests that only write+verify.
///   Signature: `fn(ctx: &ChunkCtx)` — mutate memory in `ctx.ptr[ctx.chunk_start..ctx.chunk_end]`
/// - `Verify`: Called per chunk per cycle to verify patterns. Returns error count for this chunk.
///   Signature: `fn(ctx: &ChunkCtx) -> u64` — verify and return error count
///
/// # Safety
///
/// Caller must ensure all blocks contain valid, aligned, writable memory.
#[inline(always)]
#[allow(clippy::too_many_arguments)]
pub unsafe fn run_test_v2<Init, Test, Verify>(
    blocks: &[AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
    test_name: &'static str,
    action: TestAction,
    mut init_fn: Init,
    mut test_fn: Test,
    mut verify_fn: Verify,
) -> TestStats
where
    Init: FnMut(&ChunkCtx),
    Test: FnMut(&ChunkCtx),
    Verify: FnMut(&ChunkCtx) -> u64,
{
    if blocks.is_empty() {
        return TestStats {
            name: test_name,
            action,
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

    // Prepare blocks with window limits
    let total_allocated: usize = blocks.iter().map(|b| b.buffer.size()).sum();
    let window_size = config.calculate_window_size(test_name, total_allocated);
    let test_blocks = prepare_blocks_for_window(blocks, window_size, test_name);

    if test_blocks.is_empty() {
        log::warn!("{}: No blocks prepared for testing", test_name);
        return TestStats {
            name: test_name,
            action,
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

    // Pre-compute error check interval mask from config (v1 parity)
    let check_mask = config.error_check_interval.get_check_mask();

    // Tracking
    let mut total_bytes_processed = 0usize;
    let mut total_error_count = 0u64;
    let mut total_operations = 0u64;

    // Start timer BEFORE init — v1 includes the first write in its cycle timing,
    // so v2 must include init in elapsed time for fair A/B comparison.
    let test_start = Instant::now();
    let start = Instant::now();
    let mut cycle = 0u32;

    // Progress reporting — 250ms matches v1 SIMD update frequency
    let update_interval_ms = 250u128;
    let mut last_progress_update = Instant::now();

    // Pre-compute per-block metadata once (ptr, len, chunk_size don't change between cycles)
    struct BlockMeta {
        ptr: *mut u64,
        len_elements: usize,
        chunk_size_elements: usize,
        test_size_bytes: usize,
    }
    let block_metas: Vec<BlockMeta> = test_blocks.iter().map(|tb| {
        let ptr = tb.block.buffer.as_mut_ptr() as *mut u64;
        let len_elements = tb.test_size / std::mem::size_of::<u64>();
        let ideal = calculate_ideal_chunk_size(config, test_name, tb.test_size);
        let chunk_bytes = get_safe_chunk_size(ideal, tb.test_size);
        let chunk_size_elements = chunk_bytes / std::mem::size_of::<u64>();
        BlockMeta { ptr, len_elements, chunk_size_elements, test_size_bytes: tb.test_size }
    }).collect();

    // Initialize all blocks with patterns (timed, matching v1 behavior)
    for meta in block_metas.iter() {
        let ctx = ChunkCtx {
            ptr: meta.ptr,
            chunk_start: 0,
            chunk_end: meta.len_elements,
            cycle: 0,
            thread_id,
            check_mask,
        };
        init_fn(&ctx);
    }
    std::sync::atomic::fence(Ordering::SeqCst);

    // Main test loop — interleaves across blocks
    'outer: loop {
        cycle += 1;

        for meta in block_metas.iter() {
            let mut block_errors = 0u64;

            for chunk_start in (0..meta.len_elements).step_by(meta.chunk_size_elements) {
                let chunk_end = (chunk_start + meta.chunk_size_elements).min(meta.len_elements);
                let ctx = ChunkCtx {
                    ptr: meta.ptr,
                    chunk_start,
                    chunk_end,
                    cycle,
                    thread_id,
                    check_mask,
                };

                // Test phase (mirror swap, etc.)
                test_fn(&ctx);

                std::sync::atomic::fence(Ordering::SeqCst);

                // Verify phase
                let errors = verify_fn(&ctx);
                block_errors += errors;

                // Check for shutdown
                if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
                    total_error_count += block_errors;
                    total_bytes_processed += (chunk_end - chunk_start) * std::mem::size_of::<u64>() * 2;
                    break 'outer;
                }
            }

            total_error_count += block_errors;
            total_bytes_processed += meta.test_size_bytes * 2;
            total_operations += meta.len_elements as u64 * 2;

            // Handle errors
            if block_errors > 0 {
                match error_mode {
                    ErrorMode::Panic => {
                        panic!("{}: {} memory errors detected on thread {}", test_name, block_errors, thread_id);
                    }
                    ErrorMode::Halt => {
                        break 'outer;
                    }
                    ErrorMode::Log => { /* Continue */ }
                }
            }

            // Check for shutdown between blocks
            if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
                break 'outer;
            }
        }

        // Progress update
        if let Some(progress) = progress {
            let now = Instant::now();
            if now.duration_since(last_progress_update).as_millis() >= update_interval_ms {
                progress.cycles_completed.store(cycle, Ordering::Relaxed);
                progress.bytes_processed.store(total_bytes_processed as u64, Ordering::Relaxed);
                progress.errors_found.store(total_error_count, Ordering::Relaxed);
                progress.last_update_ms.store(now.duration_since(start).as_millis() as u64, Ordering::Relaxed);
                last_progress_update = now;
            }
        }

        // Check timing
        let elapsed_secs = test_start.elapsed().as_secs() as u32;
        if !timing.should_continue(cycle, elapsed_secs) {
            break;
        }

        if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
            break;
        }
    }

    TestStats {
        name: test_name,
        action,
        bytes_processed: total_bytes_processed,
        elapsed_ms: start.elapsed().as_millis(),
        thread_id,
        error_count: total_error_count,
        total_operations,
        cycles_completed: cycle,
        cycles_planned: timing.cycles,
        stopped_by_time_limit: timing.cycles.map_or(true, |limit| cycle < limit),
    }
}

/// Run a v2 test that needs a separate init phase (written once, then test+verify loop).
/// The `bytes_multiplier` controls how bytes_processed is computed per block per cycle
/// (e.g., 5 for MirrorMove: 2R mirror + 1R verify + 2W mirror-back).
#[inline(always)]
#[allow(clippy::too_many_arguments)]
pub unsafe fn run_test_v2_with_init<Init, Test, Verify>(
    blocks: &[AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
    test_name: &'static str,
    action: TestAction,
    bytes_multiplier: usize,
    mut init_fn: Init,
    mut test_fn: Test,
    mut verify_fn: Verify,
) -> TestStats
where
    Init: FnMut(&ChunkCtx),
    Test: FnMut(&ChunkCtx),
    Verify: FnMut(&ChunkCtx) -> u64,
{
    if blocks.is_empty() {
        return TestStats {
            name: test_name,
            action,
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

    let total_allocated: usize = blocks.iter().map(|b| b.buffer.size()).sum();
    let window_size = config.calculate_window_size(test_name, total_allocated);
    let test_blocks = prepare_blocks_for_window(blocks, window_size, test_name);

    if test_blocks.is_empty() {
        log::warn!("{}: No blocks prepared for testing", test_name);
        return TestStats {
            name: test_name,
            action,
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

    // Pre-compute error check interval mask from config (v1 parity)
    let check_mask = config.error_check_interval.get_check_mask();

    let mut total_bytes_processed = 0usize;
    let mut total_error_count = 0u64;
    let mut total_operations = 0u64;

    // Start timer BEFORE init — v1 includes the first write in its cycle timing,
    // so v2 must include init in elapsed time for fair A/B comparison.
    let test_start = Instant::now();
    let start = Instant::now();
    let mut cycle = 0u32;

    // Progress reporting — 250ms matches v1 SIMD update frequency
    let update_interval_ms = 250u128;
    let mut last_progress_update = Instant::now();

    // Pre-compute per-block metadata once (ptr, len, chunk_size don't change between cycles)
    struct BlockMeta2 {
        ptr: *mut u64,
        len_elements: usize,
        chunk_size_elements: usize,
        test_size_bytes: usize,
    }
    let block_metas: Vec<BlockMeta2> = test_blocks.iter().map(|tb| {
        let ptr = tb.block.buffer.as_mut_ptr() as *mut u64;
        let len_elements = tb.test_size / std::mem::size_of::<u64>();
        let ideal = calculate_ideal_chunk_size(config, test_name, tb.test_size);
        let chunk_bytes = get_safe_chunk_size(ideal, tb.test_size);
        let chunk_size_elements = chunk_bytes / std::mem::size_of::<u64>();
        BlockMeta2 { ptr, len_elements, chunk_size_elements, test_size_bytes: tb.test_size }
    }).collect();

    // Initialize all blocks (timed, matching v1 behavior)
    for meta in block_metas.iter() {
        let ctx = ChunkCtx {
            ptr: meta.ptr,
            chunk_start: 0,
            chunk_end: meta.len_elements,
            cycle: 0,
            thread_id,
            check_mask,
        };
        init_fn(&ctx);
    }
    std::sync::atomic::fence(Ordering::SeqCst);

    'outer: loop {
        cycle += 1;

        for meta in block_metas.iter() {
            let mut block_errors = 0u64;

            for chunk_start in (0..meta.len_elements).step_by(meta.chunk_size_elements) {
                let chunk_end = (chunk_start + meta.chunk_size_elements).min(meta.len_elements);
                let ctx = ChunkCtx {
                    ptr: meta.ptr,
                    chunk_start,
                    chunk_end,
                    cycle,
                    thread_id,
                    check_mask,
                };

                // Test phase (mirror, shake, etc.)
                test_fn(&ctx);
                std::sync::atomic::fence(Ordering::SeqCst);

                // Verify phase
                let errors = verify_fn(&ctx);
                block_errors += errors;

                if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
                    total_error_count += block_errors;
                    total_bytes_processed += (chunk_end - chunk_start) * std::mem::size_of::<u64>() * bytes_multiplier;
                    break 'outer;
                }
            }

            total_error_count += block_errors;
            total_bytes_processed += meta.test_size_bytes * bytes_multiplier;
            total_operations += meta.len_elements as u64;

            if block_errors > 0 {
                match error_mode {
                    ErrorMode::Panic => {
                        panic!("{}: {} memory errors detected on thread {}", test_name, block_errors, thread_id);
                    }
                    ErrorMode::Halt => {
                        break 'outer;
                    }
                    ErrorMode::Log => {}
                }
            }

            if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
                break 'outer;
            }
        }

        if let Some(progress) = progress {
            let now = Instant::now();
            if now.duration_since(last_progress_update).as_millis() >= update_interval_ms {
                progress.cycles_completed.store(cycle, Ordering::Relaxed);
                progress.bytes_processed.store(total_bytes_processed as u64, Ordering::Relaxed);
                progress.errors_found.store(total_error_count, Ordering::Relaxed);
                progress.last_update_ms.store(now.duration_since(start).as_millis() as u64, Ordering::Relaxed);
                last_progress_update = now;
            }
        }

        let elapsed_secs = test_start.elapsed().as_secs() as u32;
        if !timing.should_continue(cycle, elapsed_secs) {
            break;
        }
        if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
            break;
        }
    }

    TestStats {
        name: test_name,
        action,
        bytes_processed: total_bytes_processed,
        elapsed_ms: start.elapsed().as_millis(),
        thread_id,
        error_count: total_error_count,
        total_operations,
        cycles_completed: cycle,
        cycles_planned: timing.cycles,
        stopped_by_time_limit: timing.cycles.map_or(true, |limit| cycle < limit),
    }
}

/// Auto-dispatch macro: generates a function that selects the best SIMD variant at runtime.
///
/// Usage:
/// ```ignore
/// auto_dispatch!(
///     pub mirror_move_v2_auto_multi,
///     mirror_move_v2_multi,        // scalar fallback
///     mirror_move_v2_128_multi,    // SSE2
///     mirror_move_v2_256_multi,    // AVX2
///     mirror_move_v2_512_multi     // AVX-512
/// );
/// ```
#[macro_export]
macro_rules! auto_dispatch {
    (
        pub $name:ident,
        $scalar:ident,
        $sse:ident,
        $avx2:ident,
        $avx512:ident
    ) => {
        pub unsafe fn $name(
            blocks: &[$crate::runner::AllocationBlock],
            thread_id: usize,
            error_mode: $crate::ErrorMode,
            timing: &$crate::tests::TestTiming,
            config: &$crate::tests::TestMemoryConfig,
            progress: Option<&$crate::tests::TestProgress>,
        ) -> $crate::tests::TestStats {
            #[cfg(target_arch = "x86_64")]
            {
                if is_x86_feature_detected!("avx512f") {
                    return $avx512(blocks, thread_id, error_mode, timing, config, progress);
                }
                if is_x86_feature_detected!("avx2") {
                    return $avx2(blocks, thread_id, error_mode, timing, config, progress);
                }
                if is_x86_feature_detected!("sse2") {
                    return $sse(blocks, thread_id, error_mode, timing, config, progress);
                }
            }
            $scalar(blocks, thread_id, error_mode, timing, config, progress)
        }
    };
}
