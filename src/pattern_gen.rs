//! Pattern generation functions for TMR v2 tests.
//!
//! All functions are `#[inline(always)]` to ensure they compile into the caller's
//! hot loop with zero overhead. No traits, no dynamic dispatch.
//!
//! # Pattern Modes (TM5 compatibility)
//!
//! - **Mode 0**: Address-derived unique pattern (`idx ^ constant`). TMR-native, not TM5
//!   topology-aware. Provides unique coverage per address with minimal computation.
//! - **Mode 1**: Inverted constant per cycle. TM5 flips pattern between write passes
//!   using `param0 ^ param1`.
//! - **Mode 2**: LCG (Linear Congruential Generator). Each element is produced by
//!   `state = state * multiplier + addend`. This is THE FIX for the broken Mode 2
//!   which previously constructed a static seed instead of running the LCG chain.
//!
//! # SIMD LCG Strategy
//!
//! For N-lane SIMD (e.g., `u64x4`), we run N independent LCG streams:
//! - Seeds: `s0, lcg(s0), lcg(lcg(s0)), ...` (one per lane)
//! - Each lane advances by N steps per iteration using pre-computed `multiplier^N`
//! - This eliminates lane dependencies while maintaining full SIMD throughput.

use std::simd::*;

// ─── Scalar Pattern Functions ────────────────────────────────────────────────

/// Mode 0: address-derived unique pattern (TMR-native).
/// Each element gets a unique value based on its index XORed with a constant base.
/// Fast, simple, good bit coverage per address.
#[inline(always)]
pub fn pattern_mode0(idx: u64, base: u64) -> u64 {
    idx ^ base
}

/// Mode 1: inverted constant per cycle.
/// TM5 flips pattern between write passes. The caller passes `param0 ^ param1`
/// pre-computed as `combined`, then each element is `idx ^ combined`.
#[inline(always)]
pub fn pattern_mode1(idx: u64, combined: u64) -> u64 {
    idx ^ combined
}

/// Mode 2: Single LCG step.
/// `state = state * multiplier + addend`
///
/// This is the core PRNG operation. For sequential use, chain calls:
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

    #[test]
    fn test_pattern_mode0_unique() {
        let base = 0xDEADBEEFDEADBEEF;
        // Each index produces a unique pattern
        let p0 = pattern_mode0(0, base);
        let p1 = pattern_mode0(1, base);
        let p2 = pattern_mode0(2, base);
        assert_ne!(p0, p1);
        assert_ne!(p1, p2);
        assert_ne!(p0, p2);
        // Deterministic
        assert_eq!(pattern_mode0(42, base), pattern_mode0(42, base));
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
