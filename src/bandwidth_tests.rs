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
use crate::runner::AllocationBlock;
use crate::test_scaffolding::TestRunner;
use crate::tests::{TestAction, TestMemoryConfig, TestProgress, TestStats, TestTiming};

/// The write pattern. Not byte-uniform: a byte-uniform fill (it was 0x5555...) lets LLVM turn the
/// cached write loop into `memset`, which measured the C runtime, the same at every width.
const STATIC_PATTERN: u64 = 0xA55AA55AA55AA55A;

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
    }};
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
    }};
}

// ============================================================================
// COPY — Hot loop macros (4x unrolled load from src + store to dst)
// ============================================================================

/// One Spd-Copy chunk, `$chunk_fn(src, dst, bytes)`: `bytes` from `src` to `dst`, the chunk at one
/// offset in each half. 4x unrolled SIMD copy loop. `$store_fn` is `_mm*_store_si*` (cached; the
/// 128-bit cached copy uses `read_volatile`/`write_volatile`) or `_mm*_stream_si*` (NT).
///
/// It walks both pointers over the chunk's vectors, four a step, then the rest, and runs in a
/// function of its own (`$chunk_fn`, called once a chunk; TODO 99): inlined in the test function,
/// where the compiler knows the halves' distance, it kept one pointer and addressed the other half
/// through an index, `(walked,distance)` for each store (or each load), and the NT copy indexed
/// both from one offset. It takes three scalars, which come in registers: as a struct by value
/// (round 3) they went through the stack, three stores and three store-forwarded loads before the
/// chunk's first load, twice a cycle, which with the call at L1 size (a 12 KiB chunk, 192 vectors
/// at 512 bits) cost about 2%.
///
/// Safety: `src` and `dst` are valid for `bytes` rounded up to a whole vector, aligned to the
/// vector, and don't overlap.
macro_rules! spd_copy_hot {
    ($chunk_fn:ident, $target_feature:literal, $arch_type:ty, $load_fn:path, $store_fn:path) => {
        #[target_feature(enable = $target_feature)]
        #[inline(never)]
        unsafe fn $chunk_fn(src: *const u8, dst: *mut u8, bytes: usize) {
            let mut sp = src as *const $arch_type;
            let mut dp = dst as *mut $arch_type;
            let vectors = bytes.div_ceil(std::mem::size_of::<$arch_type>());
            for _ in 0..vectors / 4 {
                let v0 = $load_fn(sp);
                let v1 = $load_fn(sp.add(1));
                let v2 = $load_fn(sp.add(2));
                let v3 = $load_fn(sp.add(3));
                $store_fn(dp, v0);
                $store_fn(dp.add(1), v1);
                $store_fn(dp.add(2), v2);
                $store_fn(dp.add(3), v3);
                sp = sp.wrapping_add(4);
                dp = dp.wrapping_add(4);
            }
            // The rest, at most 3, in line: as a loop, the NT copy's (its stores are asm, so the
            // compiler doesn't unroll it) indexed both from one offset
            let rest = vectors % 4;
            if rest > 0 {
                $store_fn(dp, $load_fn(sp));
            }
            if rest > 1 {
                $store_fn(dp.add(1), $load_fn(sp.add(1)));
            }
            if rest > 2 {
                $store_fn(dp.add(2), $load_fn(sp.add(2)));
            }
        }
    };
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
            blocks: &[AllocationBlock], thread_id: usize, error_mode: ErrorMode, timing: &TestTiming, config: &TestMemoryConfig,
            progress: Option<&TestProgress>,
        ) -> TestStats {
            let test_name = "Spd-Write";
            let (mut runner, extent) =
                TestRunner::new(blocks, thread_id, error_mode, timing, config, progress, test_name, TestAction::Write);
            if extent.test_size == 0 {
                return runner.finish_completed(0);
            }
            let base = extent.ptr;

            // Init phase: scalar fill to fault in pages (skipped in dependent mode), not timed
            if !config.skip_init {
                scalar_fill(base as *mut u64, extent.test_size / 8);
                runner.setup_done(crate::ops::Op::PatternFill, extent.test_size as u64);
            } else {
                runner.restart_clock();
            }

            let pattern = $set1_fn(STATIC_PATTERN as i64);
            // The extent as one chunk under the `whole` chunk mode the built-ins use
            let spread = runner.chunks(extent.test_size);

            loop {
                runner.begin_cycle();

                for k in 0..spread.count() {
                    spd_write_hot!(base.add(spread.start(k)), spread.chunk(), pattern, $arch_type, $store_fn);
                    if $need_sfence {
                        std::arch::x86_64::_mm_sfence();
                    }
                    runner.add_bytes(spread.chunk());
                }

                runner.update_progress();
                if runner.shutdown_requested() || !runner.should_continue() {
                    return runner.finish_completed((runner.bytes_processed() / 8) as u64);
                }
            }
        }

        #[doc = include_str!("test_fn_safety.md")]
        pub unsafe fn $pub_fn(
            blocks: &[AllocationBlock], thread_id: usize, error_mode: ErrorMode, timing: &TestTiming, config: &TestMemoryConfig,
            progress: Option<&TestProgress>,
        ) -> TestStats {
            $impl_fn(blocks, thread_id, error_mode, timing, config, progress)
        }
    };
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
            blocks: &[AllocationBlock], thread_id: usize, error_mode: ErrorMode, timing: &TestTiming, config: &TestMemoryConfig,
            progress: Option<&TestProgress>,
        ) -> TestStats {
            let test_name = "Spd-Read";
            let (mut runner, extent) =
                TestRunner::new(blocks, thread_id, error_mode, timing, config, progress, test_name, TestAction::Read);
            if extent.test_size == 0 {
                return runner.finish_completed(0);
            }
            let base = extent.ptr;

            // Init phase: scalar fill to ensure physical pages (skipped in dependent mode), not timed
            if !config.skip_init {
                scalar_fill(base as *mut u64, extent.test_size / 8);
                runner.setup_done(crate::ops::Op::PatternFill, extent.test_size as u64);
            } else {
                runner.restart_clock();
            }

            // The extent as one chunk under the `whole` chunk mode the built-ins use
            let spread = runner.chunks(extent.test_size);

            loop {
                runner.begin_cycle();

                for k in 0..spread.count() {
                    spd_read_hot!(base.add(spread.start(k)) as *const u8, spread.chunk(), $arch_type, $load_fn, $xor_fn, $setzero_fn);
                    runner.add_bytes(spread.chunk());
                }

                runner.update_progress();
                if runner.shutdown_requested() || !runner.should_continue() {
                    return runner.finish_completed((runner.bytes_processed() / 8) as u64);
                }
            }
        }

        #[doc = include_str!("test_fn_safety.md")]
        pub unsafe fn $pub_fn(
            blocks: &[AllocationBlock], thread_id: usize, error_mode: ErrorMode, timing: &TestTiming, config: &TestMemoryConfig,
            progress: Option<&TestProgress>,
        ) -> TestStats {
            $impl_fn(blocks, thread_id, error_mode, timing, config, progress)
        }
    };
}

// ============================================================================
// COPY test function macro — stamps out a complete bandwidth copy test
// Split allocation: read from first half, write to second half.
// bytes_processed = total traffic (read bytes + write bytes).
// ============================================================================

macro_rules! spd_copy_impl {
    (
        $impl_fn:ident, $pub_fn:ident, $chunk_fn:ident,
        $target_feature:literal,
        $arch_type:ty,
        $load_fn:path,
        $store_fn:path,
        $need_sfence:expr
    ) => {
        spd_copy_hot!($chunk_fn, $target_feature, $arch_type, $load_fn, $store_fn);

        #[target_feature(enable = $target_feature)]
        unsafe fn $impl_fn(
            blocks: &[AllocationBlock], thread_id: usize, error_mode: ErrorMode, timing: &TestTiming, config: &TestMemoryConfig,
            progress: Option<&TestProgress>,
        ) -> TestStats {
            let test_name = "Spd-Copy";
            let (mut runner, extent) =
                TestRunner::new(blocks, thread_id, error_mode, timing, config, progress, test_name, TestAction::Copy);
            if extent.test_size == 0 {
                return runner.finish_completed(0);
            }
            let base = extent.ptr;
            // The extent's halves: it is a multiple of 4 KiB, so each half is a multiple of every
            // vector width
            let half_bytes = extent.test_size / 2;

            // Init phase: scalar fill source half (skipped in dependent mode), not timed
            if !config.skip_init {
                scalar_fill(base as *mut u64, half_bytes / 8);
                runner.setup_done(crate::ops::Op::PatternFill, half_bytes as u64);
            } else {
                runner.restart_clock();
            }

            // Each half as one chunk under the `whole` chunk mode the built-ins use
            let spread = runner.half_chunks(extent.test_size);
            let (half_a, half_b) = (base, base.add(half_bytes));

            loop {
                runner.begin_cycle();

                // Bidirectional: A→B then B→A, each direction in full = the extent's traffic twice
                for (src, dst) in [(half_a, half_b), (half_b, half_a)] {
                    for k in 0..spread.count() {
                        let offset = spread.start(k);
                        $chunk_fn(src.add(offset), dst.add(offset), spread.chunk());
                        // Traffic: the chunk read plus the chunk written
                        runner.add_bytes(spread.chunk() * 2);
                    }
                    if $need_sfence {
                        std::arch::x86_64::_mm_sfence();
                    }
                }

                runner.update_progress();
                if runner.shutdown_requested() || !runner.should_continue() {
                    return runner.finish_completed((runner.bytes_processed() / 8) as u64);
                }
            }
        }

        #[doc = include_str!("test_fn_safety.md")]
        pub unsafe fn $pub_fn(
            blocks: &[AllocationBlock], thread_id: usize, error_mode: ErrorMode, timing: &TestTiming, config: &TestMemoryConfig,
            progress: Option<&TestProgress>,
        ) -> TestStats {
            $impl_fn(blocks, thread_id, error_mode, timing, config, progress)
        }
    };
}

// ============================================================================
// WRITE — Cached variants (for L1/L2/L3 cache bandwidth measurement)
// Regular stores go through cache hierarchy — measures cache write bandwidth.
// ============================================================================

spd_write_impl!(
    spd_write_128_impl,
    spd_write_128_multi,
    "sse4.2,sse4.1,ssse3,sse3,sse2,popcnt",
    std::arch::x86_64::__m128i,
    std::arch::x86_64::_mm_set1_epi64x,
    std::arch::x86_64::_mm_store_si128,
    false
);

spd_write_impl!(
    spd_write_256_impl,
    spd_write_256_multi,
    "avx2,avx,fma,bmi1,bmi2",
    std::arch::x86_64::__m256i,
    std::arch::x86_64::_mm256_set1_epi64x,
    std::arch::x86_64::_mm256_store_si256,
    false
);

spd_write_impl!(
    spd_write_512_impl,
    spd_write_512_multi,
    "avx512f,avx512bw,avx512cd,avx512dq,avx512vl,avx2,avx,fma,bmi1,bmi2",
    std::arch::x86_64::__m512i,
    std::arch::x86_64::_mm512_set1_epi64,
    std::arch::x86_64::_mm512_store_si512,
    false
);

crate::auto_dispatch!(
    pub spd_write_auto_multi,
    spd_write_128_multi,
    spd_write_256_multi,
    spd_write_512_multi
);

// ============================================================================
// WRITE — NT variants (for DRAM bandwidth measurement)
// Non-temporal stores bypass cache — measures raw memory write bandwidth.
// ============================================================================

spd_write_impl!(
    spd_write_nt_128_impl,
    spd_write_nt_128_multi,
    "sse4.2,sse4.1,ssse3,sse3,sse2,popcnt",
    std::arch::x86_64::__m128i,
    std::arch::x86_64::_mm_set1_epi64x,
    std::arch::x86_64::_mm_stream_si128,
    true
);

spd_write_impl!(
    spd_write_nt_256_impl,
    spd_write_nt_256_multi,
    "avx2,avx,fma,bmi1,bmi2",
    std::arch::x86_64::__m256i,
    std::arch::x86_64::_mm256_set1_epi64x,
    std::arch::x86_64::_mm256_stream_si256,
    true
);

spd_write_impl!(
    spd_write_nt_512_impl,
    spd_write_nt_512_multi,
    "avx512f,avx512bw,avx512cd,avx512dq,avx512vl,avx2,avx,fma,bmi1,bmi2",
    std::arch::x86_64::__m512i,
    std::arch::x86_64::_mm512_set1_epi64,
    std::arch::x86_64::_mm512_stream_si512,
    true
);

crate::auto_dispatch!(
    pub spd_write_nt_auto_multi,
    spd_write_nt_128_multi,
    spd_write_nt_256_multi,
    spd_write_nt_512_multi
);

// ============================================================================
// READ — All variants (regular loads, no NT distinction for reads on x86)
// ============================================================================

spd_read_impl!(
    spd_read_128_impl,
    spd_read_128_multi,
    "sse4.2,sse4.1,ssse3,sse3,sse2,popcnt",
    std::arch::x86_64::__m128i,
    std::arch::x86_64::_mm_load_si128,
    std::arch::x86_64::_mm_xor_si128,
    std::arch::x86_64::_mm_setzero_si128
);

spd_read_impl!(
    spd_read_256_impl,
    spd_read_256_multi,
    "avx2,avx,fma,bmi1,bmi2",
    std::arch::x86_64::__m256i,
    std::arch::x86_64::_mm256_load_si256,
    std::arch::x86_64::_mm256_xor_si256,
    std::arch::x86_64::_mm256_setzero_si256
);

spd_read_impl!(
    spd_read_512_impl,
    spd_read_512_multi,
    "avx512f,avx512bw,avx512cd,avx512dq,avx512vl,avx2,avx,fma,bmi1,bmi2",
    std::arch::x86_64::__m512i,
    std::arch::x86_64::_mm512_load_si512,
    std::arch::x86_64::_mm512_xor_si512,
    std::arch::x86_64::_mm512_setzero_si512
);

crate::auto_dispatch!(
    pub spd_read_auto_multi,
    spd_read_128_multi,
    spd_read_256_multi,
    spd_read_512_multi
);

// ============================================================================
// COPY — Cached variants (for L1/L2/L3 cache bandwidth measurement)
// Regular load + regular store — measures cache copy bandwidth.
// ============================================================================

// Volatile 16 B loads and stores, which the compiler can't merge: with `_mm_load_si128` and
// `_mm_store_si128` it copied each two adjacent vectors as one 32 B `ymm` move (the AVX2 baseline),
// so Spd-Copy-128 ran at 256 bits (TODO 99). The same instructions otherwise (`vmovaps xmm`).
spd_copy_impl!(
    spd_copy_128_impl,
    spd_copy_128_multi,
    spd_copy_128_chunk,
    "sse4.2,sse4.1,ssse3,sse3,sse2,popcnt",
    std::arch::x86_64::__m128i,
    std::ptr::read_volatile,
    std::ptr::write_volatile,
    false
);

spd_copy_impl!(
    spd_copy_256_impl,
    spd_copy_256_multi,
    spd_copy_256_chunk,
    "avx2,avx,fma,bmi1,bmi2",
    std::arch::x86_64::__m256i,
    std::arch::x86_64::_mm256_load_si256,
    std::arch::x86_64::_mm256_store_si256,
    false
);

spd_copy_impl!(
    spd_copy_512_impl,
    spd_copy_512_multi,
    spd_copy_512_chunk,
    "avx512f,avx512bw,avx512cd,avx512dq,avx512vl,avx2,avx,fma,bmi1,bmi2",
    std::arch::x86_64::__m512i,
    std::arch::x86_64::_mm512_load_si512,
    std::arch::x86_64::_mm512_store_si512,
    false
);

crate::auto_dispatch!(
    pub spd_copy_auto_multi,
    spd_copy_128_multi,
    spd_copy_256_multi,
    spd_copy_512_multi
);

// ============================================================================
// COPY — NT variants (for DRAM bandwidth measurement)
// Regular load + NT store — measures raw memory copy bandwidth.
// ============================================================================

spd_copy_impl!(
    spd_copy_nt_128_impl,
    spd_copy_nt_128_multi,
    spd_copy_nt_128_chunk,
    "sse4.2,sse4.1,ssse3,sse3,sse2,popcnt",
    std::arch::x86_64::__m128i,
    std::arch::x86_64::_mm_load_si128,
    std::arch::x86_64::_mm_stream_si128,
    true
);

spd_copy_impl!(
    spd_copy_nt_256_impl,
    spd_copy_nt_256_multi,
    spd_copy_nt_256_chunk,
    "avx2,avx,fma,bmi1,bmi2",
    std::arch::x86_64::__m256i,
    std::arch::x86_64::_mm256_load_si256,
    std::arch::x86_64::_mm256_stream_si256,
    true
);

spd_copy_impl!(
    spd_copy_nt_512_impl,
    spd_copy_nt_512_multi,
    spd_copy_nt_512_chunk,
    "avx512f,avx512bw,avx512cd,avx512dq,avx512vl,avx2,avx,fma,bmi1,bmi2",
    std::arch::x86_64::__m512i,
    std::arch::x86_64::_mm512_load_si512,
    std::arch::x86_64::_mm512_stream_si512,
    true
);

crate::auto_dispatch!(
    pub spd_copy_nt_auto_multi,
    spd_copy_nt_128_multi,
    spd_copy_nt_256_multi,
    spd_copy_nt_512_multi
);

#[cfg(test)]
mod tests {
    use super::*;

    /// TODO 99: each width's copy chunk, cached and NT, copies exactly its chunk, each vector to
    /// its own place, over lengths that take its steps of 4 vectors (and the compiler's unrolling
    /// of them), each tail of 1-3 and L1 size, and writes nothing else.
    #[test]
    fn copy_chunks_copy_every_vector_and_nothing_else() {
        type Chunk = unsafe fn(*const u8, *mut u8, usize);
        let mut widths: Vec<(usize, &str, Chunk)> = vec![(16, "128", spd_copy_128_chunk), (16, "NT 128", spd_copy_nt_128_chunk)];
        if is_x86_feature_detected!("avx2") {
            widths.extend([(32, "256", spd_copy_256_chunk as Chunk), (32, "NT 256", spd_copy_nt_256_chunk)]);
        }
        if is_x86_feature_detected!("avx512f") {
            widths.extend([(64, "512", spd_copy_512_chunk as Chunk), (64, "NT 512", spd_copy_nt_512_chunk)]);
        }
        const FILLER: u64 = 0x5A5A_5A5A_5A5A_5A5A;
        // A distinct value at every source word, so a vector copied to the wrong place shows
        let source = |i: usize| (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xA55A_A55A_A55A_A55A;
        // The destination 64 words into its buffer, a guard either side
        let guard = 64usize;
        for (vec_bytes, name, chunk) in widths {
            // ... and L1 size, 12 KiB
            for vectors in (1..=9).chain([16, 17, 18, 19, 64, 67, (12 << 10) / vec_bytes]) {
                let bytes = vectors * vec_bytes;
                let words = (bytes / 8 + 2 * guard).next_multiple_of(8);
                let mut src = vec![std::simd::u64x8::splat(0); words / 8];
                let mut dst = vec![std::simd::u64x8::splat(0); words / 8];
                let (s, d) = (src.as_mut_ptr() as *mut u64, dst.as_mut_ptr() as *mut u64);
                unsafe {
                    for i in 0..words {
                        *s.add(i) = source(i);
                        *d.add(i) = FILLER;
                    }
                    chunk(s as *const u8, d.add(guard) as *mut u8, bytes);
                    std::arch::x86_64::_mm_sfence();
                    for i in 0..words {
                        assert_eq!(*s.add(i), source(i), "{name}, {vectors} vectors: source word {i} changed");
                        let copied = (guard..guard + bytes / 8).contains(&i);
                        let want = if copied { source(i - guard) } else { FILLER };
                        assert_eq!(*d.add(i), want, "{name}, {vectors} vectors: destination word {i}");
                    }
                }
            }
        }
    }
}
