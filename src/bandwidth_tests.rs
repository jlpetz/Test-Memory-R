// Sequential Bandwidth Tests — SIMD implementations with manual 4x unroll
//
// Three test types, each with cached (L1/L2/L3) and NT (DRAM) variants:
//   - Spd-Write: Pure write bandwidth (static pattern fill)
//   - Spd-Read:  Pure read bandwidth (SIMD loads + XOR accumulate)
//   - Spd-Copy:  Read + write bandwidth (load first half, store second half)
//
// Implementation follows the NT store constraints documented in doc/nt_stores.md:
//   - Must use std::arch intrinsics (core::intrinsics::nontemporal_store is broken)
//   - Must manually unroll 4x (LLVM won't auto-unroll inline asm)
//   - Must use macros, not generics (#[target_feature] doesn't propagate through fn calls)
//
// Width variants: 128-bit (SSE2), 256-bit (AVX2), 512-bit (AVX-512F)
// Auto-dispatch selects best available at runtime.

use crate::ErrorMode;
use crate::runner::{AllocationBlock, SHUTDOWN_REQUESTED};
use crate::tests::{TestAction, TestMemoryConfig, TestProgress, TestTiming, TestStats, prepare_blocks_for_window};
use std::sync::atomic::Ordering;
use std::time::Instant;

const STATIC_PATTERN: u64 = 0x5555555555555555;
const PROGRESS_INTERVAL_MS: u128 = 250;

// ============================================================================
// Scalar init — touches every page to ensure physical backing (not timed)
// ============================================================================

#[inline(always)]
unsafe fn scalar_fill(ptr: *mut u64, len_u64: usize) {
    let mut i = 0;
    let unrolled_end = (len_u64 / 4) * 4;
    while i < unrolled_end {
        *ptr.add(i) = STATIC_PATTERN;
        *ptr.add(i + 1) = STATIC_PATTERN;
        *ptr.add(i + 2) = STATIC_PATTERN;
        *ptr.add(i + 3) = STATIC_PATTERN;
        i += 4;
    }
    while i < len_u64 {
        *ptr.add(i) = STATIC_PATTERN;
        i += 1;
    }
}

// ============================================================================
// WRITE — Hot loop macros (4x unrolled stores of a static pattern)
// ============================================================================

/// 4x unrolled SIMD write loop. $store_fn is _mm*_store_si* or _mm*_stream_si*.
macro_rules! spd_write_hot {
    ($base_ptr:expr, $byte_len:expr, $pattern:expr, $arch_type:ty, $store_fn:path) => {{
        let mut p = $base_ptr as *mut $arch_type;
        let vec_size = std::mem::size_of::<$arch_type>();
        let stride = vec_size * 4;
        let total = $byte_len;
        let unrolled_end = (total / stride) * stride;
        let end = total;

        let mut offset = 0usize;
        while offset < unrolled_end {
            $store_fn(p, $pattern);
            $store_fn(p.add(1), $pattern);
            $store_fn(p.add(2), $pattern);
            $store_fn(p.add(3), $pattern);
            p = p.add(4);
            offset += stride;
        }
        while offset < end {
            $store_fn(p, $pattern);
            p = p.add(1);
            offset += vec_size;
        }
    }}
}

// ============================================================================
// READ — Hot loop macros (4x unrolled loads, 4 independent XOR accumulators)
// ============================================================================

/// 4x unrolled SIMD read loop with 4 independent accumulators to avoid
/// serial dependency chains. XOR accumulate prevents dead-code elimination.
macro_rules! spd_read_hot {
    ($base_ptr:expr, $byte_len:expr, $arch_type:ty, $load_fn:path, $xor_fn:path, $setzero_fn:path) => {{
        let mut p = $base_ptr as *const $arch_type;
        let vec_size = std::mem::size_of::<$arch_type>();
        let stride = vec_size * 4;
        let total = $byte_len;
        let unrolled_end = (total / stride) * stride;
        let end = total;

        let mut acc0 = $setzero_fn();
        let mut acc1 = $setzero_fn();
        let mut acc2 = $setzero_fn();
        let mut acc3 = $setzero_fn();

        let mut offset = 0usize;
        while offset < unrolled_end {
            acc0 = $xor_fn(acc0, $load_fn(p));
            acc1 = $xor_fn(acc1, $load_fn(p.add(1)));
            acc2 = $xor_fn(acc2, $load_fn(p.add(2)));
            acc3 = $xor_fn(acc3, $load_fn(p.add(3)));
            p = p.add(4);
            offset += stride;
        }
        while offset < end {
            acc0 = $xor_fn(acc0, $load_fn(p));
            p = p.add(1);
            offset += vec_size;
        }

        // Merge accumulators and sink the result
        acc0 = $xor_fn(acc0, acc1);
        acc2 = $xor_fn(acc2, acc3);
        acc0 = $xor_fn(acc0, acc2);
        std::hint::black_box(acc0);
    }}
}

// ============================================================================
// COPY — Hot loop macros (4x unrolled load from src + store to dst)
// ============================================================================

/// 4x unrolled SIMD copy loop. Reads from src, writes to dst.
/// $store_fn is _mm*_store_si* (cached) or _mm*_stream_si* (NT).
macro_rules! spd_copy_hot {
    ($src_ptr:expr, $dst_ptr:expr, $byte_len:expr, $arch_type:ty,
     $load_fn:path, $store_fn:path) => {{
        let mut sp = $src_ptr as *const $arch_type;
        let mut dp = $dst_ptr as *mut $arch_type;
        let vec_size = std::mem::size_of::<$arch_type>();
        let stride = vec_size * 4;
        let total = $byte_len;
        let unrolled_end = (total / stride) * stride;
        let end = total;

        let mut offset = 0usize;
        while offset < unrolled_end {
            let v0 = $load_fn(sp);
            let v1 = $load_fn(sp.add(1));
            let v2 = $load_fn(sp.add(2));
            let v3 = $load_fn(sp.add(3));
            $store_fn(dp, v0);
            $store_fn(dp.add(1), v1);
            $store_fn(dp.add(2), v2);
            $store_fn(dp.add(3), v3);
            sp = sp.add(4);
            dp = dp.add(4);
            offset += stride;
        }
        while offset < end {
            $store_fn(dp, $load_fn(sp));
            sp = sp.add(1);
            dp = dp.add(1);
            offset += vec_size;
        }
    }}
}

// ============================================================================
// WRITE test function macro — stamps out a complete bandwidth write test
// ============================================================================

macro_rules! spd_write_impl {
    (
        $impl_fn:ident, $pub_fn:ident,
        $target_feature:literal,
        $arch_type:ty,
        $set1_fn:path,
        $store_fn:path,
        $need_sfence:expr
    ) => {
        #[target_feature(enable = $target_feature)]
        unsafe fn $impl_fn(
            blocks: &[AllocationBlock],
            thread_id: usize,
            _error_mode: ErrorMode,
            timing: &TestTiming,
            config: &TestMemoryConfig,
            progress: Option<&TestProgress>,
        ) -> TestStats {
            let test_name = "Spd-Write";
            let start = Instant::now();

            let total_allocated: usize = blocks.iter().map(|b| b.buffer.size()).sum();
            let window_size = config.calculate_window_size(test_name, total_allocated);
            let test_blocks = prepare_blocks_for_window(blocks, window_size, test_name);

            if test_blocks.is_empty() {
                return TestStats {
                    name: test_name, action: TestAction::Write,
                    bytes_processed: 0, elapsed_ms: 0, thread_id,
                    error_count: 0, total_operations: 0,
                    cycles_completed: 0, cycles_planned: timing.cycles,
                    stopped_by_time_limit: false,
                };
            }

            // Init phase: scalar fill to fault in pages (skipped in dependent mode)
            if !config.skip_init {
                for tb in &test_blocks {
                    let base = tb.block.buffer.as_mut_ptr() as *mut u64;
                    let len_u64 = tb.test_size.min(window_size) / 8;
                    scalar_fill(base, len_u64);
                }
            }

            let test_start = Instant::now();
            let pattern = $set1_fn(STATIC_PATTERN as i64);
            let mut cycle = 0u32;
            let mut total_bytes = 0u64;
            let mut last_progress = Instant::now();

            loop {
                cycle += 1;

                for tb in &test_blocks {
                    let base = tb.block.buffer.as_mut_ptr() as *mut u8;
                    let working_bytes = tb.test_size.min(window_size);

                    spd_write_hot!(base, working_bytes, pattern, $arch_type, $store_fn);

                    if $need_sfence {
                        std::arch::x86_64::_mm_sfence();
                    }

                    total_bytes += working_bytes as u64;
                }

                // Progress update
                if let Some(prog) = progress {
                    let now = Instant::now();
                    if now.duration_since(last_progress).as_millis() >= PROGRESS_INTERVAL_MS {
                        prog.cycles_completed.store(cycle, Ordering::Relaxed);
                        prog.bytes_processed.store(total_bytes, Ordering::Relaxed);
                        prog.last_update_ms.store(now.duration_since(start).as_millis() as u64, Ordering::Relaxed);
                        last_progress = now;
                    }
                }

                if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) { break; }
                if !timing.should_continue(cycle, test_start.elapsed().as_secs() as u32) { break; }
            }

            TestStats {
                name: test_name, action: TestAction::Write,
                bytes_processed: total_bytes as usize,
                elapsed_ms: start.elapsed().as_millis(),
                thread_id, error_count: 0,
                total_operations: total_bytes / 8,
                cycles_completed: cycle,
                cycles_planned: timing.cycles,
                stopped_by_time_limit: timing.cycles.map_or(true, |limit| cycle < limit),
            }
        }

        #[doc = include_str!("test_fn_safety.md")]
        pub unsafe fn $pub_fn(
            blocks: &[AllocationBlock], thread_id: usize, error_mode: ErrorMode,
            timing: &TestTiming, config: &TestMemoryConfig, progress: Option<&TestProgress>,
        ) -> TestStats {
            $impl_fn(blocks, thread_id, error_mode, timing, config, progress)
        }
    }
}

// ============================================================================
// READ test function macro — stamps out a complete bandwidth read test
// ============================================================================

macro_rules! spd_read_impl {
    (
        $impl_fn:ident, $pub_fn:ident,
        $target_feature:literal,
        $arch_type:ty,
        $load_fn:path,
        $xor_fn:path,
        $setzero_fn:path
    ) => {
        #[target_feature(enable = $target_feature)]
        unsafe fn $impl_fn(
            blocks: &[AllocationBlock],
            thread_id: usize,
            _error_mode: ErrorMode,
            timing: &TestTiming,
            config: &TestMemoryConfig,
            progress: Option<&TestProgress>,
        ) -> TestStats {
            let test_name = "Spd-Read";
            let start = Instant::now();

            let total_allocated: usize = blocks.iter().map(|b| b.buffer.size()).sum();
            let window_size = config.calculate_window_size(test_name, total_allocated);
            let test_blocks = prepare_blocks_for_window(blocks, window_size, test_name);

            if test_blocks.is_empty() {
                return TestStats {
                    name: test_name, action: TestAction::Read,
                    bytes_processed: 0, elapsed_ms: 0, thread_id,
                    error_count: 0, total_operations: 0,
                    cycles_completed: 0, cycles_planned: timing.cycles,
                    stopped_by_time_limit: false,
                };
            }

            // Init phase: scalar fill to ensure physical pages (skipped in dependent mode)
            if !config.skip_init {
                for tb in &test_blocks {
                    let base = tb.block.buffer.as_mut_ptr() as *mut u64;
                    let len_u64 = tb.test_size.min(window_size) / 8;
                    scalar_fill(base, len_u64);
                }
            }

            let test_start = Instant::now();
            let mut cycle = 0u32;
            let mut total_bytes = 0u64;
            let mut last_progress = Instant::now();

            loop {
                cycle += 1;

                for tb in &test_blocks {
                    let base = tb.block.buffer.as_mut_ptr() as *const u8;
                    let working_bytes = tb.test_size.min(window_size);

                    spd_read_hot!(base, working_bytes, $arch_type, $load_fn, $xor_fn, $setzero_fn);

                    total_bytes += working_bytes as u64;
                }

                if let Some(prog) = progress {
                    let now = Instant::now();
                    if now.duration_since(last_progress).as_millis() >= PROGRESS_INTERVAL_MS {
                        prog.cycles_completed.store(cycle, Ordering::Relaxed);
                        prog.bytes_processed.store(total_bytes, Ordering::Relaxed);
                        prog.last_update_ms.store(now.duration_since(start).as_millis() as u64, Ordering::Relaxed);
                        last_progress = now;
                    }
                }

                if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) { break; }
                if !timing.should_continue(cycle, test_start.elapsed().as_secs() as u32) { break; }
            }

            TestStats {
                name: test_name, action: TestAction::Read,
                bytes_processed: total_bytes as usize,
                elapsed_ms: start.elapsed().as_millis(),
                thread_id, error_count: 0,
                total_operations: total_bytes / 8,
                cycles_completed: cycle,
                cycles_planned: timing.cycles,
                stopped_by_time_limit: timing.cycles.map_or(true, |limit| cycle < limit),
            }
        }

        #[doc = include_str!("test_fn_safety.md")]
        pub unsafe fn $pub_fn(
            blocks: &[AllocationBlock], thread_id: usize, error_mode: ErrorMode,
            timing: &TestTiming, config: &TestMemoryConfig, progress: Option<&TestProgress>,
        ) -> TestStats {
            $impl_fn(blocks, thread_id, error_mode, timing, config, progress)
        }
    }
}

// ============================================================================
// COPY test function macro — stamps out a complete bandwidth copy test
// Split allocation: read from first half, write to second half.
// bytes_processed = total traffic (read bytes + write bytes).
// ============================================================================

macro_rules! spd_copy_impl {
    (
        $impl_fn:ident, $pub_fn:ident,
        $target_feature:literal,
        $arch_type:ty,
        $load_fn:path,
        $store_fn:path,
        $need_sfence:expr
    ) => {
        #[target_feature(enable = $target_feature)]
        unsafe fn $impl_fn(
            blocks: &[AllocationBlock],
            thread_id: usize,
            _error_mode: ErrorMode,
            timing: &TestTiming,
            config: &TestMemoryConfig,
            progress: Option<&TestProgress>,
        ) -> TestStats {
            let test_name = "Spd-Copy";
            let start = Instant::now();

            let total_allocated: usize = blocks.iter().map(|b| b.buffer.size()).sum();
            let window_size = config.calculate_window_size(test_name, total_allocated);
            let test_blocks = prepare_blocks_for_window(blocks, window_size, test_name);

            if test_blocks.is_empty() {
                return TestStats {
                    name: test_name, action: TestAction::Copy,
                    bytes_processed: 0, elapsed_ms: 0, thread_id,
                    error_count: 0, total_operations: 0,
                    cycles_completed: 0, cycles_planned: timing.cycles,
                    stopped_by_time_limit: false,
                };
            }

            // Init phase: scalar fill source half (skipped in dependent mode)
            if !config.skip_init {
                let vec_size = std::mem::size_of::<$arch_type>();
                let align = vec_size * 2;
                for tb in &test_blocks {
                    let base = tb.block.buffer.as_mut_ptr() as *mut u64;
                    let raw_bytes = tb.test_size.min(window_size);
                    let working_bytes = (raw_bytes / align) * align;
                    let half_u64 = working_bytes / 2 / 8;
                    scalar_fill(base, half_u64);
                }
            }

            let test_start = Instant::now();
            let mut cycle = 0u32;
            let mut total_bytes = 0u64;
            let mut last_progress = Instant::now();

            loop {
                cycle += 1;

                for tb in &test_blocks {
                    let base = tb.block.buffer.as_mut_ptr() as *mut u8;
                    let raw_bytes = tb.test_size.min(window_size);
                    // Round down to 2×vec_size so both halves are SIMD-aligned
                    let vec_size = std::mem::size_of::<$arch_type>();
                    let align = vec_size * 2;
                    let working_bytes = (raw_bytes / align) * align;
                    let half_bytes = working_bytes / 2;

                    let half_a = base;
                    let half_b = base.add(half_bytes);

                    // Bidirectional: A→B then B→A = full window processed per cycle
                    spd_copy_hot!(half_a, half_b, half_bytes, $arch_type, $load_fn, $store_fn);
                    if $need_sfence {
                        std::arch::x86_64::_mm_sfence();
                    }
                    spd_copy_hot!(half_b, half_a, half_bytes, $arch_type, $load_fn, $store_fn);
                    if $need_sfence {
                        std::arch::x86_64::_mm_sfence();
                    }

                    // Total traffic = 2 × (read half + write half) = full window
                    total_bytes += (working_bytes * 2) as u64;
                }

                if let Some(prog) = progress {
                    let now = Instant::now();
                    if now.duration_since(last_progress).as_millis() >= PROGRESS_INTERVAL_MS {
                        prog.cycles_completed.store(cycle, Ordering::Relaxed);
                        prog.bytes_processed.store(total_bytes, Ordering::Relaxed);
                        prog.last_update_ms.store(now.duration_since(start).as_millis() as u64, Ordering::Relaxed);
                        last_progress = now;
                    }
                }

                if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) { break; }
                if !timing.should_continue(cycle, test_start.elapsed().as_secs() as u32) { break; }
            }

            TestStats {
                name: test_name, action: TestAction::Copy,
                bytes_processed: total_bytes as usize,
                elapsed_ms: start.elapsed().as_millis(),
                thread_id, error_count: 0,
                total_operations: total_bytes / 8,
                cycles_completed: cycle,
                cycles_planned: timing.cycles,
                stopped_by_time_limit: timing.cycles.map_or(true, |limit| cycle < limit),
            }
        }

        #[doc = include_str!("test_fn_safety.md")]
        pub unsafe fn $pub_fn(
            blocks: &[AllocationBlock], thread_id: usize, error_mode: ErrorMode,
            timing: &TestTiming, config: &TestMemoryConfig, progress: Option<&TestProgress>,
        ) -> TestStats {
            $impl_fn(blocks, thread_id, error_mode, timing, config, progress)
        }
    }
}

// ============================================================================
// WRITE — Cached variants (for L1/L2/L3 cache bandwidth measurement)
// Regular stores go through cache hierarchy — measures cache write bandwidth.
// ============================================================================

spd_write_impl!(
    spd_write_128_impl, spd_write_128_multi,
    "sse4.2,sse4.1,ssse3,sse3,sse2,popcnt",
    std::arch::x86_64::__m128i,
    std::arch::x86_64::_mm_set1_epi64x,
    std::arch::x86_64::_mm_store_si128,
    false
);

spd_write_impl!(
    spd_write_256_impl, spd_write_256_multi,
    "avx2,avx,fma,bmi1,bmi2",
    std::arch::x86_64::__m256i,
    std::arch::x86_64::_mm256_set1_epi64x,
    std::arch::x86_64::_mm256_store_si256,
    false
);

spd_write_impl!(
    spd_write_512_impl, spd_write_512_multi,
    "avx512f,avx512bw,avx512cd,avx512dq,avx512vl,avx2,avx,fma,bmi1,bmi2",
    std::arch::x86_64::__m512i,
    std::arch::x86_64::_mm512_set1_epi64,
    std::arch::x86_64::_mm512_store_si512,
    false
);

crate::auto_dispatch!(
    pub spd_write_auto_multi,
    spd_write_128_multi,
    spd_write_128_multi,
    spd_write_256_multi,
    spd_write_512_multi
);

// ============================================================================
// WRITE — NT variants (for DRAM bandwidth measurement)
// Non-temporal stores bypass cache — measures raw memory write bandwidth.
// ============================================================================

spd_write_impl!(
    spd_write_nt_128_impl, spd_write_nt_128_multi,
    "sse4.2,sse4.1,ssse3,sse3,sse2,popcnt",
    std::arch::x86_64::__m128i,
    std::arch::x86_64::_mm_set1_epi64x,
    std::arch::x86_64::_mm_stream_si128,
    true
);

spd_write_impl!(
    spd_write_nt_256_impl, spd_write_nt_256_multi,
    "avx2,avx,fma,bmi1,bmi2",
    std::arch::x86_64::__m256i,
    std::arch::x86_64::_mm256_set1_epi64x,
    std::arch::x86_64::_mm256_stream_si256,
    true
);

spd_write_impl!(
    spd_write_nt_512_impl, spd_write_nt_512_multi,
    "avx512f,avx512bw,avx512cd,avx512dq,avx512vl,avx2,avx,fma,bmi1,bmi2",
    std::arch::x86_64::__m512i,
    std::arch::x86_64::_mm512_set1_epi64,
    std::arch::x86_64::_mm512_stream_si512,
    true
);

crate::auto_dispatch!(
    pub spd_write_nt_auto_multi,
    spd_write_nt_128_multi,
    spd_write_nt_128_multi,
    spd_write_nt_256_multi,
    spd_write_nt_512_multi
);

// ============================================================================
// READ — All variants (regular loads, no NT distinction for reads on x86)
// ============================================================================

spd_read_impl!(
    spd_read_128_impl, spd_read_128_multi,
    "sse4.2,sse4.1,ssse3,sse3,sse2,popcnt",
    std::arch::x86_64::__m128i,
    std::arch::x86_64::_mm_load_si128,
    std::arch::x86_64::_mm_xor_si128,
    std::arch::x86_64::_mm_setzero_si128
);

spd_read_impl!(
    spd_read_256_impl, spd_read_256_multi,
    "avx2,avx,fma,bmi1,bmi2",
    std::arch::x86_64::__m256i,
    std::arch::x86_64::_mm256_load_si256,
    std::arch::x86_64::_mm256_xor_si256,
    std::arch::x86_64::_mm256_setzero_si256
);

spd_read_impl!(
    spd_read_512_impl, spd_read_512_multi,
    "avx512f,avx512bw,avx512cd,avx512dq,avx512vl,avx2,avx,fma,bmi1,bmi2",
    std::arch::x86_64::__m512i,
    std::arch::x86_64::_mm512_load_si512,
    std::arch::x86_64::_mm512_xor_si512,
    std::arch::x86_64::_mm512_setzero_si512
);

crate::auto_dispatch!(
    pub spd_read_auto_multi,
    spd_read_128_multi,
    spd_read_128_multi,
    spd_read_256_multi,
    spd_read_512_multi
);

// ============================================================================
// COPY — Cached variants (for L1/L2/L3 cache bandwidth measurement)
// Regular load + regular store — measures cache copy bandwidth.
// ============================================================================

spd_copy_impl!(
    spd_copy_128_impl, spd_copy_128_multi,
    "sse4.2,sse4.1,ssse3,sse3,sse2,popcnt",
    std::arch::x86_64::__m128i,
    std::arch::x86_64::_mm_load_si128,
    std::arch::x86_64::_mm_store_si128,
    false
);

spd_copy_impl!(
    spd_copy_256_impl, spd_copy_256_multi,
    "avx2,avx,fma,bmi1,bmi2",
    std::arch::x86_64::__m256i,
    std::arch::x86_64::_mm256_load_si256,
    std::arch::x86_64::_mm256_store_si256,
    false
);

spd_copy_impl!(
    spd_copy_512_impl, spd_copy_512_multi,
    "avx512f,avx512bw,avx512cd,avx512dq,avx512vl,avx2,avx,fma,bmi1,bmi2",
    std::arch::x86_64::__m512i,
    std::arch::x86_64::_mm512_load_si512,
    std::arch::x86_64::_mm512_store_si512,
    false
);

crate::auto_dispatch!(
    pub spd_copy_auto_multi,
    spd_copy_128_multi,
    spd_copy_128_multi,
    spd_copy_256_multi,
    spd_copy_512_multi
);

// ============================================================================
// COPY — NT variants (for DRAM bandwidth measurement)
// Regular load + NT store — measures raw memory copy bandwidth.
// ============================================================================

spd_copy_impl!(
    spd_copy_nt_128_impl, spd_copy_nt_128_multi,
    "sse4.2,sse4.1,ssse3,sse3,sse2,popcnt",
    std::arch::x86_64::__m128i,
    std::arch::x86_64::_mm_load_si128,
    std::arch::x86_64::_mm_stream_si128,
    true
);

spd_copy_impl!(
    spd_copy_nt_256_impl, spd_copy_nt_256_multi,
    "avx2,avx,fma,bmi1,bmi2",
    std::arch::x86_64::__m256i,
    std::arch::x86_64::_mm256_load_si256,
    std::arch::x86_64::_mm256_stream_si256,
    true
);

spd_copy_impl!(
    spd_copy_nt_512_impl, spd_copy_nt_512_multi,
    "avx512f,avx512bw,avx512cd,avx512dq,avx512vl,avx2,avx,fma,bmi1,bmi2",
    std::arch::x86_64::__m512i,
    std::arch::x86_64::_mm512_load_si512,
    std::arch::x86_64::_mm512_stream_si512,
    true
);

crate::auto_dispatch!(
    pub spd_copy_nt_auto_multi,
    spd_copy_nt_128_multi,
    spd_copy_nt_128_multi,
    spd_copy_nt_256_multi,
    spd_copy_nt_512_multi
);
