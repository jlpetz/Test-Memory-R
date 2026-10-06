//! Zero-cost phased test orchestration harness for TMR memory tests.
//!
//! This replaces the duplicated boilerplate in each test function with a single
//! generic function that monomorphizes per call site. Unlike the failed
//! previous `test_framework.rs` approach which used `dyn TestPattern` (vtable dispatch in
//! hot loop → no inlining → no SIMD), this uses concrete generic types that the
//! compiler fully specializes.
//!
//! # Why this works where traits didn't
//!
//! - `Init`/`Test`/`Verify` are concrete generic types → compiler monomorphizes
//! - `#[inline(always)]` on the harness → orchestration inlines into caller
//! - Closures capture SIMD constants by value → no indirection
//! - No vtable, no boxing, no `dyn`

use crate::runner::AllocationBlock;
use crate::tests::{
    TestAction, TestMemoryConfig, TestProgress, TestStats, TestTiming,
};
use crate::ErrorMode;
use std::sync::atomic::Ordering;

/// Chunk-level context passed to closures.
/// Contains pre-computed values so closures don't need to recompute them.
pub struct ChunkCtx {
    /// Pointer to the start of this piece of the extent (as u64 elements).
    pub ptr: *mut u64,
    /// Start element index within this piece (0-based).
    pub chunk_start: usize,
    /// End element index (exclusive) within this piece.
    pub chunk_end: usize,
    /// Current cycle number (1-based).
    pub cycle: u32,
    /// Thread ID running this test.
    pub thread_id: usize,
    /// Error check interval mask from config (None = PER_CHUNK, Some(mask) = check every N elements).
    /// Verify closures should use this for intermediate error batch reporting.
    pub check_mask: Option<u32>,
}

/// Run a phased test with monomorphized closures for init, test, and verify phases.
///
/// This handles ALL orchestration boilerplate:
/// - Block preparation with extent limits
/// - Chunked iteration for shutdown responsiveness
/// - Cycle/timing loop
/// - Error accumulation and mode handling
/// - Progress reporting
/// - Stats collection
/// - Accurate bytes accounting based on actual operations performed
///
/// # Type Parameters
///
/// - `Init`: Called once per block at startup to write initial patterns (unless `skip_init`).
///   Always provided even when skipped — verify closures may need the same pattern knowledge
///   for error repair. Signature: `fn(ctx: &ChunkCtx)`
/// - `Test`: Called per chunk per cycle for the "shake" operation (mirror swap, pattern write, etc).
///   May be a no-op closure `|_| {}` for tests that only write+verify.
///   Signature: `fn(ctx: &ChunkCtx)`
/// - `Verify`: Called per chunk per cycle to verify patterns. Returns error count for this chunk.
///   Signature: `fn(ctx: &ChunkCtx) -> u64`
///
/// # Parameters
///
/// - `bytes_per_test_op`: Memory touched per test_fn call as a multiplier of block size.
///   1 for simple write (SimpleTest), 4 for MirrorMove (2R + 2W per round-trip).
///   Used for bytes accounting only — does not affect test behavior.
/// - `skip_init`: When true, init_fn is NOT called at startup. Used for dependent tests
///   where a prior test in the plan already wrote the expected patterns. Init_fn is still
///   provided for pattern knowledge (verify/repair needs it).
///
/// # test_reps / verify_reps (TM5 repetition control)
///
/// Three repetition knobs matching TM5's SimpleTest loop structure:
/// - `write_read_cycles`: Outer loop — repeat the entire write+verify sequence N times per
///   chunk. TM5 uses `ST_WriteReadCycles=4`. Each cycle re-writes the pattern and verifies
///   it `verify_reps` times. Catches intermittent errors through repetition. Default 1.
/// - `test_reps`: Run test_fn N times per chunk before verifying. For MirrorMove, this means
///   N mirror round-trips before checking — more bus stress between checks. Default 1.
/// - `verify_reps`: Run verify_fn N times per chunk after each write. TM5 SimpleTest
///   writes once then reads/verifies `dLoopCounter` times (typically 5), stressing DRAM
///   refresh and retention. Default 1.
///
/// Per cycle per chunk: `(test_fn × test_reps → fence → verify_fn × verify_reps) × write_read_cycles`
///
/// # Bytes Accounting
///
/// ```text
/// init_bytes = if skip_init { 0 } else { block_size }  (one write pass)
/// cycle_bytes = (bytes_per_test_op × test_reps + verify_reps) × block_size × write_read_cycles
/// total = init_bytes + cycle_bytes × cycles_completed
/// ```
///
/// # Safety
///
/// Caller must ensure all blocks contain valid, aligned, writable memory.
#[inline(always)]
#[allow(clippy::too_many_arguments)]
pub unsafe fn run_phased_test<Init, Test, Verify>(
    blocks: &[AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
    test_name: &'static str,
    action: TestAction,
    bytes_per_test_op: usize,
    skip_init: bool,
    test_reps: u32,
    verify_reps: u32,
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
            seal_errors: 0,
        };
    }

    // Block prep, extent sizing, the info log, and the timers all live in TestRunner
    // now — one source of truth shared with the Tier-2 loop-owning tests (TODO #19 A.3).
    // The runner also starts the timer BEFORE init, preserving the v1 behaviour of
    // counting the first write in cycle timing.
    let (mut runner, extent) = crate::test_scaffolding::TestRunner::new(
        blocks, thread_id, error_mode, timing, config, progress, test_name, action,
    );

    if extent.test_size == 0 {
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
            seal_errors: 0,
        };
    }

    // Pre-compute error check interval mask from config (v1 parity)
    let check_mask = config.error_check_interval.get_check_mask();

    // Hoist config flags into locals — never read a struct field inside the hot loop.
    let flush_before_verify = config.flush_before_verify;

    // Operation counting is unique to the phased tests, so the runner doesn't own it.
    // (Bytes and errors are accumulated through the runner.)
    let mut total_operations = 0u64;

    // The extent and its chunks, fixed for the test: every chunk is exactly the test's chunk,
    // spread evenly over the extent (TODO 76)
    let ptr = extent.ptr as *mut u64;
    let len_elements = extent.test_size / std::mem::size_of::<u64>();
    let chunks = runner.chunks(extent.test_size);
    let chunk_len = chunks.chunk() / std::mem::size_of::<u64>();

    // A sealed step wraps each chunk in the seal (TODO 74): check before, reseal after. Its chunks
    // hold the seal until the test works them, so the test's own fill moves inside each chunk,
    // where a test whose first op writes doesn't need one at all.
    let wrap = runner.seal_wrap();
    let fill_per_chunk = wrap && test_reps == 0 && !skip_init;

    // Initialize the extent with patterns (unless dependent mode — prior test already wrote them)
    if !skip_init && !wrap {
        runner.set_stage(crate::tests::Stage::FillingMemory);
        let ctx = ChunkCtx {
            ptr,
            chunk_start: 0,
            chunk_end: len_elements,
            cycle: 0,
            thread_id,
            check_mask,
        };
        init_fn(&ctx);
        std::sync::atomic::fence(Ordering::SeqCst);
        runner.set_stage(crate::tests::Stage::Testing);

        // Account for init: one write pass over the extent
        runner.add_bytes(extent.test_size);
    }

    // Per write_read_cycle: bytes_per_test_op × test_reps writes + verify_reps reads
    let ops_per_wrc = bytes_per_test_op * test_reps as usize + verify_reps as usize;
    let wrc = config.write_read_cycles;

    // Main test loop
    loop {
        let cycle = runner.begin_cycle();
        let mut cycle_errors = 0u64;

        for k in 0..chunks.count() {
            let chunk_start = chunks.start(k) / std::mem::size_of::<u64>();
            let chunk_end = chunk_start + chunk_len;
            let ctx = ChunkCtx {
                ptr,
                chunk_start,
                chunk_end,
                cycle,
                thread_id,
                check_mask,
            };

            runner.check_seal(chunk_start * std::mem::size_of::<u64>(), chunks.chunk());
            if fill_per_chunk {
                init_fn(&ctx);
            }

            // TM5-faithful loop: (write + multi-read) × write_read_cycles
            // TM5 SimpleTest: (1 write + 5 reads) × 4 = tight repeated access per chunk
            for _ in 0..wrc {
                // Test/write phase — run test_reps times (e.g., mirror round-trips)
                for _ in 0..test_reps {
                    test_fn(&ctx);
                }

                std::sync::atomic::fence(Ordering::SeqCst);

                // Optional flush phase (TODO #59): evict this chunk so the verify below
                // round-trips through DRAM instead of reading the just-written cached copy.
                // Placed AFTER the fence (writes globally ordered) and BEFORE the reads —
                // flush_range_to_dram ends in its own MFENCE, so the flushes are drained
                // before any verify load can issue. Off by default; costs real bandwidth.
                if flush_before_verify {
                    let chunk_ptr = ctx.ptr.add(chunk_start) as *const u8;
                    let chunk_bytes = (chunk_end - chunk_start) * std::mem::size_of::<u64>();
                    crate::tests::flush_range_to_dram(chunk_ptr, chunk_bytes, config.cache_line_bytes);
                }

                // Verify phase — run verify_reps times (e.g., multi-read for retention stress)
                for _ in 0..verify_reps {
                    cycle_errors += verify_fn(&ctx);
                }
            }

            runner.reseal(chunk_start * std::mem::size_of::<u64>(), chunks.chunk());

            // Count the chunk when it is done, so a halt or shutdown below reports what ran
            // (TODO 76). Overlaps count each time.
            runner.add_bytes(chunk_len * std::mem::size_of::<u64>() * ops_per_wrc * wrc as usize);
            total_operations += chunk_len as u64 * (test_reps as u64 + verify_reps as u64) * wrc as u64;

            // Halt at the chunk that erred, not at the end of the extent
            if runner.should_halt(cycle_errors) {
                return runner.finish_aborted(cycle_errors, total_operations);
            }

            // Check for shutdown (mid-extent)
            if runner.shutdown_requested() {
                return runner.finish_aborted(cycle_errors, total_operations);
            }

            // The live display, throttled inside: one extent can be a whole cycle long
            runner.update_progress_in_cycle();
        }

        runner.commit_cycle_errors(cycle_errors);
        runner.update_progress();

        // Timing/cycle gate
        if !runner.should_continue() {
            return runner.finish_completed(total_operations);
        }

        // Errors for this cycle are already committed, so a shutdown here adds none.
        if runner.shutdown_requested() {
            return runner.finish_aborted(0, total_operations);
        }
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
        #[doc = include_str!("test_fn_safety.md")]
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pattern_gen;

    /// Helper: allocate an aligned u64 buffer for testing.
    /// Returns a Vec and a raw pointer (valid for the Vec's lifetime).
    fn alloc_test_buffer(len_elements: usize) -> Vec<u64> {
        vec![0u64; len_elements]
    }

    fn make_ctx(ptr: *mut u64, len: usize, check_mask: Option<u32>) -> ChunkCtx {
        ChunkCtx {
            ptr,
            chunk_start: 0,
            chunk_end: len,
            cycle: 1,
            thread_id: 0,
            check_mask,
        }
    }

    // ── SimpleTest v2 Mode 0 error injection ──

    #[test]
    fn simple_v2_mode0_detects_single_bit_flip() {
        let len = 4096;
        let mut buf = alloc_test_buffer(len);
        let ptr = buf.as_mut_ptr();
        let base = 0xDEADBEEFDEADBEEFu64;

        // Write correct pattern
        unsafe {
            for i in 0..len {
                *ptr.add(i) = pattern_gen::pattern_mode10(i as u64, base);
            }
        }

        // Verify clean — should find 0 errors
        let ctx = make_ctx(ptr, len, None);
        let errors = unsafe {
            let mut total = 0u64;
            for i in ctx.chunk_start..ctx.chunk_end {
                let expected = pattern_gen::pattern_mode10(i as u64, base);
                let actual = *ctx.ptr.add(i);
                if actual != expected { total += 1; }
            }
            total
        };
        assert_eq!(errors, 0, "Clean buffer should have 0 errors");

        // Corrupt one element (single bit flip)
        let corrupt_idx = len / 3;
        unsafe { *ptr.add(corrupt_idx) ^= 1; }

        // Verify corrupted — should detect exactly 1 error
        let errors = unsafe {
            let mut total = 0u64;
            for i in ctx.chunk_start..ctx.chunk_end {
                let expected = pattern_gen::pattern_mode10(i as u64, base);
                let actual = *ctx.ptr.add(i);
                if actual != expected { total += 1; }
            }
            total
        };
        assert_eq!(errors, 1, "Should detect exactly 1 corrupted element");
    }

    // ── Verify batched check_mask path detects errors ──

    #[test]
    fn verify_with_check_mask_detects_corruption() {
        let len = 4096;
        let mut buf = alloc_test_buffer(len);
        let ptr = buf.as_mut_ptr();
        let base = 0xDEADBEEFDEADBEEFu64;

        // Write correct pattern
        unsafe {
            for i in 0..len {
                *ptr.add(i) = pattern_gen::pattern_mode10(i as u64, base);
            }
        }

        // Verify with check_mask=255 (check every 256 elements)
        let check_mask: u32 = 255;
        let ctx = make_ctx(ptr, len, Some(check_mask));

        // Clean verify
        let errors = unsafe {
            let mut total = 0u64;
            let mut interval_errors = 0u64;
            let mut element_count = 0u32;
            for i in ctx.chunk_start..ctx.chunk_end {
                let expected = pattern_gen::pattern_mode10(i as u64, base);
                if *ctx.ptr.add(i) != expected { interval_errors += 1; }
                element_count += 1;
                if (element_count & check_mask) == 0 {
                    total += interval_errors;
                    interval_errors = 0;
                }
            }
            total + interval_errors
        };
        assert_eq!(errors, 0, "Clean buffer with check_mask should have 0 errors");

        // Corrupt near start AND near end (different check intervals)
        unsafe {
            *ptr.add(10) ^= 0xFFFF;
            *ptr.add(3000) ^= 0xFFFF;
        }

        let errors = unsafe {
            let mut total = 0u64;
            let mut interval_errors = 0u64;
            let mut element_count = 0u32;
            for i in ctx.chunk_start..ctx.chunk_end {
                let expected = pattern_gen::pattern_mode10(i as u64, base);
                if *ctx.ptr.add(i) != expected { interval_errors += 1; }
                element_count += 1;
                if (element_count & check_mask) == 0 {
                    total += interval_errors;
                    interval_errors = 0;
                }
            }
            total + interval_errors
        };
        assert_eq!(errors, 2, "Should detect 2 corrupted elements across check intervals");
    }

    // ── Mirror subblock round-trip preserves data ──

}
