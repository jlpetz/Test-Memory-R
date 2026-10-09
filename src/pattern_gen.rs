//! Pattern generation functions for TMR v2 tests.
//!
//! All functions are `#[inline(always)]` to ensure they compile into the caller's
//! hot loop with zero overhead. No traits, no dynamic dispatch.
//!
//! # Pattern Mode Numbering
//!
//! - **Modes 0, 1, 2**: TM5's modes, adapted to 64-bit; mode 0 keeps TM5's idea rather than its
//!   bits (below). Used by default when loading TM5 .cfg files.
//! - **Modes 10, 11, 12**: TMR-native modern alternatives. Simpler, sometimes faster,
//!   but don't match TM5's specific stress patterns.
//!
//! ## TM5-Faithful Modes
//!
//! - **Mode 0**: TM5 writes every line of a block the same: three word-and-inverse pairs (its
//!   test 0's) and the first again, from the block's first page number. TMR writes a
//!   pseudo-random word everywhere, inverted on odd 4 KiB pages: not TM5's bits, but as many line
//!   flips on DDR5 and every word distinct (`doc/pattern_gen_modes.md`).
//! - **Mode 1**: Linear step + complement toggle per cache line. TM5 applies
//!   PADDD step per element and XOR complement every 64 bytes.
//!   TMR: wrapping_sub step per element, complement every cache line.
//! - **Mode 2**: Two-level evolving pattern. TM5 uses PMULLW per-page evolution
//!   with step that also evolves. TMR: each 64 B line is `seed + j * step`, and seed and
//!   step evolve by 64-bit wrapping multiply from line to line. TM5 seeds the chain once per
//!   block; TMR restarts it at every 4 KiB page (TODO 76), so a page depends only on its
//!   address and chunks may overlap.
//!
//! ## TMR-Native Modes
//!
//! - **Mode 10**: Unique-per-address XOR (`idx ^ base`). Substitutes for Mode 0.
//! - **Mode 11**: Simple XOR with combined constant (`idx ^ combined`). Substitutes for Mode 1.
//! - **Mode 12**: Mode 2's lines without the chain: each 64 B line is `seed + j * step`,
//!   with seed and step hashed from the line's address (TODO 76).
//! - **Mode 13**: Positional pseudo-random hash per word.
//! - **Mode 14**: Bus flip: each beat of a line the inverse of the one before, at the memory
//!   type's beat width (SMBIOS: 64-bit for DDR4, 32-bit for DDR5), so every data line toggles on
//!   every beat; one value a line, every byte four 1s so DBI can't undo it.
//!
//! See `doc/pattern_gen_modes.md` for detailed descriptions.

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

/// Mode 0: an address-derived pseudo-random word, inverted on odd 4 KiB pages.
///
/// TM5: once per block, a 16-bit word from the block's first page number becomes three 16 B
/// registers, each a word and its inverse (test 0's `RS_GeneratePattern`; `test dPageAddr, 1000h`
/// swaps word and inverse on an even page). Every 64 B line of the block is the three and the
/// first again: `q0 !q0 q1 !q1 q2 !q2 q0 !q0`.
///
/// TMR: wrapping multiply by the golden-ratio constant, every word distinct, the inversion every
/// 4 KiB page (512 u64 elements). Not TM5's bits; as many line flips on DDR5's 32-bit beats.
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
/// TMR alternative: Mode 12 (the same lines, hashed rather than chained).
#[inline(always)]
pub fn mode2_evolve(seed: u64, step: u64, param0: u64, param1: u64) -> (u64, u64) {
    let new_seed = seed.wrapping_mul(param0).wrapping_add(param1);
    let new_step = step.wrapping_mul(new_seed);
    (new_seed, new_step)
}

/// Words per 64 B line: TM5's block length (`dBlockLength`), the unit mode 2 evolves on and
/// mode 12 hashes. Fixed, not the CPU's cache line, so the image is the same on every machine.
pub const LINE_WORDS: usize = 8;

/// Words per 4 KiB page. Mode 2 restarts its chain on each page, and chunks start on pages.
pub const PAGE_WORDS: usize = 512;

/// Mode 2: the chain's start for the 4 KiB page at `page_addr`. TM5 seeds once per block from
/// the block's address; TMR does the same per page.
#[inline(always)]
pub fn mode2_page_start(page_addr: usize, thread_id: usize, cycle: u32) -> (u64, u64) {
    let seed = block_seed(page_addr, thread_id, cycle);
    (seed, seed.wrapping_mul(0x5DEECE66D))
}

/// One 64 B line of modes 2 and 12: `seed + j * step` for j in 0..8, from adds only.
///
/// TM5: within a block, writes xmm0, then PADDD xmm0, xmm6 (adds the step per 16-byte group).
#[inline(always)]
pub fn line_words(seed: u64, step: u64) -> [u64; LINE_WORDS] {
    let s2 = step.wrapping_add(step);
    let s4 = s2.wrapping_add(s2);
    let w1 = seed.wrapping_add(step);
    let w2 = seed.wrapping_add(s2);
    let w3 = w1.wrapping_add(s2);
    [seed, w1, w2, w3, seed.wrapping_add(s4), w1.wrapping_add(s4), w2.wrapping_add(s4), w3.wrapping_add(s4)]
}

/// Mode 12's key for one thread and cycle, folding in both parameters. Computed once per chunk.
#[inline]
pub fn mode12_key(thread_id: usize, cycle: u32, param0: u64, param1: u64) -> u64 {
    block_seed(0, thread_id, cycle) ^ param0.wrapping_mul(0xD6E8FEB86659FD93) ^ param1.rotate_left(32)
}

/// Mode 12: seed and step of the 64 B line at `line_addr`, hashed from it. No chain, so any
/// line can be computed on its own. The step is odd, so a line's 8 words all differ.
#[inline(always)]
pub fn mode12_line(line_addr: usize, key: u64) -> (u64, u64) {
    let seed = pattern_mode13(line_addr as u64, key);
    let step = (seed ^ key).rotate_left(29).wrapping_mul(0x9E3779B97F4A7C15) | 1;
    (seed, step)
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
/// Pseudo-random per word, with the positional independence of Modes 10/11. Each element is
/// computed purely from `(idx, seed)` — no sequential dependency, no carry-state needed
/// across chunks, trivially SIMD-parallelizable.
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

/// How many data bits one bus transfer (beat) of a 64 B line carries, which mode 14 flips.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BusBeat {
    /// LPDDR4 and LPDDR5: 16-bit channels.
    Bits16,
    /// DDR5: two 32-bit subchannels per DIMM (and LPDDR3's 32-bit channels).
    Bits32,
    /// DDR to DDR4: one 64-bit channel per DIMM.
    Bits64,
}

impl BusBeat {
    /// From an SMBIOS memory type (Type 17 offset 0x12), or None for a type it doesn't know.
    pub fn from_smbios_type(memory_type: u8) -> Option<Self> {
        match memory_type {
            0x12 | 0x13 | 0x18 | 0x1A => Some(BusBeat::Bits64),  // DDR, DDR2, DDR3, DDR4
            0x1D | 0x22 => Some(BusBeat::Bits32),                // LPDDR3, DDR5
            0x1E | 0x23 => Some(BusBeat::Bits16),                // LPDDR4, LPDDR5
            _ => None,
        }
    }

    pub fn bits(self) -> u32 {
        match self {
            BusBeat::Bits16 => 16,
            BusBeat::Bits32 => 32,
            BusBeat::Bits64 => 64,
        }
    }
}

/// Mode 14 (TMR-native): the bus-flip line. Each beat of the line's burst is the bitwise inverse
/// of the one before, at `beat` bits a transfer, so every data line toggles on every beat unless
/// the controller scrambles the data. Standard burst orders keep that, since they only swap
/// beats of opposite parity.
///
/// One value a line, hashed from its address: 32 bits of the hash spread one nibble to a byte,
/// each byte's high nibble the inverse of its low one, so every byte holds four 1s and four 0s and
/// data bus inversion (DBI), which inverts a byte with more than four 0s, never undoes a flip. The
/// first beat fixes the whole burst, so a line carries 8 to 32 bits of its hash (16-, 32-, 64-bit
/// beats): two lines can match, and a misdirected access between them goes unseen.
#[inline(always)]
pub fn mode14_line(line_addr: usize, key: u64, beat: BusBeat) -> [u64; LINE_WORDS] {
    const LOW_NIBBLES: u64 = 0x0F0F_0F0F_0F0F_0F0F;
    let mut x = pattern_mode13(line_addr as u64, key) & 0xFFFF_FFFF;
    x = (x | (x << 16)) & 0x0000_FFFF_0000_FFFF;
    x = (x | (x << 8)) & 0x00FF_00FF_00FF_00FF;
    x = (x | (x << 4)) & LOW_NIBBLES;
    let a = x | ((!x & LOW_NIBBLES) << 4);
    match beat {
        BusBeat::Bits64 => std::array::from_fn(|j| if j % 2 == 0 { a } else { !a }),
        BusBeat::Bits32 => [(a & 0xFFFF_FFFF) | (!a << 32); LINE_WORDS],
        BusBeat::Bits16 => {
            let pair = (a & 0xFFFF) | ((!a & 0xFFFF) << 16);
            [pair | (pair << 32); LINE_WORDS]
        }
    }
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
    fn line_words_are_an_arithmetic_run() {
        let (seed, step) = (0xDEAD_u64, 0x8000_0000_0000_0011_u64);
        let words = line_words(seed, step);
        for (j, &w) in words.iter().enumerate() {
            assert_eq!(w, seed.wrapping_add((j as u64).wrapping_mul(step)));
        }
    }

    #[test]
    fn mode12_lines_depend_on_address_thread_cycle_and_parameters() {
        let key = mode12_key(3, 1, 0x5DEECE66D, 0xB);
        let (seed, step) = mode12_line(0x4000_0040, key);
        assert_eq!(mode12_line(0x4000_0040, key), (seed, step));
        assert_eq!(step & 1, 1);
        let words = line_words(seed, step);
        let unique: std::collections::HashSet<u64> = words.iter().copied().collect();
        assert_eq!(unique.len(), 8);
        assert_ne!(mode12_line(0x4000_0080, key).0, seed);
        for other in [mode12_key(4, 1, 0x5DEECE66D, 0xB), mode12_key(3, 2, 0x5DEECE66D, 0xB),
                      mode12_key(3, 1, 0x5DEECE66E, 0xB), mode12_key(3, 1, 0x5DEECE66D, 0xC)] {
            assert_ne!(mode12_line(0x4000_0040, other).0, seed);
        }
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

    /// Mode 14: in address order, every beat of a line is the inverse of the one before, at each
    /// beat width; every byte holds four 1s (so DBI never inverts one); lines differ.
    #[test]
    fn mode14_flips_every_beat_and_dodges_dbi() {
        let key = mode12_key(3, 1, 0x5DEECE66D, 0xB);
        for beat in [BusBeat::Bits16, BusBeat::Bits32, BusBeat::Bits64] {
            let bits = beat.bits() as usize;
            let mask = if bits == 64 { u64::MAX } else { (1u64 << bits) - 1 };
            let mut values = std::collections::HashSet::new();
            for line in 0..4096usize {
                let words = mode14_line(0x1E98_0000_0000 + 64 * line, key, beat);
                let beats: Vec<u64> = words.iter().flat_map(|&w| (0..64 / bits).map(move |k| (w >> (k * bits)) & mask)).collect();
                assert!(beats.windows(2).all(|p| p[1] == !p[0] & mask), "{beat:?} line {line}: {words:x?}");
                assert!(words.iter().all(|w| w.to_le_bytes().iter().all(|b| b.count_ones() == 4)), "{beat:?} line {line}");
                values.insert(words[0]);
            }
            // 4096 lines drawn from 2^8, 2^16 and 2^32 values (16-, 32-, 64-bit beats): all 256,
            // about 3970 by the birthday bound, nearly all 4096
            let floor = match bits { 16 => 250, 32 => 3800, _ => 4090 };
            assert!(values.len() >= floor, "{beat:?}: {} distinct", values.len());
        }
        assert_eq!(BusBeat::from_smbios_type(0x22), Some(BusBeat::Bits32));
        assert_eq!(BusBeat::from_smbios_type(0x1A), Some(BusBeat::Bits64));
        assert_eq!(BusBeat::from_smbios_type(0x23), Some(BusBeat::Bits16));
        assert_eq!(BusBeat::from_smbios_type(0), None);
    }

    #[test]
    fn test_cache_line_shift_values() {
        assert_eq!(cache_line_shift(64), 3);  // 64/8 = 8 elements = 2^3
        assert_eq!(cache_line_shift(128), 4); // 128/8 = 16 elements = 2^4
        assert_eq!(cache_line_shift(32), 2);  // 32/8 = 4 elements = 2^2
    }

}
