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
        };
    }

    // Block prep, extent sizing, the info log, and the timers all live in TestRunner
    // now — one source of truth shared with the Tier-2 loop-owning tests (TODO #19 A.3).
    // The runner also starts the timer BEFORE init, preserving the v1 behaviour of
    // counting the first write in cycle timing.
    let (mut runner, test_blocks) = crate::test_scaffolding::TestRunner::new(
        blocks, thread_id, error_mode, timing, config, progress, test_name, action,
    );

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

    // Hoist config flags into locals — never read a struct field inside the hot loop.
    let flush_before_verify = config.flush_before_verify;

    // Operation counting is unique to the phased tests, so the runner doesn't own it.
    // (Bytes and errors are accumulated through the runner.)
    let mut total_operations = 0u64;

    // Pre-compute per-block metadata once (ptr, len, chunk_size don't change between cycles)
    struct BlockMeta {
        ptr: *mut u64,
        len_elements: usize,
        chunk_size_elements: usize,
        test_size_bytes: usize,
    }
    let block_metas: Vec<BlockMeta> = test_blocks.iter().map(|tb| {
        let ptr = tb.ptr as *mut u64;
        let len_elements = tb.test_size / std::mem::size_of::<u64>();
        let chunk_bytes = runner.chunk_size_bytes(tb.test_size);
        let chunk_size_elements = chunk_bytes / std::mem::size_of::<u64>();
        BlockMeta { ptr, len_elements, chunk_size_elements, test_size_bytes: tb.test_size }
    }).collect();

    // Initialize all blocks with patterns (unless dependent mode — prior test already wrote them)
    if !skip_init {
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

        // Account for init: one write pass over all blocks
        for meta in block_metas.iter() {
            runner.add_bytes(meta.test_size_bytes);
        }
    }

    // Per write_read_cycle: bytes_per_test_op × test_reps writes + verify_reps reads
    let ops_per_wrc = bytes_per_test_op * test_reps as usize + verify_reps as usize;
    let wrc = config.write_read_cycles;

    // Main test loop — interleaves across blocks
    loop {
        let cycle = runner.begin_cycle();
        let mut cycle_errors = 0u64;

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
                        let errors = verify_fn(&ctx);
                        block_errors += errors;
                    }
                }

                // Count the chunk when it is done, so a halt or shutdown below reports what ran:
                // under stitched one piece is the whole extent (TODO 76)
                let chunk_len = chunk_end - chunk_start;
                runner.add_bytes(chunk_len * std::mem::size_of::<u64>() * ops_per_wrc * wrc as usize);
                total_operations += chunk_len as u64 * (test_reps as u64 + verify_reps as u64) * wrc as u64;

                // Halt at the chunk that erred, not at the end of the piece
                if runner.should_halt(block_errors) {
                    return runner.finish_aborted(cycle_errors + block_errors, total_operations);
                }

                // Check for shutdown (mid-piece)
                if runner.shutdown_requested() {
                    return runner.finish_aborted(cycle_errors + block_errors, total_operations);
                }
            }

            cycle_errors += block_errors;

            // Check for shutdown between blocks
            if runner.shutdown_requested() {
                return runner.finish_aborted(cycle_errors, total_operations);
            }
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
    use std::simd::*;
    use std::simd::cmp::SimdPartialEq;

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

    // ── SimpleTest v2 Mode 2 (LCG) error injection ──

    #[test]
    fn simple_v2_lcg_detects_corruption() {
        let len = 4096;
        let mut buf = alloc_test_buffer(len);
        let ptr = buf.as_mut_ptr();
        let multiplier = 0x5DEECE66Du64;
        let addend = 0xBu64;
        let initial_seed = 42u64;

        // Write LCG sequence
        unsafe {
            let mut state = pattern_gen::lcg_next(initial_seed, multiplier, addend);
            for i in 0..len {
                *ptr.add(i) = state;
                state = pattern_gen::lcg_next(state, multiplier, addend);
            }
        }

        // Verify clean
        let errors = unsafe {
            let mut total = 0u64;
            let mut state = pattern_gen::lcg_next(initial_seed, multiplier, addend);
            for i in 0..len {
                if *ptr.add(i) != state { total += 1; }
                state = pattern_gen::lcg_next(state, multiplier, addend);
            }
            total
        };
        assert_eq!(errors, 0, "Clean LCG buffer should have 0 errors");

        // Corrupt middle element
        unsafe { *ptr.add(len / 2) = 0xBADBADBADBADBAD; }

        let errors = unsafe {
            let mut total = 0u64;
            let mut state = pattern_gen::lcg_next(initial_seed, multiplier, addend);
            for i in 0..len {
                if *ptr.add(i) != state { total += 1; }
                state = pattern_gen::lcg_next(state, multiplier, addend);
            }
            total
        };
        assert_eq!(errors, 1, "Should detect exactly 1 corrupted LCG element");
    }

    // ── MirrorMove v2 scalar error injection ──

    #[test]
    fn mirror_v2_scalar_detects_corruption_after_roundtrip() {
        let len = 4096;
        let mut buf = alloc_test_buffer(len);
        let ptr = buf.as_mut_ptr();
        let thread_base = pattern_gen::mirror_thread_base(0);

        // Init: write mirror pattern
        unsafe {
            for i in 0..len {
                *ptr.add(i) = pattern_gen::mirror_pattern_u64(i as u64, thread_base);
            }
        }

        // Mirror (reverse)
        unsafe {
            let mut lo = 0;
            let mut hi = len - 1;
            while lo < hi {
                let a = *ptr.add(lo);
                let b = *ptr.add(hi);
                *ptr.add(lo) = b;
                *ptr.add(hi) = a;
                lo += 1;
                hi -= 1;
            }
        }

        // Unmirror (reverse back)
        unsafe {
            let mut lo = 0;
            let mut hi = len - 1;
            while lo < hi {
                let a = *ptr.add(lo);
                let b = *ptr.add(hi);
                *ptr.add(lo) = b;
                *ptr.add(hi) = a;
                lo += 1;
                hi -= 1;
            }
        }

        // Verify clean after round-trip
        let errors = unsafe {
            let mut total = 0u64;
            for i in 0..len {
                let expected = pattern_gen::mirror_pattern_u64(i as u64, thread_base);
                if *ptr.add(i) != expected { total += 1; }
            }
            total
        };
        assert_eq!(errors, 0, "Round-trip mirror should preserve all patterns");

        // Corrupt one element
        unsafe { *ptr.add(100) ^= 0xFF00FF00FF00FF00; }

        let errors = unsafe {
            let mut total = 0u64;
            for i in 0..len {
                let expected = pattern_gen::mirror_pattern_u64(i as u64, thread_base);
                if *ptr.add(i) != expected { total += 1; }
            }
            total
        };
        assert_eq!(errors, 1, "Should detect corrupted element after mirror round-trip");
    }

    // ── MirrorMove v2 SIMD (u64x2) error injection ──

    #[test]
    fn mirror_v2_simd_u64x2_detects_corruption() {
        const SIMD_W: usize = 2;
        // Must be divisible by SIMD_W
        let len = 4096;
        let mut buf = alloc_test_buffer(len);
        let ptr = buf.as_mut_ptr();

        let thread_base = pattern_gen::mirror_thread_base(0);
        const MIRROR_CONST: u64 = 0x0123456789ABCDEFu64;

        let base_vec = u64x2::splat(thread_base);
        let const_vec = u64x2::splat(MIRROR_CONST);
        let lane_offsets = u64x2::from_array([0, 1]);
        let step = u64x2::splat((SIMD_W as u64).wrapping_mul(MIRROR_CONST));
        let zero = u64x2::splat(0);

        // Init: SIMD pattern write
        unsafe {
            let mut expected = (u64x2::splat(0) + lane_offsets + base_vec) * const_vec;
            for i in (0..len).step_by(SIMD_W) {
                *(ptr.add(i) as *mut u64x2) = expected;
                expected += step;
            }
        }

        // SIMD mirror (forward)
        unsafe {
            let mut lo = 0;
            let mut hi = len - SIMD_W;
            while lo < hi {
                let a = *(ptr.add(lo) as *const u64x2);
                let b = *(ptr.add(hi) as *const u64x2);
                *(ptr.add(lo) as *mut u64x2) = b;
                *(ptr.add(hi) as *mut u64x2) = a;
                lo += SIMD_W;
                hi -= SIMD_W;
            }
        }

        // SIMD unmirror (reverse)
        unsafe {
            let mut lo = 0;
            let mut hi = len - SIMD_W;
            while lo < hi {
                let a = *(ptr.add(lo) as *const u64x2);
                let b = *(ptr.add(hi) as *const u64x2);
                *(ptr.add(lo) as *mut u64x2) = b;
                *(ptr.add(hi) as *mut u64x2) = a;
                lo += SIMD_W;
                hi -= SIMD_W;
            }
        }

        // SIMD verify — clean
        let errors = unsafe {
            let mut error_acc = zero;
            let mut expected = (u64x2::splat(0) + lane_offsets + base_vec) * const_vec;
            for i in (0..len).step_by(SIMD_W) {
                let actual = *(ptr.add(i) as *const u64x2);
                error_acc |= actual ^ expected;
                expected += step;
            }
            if error_acc.simd_ne(zero).any() { 1u64 } else { 0u64 }
        };
        assert_eq!(errors, 0, "Clean SIMD buffer should have 0 errors");

        // Corrupt one u64 element (within a SIMD vector)
        unsafe { *ptr.add(500) ^= 0x1; }

        // SIMD verify — should detect
        let errors = unsafe {
            let mut error_acc = zero;
            let mut expected = (u64x2::splat(0) + lane_offsets + base_vec) * const_vec;
            for i in (0..len).step_by(SIMD_W) {
                let actual = *(ptr.add(i) as *const u64x2);
                error_acc |= actual ^ expected;
                expected += step;
            }
            if error_acc.simd_ne(zero).any() { 1u64 } else { 0u64 }
        };
        assert_eq!(errors, 1, "Should detect single-bit corruption in SIMD verify");
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

    #[test]
    fn mirror_subblocks_roundtrip_preserves_data() {
        let len = 4096;
        let mut buf = alloc_test_buffer(len);
        let ptr = buf.as_mut_ptr();
        let thread_base = pattern_gen::mirror_thread_base(0);

        // Init
        unsafe {
            for i in 0..len {
                *ptr.add(i) = pattern_gen::mirror_pattern_u64(i as u64, thread_base);
            }
        }

        // Save original for comparison
        let original: Vec<u64> = buf.clone();

        // Simulate 2-subblock mirror + unmirror
        let n_sub = 2;
        let sub_size = len / n_sub;
        let pairs = sub_size / 2;

        // Forward mirror
        unsafe {
            for iter in 0..pairs {
                for sub in 0..n_sub {
                    let lo = sub * sub_size + iter;
                    let hi = (sub + 1) * sub_size - iter - 1;
                    let a = *ptr.add(lo);
                    let b = *ptr.add(hi);
                    *ptr.add(lo) = b;
                    *ptr.add(hi) = a;
                }
            }
        }

        // Reverse mirror
        unsafe {
            for iter in 0..pairs {
                for sub in 0..n_sub {
                    let lo = sub * sub_size + iter;
                    let hi = (sub + 1) * sub_size - iter - 1;
                    let a = *ptr.add(lo);
                    let b = *ptr.add(hi);
                    *ptr.add(lo) = b;
                    *ptr.add(hi) = a;
                }
            }
        }

        // Verify all elements match original
        for i in 0..len {
            assert_eq!(buf[i], original[i], "Element {} changed after subblock round-trip", i);
        }
    }
}
