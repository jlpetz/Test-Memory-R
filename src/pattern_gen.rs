//! Pattern generation functions for TMR v2 tests.
//!
//! All functions are `#[inline(always)]` to ensure they compile into the caller's
//! hot loop with zero overhead. No traits, no dynamic dispatch.
//!
//! # Pattern Mode Numbering
//!
//! - **Modes 0, 1, 2**: TM5-faithful implementations. Match original TM5 behavior
//!   (adapted to 64-bit). Used by default when loading TM5 .cfg files.
//! - **Modes 10, 11, 12**: TMR-native modern alternatives. Simpler, sometimes faster,
//!   but don't match TM5's specific stress patterns.
//!
//! ## TM5-Faithful Modes
//!
//! - **Mode 0**: Address-derived static fill with rotation + complement per 4KB page.
//!   TM5 rotates 16-bit seed per word, alternates normal/complement per page.
//!   TMR: 64-bit multiply dispersion, complement toggle every 512 u64 elements.
//! - **Mode 1**: Linear step + complement toggle per cache line. TM5 applies
//!   PADDD step per element and XOR complement every 64 bytes.
//!   TMR: wrapping_sub step per element, complement every cache line.
//! - **Mode 2**: Two-level evolving pattern. TM5 uses PMULLW per-page evolution
//!   with step that also evolves. TMR: 64-bit wrapping_mul evolution per cache line
//!   with evolving step. Stateful (caller tracks page seed/step).
//!
//! ## TMR-Native Modes
//!
//! - **Mode 10**: Unique-per-address XOR (`idx ^ base`). Substitutes for Mode 0.
//! - **Mode 11**: Simple XOR with combined constant (`idx ^ combined`). Substitutes for Mode 1.
//! - **Mode 12**: Flat LCG chain (`state = state * m + a`). Substitutes for Mode 2.
//!
//! See `doc/pattern_gen_modes.md` for detailed descriptions.
//!
//! # SIMD LCG Strategy
//!
//! For N-lane SIMD (e.g., `u64x4`), we run N independent LCG streams:
//! - Seeds: `s0, lcg(s0), lcg(lcg(s0)), ...` (one per lane)
//! - Each lane advances by N steps per iteration using pre-computed `multiplier^N`
//! - This eliminates lane dependencies while maintaining full SIMD throughput.

use std::simd::*;

// ─── Block Seed Derivation ──────────────────────────────────────────────────

/// Compute a deterministic block seed from block virtual address, thread ID, and cycle.
/// Replaces TM5's physical page number hash (shr addr, 12). Computed once per block per cycle.
///
/// Uses Murmur-style finalizer for good avalanche: every input bit affects all output bits.
/// All three inputs (address, thread, cycle) contribute uniquely to avoid patterns.
#[inline]
pub fn block_seed(block_addr: usize, thread_id: usize, cycle: u32) -> u64 {
    let mut h = (block_addr as u64).wrapping_mul(0x9E3779B97F4A7C15);
    h ^= (thread_id as u64).wrapping_mul(0x517CC1B727220A95);
    h ^= (cycle as u64).wrapping_mul(0x6C62272E07BB0142);
    h ^= h >> 33;
    h = h.wrapping_mul(0xFF51AFD7ED558CCD);
    h ^= h >> 33;
    h
}

/// Compute `log2(elements_per_cache_line)` as a shift amount.
/// `cache_line_bytes` must be a power of 2 (always true for real CPUs).
/// Result: number of right-shift bits to convert element index to cache line index.
///
/// Example: cache_line_bytes=64, element_size=8 → elements=8 → shift=3.
#[inline]
pub fn cache_line_shift(cache_line_bytes: usize) -> u32 {
    let elements = cache_line_bytes / std::mem::size_of::<u64>();
    elements.trailing_zeros()
}

// ─── TM5-Faithful Pattern Modes (0, 1, 2) ──────────────────────────────────

/// Mode 0 (TM5-faithful): Address-derived pattern with bit dispersion + complement.
///
/// TM5: Derives 16-bit seed from page number, ROL per word position, alternates
/// normal/complement per 4KB page. Static within a page (no per-cache-line evolution).
///
/// TMR port: Wrapping multiply by golden-ratio prime for 64-bit bit dispersion
/// (SIMD-friendly replacement for per-element ROL). Complement toggles every 4KB page
/// (512 u64 elements), matching TM5's `test dPageAddr, 1000h` behavior.
///
/// TMR alternative: Mode 10 (unique-per-address XOR).
#[inline(always)]
pub fn pattern_mode0(idx: u64, block_seed: u64) -> u64 {
    let base = block_seed ^ idx.wrapping_mul(0x9E3779B97F4A7C15);
    // Complement toggle every 4KB page (512 u64 = 4096 bytes).
    // Branchless: mask = 0 for even pages, all-ones for odd pages.
    let mask = 0u64.wrapping_sub((idx >> 9) & 1); // >>9 = /512
    base ^ mask
}

/// Mode 1 (TM5-faithful): Linear step + complement toggle per cache line.
///
/// TM5: PADDD with step=[-3,-1,0,0] (packed 32-bit) per element, XOR complement
/// (Const_m1) every 64 bytes (cache line).
///
/// TMR port: 64-bit wrapping subtraction by 3 per element (simplification of TM5's
/// packed 32-bit step). Complement toggles every cache line. `cl_shift` controls the
/// cache line boundary (e.g., 3 for 64-byte lines with u64 elements).
///
/// TMR alternative: Mode 11 (simple XOR with combined constant).
///
/// # Arguments
/// * `cl_shift` - Bit shift for cache line size (from `cache_line_shift()`).
#[inline(always)]
pub fn pattern_mode1(idx: u64, block_seed: u64, cl_shift: u32) -> u64 {
    let cache_line = idx >> cl_shift;
    let stepped = block_seed.wrapping_sub(idx.wrapping_mul(3));
    // Branchless complement toggle per cache line
    let mask = 0u64.wrapping_sub(cache_line & 1);
    stepped ^ mask
}

/// Mode 2 (TM5-faithful): Evolve page seed and step at each cache line boundary.
///
/// TM5: Per-page, applies PMULLW(xmm, param0) + PADDD(xmm, param1). The step register
/// (xmm6) is ALSO multiplied by the evolved pattern, so the step itself changes
/// pseudo-randomly between pages.
///
/// TMR port: 64-bit wrapping_mul/add. Called once per cache line (every 8 u64 elements
/// for 64-byte cache lines). Returns (new_seed, new_step).
///
/// TMR alternative: Mode 12 (flat LCG chain).
#[inline(always)]
pub fn mode2_evolve(seed: u64, step: u64, param0: u64, param1: u64) -> (u64, u64) {
    let new_seed = seed.wrapping_mul(param0).wrapping_add(param1);
    let new_step = step.wrapping_mul(new_seed);
    (new_seed, new_step)
}

/// Mode 2 (TM5-faithful): Compute element value within a cache line.
///
/// TM5: Within a page, writes xmm0 then PADDD xmm0, xmm6 (adds step per 16-byte group).
///
/// TMR port: Element value = page_seed + element_offset * page_step. The `element_offset`
/// is the position within the current cache line (0..elements_per_cache_line).
#[inline(always)]
pub fn mode2_element(page_seed: u64, element_offset: u64, page_step: u64) -> u64 {
    page_seed.wrapping_add(element_offset.wrapping_mul(page_step))
}

// ─── TMR-Native Pattern Modes (10, 11, 12) ─────────────────────────────────

/// Mode 10 (TMR-native): Address-derived unique pattern. Substitutes for Mode 0.
///
/// Each element gets a unique value based on its index XORed with a constant base.
/// Fast, simple, good bit coverage per address. No page-level structure.
#[inline(always)]
pub fn pattern_mode10(idx: u64, base: u64) -> u64 {
    idx ^ base
}

/// Mode 11 (TMR-native): Simple XOR with pre-combined constant. Substitutes for Mode 1.
///
/// Caller passes `param0 ^ param1` pre-computed as `combined`, each element = `idx ^ combined`.
/// No step evolution, no complement toggle.
#[inline(always)]
pub fn pattern_mode11(idx: u64, combined: u64) -> u64 {
    idx ^ combined
}

/// Mode 13 (TMR-native): Positional pseudo-random hash.
///
/// Combines the pseudo-random quality of Mode 12 (LCG) with the positional independence
/// of Modes 10/11. Each element is computed purely from `(idx, seed)` — no sequential
/// dependency, no carry-state needed across chunks, trivially SIMD-parallelizable.
///
/// Uses splitmix64 finalizer (same bit mixing as java.util.SplittableRandom).
/// Excellent avalanche: every output bit depends on every input bit.
#[inline(always)]
pub fn pattern_mode13(idx: u64, seed: u64) -> u64 {
    let mut h = idx.wrapping_add(seed);
    h ^= h >> 30;
    h = h.wrapping_mul(0xbf58476d1ce4e5b9);
    h ^= h >> 27;
    h = h.wrapping_mul(0x94d049bb133111eb);
    h ^= h >> 31;
    h
}

/// Mode 12 (TMR-native) / shared LCG infrastructure: Single LCG step.
/// `state = state * multiplier + addend`
///
/// Core PRNG used by Mode 12 (flat LCG chain) and SIMD LCG variants.
/// Also usable as building block for Mode 2 (TM5-faithful) page evolution.
/// ```ignore
/// state = lcg_next(state, multiplier, addend);
/// *ptr = state;
/// state = lcg_next(state, multiplier, addend);
/// *ptr.add(1) = state;
/// ```
#[inline(always)]
pub fn lcg_next(state: u64, multiplier: u64, addend: u64) -> u64 {
    state.wrapping_mul(multiplier).wrapping_add(addend)
}

/// Compute `multiplier^n mod 2^64` by repeated squaring.
/// Used to pre-compute the stride multiplier for SIMD LCG lanes.
///
/// For N-lane SIMD, call `lcg_multiplier_power(multiplier, N)` to get the
/// multiplier that advances a lane by N steps in one operation.
#[inline]
pub fn lcg_multiplier_power(multiplier: u64, n: usize) -> u64 {
    let mut result = 1u64;
    let mut base = multiplier;
    let mut exp = n;
    while exp > 0 {
        if exp & 1 == 1 {
            result = result.wrapping_mul(base);
        }
        base = base.wrapping_mul(base);
        exp >>= 1;
    }
    result
}

/// Compute the addend for a multi-step LCG jump.
///
/// If the single-step LCG is `x' = m*x + a`, then after N steps:
/// `x_N = m^N * x_0 + a * (m^(N-1) + m^(N-2) + ... + m + 1)`
///
/// The sum `(m^N - 1) / (m - 1)` is computed iteratively to avoid division:
/// `addend_N = a * sum(m^i for i in 0..N)`
#[inline]
pub fn lcg_addend_power(multiplier: u64, addend: u64, n: usize) -> u64 {
    // Compute sum = 1 + m + m^2 + ... + m^(n-1) via repeated application
    let mut sum = 0u64;
    let mut m_power = 1u64; // m^0 = 1
    for _ in 0..n {
        sum = sum.wrapping_add(m_power);
        m_power = m_power.wrapping_mul(multiplier);
    }
    addend.wrapping_mul(sum)
}

/// Seed N independent LCG streams from a single initial state.
///
/// Returns `[s0, lcg(s0), lcg(lcg(s0)), ...]` — N consecutive LCG outputs
/// starting from `initial_state`.
#[inline]
pub fn lcg_seed_lanes(initial_state: u64, multiplier: u64, addend: u64, n: usize) -> [u64; 8] {
    let mut seeds = [0u64; 8];
    let mut state = initial_state;
    for seed in seeds.iter_mut().take(n) {
        *seed = state;
        state = lcg_next(state, multiplier, addend);
    }
    seeds
}

// ─── SIMD LCG (u64x2 — SSE2/128-bit) ────────────────────────────────────────

/// State for a 2-lane SIMD LCG (128-bit).
pub struct LcgSimd2 {
    pub state: u64x2,
    pub multiplier: u64x2,  // multiplier^2 broadcast
    pub addend: u64x2,      // multi-step addend broadcast
}

impl LcgSimd2 {
    /// Create a 2-lane SIMD LCG from scalar parameters.
    ///
    /// Each lane gets an independent stream seeded from consecutive LCG outputs.
    /// The multiplier and addend are pre-computed for 2-step jumps.
    #[inline]
    pub fn new(initial_state: u64, multiplier: u64, addend: u64) -> Self {
        let seeds = lcg_seed_lanes(initial_state, multiplier, addend, 2);
        let m2 = lcg_multiplier_power(multiplier, 2);
        let a2 = lcg_addend_power(multiplier, addend, 2);
        Self {
            state: u64x2::from_array([seeds[0], seeds[1]]),
            multiplier: u64x2::splat(m2),
            addend: u64x2::splat(a2),
        }
    }

    /// Advance all lanes by 2 LCG steps (one per lane) and return current state.
    #[inline(always)]
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> u64x2 {
        let current = self.state;
        self.state = self.state * self.multiplier + self.addend;
        current
    }
}

// ─── SIMD LCG (u64x4 — AVX2/256-bit) ────────────────────────────────────────

/// State for a 4-lane SIMD LCG (256-bit).
pub struct LcgSimd4 {
    pub state: u64x4,
    pub multiplier: u64x4,
    pub addend: u64x4,
}

impl LcgSimd4 {
    #[inline]
    pub fn new(initial_state: u64, multiplier: u64, addend: u64) -> Self {
        let seeds = lcg_seed_lanes(initial_state, multiplier, addend, 4);
        let m4 = lcg_multiplier_power(multiplier, 4);
        let a4 = lcg_addend_power(multiplier, addend, 4);
        Self {
            state: u64x4::from_array([seeds[0], seeds[1], seeds[2], seeds[3]]),
            multiplier: u64x4::splat(m4),
            addend: u64x4::splat(a4),
        }
    }

    #[inline(always)]
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> u64x4 {
        let current = self.state;
        self.state = self.state * self.multiplier + self.addend;
        current
    }
}

// ─── SIMD LCG (u64x8 — AVX-512/512-bit) ─────────────────────────────────────

/// State for an 8-lane SIMD LCG (512-bit).
pub struct LcgSimd8 {
    pub state: u64x8,
    pub multiplier: u64x8,
    pub addend: u64x8,
}

impl LcgSimd8 {
    #[inline]
    pub fn new(initial_state: u64, multiplier: u64, addend: u64) -> Self {
        let seeds = lcg_seed_lanes(initial_state, multiplier, addend, 8);
        let m8 = lcg_multiplier_power(multiplier, 8);
        let a8 = lcg_addend_power(multiplier, addend, 8);
        Self {
            state: u64x8::from_array(seeds),
            multiplier: u64x8::splat(m8),
            addend: u64x8::splat(a8),
        }
    }

    #[inline(always)]
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> u64x8 {
        let current = self.state;
        self.state = self.state * self.multiplier + self.addend;
        current
    }
}

// ─── Mirror pattern generation (u64-based, replacing i32) ────────────────────

/// Generate a mirror move pattern value for a given element index.
/// Uses u64 arithmetic for full 64-bit pattern coverage.
///
/// The pattern combines thread identity (high bits) with element position
/// (distributed via wrapping multiply with a large prime).
#[inline(always)]
pub fn mirror_pattern_u64(element_idx: u64, thread_pattern_base: u64) -> u64 {
    element_idx
        .wrapping_add(thread_pattern_base)
        .wrapping_mul(0x0123456789ABCDEFu64)
}

/// Compute the thread-specific pattern base for mirror tests.
/// Places thread_id in upper 32 bits for clean separation from element index.
#[inline(always)]
pub fn mirror_thread_base(thread_id: usize) -> u64 {
    (thread_id as u64) << 32
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── TMR-native mode tests (10, 11) ──

    #[test]
    fn test_pattern_mode10_unique() {
        let base = 0xDEADBEEFDEADBEEF;
        let p0 = pattern_mode10(0, base);
        let p1 = pattern_mode10(1, base);
        let p2 = pattern_mode10(2, base);
        assert_ne!(p0, p1);
        assert_ne!(p1, p2);
        assert_ne!(p0, p2);
        assert_eq!(pattern_mode10(42, base), pattern_mode10(42, base));
    }

    // ── TM5-faithful mode tests (0, 1, 2) ──

    #[test]
    fn test_mode0_complement_toggle() {
        let seed = 0xCAFEBABE12345678u64;
        // Same 4KB page → no complement (idx 0..511)
        let p0 = pattern_mode0(0, seed);
        let p1 = pattern_mode0(1, seed);
        assert_ne!(p0, p1, "Different indices should produce different values");

        // Across 4KB page boundary → complement toggle
        let p_even = pattern_mode0(0, seed);
        let p_odd = pattern_mode0(512, seed); // next 4KB page
        // The odd-page value should be the complement of what even-page would produce at that index
        let even_at_512 = seed ^ (512u64).wrapping_mul(0x9E3779B97F4A7C15);
        assert_eq!(p_odd, !even_at_512, "Odd pages should complement the pattern");
        // And p_even should NOT be complemented
        let even_at_0 = seed ^ (0u64).wrapping_mul(0x9E3779B97F4A7C15);
        assert_eq!(p_even, even_at_0);
    }

    #[test]
    fn test_mode0_deterministic() {
        let seed = block_seed(0x1000, 3, 1);
        assert_eq!(pattern_mode0(42, seed), pattern_mode0(42, seed));
    }

    #[test]
    fn test_mode1_complement_per_cache_line() {
        let seed = 0xAAAAAAAABBBBBBBBu64;
        let cl_shift = cache_line_shift(64); // 64-byte cache line

        // Elements 0..7 are cache line 0 (even → no complement)
        // Elements 8..15 are cache line 1 (odd → complement)
        let p7 = pattern_mode1(7, seed, cl_shift);
        let p8 = pattern_mode1(8, seed, cl_shift);

        // p7 is stepped, no complement
        let expected_7 = seed.wrapping_sub(7 * 3);
        assert_eq!(p7, expected_7);

        // p8 is stepped + complement (cache line 1)
        let expected_8 = !seed.wrapping_sub(8 * 3);
        assert_eq!(p8, expected_8);
    }

    #[test]
    fn test_mode1_step_changes_per_element() {
        let seed = 0x1234567890ABCDEFu64;
        let cl_shift = cache_line_shift(64);
        let p0 = pattern_mode1(0, seed, cl_shift);
        let p1 = pattern_mode1(1, seed, cl_shift);
        let p2 = pattern_mode1(2, seed, cl_shift);
        assert_ne!(p0, p1);
        assert_ne!(p1, p2);
    }

    #[test]
    fn test_mode2_evolve_changes_seed_and_step() {
        let seed = 0x12345678u64;
        let step = 0u64;
        let param0 = 0x5DEECE66Du64;
        let param1 = 0xBu64;

        // Step is intentionally ignored here: with step=0, new_step = 0 * new_seed = 0,
        // so it stays unchanged. Step evolution is exercised below with a non-zero seed step.
        let (s1, _st1) = mode2_evolve(seed, step, param0, param1);
        assert_ne!(s1, seed, "Seed should change");
        let step2 = 7u64;
        let (s2, st2) = mode2_evolve(seed, step2, param0, param1);
        assert_ne!(st2, step2, "Step should evolve");
        assert_ne!(st2, 0, "Step should be non-zero after evolution with non-zero input");

        // Second evolution produces different results
        let (s3, st3) = mode2_evolve(s2, st2, param0, param1);
        assert_ne!(s3, s2);
        assert_ne!(st3, st2);
    }

    #[test]
    fn test_mode2_element_within_page() {
        let page_seed = 0xDEADu64;
        let page_step = 17u64;
        // element_offset 0 → page_seed
        assert_eq!(mode2_element(page_seed, 0, page_step), page_seed);
        // element_offset 1 → page_seed + 1*step
        assert_eq!(mode2_element(page_seed, 1, page_step), page_seed + 17);
        // element_offset 7 → page_seed + 7*step
        assert_eq!(mode2_element(page_seed, 7, page_step), page_seed + 7 * 17);
    }

    #[test]
    fn test_block_seed_varies_by_inputs() {
        let s1 = block_seed(0x1000, 0, 0);
        let s2 = block_seed(0x2000, 0, 0); // different address
        let s3 = block_seed(0x1000, 1, 0); // different thread
        let s4 = block_seed(0x1000, 0, 1); // different cycle
        assert_ne!(s1, s2);
        assert_ne!(s1, s3);
        assert_ne!(s1, s4);
        // Deterministic
        assert_eq!(s1, block_seed(0x1000, 0, 0));
    }

    #[test]
    fn test_cache_line_shift_values() {
        assert_eq!(cache_line_shift(64), 3);  // 64/8 = 8 elements = 2^3
        assert_eq!(cache_line_shift(128), 4); // 128/8 = 16 elements = 2^4
        assert_eq!(cache_line_shift(32), 2);  // 32/8 = 4 elements = 2^2
    }

    #[test]
    fn test_lcg_produces_evolving_sequence() {
        // Use known params from 1usmus_v3.cfg style
        let multiplier = 0x5DEECE66Du64;
        let addend = 0xBu64;
        let mut state = 0x12345678u64;

        let mut seen = Vec::new();
        for _ in 0..100 {
            state = lcg_next(state, multiplier, addend);
            seen.push(state);
        }

        // Verify all values are different (no static seed bug)
        let unique: std::collections::HashSet<u64> = seen.iter().copied().collect();
        assert_eq!(unique.len(), 100, "LCG should produce 100 unique values, got {}", unique.len());

        // Verify deterministic: same seed produces same sequence
        let mut state2 = 0x12345678u64;
        for &expected in &seen {
            state2 = lcg_next(state2, multiplier, addend);
            assert_eq!(state2, expected);
        }
    }

    #[test]
    fn test_lcg_multiplier_power() {
        let m = 0x5DEECE66Du64;
        // m^1 = m
        assert_eq!(lcg_multiplier_power(m, 1), m);
        // m^2 = m * m
        assert_eq!(lcg_multiplier_power(m, 2), m.wrapping_mul(m));
        // m^4 = (m^2)^2
        let m2 = m.wrapping_mul(m);
        assert_eq!(lcg_multiplier_power(m, 4), m2.wrapping_mul(m2));
    }

    #[test]
    fn test_lcg_simd4_matches_scalar() {
        let multiplier = 0x5DEECE66Du64;
        let addend = 0xBu64;
        let initial = 0xCAFEBABEu64;

        // Run scalar LCG for 16 steps
        let mut scalar_results = Vec::new();
        let mut state = initial;
        for _ in 0..16 {
            scalar_results.push(state);
            state = lcg_next(state, multiplier, addend);
        }

        // Run SIMD LCG for 4 steps (4 lanes × 4 steps = 16 values)
        let mut simd = LcgSimd4::new(initial, multiplier, addend);
        let mut simd_results = Vec::new();
        for _ in 0..4 {
            let v = simd.next();
            simd_results.extend_from_slice(&v.to_array());
        }

        // Results should match: SIMD lanes interleave the same sequence
        // Lane 0 gets elements 0, 4, 8, 12
        // Lane 1 gets elements 1, 5, 9, 13
        // etc.
        for step in 0..4 {
            for lane in 0..4 {
                let scalar_idx = step * 4 + lane;
                let simd_val = simd_results[step * 4 + lane];
                assert_eq!(
                    simd_val, scalar_results[scalar_idx],
                    "Mismatch at step={}, lane={}: SIMD={:#x}, scalar={:#x}",
                    step, lane, simd_val, scalar_results[scalar_idx]
                );
            }
        }
    }

    #[test]
    fn test_lcg_simd2_matches_scalar() {
        let multiplier = 0x5DEECE66Du64;
        let addend = 0xBu64;
        let initial = 0x42u64;

        let mut scalar = Vec::new();
        let mut state = initial;
        for _ in 0..8 {
            scalar.push(state);
            state = lcg_next(state, multiplier, addend);
        }

        let mut simd = LcgSimd2::new(initial, multiplier, addend);
        let mut simd_results = Vec::new();
        for _ in 0..4 {
            let v = simd.next();
            simd_results.extend_from_slice(&v.to_array());
        }

        for step in 0..4 {
            for lane in 0..2 {
                let scalar_idx = step * 2 + lane;
                assert_eq!(
                    simd_results[step * 2 + lane], scalar[scalar_idx],
                    "Mismatch at step={}, lane={}", step, lane
                );
            }
        }
    }

    #[test]
    fn test_mirror_pattern_u64_coverage() {
        let base = mirror_thread_base(1);
        let p0 = mirror_pattern_u64(0, base);
        let p1 = mirror_pattern_u64(1, base);

        // Should use full 64-bit range (high bits should differ)
        assert_ne!(p0 >> 32, p1 >> 32, "Upper 32 bits should differ between elements");
        // Different threads produce different patterns
        let base2 = mirror_thread_base(2);
        assert_ne!(mirror_pattern_u64(0, base), mirror_pattern_u64(0, base2));
    }

    #[test]
    fn test_mirror_thread_base_separation() {
        // Thread bases should be in upper 32 bits
        let b0 = mirror_thread_base(0);
        let b1 = mirror_thread_base(1);
        assert_eq!(b0, 0);
        assert_eq!(b1, 1u64 << 32);
        assert_eq!(mirror_thread_base(255), 255u64 << 32);
    }
}
