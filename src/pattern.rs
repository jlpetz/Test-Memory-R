//! The native patterns (pattern spec v2, `doc/pattern_spec.md`): what a SimpleTest step writes.
//!
//! **Families.** Each family answers one way memory fails:
//! - `ident`: every word of the run a different value (a cheap bijection of its id), so a read
//!   or write that lands at the wrong address shows. Needs no bus width.
//! - `random`: strongly mixed words (splitmix64's finalizer, also a bijection), so every data
//!   line sees every short bit sequence, which DDR5's equalisers respond to. Needs no bus width.
//! - `switch`: each beat of a line's burst the inverse of the one before, at the memory's
//!   transfer width, so every data line toggles on every beat (mode 30 found the 8600G's fault).
//! - `lines`: each line an arithmetic chain `seed + j * step` hashed from the line (TM5 mode 2's
//!   idea, without the chain that collapses).
//! - `linepair`: aligned pairs of lines, the second the first's complement (TM5 mode 1's idea).
//!
//! **Programs and pairs.** A step runs a program, an ordered list of families. Each family is
//! written as a pair of rounds: its image P, then the complement ~P under the same key, so every
//! bit is written and verified as both 1 and 0. Pairs walk the program across the step's write
//! rounds and the suite's cycles (`Program::phase`).
//!
//! **Keys.** A pair's key comes from the run seed, the suite cycle, the step and the pair, never
//! the thread or the chunk. A word's value is a function of its id (the thread and its offset in
//! the thread's span), the family, the key and the polarity only, so chunks may overlap, any walk
//! order works, every thread's words differ, and `seed=` replays a run's bytes on the same layout.

use crate::pattern_gen::BusBeat;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

/// The pattern spec this module implements, recorded in results. Version 1 is the numbered modes
/// (`pattern_gen::MODES`), which TMR still runs when a config names one.
pub const VERSION: u32 = 2;

/// Words in a 64 B cache line.
pub const LINE_WORDS: usize = 8;

/// The golden-ratio multiplier: odd, so `x * G` is a bijection on u64.
const G: u64 = 0x9E37_79B9_7F4A_7C15;

/// splitmix64's finalizer (TMR mode 13). Each step is invertible, so it is a bijection on u64.
#[inline(always)]
pub const fn fmix(mut z: u64) -> u64 {
    z ^= z >> 30;
    z = z.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z ^= z >> 27;
    z = z.wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// Two values into one key; not symmetric, so `mix(a, b) != mix(b, a)` in general.
#[inline]
const fn mix(a: u64, b: u64) -> u64 {
    fmix(a ^ fmix(b.wrapping_add(G)))
}

/// The id of line 0 of thread `thread_id`'s span: word `idx` of the span is word id
/// `thread_id << 40 | idx`, so line `n` of it is line id `line_base(thread_id) + n`. A span holds
/// fewer than 2^40 words (8 TiB).
#[inline(always)]
pub fn line_base(thread_id: usize) -> u64 {
    (thread_id as u64) << 37
}

/// The id of word 0 of thread `thread_id`'s span, `thread_id << 40`: word `idx` of the span is
/// word id `word_base(thread_id) | idx`. The other tests add it to their values, so no word of one
/// thread holds what a word of another does (TODO 97).
#[inline(always)]
pub fn word_base(thread_id: usize) -> u64 {
    (thread_id as u64) << 40
}

// ─── Families ────────────────────────────────────────────────────────────────────────────────────

/// A native pattern family (see the module doc).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Family {
    Ident,
    Random,
    Switch,
    Lines,
    Linepair,
    Quiet,
    Header,
}

impl Family {
    pub const ALL: [Family; 7] = [Family::Ident, Family::Random, Family::Switch, Family::Lines, Family::Linepair,
                                  Family::Quiet, Family::Header];

    pub fn name(self) -> &'static str {
        match self {
            Family::Ident => "ident",
            Family::Random => "random",
            Family::Switch => "switch",
            Family::Lines => "lines",
            Family::Linepair => "linepair",
            Family::Quiet => "quiet",
            Family::Header => "header",
        }
    }

    /// Whether its lines depend on the transfer width: the switching families'.
    pub fn uses_beat(self) -> bool {
        matches!(self, Family::Switch | Family::Quiet | Family::Header)
    }

    pub fn parse(s: &str) -> Option<Self> {
        Family::ALL.into_iter().find(|f| f.name().eq_ignore_ascii_case(s.trim()))
    }

    /// Its number, in the groups of the numbered modes (`pattern_gen::MODES`): 1x identities, 2x
    /// in-line arithmetic, 3x switching. A `pattern_mode` of it runs the family alone.
    pub const fn number(self) -> u32 {
        match self {
            Family::Ident => 15,
            Family::Random => 16,
            Family::Linepair => 17,
            Family::Lines => 21,
            Family::Switch => 31,
            Family::Quiet => 32,
            Family::Header => 33,
        }
    }

    pub fn from_number(n: u32) -> Option<Self> {
        Family::ALL.into_iter().find(|f| f.number() == n)
    }

    /// Folded into the pair key, so two families never share one.
    fn code(self) -> u64 {
        match self {
            Family::Ident => 0x1D,
            Family::Random => 0x2A,
            Family::Switch => 0x3C,
            Family::Lines => 0x4B,
            Family::Linepair => 0x5E,
            Family::Quiet => 0x69,
            Family::Header => 0x78,
        }
    }
}

/// Whether `pattern_mode` takes `n`: a numbered mode or a native family's number.
pub fn is_pattern_mode(n: u32) -> bool {
    crate::pattern_gen::MODES.contains(&n) || Family::from_number(n).is_some()
}

/// What `pattern_mode` takes, for an error that names a number it doesn't.
pub fn pattern_modes() -> String {
    let (tm5, tmr): (Vec<u32>, Vec<u32>) = crate::pattern_gen::MODES.iter().partition(|&&m| m < 10);
    let list = |modes: &[u32]| modes.iter().map(u32::to_string).collect::<Vec<_>>().join(", ");
    let mut native: Vec<Family> = Family::ALL.to_vec();
    native.sort_by_key(|f| f.number());
    let native: Vec<String> = native.iter().map(|f| format!("{} {}", f.number(), f.name())).collect();
    format!("{} (TM5's), {} (TMR's), or a native family's: {}", list(&tm5), list(&tmr), native.join(", "))
}

/// For an error naming a refused number: what 12 and 14 became, else nothing.
pub fn renumbered(n: u32) -> &'static str {
    match n {
        12 => " (TMR's mode 12 is 20 since 2026-10-16)",
        14 => " (TMR's mode 14 is 30 since 2026-10-16)",
        _ => "",
    }
}

/// `ident` line `lid`: word id `i` holds `x ^ (x >> 32)` with `x = i * G + key`, a bijection of
/// `i`, so every word differs. Cheap (a Weyl step and a shift per word), but each upper bit of a
/// word is a bit of an arithmetic progression, which shows at most 10 of the 32 five-bit
/// histories on a 64-bit beat: `random` covers those.
#[inline(always)]
pub fn ident_line(lid: u64, key: u64) -> [u64; LINE_WORDS] {
    let x0 = (lid << 3).wrapping_mul(G).wrapping_add(key);
    std::array::from_fn(|j| {
        let x = x0.wrapping_add((j as u64).wrapping_mul(G));
        x ^ (x >> 32)
    })
}

/// `ident`'s word of word id `id` under `key`, for the tests that write a word at a time.
#[inline(always)]
pub fn ident_word(id: u64, key: u64) -> u64 {
    let x = id.wrapping_mul(G).wrapping_add(key);
    x ^ (x >> 32)
}

/// `random` line `lid`: word id `i` holds `fmix(i + key)`, a bijection of `i` (TMR mode 13 keyed
/// by the word's id), strongly mixed: every bit position sees all short bit sequences.
#[inline(always)]
pub fn random_line(lid: u64, key: u64) -> [u64; LINE_WORDS] {
    let i0 = (lid << 3).wrapping_add(key);
    std::array::from_fn(|j| fmix(i0.wrapping_add(j as u64)))
}

/// `switch` line `lid` at `beat` bits a transfer: each transfer of the line's bursts the inverse
/// of the one before (TMR mode 30 keyed by the line's id, `pattern_gen::bus_flip_line`; LPDDR6's
/// 12 and DDR6's 24 are models). 32 bits of `fmix(lid + key)` go one nibble to a byte, each byte's
/// high nibble the inverse of its low, so every byte holds four 1s and a count-based DBI never
/// undoes a flip at 16, 32 or 64 bits. The first transfer fixes the line, so it carries 8 to 32
/// bits of the hash.
#[inline(always)]
pub fn switch_line(lid: u64, key: u64, beat: BusBeat) -> [u64; LINE_WORDS] {
    crate::pattern_gen::bus_flip_line(crate::pattern_gen::balanced_bytes(fmix(lid.wrapping_add(key))), beat)
}

/// The pin `quiet` holds on its line, from the high half of the hash `switch` takes the low half
/// of: uniform over the transfer's `beat.bits()` pins, so every pin gets turns.
#[inline(always)]
pub fn quiet_pin(hash: u64, beat: BusBeat) -> u32 {
    (((hash >> 32) * u64::from(beat.bits())) >> 32) as u32
}

/// `quiet` line `lid`: `switch`'s line with one pin held. The line's quiet pin (`quiet_pin`) is 1
/// in every transfer, 0 under ~P, while every other pin toggles on every beat, so the held line
/// sits among switching neighbours, the crosstalk case `switch` (all toggling) doesn't make. Its
/// byte keeps four or five 1s a transfer in P (DDR4's DBI, which counts 0s, leaves it alone) and
/// four or three in ~P (LPDDR's, which counts 1s, leaves that alone).
#[inline(always)]
pub fn quiet_line(lid: u64, key: u64, beat: BusBeat) -> [u64; LINE_WORDS] {
    let hash = fmix(lid.wrapping_add(key));
    let held = crate::pattern_gen::pin_mask(quiet_pin(hash, beat), beat);
    let line = crate::pattern_gen::bus_flip_line(crate::pattern_gen::balanced_bytes(hash), beat);
    std::array::from_fn(|j| line[j] | held[j])
}

/// `header` line `lid`: `switch`'s line with an identity word heading each 32 B half: words 0 and
/// 4 are `ident`'s words 0 and 4 of the line under the same key. So every half-line of the run
/// differs (a misplaced half shows, as in `switch` it may not), and six words of eight switch.
#[inline(always)]
pub fn header_line(lid: u64, key: u64, beat: BusBeat) -> [u64; LINE_WORDS] {
    let (mut line, ident) = (switch_line(lid, key, beat), ident_line(lid, key));
    (line[0], line[4]) = (ident[0], ident[4]);
    line
}

/// `lines` line `lid`: `seed + j * step`, both hashed from the line's id (TMR mode 20 keyed by the
/// line's id). The step is odd, so a line's 8 words all differ.
#[inline(always)]
pub fn lines_line(lid: u64, key: u64) -> [u64; LINE_WORDS] {
    let seed = fmix(lid.wrapping_add(key));
    let step = (seed ^ key).rotate_left(29).wrapping_mul(G) | 1;
    crate::pattern_gen::line_words(seed, step)
}

/// `linepair` line `lid`: lines `2n` and `2n + 1` are a pair, the first `ident`'s line `2n`, the
/// second its complement. So each aligned pair of neighbours is opposite in every bit (TM5 mode 1
/// inverted every line); a line and the next pair's first are not.
#[inline(always)]
pub fn linepair_line(lid: u64, key: u64) -> [u64; LINE_WORDS] {
    let invert = 0u64.wrapping_sub(lid & 1);
    ident_line(lid & !1, key).map(|w| w ^ invert)
}


// ─── The line functions at a SIMD width ──────────────────────────────────────────────────────────
//
// The same words as the functions above, computed as `V` vectors of `N` lanes (`N * V` = 8), lane
// `l` of vector `v` the line's word `v * N + l`. The SIMD kernels use these: computed from the
// scalar words instead, LLVM's SLP vectoriser did the 128-bit variant's math and stores at 256 bits
// on the AVX2 baseline, so the variant wasn't the width it is named for.

type V64<const N: usize> = std::simd::Simd<u64, N>;

/// The words' places in the line, as vectors.
#[inline(always)]
fn word_lanes<const N: usize, const V: usize>() -> [V64<N>; V] {
    std::array::from_fn(|v| V64::from_array(std::array::from_fn(|l| (v * N + l) as u64)))
}

/// `fmix` on every lane.
#[inline(always)]
pub fn fmix_v<const N: usize>(mut z: V64<N>) -> V64<N> {
    z ^= z >> V64::splat(30);
    z *= V64::splat(0xbf58_476d_1ce4_e5b9);
    z ^= z >> V64::splat(27);
    z *= V64::splat(0x94d0_49bb_1331_11eb);
    z ^ (z >> V64::splat(31))
}

/// `ident_line` as vectors.
#[inline(always)]
pub fn ident_line_v<const N: usize, const V: usize>(lid: u64, key: u64) -> [V64<N>; V] {
    let x0 = V64::splat((lid << 3).wrapping_mul(G).wrapping_add(key));
    let lanes = word_lanes::<N, V>();
    std::array::from_fn(|v| {
        let x = x0 + lanes[v] * V64::splat(G);
        x ^ (x >> V64::splat(32))
    })
}

/// `random_line` as vectors.
#[inline(always)]
pub fn random_line_v<const N: usize, const V: usize>(lid: u64, key: u64) -> [V64<N>; V] {
    let i0 = V64::splat((lid << 3).wrapping_add(key));
    let lanes = word_lanes::<N, V>();
    std::array::from_fn(|v| fmix_v(i0 + lanes[v]))
}

/// `switch_line` as vectors: the line's first word (and, at 64 bits, its inverse in odd lanes); at
/// 12 and 24 bits, whose words differ, its words in their lanes.
#[inline(always)]
pub fn switch_line_v<const N: usize, const V: usize>(lid: u64, key: u64, beat: BusBeat) -> [V64<N>; V] {
    let w = switch_line(lid, key, beat);
    match beat {
        // Even words `a`, odd ones `!a`: `a` XOR a mask that is all ones in the odd words
        BusBeat::Bits64 => std::array::from_fn(|v| {
            V64::splat(w[0]) ^ V64::from_array(std::array::from_fn(|l| if (v * N + l) % 2 == 1 { !0 } else { 0 }))
        }),
        BusBeat::Bits32 | BusBeat::Bits16 => std::array::from_fn(|_| V64::splat(w[0])),
        BusBeat::Bits12 | BusBeat::Bits24 => std::array::from_fn(|v| V64::from_array(std::array::from_fn(|l| w[v * N + l]))),
    }
}

/// `quiet_line` as vectors: `switch`'s with the held pin's bits set. At 12 and 24 bits, whose words
/// differ, from the scalar words, as `switch`'s at 12.
#[inline(always)]
pub fn quiet_line_v<const N: usize, const V: usize>(lid: u64, key: u64, beat: BusBeat) -> [V64<N>; V] {
    match beat {
        BusBeat::Bits64 | BusBeat::Bits32 | BusBeat::Bits16 => {
            // Every word holds the same bits of the held pin at these widths
            let hash = fmix(lid.wrapping_add(key));
            let held = V64::splat(crate::pattern_gen::pin_mask(quiet_pin(hash, beat), beat)[0]);
            switch_line_v::<N, V>(lid, key, beat).map(|x| x | held)
        }
        BusBeat::Bits12 | BusBeat::Bits24 => {
            let w = quiet_line(lid, key, beat);
            std::array::from_fn(|v| V64::from_array(std::array::from_fn(|l| w[v * N + l])))
        }
    }
}

/// `header_line` as vectors: `switch`'s with `ident`'s words 0 and 4 in their lanes.
#[inline(always)]
pub fn header_line_v<const N: usize, const V: usize>(lid: u64, key: u64, beat: BusBeat) -> [V64<N>; V] {
    let mut line = switch_line_v::<N, V>(lid, key, beat);
    let x0 = (lid << 3).wrapping_mul(G).wrapping_add(key);
    for j in [0, 4] {
        let x = x0.wrapping_add((j as u64).wrapping_mul(G));
        line[j / N][j % N] = x ^ (x >> 32);
    }
    line
}

/// `lines_line` as vectors.
#[inline(always)]
pub fn lines_line_v<const N: usize, const V: usize>(lid: u64, key: u64) -> [V64<N>; V] {
    let seed = fmix(lid.wrapping_add(key));
    let step = (seed ^ key).rotate_left(29).wrapping_mul(G) | 1;
    let lanes = word_lanes::<N, V>();
    std::array::from_fn(|v| V64::splat(seed) + V64::splat(step) * lanes[v])
}

/// `linepair_line` as vectors.
#[inline(always)]
pub fn linepair_line_v<const N: usize, const V: usize>(lid: u64, key: u64) -> [V64<N>; V] {
    let invert = V64::splat(0u64.wrapping_sub(lid & 1));
    ident_line_v::<N, V>(lid & !1, key).map(|x| x ^ invert)
}

/// As `with_phase_line!`, at `$n` lanes: `$line` maps a line id to its `8 / $n` vectors.
macro_rules! with_phase_vectors {
    ($n:tt, $phase:expr, |$line:ident| $body:block) => {{
        const V: usize = $crate::pattern::LINE_WORDS / $n;
        let phase: $crate::pattern::Phase = $phase;
        let key = phase.key;
        let mask = std::simd::Simd::<u64, $n>::splat(phase.mask);
        use $crate::pattern::Family;
        use $crate::pattern_gen::BusBeat;
        match (phase.family, phase.beat) {
            (Family::Ident, _) => {
                let $line = |lid: u64| $crate::pattern::ident_line_v::<$n, V>(lid, key).map(|x| x ^ mask);
                $body
            }
            (Family::Random, _) => {
                let $line = |lid: u64| $crate::pattern::random_line_v::<$n, V>(lid, key).map(|x| x ^ mask);
                $body
            }
            (Family::Lines, _) => {
                let $line = |lid: u64| $crate::pattern::lines_line_v::<$n, V>(lid, key).map(|x| x ^ mask);
                $body
            }
            (Family::Linepair, _) => {
                let $line = |lid: u64| $crate::pattern::linepair_line_v::<$n, V>(lid, key).map(|x| x ^ mask);
                $body
            }
            (Family::Switch, BusBeat::Bits12) => {
                let $line = |lid: u64| $crate::pattern::switch_line_v::<$n, V>(lid, key, BusBeat::Bits12).map(|x| x ^ mask);
                $body
            }
            (Family::Switch, BusBeat::Bits16) => {
                let $line = |lid: u64| $crate::pattern::switch_line_v::<$n, V>(lid, key, BusBeat::Bits16).map(|x| x ^ mask);
                $body
            }
            (Family::Switch, BusBeat::Bits24) => {
                let $line = |lid: u64| {
                    let hash = $crate::pattern::fmix(lid.wrapping_add(key));
                    $crate::pattern_switch24::vectors!($n, hash).map(|x| x ^ mask)
                };
                $body
            }
            (Family::Switch, BusBeat::Bits32) => {
                let $line = |lid: u64| $crate::pattern::switch_line_v::<$n, V>(lid, key, BusBeat::Bits32).map(|x| x ^ mask);
                $body
            }
            (Family::Switch, BusBeat::Bits64) => {
                let $line = |lid: u64| $crate::pattern::switch_line_v::<$n, V>(lid, key, BusBeat::Bits64).map(|x| x ^ mask);
                $body
            }
            (Family::Quiet, BusBeat::Bits12) => {
                let $line = |lid: u64| $crate::pattern::quiet_line_v::<$n, V>(lid, key, BusBeat::Bits12).map(|x| x ^ mask);
                $body
            }
            (Family::Quiet, BusBeat::Bits16) => {
                let $line = |lid: u64| $crate::pattern::quiet_line_v::<$n, V>(lid, key, BusBeat::Bits16).map(|x| x ^ mask);
                $body
            }
            (Family::Quiet, BusBeat::Bits24) => {
                let $line = |lid: u64| $crate::pattern::quiet_line_v::<$n, V>(lid, key, BusBeat::Bits24).map(|x| x ^ mask);
                $body
            }
            (Family::Quiet, BusBeat::Bits32) => {
                let $line = |lid: u64| $crate::pattern::quiet_line_v::<$n, V>(lid, key, BusBeat::Bits32).map(|x| x ^ mask);
                $body
            }
            (Family::Quiet, BusBeat::Bits64) => {
                let $line = |lid: u64| $crate::pattern::quiet_line_v::<$n, V>(lid, key, BusBeat::Bits64).map(|x| x ^ mask);
                $body
            }
            (Family::Header, BusBeat::Bits12) => {
                let $line = |lid: u64| $crate::pattern::header_line_v::<$n, V>(lid, key, BusBeat::Bits12).map(|x| x ^ mask);
                $body
            }
            (Family::Header, BusBeat::Bits16) => {
                let $line = |lid: u64| $crate::pattern::header_line_v::<$n, V>(lid, key, BusBeat::Bits16).map(|x| x ^ mask);
                $body
            }
            (Family::Header, BusBeat::Bits24) => {
                let $line = |lid: u64| $crate::pattern::header_line_v::<$n, V>(lid, key, BusBeat::Bits24).map(|x| x ^ mask);
                $body
            }
            (Family::Header, BusBeat::Bits32) => {
                let $line = |lid: u64| $crate::pattern::header_line_v::<$n, V>(lid, key, BusBeat::Bits32).map(|x| x ^ mask);
                $body
            }
            (Family::Header, BusBeat::Bits64) => {
                let $line = |lid: u64| $crate::pattern::header_line_v::<$n, V>(lid, key, BusBeat::Bits64).map(|x| x ^ mask);
                $body
            }
        }
    }};
}
pub(crate) use with_phase_vectors;

// ─── Programs ────────────────────────────────────────────────────────────────────────────────────

/// The most families a program lists.
pub const MAX_PROGRAM: usize = 8;

/// A step's program: its families in order, each run as a P/~P pair (see the module doc).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Program {
    families: [Family; MAX_PROGRAM],
    len: u8,
}

impl Program {
    pub fn new(families: &[Family]) -> Result<Self, String> {
        if families.is_empty() || families.len() > MAX_PROGRAM {
            return Err(format!("a program lists 1 to {MAX_PROGRAM} families, not {}", families.len()));
        }
        let mut list = [Family::Ident; MAX_PROGRAM];
        list[..families.len()].copy_from_slice(families);
        Ok(Program { families: list, len: families.len() as u8 })
    }

    /// From family names, e.g. `["ident", "switch"]`.
    pub fn parse<S: AsRef<str>>(names: &[S]) -> Result<Self, String> {
        let families = names.iter()
            .map(|n| Family::parse(n.as_ref()).ok_or_else(|| format!(
                "'{}' is not a pattern family; valid: {}", n.as_ref(),
                Family::ALL.map(Family::name).join(", "))))
            .collect::<Result<Vec<_>, _>>()?;
        Program::new(&families)
    }

    pub fn families(&self) -> &[Family] {
        &self.families[..self.len as usize]
    }

    /// `ident+switch+random+switch`
    pub fn name(&self) -> String {
        self.families().iter().map(|f| f.name()).collect::<Vec<_>>().join("+")
    }

    pub fn names(&self) -> Vec<String> {
        self.families().iter().map(|f| f.name().to_string()).collect()
    }

    /// One family alone, as the Bench pairs run it.
    pub fn single(family: Family) -> Self {
        Program { families: [family; MAX_PROGRAM], len: 1 }
    }

    /// A SimpleTest's program when its config names none: `random` and `switch`, so every cycle
    /// with TM5's four write rounds a chunk runs a pair of each. `random` is an identity too (every
    /// word differs) and gives every data line all short bit histories; `ident`, cheaper and more
    /// regular, stays available by name and in a TM5 mode-0 import.
    pub fn default_simple() -> Self {
        Program::new(&[Family::Random, Family::Switch]).expect("a valid program")
    }

    /// What a TM5 `.cfg`'s SimpleTest `Pattern Mode` imports as: its intent's family and
    /// switching, then mixed data and switching. `None` for a mode TM5 doesn't have.
    pub fn from_tm5_mode(mode: u32) -> Option<Self> {
        let intent = match mode {
            0 => Family::Ident,
            1 => Family::Linepair,
            2 => Family::Lines,
            _ => return None,
        };
        Some(Program::new(&[intent, Family::Switch, Family::Random, Family::Switch]).expect("a valid program"))
    }

    /// The phase of a step's write round `g` (from 0, counted across the step's own cycles; a step
    /// that writes only before its verifies stays at 0), as `schedule` pairs them:
    /// - `Rounds`: rounds `2n` and `2n + 1` are a pair, P then ~P, one key. A chunk's rounds run
    ///   back to back, and `wrc` is even (`pair_native_rounds` rounds it up), so every chunk ends
    ///   each cycle on whole pairs. The pair's place in the program advances `wrc / 2` a suite
    ///   cycle, so a TM5 step (one cycle of its own, four rounds) runs the program's next two
    ///   pairs each suite cycle.
    /// - `OncePerRun`: P in odd suite cycles, ~P in even ones, one key over the two, as the seal
    ///   does; the place advances one pair each two suite cycles.
    ///
    /// An unknown transfer width alternates the 64- and 32-bit `switch` profiles from one `switch`
    /// pair to the next, whatever the program's shape.
    pub fn phase(&self, epoch: &StepEpoch, wrc: u32, g: u64, schedule: Schedule, beat: Option<BusBeat>) -> Phase {
        let suite_cycle = epoch.suite_cycle.max(1);
        let (pair, inverse, epoch) = match schedule {
            Schedule::Rounds => (g / 2 + u64::from(suite_cycle - 1) * u64::from(wrc.max(1)).div_ceil(2), g % 2 == 1, *epoch),
            Schedule::OncePerRun => (u64::from((suite_cycle - 1) / 2), suite_cycle.is_multiple_of(2),
                                     StepEpoch { suite_cycle: suite_cycle.div_ceil(2), ..*epoch }),
        };
        let len = u64::from(self.len);
        let at = (pair % len) as usize;
        let family = self.families()[at];
        // This switching pair's place among all of them: the program's switching families per lap,
        // and the ones before it in this lap
        let switches = |list: &[Family]| list.iter().filter(|f| f.uses_beat()).count() as u64;
        let nth_switch = pair / len * switches(self.families()) + switches(&self.families()[..at]);
        let beat = beat.unwrap_or(if nth_switch.is_multiple_of(2) { BusBeat::Bits64 } else { BusBeat::Bits32 });
        Phase {
            family,
            key: mix(mix(epoch.key(), pair), family.code()),
            mask: if inverse { !0 } else { 0 },
            beat,
        }
    }
}

/// How a step's write rounds take the program's pairs (`Program::phase`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Schedule {
    /// A step that writes first: a pair every two rounds
    Rounds,
    /// A step that verifies its fill (`test_reps` 0): one image a suite cycle (each dispatch of
    /// the step), so a pair spans two
    OncePerRun,
}

/// Where a step runs in the suite, set per dispatch by the runner. All zero (the default) outside
/// a run (unit tests) and for the Bench pairs, which pin it so their images repeat.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StepEpoch {
    pub seed: u64,
    /// The suite cycle, from 1; 0 outside a run.
    pub suite_cycle: u32,
    /// The step's place in the cycle, from 0: a test listed twice is two steps.
    pub step: u32,
}

impl StepEpoch {
    /// The step's key, from which its pairs' keys come.
    pub fn key(&self) -> u64 {
        mix(mix(mix(self.seed, u64::from(self.suite_cycle)), u64::from(self.step)), 0x5EED)
    }
}

/// One write round's pattern: a family, its pair's key, the polarity (0 for P, all ones for ~P)
/// and the `switch` profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Phase {
    pub family: Family,
    pub key: u64,
    pub mask: u64,
    pub beat: BusBeat,
}

impl Phase {
    /// The 8 words of line `lid`: one match a line, for a cold path or a test. Hot loops match once
    /// outside the loop with `with_phase_line!`.
    pub fn line(&self, lid: u64) -> [u64; LINE_WORDS] {
        let words = match self.family {
            Family::Ident => ident_line(lid, self.key),
            Family::Random => random_line(lid, self.key),
            Family::Switch => switch_line(lid, self.key, self.beat),
            Family::Lines => lines_line(lid, self.key),
            Family::Linepair => linepair_line(lid, self.key),
            Family::Quiet => quiet_line(lid, self.key, self.beat),
            Family::Header => header_line(lid, self.key, self.beat),
        };
        words.map(|w| w ^ self.mask)
    }

    /// `ident-P`, `switch32-~P`
    pub fn label(&self) -> String {
        let beat = if self.family.uses_beat() { self.beat.bits().to_string() } else { String::new() };
        format!("{}{}-{}", self.family.name(), beat, if self.mask == 0 { "P" } else { "~P" })
    }
}

/// Runs `$body` with `$line` the phase's line function (a line id to its 8 words, the polarity
/// applied), matched once outside the body, so a loop in it is compiled per family and beat with
/// no branch on either.
macro_rules! with_phase_line {
    ($phase:expr, |$line:ident| $body:block) => {{
        let phase: $crate::pattern::Phase = $phase;
        let (key, mask) = (phase.key, phase.mask);
        use $crate::pattern::{Family, LINE_WORDS};
        use $crate::pattern_gen::BusBeat;
        #[inline(always)]
        fn xor(words: [u64; LINE_WORDS], mask: u64) -> [u64; LINE_WORDS] {
            words.map(|w| w ^ mask)
        }
        match (phase.family, phase.beat) {
            (Family::Ident, _) => {
                let $line = |lid: u64| xor($crate::pattern::ident_line(lid, key), mask);
                $body
            }
            (Family::Random, _) => {
                let $line = |lid: u64| xor($crate::pattern::random_line(lid, key), mask);
                $body
            }
            (Family::Lines, _) => {
                let $line = |lid: u64| xor($crate::pattern::lines_line(lid, key), mask);
                $body
            }
            (Family::Linepair, _) => {
                let $line = |lid: u64| xor($crate::pattern::linepair_line(lid, key), mask);
                $body
            }
            (Family::Switch, BusBeat::Bits12) => {
                let $line = |lid: u64| xor($crate::pattern::switch_line(lid, key, BusBeat::Bits12), mask);
                $body
            }
            (Family::Switch, BusBeat::Bits16) => {
                let $line = |lid: u64| xor($crate::pattern::switch_line(lid, key, BusBeat::Bits16), mask);
                $body
            }
            (Family::Switch, BusBeat::Bits24) => {
                let $line = |lid: u64| xor($crate::pattern::switch_line(lid, key, BusBeat::Bits24), mask);
                $body
            }
            (Family::Switch, BusBeat::Bits32) => {
                let $line = |lid: u64| xor($crate::pattern::switch_line(lid, key, BusBeat::Bits32), mask);
                $body
            }
            (Family::Switch, BusBeat::Bits64) => {
                let $line = |lid: u64| xor($crate::pattern::switch_line(lid, key, BusBeat::Bits64), mask);
                $body
            }
            (Family::Quiet, BusBeat::Bits12) => {
                let $line = |lid: u64| xor($crate::pattern::quiet_line(lid, key, BusBeat::Bits12), mask);
                $body
            }
            (Family::Quiet, BusBeat::Bits16) => {
                let $line = |lid: u64| xor($crate::pattern::quiet_line(lid, key, BusBeat::Bits16), mask);
                $body
            }
            (Family::Quiet, BusBeat::Bits24) => {
                let $line = |lid: u64| xor($crate::pattern::quiet_line(lid, key, BusBeat::Bits24), mask);
                $body
            }
            (Family::Quiet, BusBeat::Bits32) => {
                let $line = |lid: u64| xor($crate::pattern::quiet_line(lid, key, BusBeat::Bits32), mask);
                $body
            }
            (Family::Quiet, BusBeat::Bits64) => {
                let $line = |lid: u64| xor($crate::pattern::quiet_line(lid, key, BusBeat::Bits64), mask);
                $body
            }
            (Family::Header, BusBeat::Bits12) => {
                let $line = |lid: u64| xor($crate::pattern::header_line(lid, key, BusBeat::Bits12), mask);
                $body
            }
            (Family::Header, BusBeat::Bits16) => {
                let $line = |lid: u64| xor($crate::pattern::header_line(lid, key, BusBeat::Bits16), mask);
                $body
            }
            (Family::Header, BusBeat::Bits24) => {
                let $line = |lid: u64| xor($crate::pattern::header_line(lid, key, BusBeat::Bits24), mask);
                $body
            }
            (Family::Header, BusBeat::Bits32) => {
                let $line = |lid: u64| xor($crate::pattern::header_line(lid, key, BusBeat::Bits32), mask);
                $body
            }
            (Family::Header, BusBeat::Bits64) => {
                let $line = |lid: u64| xor($crate::pattern::header_line(lid, key, BusBeat::Bits64), mask);
                $body
            }
        }
    }};
}
pub(crate) use with_phase_line;

// ─── Run-wide settings ───────────────────────────────────────────────────────────────────────────

static RUN_SEED: OnceLock<u64> = OnceLock::new();
static SEED_GIVEN: AtomicBool = AtomicBool::new(false);

/// `seed=`: the run's seed, so a run's bytes can be replayed. Call before the run starts.
pub fn set_run_seed(seed: u64) {
    if RUN_SEED.set(seed).is_ok() {
        SEED_GIVEN.store(true, Ordering::Relaxed);
    }
}

/// The run's seed: `seed=`'s, or one from the clock, fixed at the first call.
pub fn run_seed() -> u64 {
    *RUN_SEED.get_or_init(|| {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64);
        fmix(nanos ^ (u64::from(std::process::id()) << 32))
    })
}

/// Whether `seed=` gave the seed.
pub fn seed_given() -> bool {
    SEED_GIVEN.load(Ordering::Relaxed)
}

/// Parses `seed=`: decimal, or hex with `0x`.
pub fn parse_seed(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let parsed = match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(hex) => u64::from_str_radix(hex, 16),
        None => s.parse::<u64>(),
    };
    parsed.map_err(|_| format!("seed '{s}': a 64-bit number, decimal or 0x hex"))
}

static RESCAN: AtomicBool = AtomicBool::new(true);

/// `rescan=`: whether a verify that finds a difference rescans the region to name each bad word
/// and reread it (the default), or only counts the failed check (`off`, for a run with many errors).
pub fn set_rescan(on: bool) {
    RESCAN.store(on, Ordering::Relaxed);
}

pub fn rescan_enabled() -> bool {
    RESCAN.load(Ordering::Relaxed)
}

static LEGACY_IMPORT: AtomicBool = AtomicBool::new(false);

/// `patterns=legacy`: a TM5 `.cfg` imports its SimpleTest modes as TMR's mode numbers (pattern
/// spec 1) instead of native programs, to compare the two.
pub fn set_legacy_import(on: bool) {
    LEGACY_IMPORT.store(on, Ordering::Relaxed);
}

pub fn legacy_import() -> bool {
    LEGACY_IMPORT.load(Ordering::Relaxed)
}

static BEAT_OVERRIDE: OnceLock<BusBeat> = OnceLock::new();

/// `beat=`: the transfer width `switch` (and mode 30) flips at, instead of SMBIOS's.
pub fn set_beat_override(beat: BusBeat) {
    let _ = BEAT_OVERRIDE.set(beat);
}

/// Parses `beat=`: `auto`, a width (12, 16, 24, 32, 64) or a memory type. `Ok(None)` is auto.
pub fn parse_beat(s: &str) -> Result<Option<BusBeat>, String> {
    match s.trim().to_ascii_lowercase().as_str() {
        "auto" => Ok(None),
        "12" | "lpddr6" => Ok(Some(BusBeat::Bits12)),
        "16" | "lpddr4" | "lpddr5" => Ok(Some(BusBeat::Bits16)),
        "24" | "ddr6" => Ok(Some(BusBeat::Bits24)),
        "32" | "ddr5" | "lpddr3" => Ok(Some(BusBeat::Bits32)),
        "64" | "ddr3" | "ddr4" => Ok(Some(BusBeat::Bits64)),
        other => Err(format!("beat '{other}': use auto, a width (12, 16, 24, 32, 64) or a memory type (ddr3, ddr4, \
                              ddr5, ddr6, lpddr3, lpddr4, lpddr5, lpddr6)")),
    }
}

#[cfg(test)]
thread_local! {
    /// A test's transfer width on its own thread, in place of the run's (`TestBeat`)
    static TEST_BEAT: std::cell::Cell<Option<Option<BusBeat>>> = const { std::cell::Cell::new(None) };
}

/// While it lives, `transfer_beat` on this thread gives `beat` (a test's): the run's is resolved
/// once a process, so tests can't set it. Not for mode 30, whose width `bus_beat` keeps for the
/// process at its first call.
#[cfg(test)]
pub(crate) struct TestBeat {
    before: Option<Option<BusBeat>>,
}

#[cfg(test)]
impl TestBeat {
    #[must_use = "the width holds only while the guard lives"]
    pub(crate) fn set(beat: Option<BusBeat>) -> Self {
        TestBeat { before: TEST_BEAT.with(|b| b.replace(Some(beat))) }
    }
}

#[cfg(test)]
impl Drop for TestBeat {
    fn drop(&mut self) {
        let before = self.before;
        TEST_BEAT.with(|b| b.set(before));
    }
}

/// What a modelled width rests on, for the log and the results (`doc/pattern_spec.md`).
fn model_note(beat: BusBeat) -> &'static str {
    match beat {
        BusBeat::Bits12 => "a model of LPDDR6: TMR hasn't verified its packet map or how a controller places a line in it, \
                            so the payload is taken to come first",
        BusBeat::Bits24 => "a model of DDR6 as drafted: TMR has verified no DDR6 layout, so 64 B bursts in transfer order are assumed",
        _ => "",
    }
}

/// The transfer width and where it came from, resolved and logged once: `beat=`, else the DIMMs'
/// SMBIOS memory type, else unknown (`None`), when `switch` alternates its 64- and 32-bit profiles.
pub fn transfer_beat() -> (Option<BusBeat>, &'static str) {
    #[cfg(test)]
    if let Some(beat) = TEST_BEAT.with(|b| b.get()) {
        return (beat, "a test's");
    }
    static RESOLVED: OnceLock<(Option<BusBeat>, String)> = OnceLock::new();
    let (beat, source) = RESOLVED.get_or_init(|| {
        if let Some(&beat) = BEAT_OVERRIDE.get() {
            if beat.modelled() {
                log::info!("Transfer width: {} bits (beat=), {}", beat.bits(), model_note(beat));
                return (Some(beat), format!("beat=, {}", model_note(beat)));
            }
            log::info!("Transfer width: {} bits (beat=)", beat.bits());
            return (Some(beat), "beat=".to_string());
        }
        let types: Vec<u8> = crate::smbios::get_smbios().memory_modules.iter().map(|m| m.memory_type).collect();
        match types.iter().find_map(|&t| BusBeat::from_smbios_type(t).map(|b| (b, t))) {
            Some((beat, t)) => {
                log::info!("Transfer width: {} bits, from SMBIOS memory type {t:#04x}", beat.bits());
                (Some(beat), format!("SMBIOS memory type {t:#04x}"))
            }
            None => {
                log::warn!("Transfer width unknown: SMBIOS gives no known memory type ({types:#04x?}); the switch \
                            pattern alternates its 64- and 32-bit profiles, so on a 64- or 32-bit bus it flips every \
                            beat only half the time, and on other widths less (set beat= to the memory type: ddr4, \
                            ddr5, ddr6, lpddr4, lpddr5 or lpddr6)");
                (None, "unknown: switch alternates 64 and 32".to_string())
            }
        }
    });
    (*beat, source.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The spec's formulas written out a word at a time, separately from the line functions.
    fn reference(phase: &Phase, id: u64) -> u64 {
        let (lid, j) = (id >> 3, id & 7);
        let ident = |id: u64| {
            let x = id.wrapping_mul(G).wrapping_add(phase.key);
            x ^ (x >> 32)
        };
        let (bits, burst) = (u64::from(phase.beat.bits()), u64::from(phase.beat.burst_bits()));
        let h = fmix(lid.wrapping_add(phase.key));
        // `switch`'s word: byte b of `a` holds nibble b of the hash's low half and its inverse; bit p
        // of the line is place q = p mod burst of its burst, pin q mod bits of transfer q / bits:
        // that bit of `a`, inverted on odd transfers
        let switch = || {
            let a = (0..8).fold(0u64, |acc, b| {
                let n = (h >> (4 * b)) & 0xF;
                acc | ((n | ((!n & 0xF) << 4)) << (8 * b))
            });
            (0..64u64).fold(0u64, |acc, k| {
                let q = (j * 64 + k) % burst;
                let bit = ((a >> (q % bits)) & 1) ^ ((q / bits) & 1);
                acc | (bit << k)
            })
        };
        let w = match phase.family {
            Family::Ident => ident(id),
            Family::Random => fmix(id.wrapping_add(phase.key)),
            Family::Lines => {
                let seed = fmix(lid.wrapping_add(phase.key));
                let step = (seed ^ phase.key).rotate_left(29).wrapping_mul(G) | 1;
                seed.wrapping_add(j.wrapping_mul(step))
            }
            Family::Linepair => {
                let base = ((lid & !1) << 3) | j;
                let x = base.wrapping_mul(G).wrapping_add(phase.key);
                (x ^ (x >> 32)) ^ if lid & 1 == 1 { !0 } else { 0 }
            }
            Family::Switch => switch(),
            // `switch`'s word with every bit of pin `(h >> 32) * bits >> 32` set
            Family::Quiet => {
                let pin = ((h >> 32) * bits) >> 32;
                switch() | (0..64u64).fold(0u64, |acc, k| acc | (u64::from((j * 64 + k) % burst % bits == pin) << k))
            }
            // Words 0 and 4 `ident`'s, the rest `switch`'s
            Family::Header if j % 4 == 0 => ident(id),
            Family::Header => switch(),
        };
        w ^ phase.mask
    }

    const BEATS: [BusBeat; 5] = [BusBeat::Bits12, BusBeat::Bits16, BusBeat::Bits24, BusBeat::Bits32, BusBeat::Bits64];

    fn phases() -> Vec<Phase> {
        let mut out = Vec::new();
        for family in Family::ALL {
            for (key, mask) in [(0u64, 0u64), (0x0123_4567_89AB_CDEF, !0), (u64::MAX, 0)] {
                for beat in BEATS {
                    out.push(Phase { family, key, mask, beat });
                }
            }
        }
        out
    }

    #[test]
    fn line_functions_match_the_reference() {
        for phase in phases() {
            for lid in [0u64, 1, 2, 3, 77, line_base(5) + 12_345, u64::MAX >> 3] {
                let words = phase.line(lid);
                for (j, &w) in words.iter().enumerate() {
                    assert_eq!(w, reference(&phase, (lid << 3) | j as u64), "{} line {lid} word {j}", phase.label());
                }
            }
        }
    }


    #[test]
    fn vector_lines_match_the_scalar_ones() {
        fn check<const N: usize, const V: usize>(phase: &Phase) {
            for lid in [0u64, 1, 2, 3, 77, line_base(5) + 12_345] {
                let words = phase.line(lid);
                let vectors: Vec<[u64; N]> = with_phase_vectors_dyn::<N, V>(phase, lid).iter().map(|v| v.to_array()).collect();
                let flat: Vec<u64> = vectors.concat();
                assert_eq!(flat, words, "{} at {N} lanes, line {lid}", phase.label());
            }
        }
        for phase in phases() {
            check::<1, 8>(&phase);
            check::<2, 4>(&phase);
            check::<4, 2>(&phase);
            check::<8, 1>(&phase);
        }
    }

    /// The vector functions by family, for the test (the macro needs a literal width).
    fn with_phase_vectors_dyn<const N: usize, const V: usize>(phase: &Phase, lid: u64) -> [V64<N>; V] {
        let lines = match phase.family {
            Family::Ident => ident_line_v::<N, V>(lid, phase.key),
            Family::Random => random_line_v::<N, V>(lid, phase.key),
            Family::Switch => switch_line_v::<N, V>(lid, phase.key, phase.beat),
            Family::Lines => lines_line_v::<N, V>(lid, phase.key),
            Family::Linepair => linepair_line_v::<N, V>(lid, phase.key),
            Family::Quiet => quiet_line_v::<N, V>(lid, phase.key, phase.beat),
            Family::Header => header_line_v::<N, V>(lid, phase.key, phase.beat),
        };
        lines.map(|x| x ^ V64::splat(phase.mask))
    }

    #[test]
    fn the_vector_macro_matches_phase_line() {
        for phase in phases() {
            let got: Vec<u64> = with_phase_vectors!(4, phase, |line| { (0..32u64).flat_map(|lid| line(lid).map(|v| v.to_array()).concat()).collect() });
            let want: Vec<u64> = (0..32u64).flat_map(|lid| phase.line(lid)).collect();
            assert_eq!(got, want, "{}", phase.label());
        }
    }




    /// Known answers from an independent implementation of `doc/pattern_spec.md`: Python written
    /// from the spec's text alone, without this code (2026-10-11). A mismatch is this code and the
    /// spec disagreeing.
    #[test]
    fn independent_known_answers() {
        let fmix_cases: &[(u64, u64)] = &[
            (0x0, 0x0),
            (0x1, 0x5692161d100b05e5),
            (0x2, 0xdbd238973a2b148a),
            (0x123456789abcdef, 0xb2c058e4ebb5112c),
            (0xffffffffffffffff, 0xb4d055fcf2cbbd7b),
            (0x9e3779b97f4a7c15, 0xe220a8397b1dcdaf),
            (0x3c6ef372fe94f82a, 0x6e789e6aa1b965f4)
        ];
        for &(z, out) in fmix_cases {
            assert_eq!(fmix(z), out, "fmix({z:#x})");
        }
        type Line = (Family, u64, u64, u64, Option<BusBeat>, [u64; LINE_WORDS]);
        let lines: &[Line] = &[
            (Family::Ident, 0x0, 0x0, 0x0, None, [0x0, 0x9e3779b9e17d05ac, 0x3c6ef372c2fa0b58, 0xdaa66d2ca7791913, 0x78dde6e585f416b1, 0x1715609f6b610cf6, 0xb54cda584ef23226, 0x53845412288d3081]),
            (Family::Ident, 0x0, 0x123456789abcdef, 0xffffffffffffffff, None, [0xfedcba9877777777, 0x60a540de68530ada, 0xc26dc7254a2d013c, 0x24364d6b23bd0f45, 0x85fed3b2032b6df1, 0xe7c759f8e1e763a0, 0x498fe03fcce55652, 0xab586685afed5407]),
            (Family::Ident, 0x1, 0x0, 0xffffffffffffffff, None, [0xe443234f417d29c, 0x700cb87a0992e4c7, 0xd1d53ec1293de613, 0x339dc5074bae91e0, 0x95664b4e621b9bb2, 0xf72ed19481e69c85, 0x58f757dbaee59efd, 0xbabfde21cfe29b1a]),
            (Family::Ident, 0x1, 0x123456789abcdef, 0x0, None, [0xf2df13337120bda4, 0x91168ced925ca641, 0x2f4e06a6addaa067, 0xcd858060cc5aa2b6, 0x6bbcfa19ea9564f2, 0x9f473d3098068d3, 0xa82bed8cd7957a99, 0x46636745b96a746f]),
            (Family::Ident, 0x7, 0x0, 0x0, None, [0x9c22a0934469840b, 0x3a5a1a4d6dcfbae0, 0xd89194060e7188c4, 0x76c90dc020e39517, 0x15008779c0759395, 0xb3380133e7879032, 0x516f7aec856577fa, 0xefa6f4a6bcf27d8d]),
            (Family::Ident, 0x7, 0x123456789abcdef, 0xffffffffffffffff, None, [0x62ba1a04034ce883, 0xc482a04b25c3ced7, 0x264b269146c0cc20, 0x8813acd857c5ca1e, 0xe9dc331eb6fcd1c5, 0x4ba4b96595cfe795, 0xad6d3fabf0d8e4ae, 0xf35c5f2d23592e8]),
            (Family::Ident, 0x6000003039, 0x0, 0xffffffffffffffff, None, [0x1724afa561e82acd, 0x78ed35ec8efa3491, 0xdab5bc32afd4c1a0, 0x3c7e4279c8d5bbde, 0x9e46c8bfedb0bd03, 0xf4f06f34fbed7, 0x61d7d54c135cb8aa, 0xc3a05b933275b268]),
            (Family::Ident, 0x6000003039, 0x123456789abcdef, 0x0, None, [0xe9fe95c2e986c695, 0x88360f7bf7f4c017, 0x266d8934d960c2b5, 0xc4a502eebaf2c578, 0x62dc7ca79f7e3f0c, 0x113f6617dff49a1, 0x9f4b701a637c4bcf, 0x3d82e9d446035e3e]),
            (Family::Random, 0x0, 0x0, 0x0, None, [0x0, 0x5692161d100b05e5, 0xdbd238973a2b148a, 0x1e535eede31428f0, 0xb7a4712c74562914, 0xb6bf613dbebb45dc, 0xd17707977078336c, 0x12ae30237b17df14]),
            (Family::Random, 0x0, 0x123456789abcdef, 0xffffffffffffffff, None, [0x4d3fa71b144aeed3, 0xe86acc300d8dfda5, 0x3d4c09929b59b307, 0x7095bb769fcc3d0d, 0xed9a34ac2e1ecd00, 0xa6d1f4fc29a390d1, 0x68293a5bc5bd4732, 0x2639fc2ef0f9040f]),
            (Family::Random, 0x1, 0x0, 0xffffffffffffffff, None, [0x2a94e04463145617, 0x7d9395408022a528, 0xf8a37ae656cdfa86, 0xcb9d27b70ac54492, 0xc841a71728dec443, 0x23056aaa4a077e2e, 0xdaa39fb909d041d6, 0xfc6d8ab6cb15eac6]),
            (Family::Random, 0x1, 0x123456789abcdef, 0x0, None, [0xecebcd5f5c26f577, 0x9ceb2936b4130119, 0xe72cde925072efba, 0xb3289ae15a585fc3, 0x82bb3863a786996a, 0xc939500739b0ca10, 0x5cc76cf9db5a6284, 0x5789fbd56e6ca021]),
            (Family::Random, 0x7, 0x0, 0x0, None, [0x58efd731a91fb004, 0x6231eab2525ba011, 0x99e7fe09b67a7978, 0x8bd899976cb6021e, 0xe49d524d3a854e5, 0x926465ef67d04f3f, 0x3cee781815ce206b, 0xe1baa47d01408015]),
            (Family::Random, 0x7, 0x123456789abcdef, 0xffffffffffffffff, None, [0xfa025e4e471e4571, 0xa72a8f00d9e1942f, 0x28c19409955b92cb, 0xd056ccc2f3800a98, 0x5e7291c75f6a8bc2, 0xd16b06f697c5a567, 0xa781ccf5667759c2, 0xbd45e4be239e882a]),
            (Family::Random, 0x6000003039, 0x0, 0xffffffffffffffff, None, [0xbcb2b623d28f5d15, 0x8cbf7239d003f835, 0x8302efa7194c8099, 0x8abeca5b4968e292, 0xcd5ad6d5da2ffe9a, 0x9e372083f86b71c4, 0xf930c1c56fed5d39, 0xbf8da923d8b1d439]),
            (Family::Random, 0x6000003039, 0x123456789abcdef, 0x0, None, [0x21d3a0bf90be2fee, 0x6775ace22d2c2ccf, 0xa3be2636a93938e3, 0xef2c51983d8f6bf, 0xdf25ae8913976d82, 0x1b40fdd74073e1f7, 0x6db646d10676f6b1, 0x222a1bc7eaea7d69]),
            (Family::Lines, 0x0, 0x0, 0x0, None, [0x0, 0x1, 0x2, 0x3, 0x4, 0x5, 0x6, 0x7]),
            (Family::Lines, 0x0, 0x123456789abcdef, 0xffffffffffffffff, None, [0x4d3fa71b144aeed3, 0xfaa49a9bfaed8162, 0xa8098e1ce19013f1, 0x556e819dc832a680, 0x2d3751eaed5390f, 0xb038689f9577cb9e, 0x5d9d5c207c1a5e2d, 0xb024fa162bcf0bc]),
            (Family::Lines, 0x1, 0x0, 0xffffffffffffffff, None, [0xa96de9e2eff4fa1a, 0x9fc87dda0b010c1b, 0x962311d1260d1e1c, 0x8c7da5c84119301d, 0x82d839bf5c25421e, 0x7932cdb67731541f, 0x6f8d61ad923d6620, 0x65e7f5a4ad497821]),
            (Family::Lines, 0x1, 0x123456789abcdef, 0x0, None, [0x179533cff272025a, 0x4399338bced225d3, 0x6f9d3347ab32494c, 0x9ba1330387926cc5, 0xc7a532bf63f2903e, 0xf3a9327b4052b3b7, 0x1fad32371cb2d730, 0x4bb131f2f912faa9]),
            (Family::Lines, 0x7, 0x0, 0x0, None, [0x12ae30237b17df14, 0x1360c7c4f1330d69, 0x14135f66674e3bbe, 0x14c5f707dd696a13, 0x15788ea953849868, 0x162b264ac99fc6bd, 0x16ddbdec3fbaf512, 0x1790558db5d62367]),
            (Family::Lines, 0x7, 0x123456789abcdef, 0xffffffffffffffff, None, [0x2639fc2ef0f9040f, 0xbc3569c87dfb8280, 0x5230d7620afe00f1, 0xe82c44fb98007f62, 0x7e27b2952502fdd3, 0x1423202eb2057c44, 0xaa1e8dc83f07fab5, 0x4019fb61cc0a7926]),
            (Family::Lines, 0x6000003039, 0x0, 0xffffffffffffffff, None, [0xaa305ffb67779c1a, 0x324d0942cc069819, 0xba69b28a30959418, 0x42865bd195249017, 0xcaa30518f9b38c16, 0x52bfae605e428815, 0xdadc57a7c2d18414, 0x62f900ef27608013]),
            (Family::Lines, 0x6000003039, 0x123456789abcdef, 0x0, None, [0x3f8572e5a966bed2, 0xd4af3c540f975083, 0x69d905c275c7e234, 0xff02cf30dbf873e5, 0x942c989f42290596, 0x2956620da8599747, 0xbe802b7c0e8a28f8, 0x53a9f4ea74babaa9]),
            (Family::Linepair, 0x0, 0x0, 0x0, None, [0x0, 0x9e3779b9e17d05ac, 0x3c6ef372c2fa0b58, 0xdaa66d2ca7791913, 0x78dde6e585f416b1, 0x1715609f6b610cf6, 0xb54cda584ef23226, 0x53845412288d3081]),
            (Family::Linepair, 0x0, 0x123456789abcdef, 0xffffffffffffffff, None, [0xfedcba9877777777, 0x60a540de68530ada, 0xc26dc7254a2d013c, 0x24364d6b23bd0f45, 0x85fed3b2032b6df1, 0xe7c759f8e1e763a0, 0x498fe03fcce55652, 0xab586685afed5407]),
            (Family::Linepair, 0x1, 0x0, 0xffffffffffffffff, None, [0x0, 0x9e3779b9e17d05ac, 0x3c6ef372c2fa0b58, 0xdaa66d2ca7791913, 0x78dde6e585f416b1, 0x1715609f6b610cf6, 0xb54cda584ef23226, 0x53845412288d3081]),
            (Family::Linepair, 0x1, 0x123456789abcdef, 0x0, None, [0xfedcba9877777777, 0x60a540de68530ada, 0xc26dc7254a2d013c, 0x24364d6b23bd0f45, 0x85fed3b2032b6df1, 0xe7c759f8e1e763a0, 0x498fe03fcce55652, 0xab586685afed5407]),
            (Family::Linepair, 0x7, 0x0, 0x0, None, [0x55992d38886e6ec8, 0xb761b37eea20737b, 0x192a39c5c5a605df, 0x7af2c00b21247824, 0xdcbb4652079a7216, 0x3e83cc9864e87cc1, 0xa04c52df79fa7eb1, 0x214d9255b1471a6]),
            (Family::Linepair, 0x7, 0x123456789abcdef, 0xffffffffffffffff, None, [0xab8a182fcc2909f0, 0x49c191e8af2c1c1c, 0xe7f90ba281c101ab, 0x8630855b63b20345, 0x2467ff1540aafd26, 0xc29f78ce26880686, 0x60d6f28803b708d5, 0xff0e6c411da21a33]),
            (Family::Linepair, 0x6000003039, 0x0, 0xffffffffffffffff, None, [0xf71f828e8b67264e, 0x9556fc476e95dc92, 0x338e76014883eaeb, 0xd1c5efba2b9df745, 0x6ffd6974165ffc60, 0xe34e32df6d9f204, 0xac6c5ce7d45bd1d9, 0x4aa3d6a0bd21dff3]),
            (Family::Linepair, 0x6000003039, 0x123456789abcdef, 0x0, None, [0x7bd380901994aa6, 0x6985be50eceb5094, 0xcb4e4496cff72e4f, 0x2d16cadda9152c33, 0x8edf51238d913220, 0xf0a7d76a723f0872, 0x52705db05393069d, 0xb438e3f7351534b5]),
            (Family::Linepair, 0x6, 0x0, 0x0, None, [0xaa66d2c777919137, 0x489e4c8115df8c84, 0xe6d5c63a3a59fa20, 0x850d3ff4dedb87db, 0x2344b9adf8658de9, 0xc17c33679b17833e, 0x5fb3ad208605814e, 0xfdeb26daa4eb8e59]),
            (Family::Linepair, 0x6000003038, 0x0, 0xffffffffffffffff, None, [0x8e07d717498d9b1, 0x6aa903b8916a236d, 0xcc7189feb77c1514, 0x2e3a1045d46208ba, 0x9002968be9a0039f, 0xf1cb1cd209260dfb, 0x5393a3182ba42e26, 0xb55c295f42de200c]),
            (Family::Linepair, 0x6, 0x123456789abcdef, 0xffffffffffffffff, None, [0x5475e7d033d6f60f, 0xb63e6e1750d3e3e3, 0x1806f45d7e3efe54, 0x79cf7aa49c4dfcba, 0xdb9800eabf5502d9, 0x3d608731d977f979, 0x9f290d77fc48f72a, 0xf193bee25de5cc]),
            (Family::Linepair, 0x6000003038, 0x123456789abcdef, 0x0, None, [0xf842c7f6fe66b559, 0x967a41af1314af6b, 0x34b1bb693008d1b0, 0xd2e9352256ead3cc, 0x7120aedc726ecddf, 0xf5828958dc0f78d, 0xad8fa24fac6cf962, 0x4bc71c08caeacb4a]),
            (Family::Switch, 0x0, 0x0, 0x0, Some(BusBeat::Bits16), [0xf0ff0f00f0ff0f0, 0xf0ff0f00f0ff0f0, 0xf0ff0f00f0ff0f0, 0xf0ff0f00f0ff0f0, 0xf0ff0f00f0ff0f0, 0xf0ff0f00f0ff0f0, 0xf0ff0f00f0ff0f0, 0xf0ff0f00f0ff0f0]),
            (Family::Switch, 0x0, 0x123456789abcdef, 0xffffffffffffffff, Some(BusBeat::Bits16), [0xd23c2dc3d23c2dc3, 0xd23c2dc3d23c2dc3, 0xd23c2dc3d23c2dc3, 0xd23c2dc3d23c2dc3, 0xd23c2dc3d23c2dc3, 0xd23c2dc3d23c2dc3, 0xd23c2dc3d23c2dc3, 0xd23c2dc3d23c2dc3]),
            (Family::Switch, 0x0, 0x0, 0xffffffffffffffff, Some(BusBeat::Bits32), [0xf0f0f0f00f0f0f0f, 0xf0f0f0f00f0f0f0f, 0xf0f0f0f00f0f0f0f, 0xf0f0f0f00f0f0f0f, 0xf0f0f0f00f0f0f0f, 0xf0f0f0f00f0f0f0f, 0xf0f0f0f00f0f0f0f, 0xf0f0f0f00f0f0f0f]),
            (Family::Switch, 0x0, 0x123456789abcdef, 0x0, Some(BusBeat::Bits32), [0x1e1e2dc3e1e1d23c, 0x1e1e2dc3e1e1d23c, 0x1e1e2dc3e1e1d23c, 0x1e1e2dc3e1e1d23c, 0x1e1e2dc3e1e1d23c, 0x1e1e2dc3e1e1d23c, 0x1e1e2dc3e1e1d23c, 0x1e1e2dc3e1e1d23c]),
            (Family::Switch, 0x0, 0x0, 0x0, Some(BusBeat::Bits64), [0xf0f0f0f0f0f0f0f0, 0xf0f0f0f0f0f0f0f, 0xf0f0f0f0f0f0f0f0, 0xf0f0f0f0f0f0f0f, 0xf0f0f0f0f0f0f0f0, 0xf0f0f0f0f0f0f0f, 0xf0f0f0f0f0f0f0f0, 0xf0f0f0f0f0f0f0f]),
            (Family::Switch, 0x0, 0x123456789abcdef, 0xffffffffffffffff, Some(BusBeat::Bits64), [0xe1b4b45a1e1e2dc3, 0x1e4b4ba5e1e1d23c, 0xe1b4b45a1e1e2dc3, 0x1e4b4ba5e1e1d23c, 0xe1b4b45a1e1e2dc3, 0x1e4b4ba5e1e1d23c, 0xe1b4b45a1e1e2dc3, 0x1e4b4ba5e1e1d23c]),
            (Family::Switch, 0x1, 0x0, 0xffffffffffffffff, Some(BusBeat::Bits16), [0x1ea5e15a1ea5e15a, 0x1ea5e15a1ea5e15a, 0x1ea5e15a1ea5e15a, 0x1ea5e15a1ea5e15a, 0x1ea5e15a1ea5e15a, 0x1ea5e15a1ea5e15a, 0x1ea5e15a1ea5e15a, 0x1ea5e15a1ea5e15a]),
            (Family::Switch, 0x1, 0x123456789abcdef, 0x0, Some(BusBeat::Bits16), [0x5aa5a55a5aa5a55a, 0x5aa5a55a5aa5a55a, 0x5aa5a55a5aa5a55a, 0x5aa5a55a5aa5a55a, 0x5aa5a55a5aa5a55a, 0x5aa5a55a5aa5a55a, 0x5aa5a55a5aa5a55a, 0x5aa5a55a5aa5a55a]),
            (Family::Switch, 0x1, 0x0, 0x0, Some(BusBeat::Bits32), [0xf5ae15af0a51ea5, 0xf5ae15af0a51ea5, 0xf5ae15af0a51ea5, 0xf5ae15af0a51ea5, 0xf5ae15af0a51ea5, 0xf5ae15af0a51ea5, 0xf5ae15af0a51ea5, 0xf5ae15af0a51ea5]),
            (Family::Switch, 0x1, 0x123456789abcdef, 0xffffffffffffffff, Some(BusBeat::Bits32), [0xf0d2a55a0f2d5aa5, 0xf0d2a55a0f2d5aa5, 0xf0d2a55a0f2d5aa5, 0xf0d2a55a0f2d5aa5, 0xf0d2a55a0f2d5aa5, 0xf0d2a55a0f2d5aa5, 0xf0d2a55a0f2d5aa5, 0xf0d2a55a0f2d5aa5]),
            (Family::Switch, 0x1, 0x0, 0xffffffffffffffff, Some(BusBeat::Bits64), [0x1e0f0fb40f5ae15a, 0xe1f0f04bf0a51ea5, 0x1e0f0fb40f5ae15a, 0xe1f0f04bf0a51ea5, 0x1e0f0fb40f5ae15a, 0xe1f0f04bf0a51ea5, 0x1e0f0fb40f5ae15a, 0xe1f0f04bf0a51ea5]),
            (Family::Switch, 0x1, 0x123456789abcdef, 0x0, Some(BusBeat::Bits64), [0xfd287d2f0d2a55a, 0xf02d782d0f2d5aa5, 0xfd287d2f0d2a55a, 0xf02d782d0f2d5aa5, 0xfd287d2f0d2a55a, 0xf02d782d0f2d5aa5, 0xfd287d2f0d2a55a, 0xf02d782d0f2d5aa5]),
            (Family::Switch, 0x7, 0x0, 0x0, Some(BusBeat::Bits16), [0x1e4be1b41e4be1b4, 0x1e4be1b41e4be1b4, 0x1e4be1b41e4be1b4, 0x1e4be1b41e4be1b4, 0x1e4be1b41e4be1b4, 0x1e4be1b41e4be1b4, 0x1e4be1b41e4be1b4, 0x1e4be1b41e4be1b4]),
            (Family::Switch, 0x7, 0x123456789abcdef, 0xffffffffffffffff, Some(BusBeat::Bits16), [0xff0f00f0ff0f00f, 0xff0f00f0ff0f00f, 0xff0f00f0ff0f00f, 0xff0f00f0ff0f00f, 0xff0f00f0ff0f00f, 0xff0f00f0ff0f00f, 0xff0f00f0ff0f00f, 0xff0f00f0ff0f00f]),
            (Family::Switch, 0x7, 0x0, 0xffffffffffffffff, Some(BusBeat::Bits32), [0x2d0fe1b4d2f01e4b, 0x2d0fe1b4d2f01e4b, 0x2d0fe1b4d2f01e4b, 0x2d0fe1b4d2f01e4b, 0x2d0fe1b4d2f01e4b, 0x2d0fe1b4d2f01e4b, 0x2d0fe1b4d2f01e4b, 0x2d0fe1b4d2f01e4b]),
            (Family::Switch, 0x7, 0x123456789abcdef, 0x0, Some(BusBeat::Bits32), [0xf0b4f00f0f4b0ff0, 0xf0b4f00f0f4b0ff0, 0xf0b4f00f0f4b0ff0, 0xf0b4f00f0f4b0ff0, 0xf0b4f00f0f4b0ff0, 0xf0b4f00f0f4b0ff0, 0xf0b4f00f0f4b0ff0, 0xf0b4f00f0f4b0ff0]),
            (Family::Switch, 0x7, 0x0, 0x0, Some(BusBeat::Bits64), [0x874be1872d0fe1b4, 0x78b41e78d2f01e4b, 0x874be1872d0fe1b4, 0x78b41e78d2f01e4b, 0x874be1872d0fe1b4, 0x78b41e78d2f01e4b, 0x874be1872d0fe1b4, 0x78b41e78d2f01e4b]),
            (Family::Switch, 0x7, 0x123456789abcdef, 0xffffffffffffffff, Some(BusBeat::Bits64), [0xff00f69f0b4f00f, 0xf00ff0960f4b0ff0, 0xff00f69f0b4f00f, 0xf00ff0960f4b0ff0, 0xff00f69f0b4f00f, 0xf00ff0960f4b0ff0, 0xff00f69f0b4f00f, 0xf00ff0960f4b0ff0]),
            (Family::Switch, 0x6000003039, 0x0, 0xffffffffffffffff, Some(BusBeat::Bits16), [0x1ea5e15a1ea5e15a, 0x1ea5e15a1ea5e15a, 0x1ea5e15a1ea5e15a, 0x1ea5e15a1ea5e15a, 0x1ea5e15a1ea5e15a, 0x1ea5e15a1ea5e15a, 0x1ea5e15a1ea5e15a, 0x1ea5e15a1ea5e15a]),
            (Family::Switch, 0x6000003039, 0x123456789abcdef, 0x0, Some(BusBeat::Bits16), [0xd22d2dd2d22d2dd2, 0xd22d2dd2d22d2dd2, 0xd22d2dd2d22d2dd2, 0xd22d2dd2d22d2dd2, 0xd22d2dd2d22d2dd2, 0xd22d2dd2d22d2dd2, 0xd22d2dd2d22d2dd2, 0xd22d2dd2d22d2dd2]),
            (Family::Switch, 0x6000003039, 0x0, 0x0, Some(BusBeat::Bits32), [0x693ce15a96c31ea5, 0x693ce15a96c31ea5, 0x693ce15a96c31ea5, 0x693ce15a96c31ea5, 0x693ce15a96c31ea5, 0x693ce15a96c31ea5, 0x693ce15a96c31ea5, 0x693ce15a96c31ea5]),
            (Family::Switch, 0x6000003039, 0x123456789abcdef, 0xffffffffffffffff, Some(BusBeat::Bits32), [0x4b1e2dd2b4e1d22d, 0x4b1e2dd2b4e1d22d, 0x4b1e2dd2b4e1d22d, 0x4b1e2dd2b4e1d22d, 0x4b1e2dd2b4e1d22d, 0x4b1e2dd2b4e1d22d, 0x4b1e2dd2b4e1d22d, 0x4b1e2dd2b4e1d22d]),
            (Family::Switch, 0x6000003039, 0x0, 0xffffffffffffffff, Some(BusBeat::Bits64), [0x96878787693ce15a, 0x6978787896c31ea5, 0x96878787693ce15a, 0x6978787896c31ea5, 0x96878787693ce15a, 0x6978787896c31ea5, 0x96878787693ce15a, 0x6978787896c31ea5]),
            (Family::Switch, 0x6000003039, 0x123456789abcdef, 0x0, Some(BusBeat::Bits64), [0x5a6996964b1e2dd2, 0xa5966969b4e1d22d, 0x5a6996964b1e2dd2, 0xa5966969b4e1d22d, 0x5a6996964b1e2dd2, 0xa5966969b4e1d22d, 0x5a6996964b1e2dd2, 0xa5966969b4e1d22d]),
            // 12 and 24 bits (LPDDR6, DDR6 models): the same implementation's general bit rule
            (Family::Switch, 0x0, 0x0, 0x0, Some(BusBeat::Bits12), [0xf0f0f0f0f0f0f0f0, 0xf0f0f0f0f0f0f0f0, 0xf0f0f0f0f0f0f0f0, 0xf0f0f0f0f0f0f0f0, 0xf0f0f0f0f0f0f0f0, 0xf0f0f0f0f0f0f0f0, 0xf0f0f0f0f0f0f0f0, 0xf0f0f0f0f0f0f0f0]),
            (Family::Switch, 0x0, 0x0, 0xffffffffffffffff, Some(BusBeat::Bits12), [0xf0f0f0f0f0f0f0f, 0xf0f0f0f0f0f0f0f, 0xf0f0f0f0f0f0f0f, 0xf0f0f0f0f0f0f0f, 0xf0f0f0f0f0f0f0f, 0xf0f0f0f0f0f0f0f, 0xf0f0f0f0f0f0f0f, 0xf0f0f0f0f0f0f0f]),
            (Family::Switch, 0x0, 0x123456789abcdef, 0x0, Some(BusBeat::Bits12), [0x323cdc323cdc323c, 0x3cdc323cdc323cdc, 0xdc323cdc323cdc32, 0x323cdc323cdc323c, 0x323cdc323cdc323c, 0x3cdc323cdc323cdc, 0xdc323cdc323cdc32, 0x323cdc323cdc323c]),
            (Family::Switch, 0x0, 0x123456789abcdef, 0xffffffffffffffff, Some(BusBeat::Bits12), [0xcdc323cdc323cdc3, 0xc323cdc323cdc323, 0x23cdc323cdc323cd, 0xcdc323cdc323cdc3, 0xcdc323cdc323cdc3, 0xc323cdc323cdc323, 0x23cdc323cdc323cd, 0xcdc323cdc323cdc3]),
            (Family::Switch, 0x1, 0x0, 0x0, Some(BusBeat::Bits12), [0xaea515aea515aea5, 0xa515aea515aea515, 0x15aea515aea515ae, 0xaea515aea515aea5, 0xaea515aea515aea5, 0xa515aea515aea515, 0x15aea515aea515ae, 0xaea515aea515aea5]),
            (Family::Switch, 0x1, 0x0, 0xffffffffffffffff, Some(BusBeat::Bits12), [0x515aea515aea515a, 0x5aea515aea515aea, 0xea515aea515aea51, 0x515aea515aea515a, 0x515aea515aea515a, 0x5aea515aea515aea, 0xea515aea515aea51, 0x515aea515aea515a]),
            (Family::Switch, 0x1, 0x123456789abcdef, 0x0, Some(BusBeat::Bits12), [0x555aaa555aaa555a, 0x5aaa555aaa555aaa, 0xaa555aaa555aaa55, 0x555aaa555aaa555a, 0x555aaa555aaa555a, 0x5aaa555aaa555aaa, 0xaa555aaa555aaa55, 0x555aaa555aaa555a]),
            (Family::Switch, 0x1, 0x123456789abcdef, 0xffffffffffffffff, Some(BusBeat::Bits12), [0xaaa555aaa555aaa5, 0xa555aaa555aaa555, 0x55aaa555aaa555aa, 0xaaa555aaa555aaa5, 0xaaa555aaa555aaa5, 0xa555aaa555aaa555, 0x55aaa555aaa555aa, 0xaaa555aaa555aaa5]),
            (Family::Switch, 0x7, 0x0, 0x0, Some(BusBeat::Bits12), [0xb1b4e4b1b4e4b1b4, 0xb4e4b1b4e4b1b4e4, 0xe4b1b4e4b1b4e4b1, 0xb1b4e4b1b4e4b1b4, 0xb1b4e4b1b4e4b1b4, 0xb4e4b1b4e4b1b4e4, 0xe4b1b4e4b1b4e4b1, 0xb1b4e4b1b4e4b1b4]),
            (Family::Switch, 0x7, 0x0, 0xffffffffffffffff, Some(BusBeat::Bits12), [0x4e4b1b4e4b1b4e4b, 0x4b1b4e4b1b4e4b1b, 0x1b4e4b1b4e4b1b4e, 0x4e4b1b4e4b1b4e4b, 0x4e4b1b4e4b1b4e4b, 0x4b1b4e4b1b4e4b1b, 0x1b4e4b1b4e4b1b4e, 0x4e4b1b4e4b1b4e4b]),
            (Family::Switch, 0x7, 0x123456789abcdef, 0x0, Some(BusBeat::Bits12), [0xfff000fff000fff0, 0xf000fff000fff000, 0xfff000fff000ff, 0xfff000fff000fff0, 0xfff000fff000fff0, 0xf000fff000fff000, 0xfff000fff000ff, 0xfff000fff000fff0]),
            (Family::Switch, 0x7, 0x123456789abcdef, 0xffffffffffffffff, Some(BusBeat::Bits12), [0xfff000fff000f, 0xfff000fff000fff, 0xff000fff000fff00, 0xfff000fff000f, 0xfff000fff000f, 0xfff000fff000fff, 0xff000fff000fff00, 0xfff000fff000f]),
            (Family::Switch, 0x6000003039, 0x0, 0x0, Some(BusBeat::Bits12), [0xaea515aea515aea5, 0xa515aea515aea515, 0x15aea515aea515ae, 0xaea515aea515aea5, 0xaea515aea515aea5, 0xa515aea515aea515, 0x15aea515aea515ae, 0xaea515aea515aea5]),
            (Family::Switch, 0x6000003039, 0x0, 0xffffffffffffffff, Some(BusBeat::Bits12), [0x515aea515aea515a, 0x5aea515aea515aea, 0xea515aea515aea51, 0x515aea515aea515a, 0x515aea515aea515a, 0x5aea515aea515aea, 0xea515aea515aea51, 0x515aea515aea515a]),
            (Family::Switch, 0x6000003039, 0x123456789abcdef, 0x0, Some(BusBeat::Bits12), [0xddd222ddd222ddd2, 0xd222ddd222ddd222, 0x22ddd222ddd222dd, 0xddd222ddd222ddd2, 0xddd222ddd222ddd2, 0xd222ddd222ddd222, 0x22ddd222ddd222dd, 0xddd222ddd222ddd2]),
            (Family::Switch, 0x6000003039, 0x123456789abcdef, 0xffffffffffffffff, Some(BusBeat::Bits12), [0x222ddd222ddd222d, 0x2ddd222ddd222ddd, 0xdd222ddd222ddd22, 0x222ddd222ddd222d, 0x222ddd222ddd222d, 0x2ddd222ddd222ddd, 0xdd222ddd222ddd22, 0x222ddd222ddd222d]),
            (Family::Switch, 0x2000000000000005, 0x0, 0x0, Some(BusBeat::Bits12), [0x9f96069f96069f96, 0x96069f96069f9606, 0x69f96069f96069f, 0x9f96069f96069f96, 0x9f96069f96069f96, 0x96069f96069f9606, 0x69f96069f96069f, 0x9f96069f96069f96]),
            (Family::Switch, 0x2000000000000005, 0x0, 0xffffffffffffffff, Some(BusBeat::Bits12), [0x6069f96069f96069, 0x69f96069f96069f9, 0xf96069f96069f960, 0x6069f96069f96069, 0x6069f96069f96069, 0x69f96069f96069f9, 0xf96069f96069f960, 0x6069f96069f96069]),
            (Family::Switch, 0x2000000000000005, 0x123456789abcdef, 0x0, Some(BusBeat::Bits12), [0x282d7d282d7d282d, 0x2d7d282d7d282d7d, 0x7d282d7d282d7d28, 0x282d7d282d7d282d, 0x282d7d282d7d282d, 0x2d7d282d7d282d7d, 0x7d282d7d282d7d28, 0x282d7d282d7d282d]),
            (Family::Switch, 0x2000000000000005, 0x123456789abcdef, 0xffffffffffffffff, Some(BusBeat::Bits12), [0xd7d282d7d282d7d2, 0xd282d7d282d7d282, 0x82d7d282d7d282d7, 0xd7d282d7d282d7d2, 0xd7d282d7d282d7d2, 0xd282d7d282d7d282, 0x82d7d282d7d282d7, 0xd7d282d7d282d7d2]),
            (Family::Switch, 0x0, 0x0, 0x0, Some(BusBeat::Bits24), [0xf0f00f0f0ff0f0f0, 0xff0f0f00f0f0ff0, 0xf0f0ff0f0f00f0f, 0xf0f00f0f0ff0f0f0, 0xff0f0f00f0f0ff0, 0xf0f0ff0f0f00f0f, 0xf0f00f0f0ff0f0f0, 0xff0f0f00f0f0ff0]),
            (Family::Switch, 0x0, 0x0, 0xffffffffffffffff, Some(BusBeat::Bits24), [0xf0ff0f0f00f0f0f, 0xf00f0f0ff0f0f00f, 0xf0f0f00f0f0ff0f0, 0xf0ff0f0f00f0f0f, 0xf00f0f0ff0f0f00f, 0xf0f0f00f0f0ff0f0, 0xf0ff0f0f00f0f0f, 0xf00f0f0ff0f0f00f]),
            (Family::Switch, 0x0, 0x123456789abcdef, 0x0, Some(BusBeat::Bits24), [0xd23c1e2dc3e1d23c, 0xc3e1d23c1e2dc3e1, 0x1e2dc3e1d23c1e2d, 0xd23c1e2dc3e1d23c, 0xc3e1d23c1e2dc3e1, 0x1e2dc3e1d23c1e2d, 0xd23c1e2dc3e1d23c, 0xc3e1d23c1e2dc3e1]),
            (Family::Switch, 0x0, 0x123456789abcdef, 0xffffffffffffffff, Some(BusBeat::Bits24), [0x2dc3e1d23c1e2dc3, 0x3c1e2dc3e1d23c1e, 0xe1d23c1e2dc3e1d2, 0x2dc3e1d23c1e2dc3, 0x3c1e2dc3e1d23c1e, 0xe1d23c1e2dc3e1d2, 0x2dc3e1d23c1e2dc3, 0x3c1e2dc3e1d23c1e]),
            (Family::Switch, 0x1, 0x0, 0x0, Some(BusBeat::Bits24), [0x1ea55ae15aa51ea5, 0x5aa51ea55ae15aa5, 0x5ae15aa51ea55ae1, 0x1ea55ae15aa51ea5, 0x5aa51ea55ae15aa5, 0x5ae15aa51ea55ae1, 0x1ea55ae15aa51ea5, 0x5aa51ea55ae15aa5]),
            (Family::Switch, 0x1, 0x0, 0xffffffffffffffff, Some(BusBeat::Bits24), [0xe15aa51ea55ae15a, 0xa55ae15aa51ea55a, 0xa51ea55ae15aa51e, 0xe15aa51ea55ae15a, 0xa55ae15aa51ea55a, 0xa51ea55ae15aa51e, 0xe15aa51ea55ae15a, 0xa55ae15aa51ea55a]),
            (Family::Switch, 0x1, 0x123456789abcdef, 0x0, Some(BusBeat::Bits24), [0xa55a2d5aa5d2a55a, 0xa5d2a55a2d5aa5d2, 0x2d5aa5d2a55a2d5a, 0xa55a2d5aa5d2a55a, 0xa5d2a55a2d5aa5d2, 0x2d5aa5d2a55a2d5a, 0xa55a2d5aa5d2a55a, 0xa5d2a55a2d5aa5d2]),
            (Family::Switch, 0x1, 0x123456789abcdef, 0xffffffffffffffff, Some(BusBeat::Bits24), [0x5aa5d2a55a2d5aa5, 0x5a2d5aa5d2a55a2d, 0xd2a55a2d5aa5d2a5, 0x5aa5d2a55a2d5aa5, 0x5a2d5aa5d2a55a2d, 0xd2a55a2d5aa5d2a5, 0x5aa5d2a55a2d5aa5, 0x5a2d5aa5d2a55a2d]),
            (Family::Switch, 0x7, 0x0, 0x0, Some(BusBeat::Bits24), [0xe1b4f01e4b0fe1b4, 0x4b0fe1b4f01e4b0f, 0xf01e4b0fe1b4f01e, 0xe1b4f01e4b0fe1b4, 0x4b0fe1b4f01e4b0f, 0xf01e4b0fe1b4f01e, 0xe1b4f01e4b0fe1b4, 0x4b0fe1b4f01e4b0f]),
            (Family::Switch, 0x7, 0x0, 0xffffffffffffffff, Some(BusBeat::Bits24), [0x1e4b0fe1b4f01e4b, 0xb4f01e4b0fe1b4f0, 0xfe1b4f01e4b0fe1, 0x1e4b0fe1b4f01e4b, 0xb4f01e4b0fe1b4f0, 0xfe1b4f01e4b0fe1, 0x1e4b0fe1b4f01e4b, 0xb4f01e4b0fe1b4f0]),
            (Family::Switch, 0x7, 0x123456789abcdef, 0x0, Some(BusBeat::Bits24), [0xff0b4f00f4b0ff0, 0xf4b0ff0b4f00f4b, 0xb4f00f4b0ff0b4f0, 0xff0b4f00f4b0ff0, 0xf4b0ff0b4f00f4b, 0xb4f00f4b0ff0b4f0, 0xff0b4f00f4b0ff0, 0xf4b0ff0b4f00f4b]),
            (Family::Switch, 0x7, 0x123456789abcdef, 0xffffffffffffffff, Some(BusBeat::Bits24), [0xf00f4b0ff0b4f00f, 0xf0b4f00f4b0ff0b4, 0x4b0ff0b4f00f4b0f, 0xf00f4b0ff0b4f00f, 0xf0b4f00f4b0ff0b4, 0x4b0ff0b4f00f4b0f, 0xf00f4b0ff0b4f00f, 0xf0b4f00f4b0ff0b4]),
            (Family::Switch, 0x6000003039, 0x0, 0x0, Some(BusBeat::Bits24), [0x1ea53ce15ac31ea5, 0x5ac31ea53ce15ac3, 0x3ce15ac31ea53ce1, 0x1ea53ce15ac31ea5, 0x5ac31ea53ce15ac3, 0x3ce15ac31ea53ce1, 0x1ea53ce15ac31ea5, 0x5ac31ea53ce15ac3]),
            (Family::Switch, 0x6000003039, 0x0, 0xffffffffffffffff, Some(BusBeat::Bits24), [0xe15ac31ea53ce15a, 0xa53ce15ac31ea53c, 0xc31ea53ce15ac31e, 0xe15ac31ea53ce15a, 0xa53ce15ac31ea53c, 0xc31ea53ce15ac31e, 0xe15ac31ea53ce15a, 0xa53ce15ac31ea53c]),
            (Family::Switch, 0x6000003039, 0x123456789abcdef, 0x0, Some(BusBeat::Bits24), [0x2dd2e1d22d1e2dd2, 0x2d1e2dd2e1d22d1e, 0xe1d22d1e2dd2e1d2, 0x2dd2e1d22d1e2dd2, 0x2d1e2dd2e1d22d1e, 0xe1d22d1e2dd2e1d2, 0x2dd2e1d22d1e2dd2, 0x2d1e2dd2e1d22d1e]),
            (Family::Switch, 0x6000003039, 0x123456789abcdef, 0xffffffffffffffff, Some(BusBeat::Bits24), [0xd22d1e2dd2e1d22d, 0xd2e1d22d1e2dd2e1, 0x1e2dd2e1d22d1e2d, 0xd22d1e2dd2e1d22d, 0xd2e1d22d1e2dd2e1, 0x1e2dd2e1d22d1e2d, 0xd22d1e2dd2e1d22d, 0xd2e1d22d1e2dd2e1]),
            (Family::Switch, 0x2000000000000005, 0x0, 0x0, Some(BusBeat::Bits24), [0xf964bf069b40f96, 0x69b40f964bf069b4, 0x4bf069b40f964bf0, 0xf964bf069b40f96, 0x69b40f964bf069b4, 0x4bf069b40f964bf0, 0xf964bf069b40f96, 0x69b40f964bf069b4]),
            (Family::Switch, 0x2000000000000005, 0x0, 0xffffffffffffffff, Some(BusBeat::Bits24), [0xf069b40f964bf069, 0x964bf069b40f964b, 0xb40f964bf069b40f, 0xf069b40f964bf069, 0x964bf069b40f964b, 0xb40f964bf069b40f, 0xf069b40f964bf069, 0x964bf069b40f964b]),
            (Family::Switch, 0x2000000000000005, 0x123456789abcdef, 0x0, Some(BusBeat::Bits24), [0x782d5a87d2a5782d, 0xd2a5782d5a87d2a5, 0x5a87d2a5782d5a87, 0x782d5a87d2a5782d, 0xd2a5782d5a87d2a5, 0x5a87d2a5782d5a87, 0x782d5a87d2a5782d, 0xd2a5782d5a87d2a5]),
            (Family::Switch, 0x2000000000000005, 0x123456789abcdef, 0xffffffffffffffff, Some(BusBeat::Bits24), [0x87d2a5782d5a87d2, 0x2d5a87d2a5782d5a, 0xa5782d5a87d2a578, 0x87d2a5782d5a87d2, 0x2d5a87d2a5782d5a, 0xa5782d5a87d2a578, 0x87d2a5782d5a87d2, 0x2d5a87d2a5782d5a])
        ];
        for &(family, lid, key, mask, beat, words) in lines {
            let phase = Phase { family, key, mask, beat: beat.unwrap_or(BusBeat::Bits64) };
            assert_eq!(phase.line(lid), words, "{} line {lid:#x} key {key:#x}", phase.label());
        }
        // `quiet` and `header` (and `switch` again) from another implementation of the spec alone
        // (2026-10-16), at every transfer width
        let stage6: &[Line] = &[
            (Family::Quiet, 0x0, 0x0, 0x0, Some(BusBeat::Bits12), [0xf0f1f0f0f1f0f0f1, 0xf1f0f0f1f0f0f1f0, 0xf0f0f1f0f0f1f0f0, 0xf0f1f0f0f1f0f0f1, 0xf0f1f0f0f1f0f0f1, 0xf1f0f0f1f0f0f1f0, 0xf0f0f1f0f0f1f0f0, 0xf0f1f0f0f1f0f0f1]),
            (Family::Quiet, 0x0, 0x0, 0x0, Some(BusBeat::Bits16), [0xf0ff0f10f0ff0f1, 0xf0ff0f10f0ff0f1, 0xf0ff0f10f0ff0f1, 0xf0ff0f10f0ff0f1, 0xf0ff0f10f0ff0f1, 0xf0ff0f10f0ff0f1, 0xf0ff0f10f0ff0f1, 0xf0ff0f10f0ff0f1]),
            (Family::Quiet, 0x0, 0x0, 0x0, Some(BusBeat::Bits24), [0xf0f10f0f0ff0f0f1, 0xff0f0f10f0f0ff0, 0xf0f0ff0f0f10f0f, 0xf0f10f0f0ff0f0f1, 0xff0f0f10f0f0ff0, 0xf0f0ff0f0f10f0f, 0xf0f10f0f0ff0f0f1, 0xff0f0f10f0f0ff0]),
            (Family::Quiet, 0x0, 0x0, 0x0, Some(BusBeat::Bits32), [0xf0f0f0ff0f0f0f1, 0xf0f0f0ff0f0f0f1, 0xf0f0f0ff0f0f0f1, 0xf0f0f0ff0f0f0f1, 0xf0f0f0ff0f0f0f1, 0xf0f0f0ff0f0f0f1, 0xf0f0f0ff0f0f0f1, 0xf0f0f0ff0f0f0f1]),
            (Family::Quiet, 0x0, 0x0, 0x0, Some(BusBeat::Bits64), [0xf0f0f0f0f0f0f0f1, 0xf0f0f0f0f0f0f0f, 0xf0f0f0f0f0f0f0f1, 0xf0f0f0f0f0f0f0f, 0xf0f0f0f0f0f0f0f1, 0xf0f0f0f0f0f0f0f, 0xf0f0f0f0f0f0f0f1, 0xf0f0f0f0f0f0f0f]),
            (Family::Quiet, 0x0, 0x123456789abcdef, 0xffffffffffffffff, Some(BusBeat::Bits12), [0xccc323ccc323ccc3, 0xc323ccc323ccc323, 0x23ccc323ccc323cc, 0xccc323ccc323ccc3, 0xccc323ccc323ccc3, 0xc323ccc323ccc323, 0x23ccc323ccc323cc, 0xccc323ccc323ccc3]),
            (Family::Quiet, 0x0, 0x123456789abcdef, 0xffffffffffffffff, Some(BusBeat::Bits16), [0xd23c25c3d23c25c3, 0xd23c25c3d23c25c3, 0xd23c25c3d23c25c3, 0xd23c25c3d23c25c3, 0xd23c25c3d23c25c3, 0xd23c25c3d23c25c3, 0xd23c25c3d23c25c3, 0xd23c25c3d23c25c3]),
            (Family::Quiet, 0x0, 0x123456789abcdef, 0xffffffffffffffff, Some(BusBeat::Bits24), [0x2dc3e0d23c1e2dc3, 0x3c1e2dc3e0d23c1e, 0xe0d23c1e2dc3e0d2, 0x2dc3e0d23c1e2dc3, 0x3c1e2dc3e0d23c1e, 0xe0d23c1e2dc3e0d2, 0x2dc3e0d23c1e2dc3, 0x3c1e2dc3e0d23c1e]),
            (Family::Quiet, 0x0, 0x123456789abcdef, 0xffffffffffffffff, Some(BusBeat::Bits32), [0xe1a1d23c1e1e2dc3, 0xe1a1d23c1e1e2dc3, 0xe1a1d23c1e1e2dc3, 0xe1a1d23c1e1e2dc3, 0xe1a1d23c1e1e2dc3, 0xe1a1d23c1e1e2dc3, 0xe1a1d23c1e1e2dc3, 0xe1a1d23c1e1e2dc3]),
            (Family::Quiet, 0x0, 0x123456789abcdef, 0xffffffffffffffff, Some(BusBeat::Bits64), [0xe1b4a45a1e1e2dc3, 0x1e4b4ba5e1e1d23c, 0xe1b4a45a1e1e2dc3, 0x1e4b4ba5e1e1d23c, 0xe1b4a45a1e1e2dc3, 0x1e4b4ba5e1e1d23c, 0xe1b4a45a1e1e2dc3, 0x1e4b4ba5e1e1d23c]),
            (Family::Quiet, 0x1, 0x0, 0x0, Some(BusBeat::Bits12), [0xaeb515aeb515aeb5, 0xb515aeb515aeb515, 0x15aeb515aeb515ae, 0xaeb515aeb515aeb5, 0xaeb515aeb515aeb5, 0xb515aeb515aeb515, 0x15aeb515aeb515ae, 0xaeb515aeb515aeb5]),
            (Family::Quiet, 0x1, 0x0, 0x0, Some(BusBeat::Bits16), [0xe17a1ea5e17a1ea5, 0xe17a1ea5e17a1ea5, 0xe17a1ea5e17a1ea5, 0xe17a1ea5e17a1ea5, 0xe17a1ea5e17a1ea5, 0xe17a1ea5e17a1ea5, 0xe17a1ea5e17a1ea5, 0xe17a1ea5e17a1ea5]),
            (Family::Quiet, 0x1, 0x0, 0x0, Some(BusBeat::Bits24), [0x1fa55ae15aa51fa5, 0x5aa51fa55ae15aa5, 0x5ae15aa51fa55ae1, 0x1fa55ae15aa51fa5, 0x5aa51fa55ae15aa5, 0x5ae15aa51fa55ae1, 0x1fa55ae15aa51fa5, 0x5aa51fa55ae15aa5]),
            (Family::Quiet, 0x1, 0x0, 0x0, Some(BusBeat::Bits32), [0xf5ae55af0a51ea5, 0xf5ae55af0a51ea5, 0xf5ae55af0a51ea5, 0xf5ae55af0a51ea5, 0xf5ae55af0a51ea5, 0xf5ae55af0a51ea5, 0xf5ae55af0a51ea5, 0xf5ae55af0a51ea5]),
            (Family::Quiet, 0x1, 0x0, 0x0, Some(BusBeat::Bits64), [0xe1f0f04bf0a51ea5, 0x1e0f0fb40f7ae15a, 0xe1f0f04bf0a51ea5, 0x1e0f0fb40f7ae15a, 0xe1f0f04bf0a51ea5, 0x1e0f0fb40f7ae15a, 0xe1f0f04bf0a51ea5, 0x1e0f0fb40f7ae15a]),
            (Family::Quiet, 0x1, 0x123456789abcdef, 0xffffffffffffffff, Some(BusBeat::Bits12), [0x8aa5558aa5558aa5, 0xa5558aa5558aa555, 0x558aa5558aa5558a, 0x8aa5558aa5558aa5, 0x8aa5558aa5558aa5, 0xa5558aa5558aa555, 0x558aa5558aa5558a, 0x8aa5558aa5558aa5]),
            (Family::Quiet, 0x1, 0x123456789abcdef, 0xffffffffffffffff, Some(BusBeat::Bits16), [0xa5585aa5a5585aa5, 0xa5585aa5a5585aa5, 0xa5585aa5a5585aa5, 0xa5585aa5a5585aa5, 0xa5585aa5a5585aa5, 0xa5585aa5a5585aa5, 0xa5585aa5a5585aa5, 0xa5585aa5a5585aa5]),
            (Family::Quiet, 0x1, 0x123456789abcdef, 0xffffffffffffffff, Some(BusBeat::Bits24), [0x5aa1d2a55a2d5aa1, 0x5a2d5aa1d2a55a2d, 0xd2a55a2d5aa1d2a5, 0x5aa1d2a55a2d5aa1, 0x5a2d5aa1d2a55a2d, 0xd2a55a2d5aa1d2a5, 0x5aa1d2a55a2d5aa1, 0x5a2d5aa1d2a55a2d]),
            (Family::Quiet, 0x1, 0x123456789abcdef, 0xffffffffffffffff, Some(BusBeat::Bits32), [0xf0d2a55a0f2d5aa1, 0xf0d2a55a0f2d5aa1, 0xf0d2a55a0f2d5aa1, 0xf0d2a55a0f2d5aa1, 0xf0d2a55a0f2d5aa1, 0xf0d2a55a0f2d5aa1, 0xf0d2a55a0f2d5aa1, 0xf0d2a55a0f2d5aa1]),
            (Family::Quiet, 0x1, 0x123456789abcdef, 0xffffffffffffffff, Some(BusBeat::Bits64), [0xf02d782d0f2d5a85, 0xfd287d2f0d2a55a, 0xf02d782d0f2d5a85, 0xfd287d2f0d2a55a, 0xf02d782d0f2d5a85, 0xfd287d2f0d2a55a, 0xf02d782d0f2d5a85, 0xfd287d2f0d2a55a]),
            (Family::Quiet, 0x7, 0x0, 0x0, Some(BusBeat::Bits12), [0xb1b5e4b1b5e4b1b5, 0xb5e4b1b5e4b1b5e4, 0xe4b1b5e4b1b5e4b1, 0xb1b5e4b1b5e4b1b5, 0xb1b5e4b1b5e4b1b5, 0xb5e4b1b5e4b1b5e4, 0xe4b1b5e4b1b5e4b1, 0xb1b5e4b1b5e4b1b5]),
            (Family::Quiet, 0x7, 0x0, 0x0, Some(BusBeat::Bits16), [0x1e4be1b61e4be1b6, 0x1e4be1b61e4be1b6, 0x1e4be1b61e4be1b6, 0x1e4be1b61e4be1b6, 0x1e4be1b61e4be1b6, 0x1e4be1b61e4be1b6, 0x1e4be1b61e4be1b6, 0x1e4be1b61e4be1b6]),
            (Family::Quiet, 0x7, 0x0, 0x0, Some(BusBeat::Bits24), [0xe1b6f01e4b0fe1b6, 0x4b0fe1b6f01e4b0f, 0xf01e4b0fe1b6f01e, 0xe1b6f01e4b0fe1b6, 0x4b0fe1b6f01e4b0f, 0xf01e4b0fe1b6f01e, 0xe1b6f01e4b0fe1b6, 0x4b0fe1b6f01e4b0f]),
            (Family::Quiet, 0x7, 0x0, 0x0, Some(BusBeat::Bits32), [0xd2f01e4f2d0fe1b4, 0xd2f01e4f2d0fe1b4, 0xd2f01e4f2d0fe1b4, 0xd2f01e4f2d0fe1b4, 0xd2f01e4f2d0fe1b4, 0xd2f01e4f2d0fe1b4, 0xd2f01e4f2d0fe1b4, 0xd2f01e4f2d0fe1b4]),
            (Family::Quiet, 0x7, 0x0, 0x0, Some(BusBeat::Bits64), [0x874be1872d0fe1b4, 0x78b41e78d2f01e5b, 0x874be1872d0fe1b4, 0x78b41e78d2f01e5b, 0x874be1872d0fe1b4, 0x78b41e78d2f01e5b, 0x874be1872d0fe1b4, 0x78b41e78d2f01e5b]),
            (Family::Quiet, 0x7, 0x123456789abcdef, 0xffffffffffffffff, Some(BusBeat::Bits12), [0xfbf000fbf000f, 0xfbf000fbf000fbf, 0xbf000fbf000fbf00, 0xfbf000fbf000f, 0xfbf000fbf000f, 0xfbf000fbf000fbf, 0xbf000fbf000fbf00, 0xfbf000fbf000f]),
            (Family::Quiet, 0x7, 0x123456789abcdef, 0xffffffffffffffff, Some(BusBeat::Bits16), [0xff0d00f0ff0d00f, 0xff0d00f0ff0d00f, 0xff0d00f0ff0d00f, 0xff0d00f0ff0d00f, 0xff0d00f0ff0d00f, 0xff0d00f0ff0d00f, 0xff0d00f0ff0d00f, 0xff0d00f0ff0d00f]),
            (Family::Quiet, 0x7, 0x123456789abcdef, 0xffffffffffffffff, Some(BusBeat::Bits24), [0xf00f4b0ff0a4f00f, 0xf0a4f00f4b0ff0a4, 0x4b0ff0a4f00f4b0f, 0xf00f4b0ff0a4f00f, 0xf0a4f00f4b0ff0a4, 0x4b0ff0a4f00f4b0f, 0xf00f4b0ff0a4f00f, 0xf0a4f00f4b0ff0a4]),
            (Family::Quiet, 0x7, 0x123456789abcdef, 0xffffffffffffffff, Some(BusBeat::Bits32), [0x74b0ff0f0b4f00f, 0x74b0ff0f0b4f00f, 0x74b0ff0f0b4f00f, 0x74b0ff0f0b4f00f, 0x74b0ff0f0b4f00f, 0x74b0ff0f0b4f00f, 0x74b0ff0f0b4f00f, 0x74b0ff0f0b4f00f]),
            (Family::Quiet, 0x7, 0x123456789abcdef, 0xffffffffffffffff, Some(BusBeat::Bits64), [0xfb00f69f0b4f00f, 0xf00ff0960f4b0ff0, 0xfb00f69f0b4f00f, 0xf00ff0960f4b0ff0, 0xfb00f69f0b4f00f, 0xf00ff0960f4b0ff0, 0xfb00f69f0b4f00f, 0xf00ff0960f4b0ff0]),
            (Family::Quiet, 0x6000003039, 0x0, 0x0, Some(BusBeat::Bits12), [0xaeb515aeb515aeb5, 0xb515aeb515aeb515, 0x15aeb515aeb515ae, 0xaeb515aeb515aeb5, 0xaeb515aeb515aeb5, 0xb515aeb515aeb515, 0x15aeb515aeb515ae, 0xaeb515aeb515aeb5]),
            (Family::Quiet, 0x6000003039, 0x0, 0x0, Some(BusBeat::Bits16), [0xe17a1ea5e17a1ea5, 0xe17a1ea5e17a1ea5, 0xe17a1ea5e17a1ea5, 0xe17a1ea5e17a1ea5, 0xe17a1ea5e17a1ea5, 0xe17a1ea5e17a1ea5, 0xe17a1ea5e17a1ea5, 0xe17a1ea5e17a1ea5]),
            (Family::Quiet, 0x6000003039, 0x0, 0x0, Some(BusBeat::Bits24), [0x1fa53ce15ac31fa5, 0x5ac31fa53ce15ac3, 0x3ce15ac31fa53ce1, 0x1fa53ce15ac31fa5, 0x5ac31fa53ce15ac3, 0x3ce15ac31fa53ce1, 0x1fa53ce15ac31fa5, 0x5ac31fa53ce15ac3]),
            (Family::Quiet, 0x6000003039, 0x0, 0x0, Some(BusBeat::Bits32), [0x693ce55a96c31ea5, 0x693ce55a96c31ea5, 0x693ce55a96c31ea5, 0x693ce55a96c31ea5, 0x693ce55a96c31ea5, 0x693ce55a96c31ea5, 0x693ce55a96c31ea5, 0x693ce55a96c31ea5]),
            (Family::Quiet, 0x6000003039, 0x0, 0x0, Some(BusBeat::Bits64), [0x6978787896e31ea5, 0x96878787693ce15a, 0x6978787896e31ea5, 0x96878787693ce15a, 0x6978787896e31ea5, 0x96878787693ce15a, 0x6978787896e31ea5, 0x96878787693ce15a]),
            (Family::Quiet, 0x6000003039, 0x123456789abcdef, 0xffffffffffffffff, Some(BusBeat::Bits12), [0x2229dd2229dd2229, 0x29dd2229dd2229dd, 0xdd2229dd2229dd22, 0x2229dd2229dd2229, 0x2229dd2229dd2229, 0x29dd2229dd2229dd, 0xdd2229dd2229dd22, 0x2229dd2229dd2229]),
            (Family::Quiet, 0x6000003039, 0x123456789abcdef, 0xffffffffffffffff, Some(BusBeat::Bits16), [0x2dd2d2252dd2d225, 0x2dd2d2252dd2d225, 0x2dd2d2252dd2d225, 0x2dd2d2252dd2d225, 0x2dd2d2252dd2d225, 0x2dd2d2252dd2d225, 0x2dd2d2252dd2d225, 0x2dd2d2252dd2d225]),
            (Family::Quiet, 0x6000003039, 0x123456789abcdef, 0xffffffffffffffff, Some(BusBeat::Bits24), [0xd20d1e2dd2e1d20d, 0xd2e1d20d1e2dd2e1, 0x1e2dd2e1d20d1e2d, 0xd20d1e2dd2e1d20d, 0xd2e1d20d1e2dd2e1, 0x1e2dd2e1d20d1e2d, 0xd20d1e2dd2e1d20d, 0xd2e1d20d1e2dd2e1]),
            (Family::Quiet, 0x6000003039, 0x123456789abcdef, 0xffffffffffffffff, Some(BusBeat::Bits32), [0x4b1e2d52b4e1d22d, 0x4b1e2d52b4e1d22d, 0x4b1e2d52b4e1d22d, 0x4b1e2d52b4e1d22d, 0x4b1e2d52b4e1d22d, 0x4b1e2d52b4e1d22d, 0x4b1e2d52b4e1d22d, 0x4b1e2d52b4e1d22d]),
            (Family::Quiet, 0x6000003039, 0x123456789abcdef, 0xffffffffffffffff, Some(BusBeat::Bits64), [0xa5966969b4e1522d, 0x5a6996964b1e2dd2, 0xa5966969b4e1522d, 0x5a6996964b1e2dd2, 0xa5966969b4e1522d, 0x5a6996964b1e2dd2, 0xa5966969b4e1522d, 0x5a6996964b1e2dd2]),
            (Family::Header, 0x0, 0x0, 0x0, Some(BusBeat::Bits12), [0x0, 0xf0f0f0f0f0f0f0f0, 0xf0f0f0f0f0f0f0f0, 0xf0f0f0f0f0f0f0f0, 0x78dde6e585f416b1, 0xf0f0f0f0f0f0f0f0, 0xf0f0f0f0f0f0f0f0, 0xf0f0f0f0f0f0f0f0]),
            (Family::Header, 0x0, 0x0, 0x0, Some(BusBeat::Bits16), [0x0, 0xf0ff0f00f0ff0f0, 0xf0ff0f00f0ff0f0, 0xf0ff0f00f0ff0f0, 0x78dde6e585f416b1, 0xf0ff0f00f0ff0f0, 0xf0ff0f00f0ff0f0, 0xf0ff0f00f0ff0f0]),
            (Family::Header, 0x0, 0x0, 0x0, Some(BusBeat::Bits24), [0x0, 0xff0f0f00f0f0ff0, 0xf0f0ff0f0f00f0f, 0xf0f00f0f0ff0f0f0, 0x78dde6e585f416b1, 0xf0f0ff0f0f00f0f, 0xf0f00f0f0ff0f0f0, 0xff0f0f00f0f0ff0]),
            (Family::Header, 0x0, 0x0, 0x0, Some(BusBeat::Bits32), [0x0, 0xf0f0f0ff0f0f0f0, 0xf0f0f0ff0f0f0f0, 0xf0f0f0ff0f0f0f0, 0x78dde6e585f416b1, 0xf0f0f0ff0f0f0f0, 0xf0f0f0ff0f0f0f0, 0xf0f0f0ff0f0f0f0]),
            (Family::Header, 0x0, 0x0, 0x0, Some(BusBeat::Bits64), [0x0, 0xf0f0f0f0f0f0f0f, 0xf0f0f0f0f0f0f0f0, 0xf0f0f0f0f0f0f0f, 0x78dde6e585f416b1, 0xf0f0f0f0f0f0f0f, 0xf0f0f0f0f0f0f0f0, 0xf0f0f0f0f0f0f0f]),
            (Family::Header, 0x0, 0x123456789abcdef, 0xffffffffffffffff, Some(BusBeat::Bits12), [0xfedcba9877777777, 0xc323cdc323cdc323, 0x23cdc323cdc323cd, 0xcdc323cdc323cdc3, 0x85fed3b2032b6df1, 0xc323cdc323cdc323, 0x23cdc323cdc323cd, 0xcdc323cdc323cdc3]),
            (Family::Header, 0x0, 0x123456789abcdef, 0xffffffffffffffff, Some(BusBeat::Bits16), [0xfedcba9877777777, 0xd23c2dc3d23c2dc3, 0xd23c2dc3d23c2dc3, 0xd23c2dc3d23c2dc3, 0x85fed3b2032b6df1, 0xd23c2dc3d23c2dc3, 0xd23c2dc3d23c2dc3, 0xd23c2dc3d23c2dc3]),
            (Family::Header, 0x0, 0x123456789abcdef, 0xffffffffffffffff, Some(BusBeat::Bits24), [0xfedcba9877777777, 0x3c1e2dc3e1d23c1e, 0xe1d23c1e2dc3e1d2, 0x2dc3e1d23c1e2dc3, 0x85fed3b2032b6df1, 0xe1d23c1e2dc3e1d2, 0x2dc3e1d23c1e2dc3, 0x3c1e2dc3e1d23c1e]),
            (Family::Header, 0x0, 0x123456789abcdef, 0xffffffffffffffff, Some(BusBeat::Bits32), [0xfedcba9877777777, 0xe1e1d23c1e1e2dc3, 0xe1e1d23c1e1e2dc3, 0xe1e1d23c1e1e2dc3, 0x85fed3b2032b6df1, 0xe1e1d23c1e1e2dc3, 0xe1e1d23c1e1e2dc3, 0xe1e1d23c1e1e2dc3]),
            (Family::Header, 0x0, 0x123456789abcdef, 0xffffffffffffffff, Some(BusBeat::Bits64), [0xfedcba9877777777, 0x1e4b4ba5e1e1d23c, 0xe1b4b45a1e1e2dc3, 0x1e4b4ba5e1e1d23c, 0x85fed3b2032b6df1, 0x1e4b4ba5e1e1d23c, 0xe1b4b45a1e1e2dc3, 0x1e4b4ba5e1e1d23c]),
            (Family::Header, 0x1, 0x0, 0x0, Some(BusBeat::Bits12), [0xf1bbcdcb0be82d63, 0xa515aea515aea515, 0x15aea515aea515ae, 0xaea515aea515aea5, 0x6a99b4b19de4644d, 0xa515aea515aea515, 0x15aea515aea515ae, 0xaea515aea515aea5]),
            (Family::Header, 0x1, 0x0, 0x0, Some(BusBeat::Bits16), [0xf1bbcdcb0be82d63, 0xe15a1ea5e15a1ea5, 0xe15a1ea5e15a1ea5, 0xe15a1ea5e15a1ea5, 0x6a99b4b19de4644d, 0xe15a1ea5e15a1ea5, 0xe15a1ea5e15a1ea5, 0xe15a1ea5e15a1ea5]),
            (Family::Header, 0x1, 0x0, 0x0, Some(BusBeat::Bits24), [0xf1bbcdcb0be82d63, 0x5aa51ea55ae15aa5, 0x5ae15aa51ea55ae1, 0x1ea55ae15aa51ea5, 0x6a99b4b19de4644d, 0x5ae15aa51ea55ae1, 0x1ea55ae15aa51ea5, 0x5aa51ea55ae15aa5]),
            (Family::Header, 0x1, 0x0, 0x0, Some(BusBeat::Bits32), [0xf1bbcdcb0be82d63, 0xf5ae15af0a51ea5, 0xf5ae15af0a51ea5, 0xf5ae15af0a51ea5, 0x6a99b4b19de4644d, 0xf5ae15af0a51ea5, 0xf5ae15af0a51ea5, 0xf5ae15af0a51ea5]),
            (Family::Header, 0x1, 0x0, 0x0, Some(BusBeat::Bits64), [0xf1bbcdcb0be82d63, 0x1e0f0fb40f5ae15a, 0xe1f0f04bf0a51ea5, 0x1e0f0fb40f5ae15a, 0x6a99b4b19de4644d, 0x1e0f0fb40f5ae15a, 0xe1f0f04bf0a51ea5, 0x1e0f0fb40f5ae15a]),
            (Family::Header, 0x1, 0x123456789abcdef, 0xffffffffffffffff, Some(BusBeat::Bits12), [0xd20eccc8edf425b, 0xa555aaa555aaa555, 0x55aaa555aaa555aa, 0xaaa555aaa555aaa5, 0x944305e6156a9b0d, 0xa555aaa555aaa555, 0x55aaa555aaa555aa, 0xaaa555aaa555aaa5]),
            (Family::Header, 0x1, 0x123456789abcdef, 0xffffffffffffffff, Some(BusBeat::Bits16), [0xd20eccc8edf425b, 0xa55a5aa5a55a5aa5, 0xa55a5aa5a55a5aa5, 0xa55a5aa5a55a5aa5, 0x944305e6156a9b0d, 0xa55a5aa5a55a5aa5, 0xa55a5aa5a55a5aa5, 0xa55a5aa5a55a5aa5]),
            (Family::Header, 0x1, 0x123456789abcdef, 0xffffffffffffffff, Some(BusBeat::Bits24), [0xd20eccc8edf425b, 0x5a2d5aa5d2a55a2d, 0xd2a55a2d5aa5d2a5, 0x5aa5d2a55a2d5aa5, 0x944305e6156a9b0d, 0xd2a55a2d5aa5d2a5, 0x5aa5d2a55a2d5aa5, 0x5a2d5aa5d2a55a2d]),
            (Family::Header, 0x1, 0x123456789abcdef, 0xffffffffffffffff, Some(BusBeat::Bits32), [0xd20eccc8edf425b, 0xf0d2a55a0f2d5aa5, 0xf0d2a55a0f2d5aa5, 0xf0d2a55a0f2d5aa5, 0x944305e6156a9b0d, 0xf0d2a55a0f2d5aa5, 0xf0d2a55a0f2d5aa5, 0xf0d2a55a0f2d5aa5]),
            (Family::Header, 0x1, 0x123456789abcdef, 0xffffffffffffffff, Some(BusBeat::Bits64), [0xd20eccc8edf425b, 0xfd287d2f0d2a55a, 0xf02d782d0f2d5aa5, 0xfd287d2f0d2a55a, 0x944305e6156a9b0d, 0xfd287d2f0d2a55a, 0xf02d782d0f2d5aa5, 0xfd287d2f0d2a55a]),
            (Family::Header, 0x7, 0x0, 0x0, Some(BusBeat::Bits12), [0x9c22a0934469840b, 0xb4e4b1b4e4b1b4e4, 0xe4b1b4e4b1b4e4b1, 0xb1b4e4b1b4e4b1b4, 0x15008779c0759395, 0xb4e4b1b4e4b1b4e4, 0xe4b1b4e4b1b4e4b1, 0xb1b4e4b1b4e4b1b4]),
            (Family::Header, 0x7, 0x0, 0x0, Some(BusBeat::Bits16), [0x9c22a0934469840b, 0x1e4be1b41e4be1b4, 0x1e4be1b41e4be1b4, 0x1e4be1b41e4be1b4, 0x15008779c0759395, 0x1e4be1b41e4be1b4, 0x1e4be1b41e4be1b4, 0x1e4be1b41e4be1b4]),
            (Family::Header, 0x7, 0x0, 0x0, Some(BusBeat::Bits24), [0x9c22a0934469840b, 0x4b0fe1b4f01e4b0f, 0xf01e4b0fe1b4f01e, 0xe1b4f01e4b0fe1b4, 0x15008779c0759395, 0xf01e4b0fe1b4f01e, 0xe1b4f01e4b0fe1b4, 0x4b0fe1b4f01e4b0f]),
            (Family::Header, 0x7, 0x0, 0x0, Some(BusBeat::Bits32), [0x9c22a0934469840b, 0xd2f01e4b2d0fe1b4, 0xd2f01e4b2d0fe1b4, 0xd2f01e4b2d0fe1b4, 0x15008779c0759395, 0xd2f01e4b2d0fe1b4, 0xd2f01e4b2d0fe1b4, 0xd2f01e4b2d0fe1b4]),
            (Family::Header, 0x7, 0x0, 0x0, Some(BusBeat::Bits64), [0x9c22a0934469840b, 0x78b41e78d2f01e4b, 0x874be1872d0fe1b4, 0x78b41e78d2f01e4b, 0x15008779c0759395, 0x78b41e78d2f01e4b, 0x874be1872d0fe1b4, 0x78b41e78d2f01e4b]),
            (Family::Header, 0x7, 0x123456789abcdef, 0xffffffffffffffff, Some(BusBeat::Bits12), [0x62ba1a04034ce883, 0xfff000fff000fff, 0xff000fff000fff00, 0xfff000fff000f, 0xe9dc331eb6fcd1c5, 0xfff000fff000fff, 0xff000fff000fff00, 0xfff000fff000f]),
            (Family::Header, 0x7, 0x123456789abcdef, 0xffffffffffffffff, Some(BusBeat::Bits16), [0x62ba1a04034ce883, 0xff0f00f0ff0f00f, 0xff0f00f0ff0f00f, 0xff0f00f0ff0f00f, 0xe9dc331eb6fcd1c5, 0xff0f00f0ff0f00f, 0xff0f00f0ff0f00f, 0xff0f00f0ff0f00f]),
            (Family::Header, 0x7, 0x123456789abcdef, 0xffffffffffffffff, Some(BusBeat::Bits24), [0x62ba1a04034ce883, 0xf0b4f00f4b0ff0b4, 0x4b0ff0b4f00f4b0f, 0xf00f4b0ff0b4f00f, 0xe9dc331eb6fcd1c5, 0x4b0ff0b4f00f4b0f, 0xf00f4b0ff0b4f00f, 0xf0b4f00f4b0ff0b4]),
            (Family::Header, 0x7, 0x123456789abcdef, 0xffffffffffffffff, Some(BusBeat::Bits32), [0x62ba1a04034ce883, 0xf4b0ff0f0b4f00f, 0xf4b0ff0f0b4f00f, 0xf4b0ff0f0b4f00f, 0xe9dc331eb6fcd1c5, 0xf4b0ff0f0b4f00f, 0xf4b0ff0f0b4f00f, 0xf4b0ff0f0b4f00f]),
            (Family::Header, 0x7, 0x123456789abcdef, 0xffffffffffffffff, Some(BusBeat::Bits64), [0x62ba1a04034ce883, 0xf00ff0960f4b0ff0, 0xff00f69f0b4f00f, 0xf00ff0960f4b0ff0, 0xe9dc331eb6fcd1c5, 0xf00ff0960f4b0ff0, 0xff00f69f0b4f00f, 0xf00ff0960f4b0ff0]),
            (Family::Header, 0x6000003039, 0x0, 0x0, Some(BusBeat::Bits12), [0xe8db505a9e17d532, 0xa515aea515aea515, 0x15aea515aea515ae, 0xaea515aea515aea5, 0x61b93740124f42fc, 0xa515aea515aea515, 0x15aea515aea515ae, 0xaea515aea515aea5]),
            (Family::Header, 0x6000003039, 0x0, 0x0, Some(BusBeat::Bits16), [0xe8db505a9e17d532, 0xe15a1ea5e15a1ea5, 0xe15a1ea5e15a1ea5, 0xe15a1ea5e15a1ea5, 0x61b93740124f42fc, 0xe15a1ea5e15a1ea5, 0xe15a1ea5e15a1ea5, 0xe15a1ea5e15a1ea5]),
            (Family::Header, 0x6000003039, 0x0, 0x0, Some(BusBeat::Bits24), [0xe8db505a9e17d532, 0x5ac31ea53ce15ac3, 0x3ce15ac31ea53ce1, 0x1ea53ce15ac31ea5, 0x61b93740124f42fc, 0x3ce15ac31ea53ce1, 0x1ea53ce15ac31ea5, 0x5ac31ea53ce15ac3]),
            (Family::Header, 0x6000003039, 0x0, 0x0, Some(BusBeat::Bits32), [0xe8db505a9e17d532, 0x693ce15a96c31ea5, 0x693ce15a96c31ea5, 0x693ce15a96c31ea5, 0x61b93740124f42fc, 0x693ce15a96c31ea5, 0x693ce15a96c31ea5, 0x693ce15a96c31ea5]),
            (Family::Header, 0x6000003039, 0x0, 0x0, Some(BusBeat::Bits64), [0xe8db505a9e17d532, 0x96878787693ce15a, 0x6978787896c31ea5, 0x96878787693ce15a, 0x61b93740124f42fc, 0x96878787693ce15a, 0x6978787896c31ea5, 0x96878787693ce15a]),
            (Family::Header, 0x6000003039, 0x123456789abcdef, 0xffffffffffffffff, Some(BusBeat::Bits12), [0x16016a3d1679396a, 0x2ddd222ddd222ddd, 0xdd222ddd222ddd22, 0x222ddd222ddd222d, 0x9d2383586081c0f3, 0x2ddd222ddd222ddd, 0xdd222ddd222ddd22, 0x222ddd222ddd222d]),
            (Family::Header, 0x6000003039, 0x123456789abcdef, 0xffffffffffffffff, Some(BusBeat::Bits16), [0x16016a3d1679396a, 0x2dd2d22d2dd2d22d, 0x2dd2d22d2dd2d22d, 0x2dd2d22d2dd2d22d, 0x9d2383586081c0f3, 0x2dd2d22d2dd2d22d, 0x2dd2d22d2dd2d22d, 0x2dd2d22d2dd2d22d]),
            (Family::Header, 0x6000003039, 0x123456789abcdef, 0xffffffffffffffff, Some(BusBeat::Bits24), [0x16016a3d1679396a, 0xd2e1d22d1e2dd2e1, 0x1e2dd2e1d22d1e2d, 0xd22d1e2dd2e1d22d, 0x9d2383586081c0f3, 0x1e2dd2e1d22d1e2d, 0xd22d1e2dd2e1d22d, 0xd2e1d22d1e2dd2e1]),
            (Family::Header, 0x6000003039, 0x123456789abcdef, 0xffffffffffffffff, Some(BusBeat::Bits32), [0x16016a3d1679396a, 0x4b1e2dd2b4e1d22d, 0x4b1e2dd2b4e1d22d, 0x4b1e2dd2b4e1d22d, 0x9d2383586081c0f3, 0x4b1e2dd2b4e1d22d, 0x4b1e2dd2b4e1d22d, 0x4b1e2dd2b4e1d22d]),
            (Family::Header, 0x6000003039, 0x123456789abcdef, 0xffffffffffffffff, Some(BusBeat::Bits64), [0x16016a3d1679396a, 0x5a6996964b1e2dd2, 0xa5966969b4e1d22d, 0x5a6996964b1e2dd2, 0x9d2383586081c0f3, 0x5a6996964b1e2dd2, 0xa5966969b4e1d22d, 0x5a6996964b1e2dd2]),
            (Family::Switch, 0x6000003039, 0x0, 0x0, Some(BusBeat::Bits12), [0xaea515aea515aea5, 0xa515aea515aea515, 0x15aea515aea515ae, 0xaea515aea515aea5, 0xaea515aea515aea5, 0xa515aea515aea515, 0x15aea515aea515ae, 0xaea515aea515aea5]),
            (Family::Switch, 0x6000003039, 0x0, 0x0, Some(BusBeat::Bits16), [0xe15a1ea5e15a1ea5, 0xe15a1ea5e15a1ea5, 0xe15a1ea5e15a1ea5, 0xe15a1ea5e15a1ea5, 0xe15a1ea5e15a1ea5, 0xe15a1ea5e15a1ea5, 0xe15a1ea5e15a1ea5, 0xe15a1ea5e15a1ea5]),
            (Family::Switch, 0x6000003039, 0x0, 0x0, Some(BusBeat::Bits24), [0x1ea53ce15ac31ea5, 0x5ac31ea53ce15ac3, 0x3ce15ac31ea53ce1, 0x1ea53ce15ac31ea5, 0x5ac31ea53ce15ac3, 0x3ce15ac31ea53ce1, 0x1ea53ce15ac31ea5, 0x5ac31ea53ce15ac3]),
            (Family::Switch, 0x6000003039, 0x0, 0x0, Some(BusBeat::Bits32), [0x693ce15a96c31ea5, 0x693ce15a96c31ea5, 0x693ce15a96c31ea5, 0x693ce15a96c31ea5, 0x693ce15a96c31ea5, 0x693ce15a96c31ea5, 0x693ce15a96c31ea5, 0x693ce15a96c31ea5]),
            (Family::Switch, 0x6000003039, 0x0, 0x0, Some(BusBeat::Bits64), [0x6978787896c31ea5, 0x96878787693ce15a, 0x6978787896c31ea5, 0x96878787693ce15a, 0x6978787896c31ea5, 0x96878787693ce15a, 0x6978787896c31ea5, 0x96878787693ce15a]),
            (Family::Switch, 0x6000003039, 0x123456789abcdef, 0xffffffffffffffff, Some(BusBeat::Bits12), [0x222ddd222ddd222d, 0x2ddd222ddd222ddd, 0xdd222ddd222ddd22, 0x222ddd222ddd222d, 0x222ddd222ddd222d, 0x2ddd222ddd222ddd, 0xdd222ddd222ddd22, 0x222ddd222ddd222d]),
            (Family::Switch, 0x6000003039, 0x123456789abcdef, 0xffffffffffffffff, Some(BusBeat::Bits16), [0x2dd2d22d2dd2d22d, 0x2dd2d22d2dd2d22d, 0x2dd2d22d2dd2d22d, 0x2dd2d22d2dd2d22d, 0x2dd2d22d2dd2d22d, 0x2dd2d22d2dd2d22d, 0x2dd2d22d2dd2d22d, 0x2dd2d22d2dd2d22d]),
            (Family::Switch, 0x6000003039, 0x123456789abcdef, 0xffffffffffffffff, Some(BusBeat::Bits24), [0xd22d1e2dd2e1d22d, 0xd2e1d22d1e2dd2e1, 0x1e2dd2e1d22d1e2d, 0xd22d1e2dd2e1d22d, 0xd2e1d22d1e2dd2e1, 0x1e2dd2e1d22d1e2d, 0xd22d1e2dd2e1d22d, 0xd2e1d22d1e2dd2e1]),
            (Family::Switch, 0x6000003039, 0x123456789abcdef, 0xffffffffffffffff, Some(BusBeat::Bits32), [0x4b1e2dd2b4e1d22d, 0x4b1e2dd2b4e1d22d, 0x4b1e2dd2b4e1d22d, 0x4b1e2dd2b4e1d22d, 0x4b1e2dd2b4e1d22d, 0x4b1e2dd2b4e1d22d, 0x4b1e2dd2b4e1d22d, 0x4b1e2dd2b4e1d22d]),
            (Family::Switch, 0x6000003039, 0x123456789abcdef, 0xffffffffffffffff, Some(BusBeat::Bits64), [0xa5966969b4e1d22d, 0x5a6996964b1e2dd2, 0xa5966969b4e1d22d, 0x5a6996964b1e2dd2, 0xa5966969b4e1d22d, 0x5a6996964b1e2dd2, 0xa5966969b4e1d22d, 0x5a6996964b1e2dd2])
        ];
        for &(family, lid, key, mask, beat, words) in stage6 {
            let phase = Phase { family, key, mask, beat: beat.expect("a width") };
            assert_eq!(phase.line(lid), words, "{} line {lid:#x} key {key:#x}", phase.label());
        }
        type Row = (&'static [Family], u64, u32, u32, u32, u64, Option<BusBeat>, Family, u64, u64, Option<BusBeat>);
        let phases: &[Row] = &[
            (&[Family::Ident, Family::Switch, Family::Random, Family::Switch], 0x7, 2, 3, 4, 0, Some(BusBeat::Bits32), Family::Random, 0x754e7689d10c5f8f, 0x0, None),
            (&[Family::Ident, Family::Switch, Family::Random, Family::Switch], 0x7, 2, 3, 4, 1, Some(BusBeat::Bits32), Family::Random, 0x754e7689d10c5f8f, 0xffffffffffffffff, None),
            (&[Family::Ident, Family::Switch, Family::Random, Family::Switch], 0x7, 2, 3, 4, 2, Some(BusBeat::Bits32), Family::Switch, 0xdf4c15a2e51e595f, 0x0, Some(BusBeat::Bits32)),
            (&[Family::Ident, Family::Switch, Family::Random, Family::Switch], 0x7, 2, 3, 4, 3, Some(BusBeat::Bits32), Family::Switch, 0xdf4c15a2e51e595f, 0xffffffffffffffff, Some(BusBeat::Bits32)),
            (&[Family::Ident, Family::Switch, Family::Random, Family::Switch], 0x7, 2, 3, 4, 4, Some(BusBeat::Bits32), Family::Ident, 0xd5e47d24cc55dcb2, 0x0, None),
            (&[Family::Ident, Family::Switch, Family::Random, Family::Switch], 0x7, 2, 3, 4, 5, Some(BusBeat::Bits32), Family::Ident, 0xd5e47d24cc55dcb2, 0xffffffffffffffff, None),
            (&[Family::Ident, Family::Switch, Family::Random, Family::Switch], 0x7, 2, 3, 4, 6, Some(BusBeat::Bits32), Family::Switch, 0x5f8562d3cfe30471, 0x0, Some(BusBeat::Bits32)),
            (&[Family::Ident, Family::Switch, Family::Random, Family::Switch], 0x7, 2, 3, 4, 7, Some(BusBeat::Bits32), Family::Switch, 0x5f8562d3cfe30471, 0xffffffffffffffff, Some(BusBeat::Bits32)),
            (&[Family::Ident, Family::Switch, Family::Random, Family::Switch], 0x7, 2, 3, 4, 8, Some(BusBeat::Bits32), Family::Random, 0x43d504c2d2e69d46, 0x0, None),
            (&[Family::Ident, Family::Switch, Family::Random, Family::Switch], 0x7, 2, 3, 4, 0, None, Family::Random, 0x754e7689d10c5f8f, 0x0, None),
            (&[Family::Ident, Family::Switch, Family::Random, Family::Switch], 0x7, 2, 3, 4, 1, None, Family::Random, 0x754e7689d10c5f8f, 0xffffffffffffffff, None),
            (&[Family::Ident, Family::Switch, Family::Random, Family::Switch], 0x7, 2, 3, 4, 2, None, Family::Switch, 0xdf4c15a2e51e595f, 0x0, Some(BusBeat::Bits32)),
            (&[Family::Ident, Family::Switch, Family::Random, Family::Switch], 0x7, 2, 3, 4, 3, None, Family::Switch, 0xdf4c15a2e51e595f, 0xffffffffffffffff, Some(BusBeat::Bits32)),
            (&[Family::Ident, Family::Switch, Family::Random, Family::Switch], 0x7, 2, 3, 4, 4, None, Family::Ident, 0xd5e47d24cc55dcb2, 0x0, None),
            (&[Family::Ident, Family::Switch, Family::Random, Family::Switch], 0x7, 2, 3, 4, 5, None, Family::Ident, 0xd5e47d24cc55dcb2, 0xffffffffffffffff, None),
            (&[Family::Ident, Family::Switch, Family::Random, Family::Switch], 0x7, 2, 3, 4, 6, None, Family::Switch, 0x5f8562d3cfe30471, 0x0, Some(BusBeat::Bits64)),
            (&[Family::Ident, Family::Switch, Family::Random, Family::Switch], 0x7, 2, 3, 4, 7, None, Family::Switch, 0x5f8562d3cfe30471, 0xffffffffffffffff, Some(BusBeat::Bits64)),
            (&[Family::Ident, Family::Switch, Family::Random, Family::Switch], 0x7, 2, 3, 4, 8, None, Family::Random, 0x43d504c2d2e69d46, 0x0, None),
            (&[Family::Linepair, Family::Switch, Family::Random, Family::Switch], 0x7, 1, 3, 1, 0, Some(BusBeat::Bits32), Family::Linepair, 0x169b4bee48d6215f, 0x0, None),
            (&[Family::Linepair, Family::Switch, Family::Random, Family::Switch], 0x7, 1, 3, 1, 1, Some(BusBeat::Bits32), Family::Linepair, 0x169b4bee48d6215f, 0xffffffffffffffff, None),
            (&[Family::Linepair, Family::Switch, Family::Random, Family::Switch], 0x7, 1, 3, 1, 2, Some(BusBeat::Bits32), Family::Switch, 0xe95da78feac29051, 0x0, Some(BusBeat::Bits32)),
            (&[Family::Linepair, Family::Switch, Family::Random, Family::Switch], 0x7, 1, 3, 1, 3, Some(BusBeat::Bits32), Family::Switch, 0xe95da78feac29051, 0xffffffffffffffff, Some(BusBeat::Bits32)),
            (&[Family::Linepair, Family::Switch, Family::Random, Family::Switch], 0x7, 1, 3, 1, 4, Some(BusBeat::Bits32), Family::Random, 0xd585751a1d2945e8, 0x0, None),
            (&[Family::Linepair, Family::Switch, Family::Random, Family::Switch], 0x7, 1, 3, 1, 0, None, Family::Linepair, 0x169b4bee48d6215f, 0x0, None),
            (&[Family::Linepair, Family::Switch, Family::Random, Family::Switch], 0x7, 1, 3, 1, 1, None, Family::Linepair, 0x169b4bee48d6215f, 0xffffffffffffffff, None),
            (&[Family::Linepair, Family::Switch, Family::Random, Family::Switch], 0x7, 1, 3, 1, 2, None, Family::Switch, 0xe95da78feac29051, 0x0, Some(BusBeat::Bits64)),
            (&[Family::Linepair, Family::Switch, Family::Random, Family::Switch], 0x7, 1, 3, 1, 3, None, Family::Switch, 0xe95da78feac29051, 0xffffffffffffffff, Some(BusBeat::Bits64)),
            (&[Family::Linepair, Family::Switch, Family::Random, Family::Switch], 0x7, 1, 3, 1, 4, None, Family::Random, 0xd585751a1d2945e8, 0x0, None),
            (&[Family::Ident, Family::Switch, Family::Random, Family::Switch], 0x0, 0, 0, 4, 0, None, Family::Ident, 0x44f96f0d96abc5e1, 0x0, None),
            (&[Family::Ident, Family::Switch, Family::Random, Family::Switch], 0x0, 0, 0, 4, 1, None, Family::Ident, 0x44f96f0d96abc5e1, 0xffffffffffffffff, None),
            (&[Family::Ident, Family::Switch, Family::Random, Family::Switch], 0x0, 0, 0, 4, 2, None, Family::Switch, 0xf1972f70ba1b6d5b, 0x0, Some(BusBeat::Bits64)),
            (&[Family::Ident, Family::Switch, Family::Random, Family::Switch], 0x0, 0, 0, 4, 3, None, Family::Switch, 0xf1972f70ba1b6d5b, 0xffffffffffffffff, Some(BusBeat::Bits64)),
            (&[Family::Lines, Family::Switch, Family::Random, Family::Switch], 0x123456789abcdef, 3, 1, 3, 0, Some(BusBeat::Bits64), Family::Lines, 0x9a45be22236282fe, 0x0, None),
            (&[Family::Lines, Family::Switch, Family::Random, Family::Switch], 0x123456789abcdef, 3, 1, 3, 1, Some(BusBeat::Bits64), Family::Lines, 0x9a45be22236282fe, 0xffffffffffffffff, None),
            (&[Family::Lines, Family::Switch, Family::Random, Family::Switch], 0x123456789abcdef, 3, 1, 3, 2, Some(BusBeat::Bits64), Family::Switch, 0xf7bd6fa7e2d094b3, 0x0, Some(BusBeat::Bits64)),
            (&[Family::Lines, Family::Switch, Family::Random, Family::Switch], 0x123456789abcdef, 3, 1, 3, 3, Some(BusBeat::Bits64), Family::Switch, 0xf7bd6fa7e2d094b3, 0xffffffffffffffff, Some(BusBeat::Bits64)),
            (&[Family::Lines, Family::Switch, Family::Random, Family::Switch], 0x123456789abcdef, 3, 1, 3, 4, Some(BusBeat::Bits64), Family::Random, 0x49cb70c87d2032cf, 0x0, None),
            (&[Family::Lines, Family::Switch, Family::Random, Family::Switch], 0x123456789abcdef, 3, 1, 3, 5, Some(BusBeat::Bits64), Family::Random, 0x49cb70c87d2032cf, 0xffffffffffffffff, None),
            (&[Family::Switch], 0x1, 5, 0, 4, 0, Some(BusBeat::Bits16), Family::Switch, 0x74ae64b125831171, 0x0, Some(BusBeat::Bits16)),
            (&[Family::Switch], 0x1, 5, 0, 4, 1, Some(BusBeat::Bits16), Family::Switch, 0x74ae64b125831171, 0xffffffffffffffff, Some(BusBeat::Bits16)),
            (&[Family::Switch], 0x1, 5, 0, 4, 2, Some(BusBeat::Bits16), Family::Switch, 0xdec70fbb424d56ba, 0x0, Some(BusBeat::Bits16)),
            (&[Family::Switch], 0x1, 5, 0, 4, 3, Some(BusBeat::Bits16), Family::Switch, 0xdec70fbb424d56ba, 0xffffffffffffffff, Some(BusBeat::Bits16)),
            (&[Family::Switch], 0x1, 5, 0, 4, 0, None, Family::Switch, 0x74ae64b125831171, 0x0, Some(BusBeat::Bits64)),
            (&[Family::Switch], 0x1, 5, 0, 4, 1, None, Family::Switch, 0x74ae64b125831171, 0xffffffffffffffff, Some(BusBeat::Bits64)),
            (&[Family::Switch], 0x1, 5, 0, 4, 2, None, Family::Switch, 0xdec70fbb424d56ba, 0x0, Some(BusBeat::Bits32)),
            (&[Family::Switch], 0x1, 5, 0, 4, 3, None, Family::Switch, 0xdec70fbb424d56ba, 0xffffffffffffffff, Some(BusBeat::Bits32)),
            (&[Family::Ident, Family::Random, Family::Lines], 0x7, 4, 2, 4, 0, None, Family::Ident, 0x79d2a9c9ca518ffe, 0x0, None),
            (&[Family::Ident, Family::Random, Family::Lines], 0x7, 4, 2, 4, 1, None, Family::Ident, 0x79d2a9c9ca518ffe, 0xffffffffffffffff, None),
            (&[Family::Ident, Family::Random, Family::Lines], 0x7, 4, 2, 4, 2, None, Family::Random, 0xde80854296ef83f3, 0x0, None),
            (&[Family::Ident, Family::Random, Family::Lines], 0x7, 4, 2, 4, 3, None, Family::Random, 0xde80854296ef83f3, 0xffffffffffffffff, None)
        ];
        for &(families, seed, suite_cycle, step, wrc, g, beat, family, key, mask, beat_out) in phases {
            let program = Program::new(families).unwrap();
            let phase = program.phase(&StepEpoch { seed, suite_cycle, step }, wrc, g, Schedule::Rounds, beat);
            let what = format!("{} cycle {suite_cycle} wrc {wrc} g {g} beat {beat:?}", program.name());
            assert_eq!((phase.family, phase.key, phase.mask), (family, key, mask), "{what}");
            if let Some(b) = beat_out {
                assert_eq!(phase.beat, b, "{what}");
            }
        }
    }

    #[test]
    fn the_macro_matches_phase_line() {
        for phase in phases() {
            let got: Vec<[u64; LINE_WORDS]> = with_phase_line!(phase, |line| { (0..64u64).map(line).collect() });
            let want: Vec<[u64; LINE_WORDS]> = (0..64u64).map(|lid| phase.line(lid)).collect();
            assert_eq!(got, want, "{}", phase.label());
        }
    }

    /// Fixed words, so a change to a formula fails here, not only against the reference.
    #[test]
    fn known_answers() {
        let p = |family, key, mask, beat| Phase { family, key, mask, beat };
        assert_eq!(p(Family::Ident, 0, 0, BusBeat::Bits64).line(0)[0], 0);
        assert_eq!(p(Family::Random, 0, 0, BusBeat::Bits64).line(0)[1], fmix(1));
        assert_eq!(fmix(1), 0x5692_161d_100b_05e5);
        assert_eq!(p(Family::Ident, 0, !0, BusBeat::Bits64).line(0)[0], !0);
        let s = p(Family::Switch, 7, 0, BusBeat::Bits32).line(3);
        assert!(s.iter().all(|&w| w == s[0]) && (s[0] >> 32) == (!s[0] & 0xFFFF_FFFF));
    }

    #[test]
    fn identity_families_are_distinct_across_threads() {
        for family in [Family::Ident, Family::Random] {
            let phase = Phase { family, key: 0xC0FF_EE00_DEAD_BEEF, mask: 0, beat: BusBeat::Bits64 };
            let mut seen = std::collections::HashSet::new();
            for thread in 0..4 {
                for n in 0..20_000u64 {
                    for w in phase.line(line_base(thread) + n) {
                        assert!(seen.insert(w), "{}: thread {thread} line {n} repeats a value", family.name());
                    }
                }
            }
        }
    }

    #[test]
    fn a_pair_complements_every_bit() {
        let epoch = StepEpoch { seed: 42, suite_cycle: 3, step: 5 };
        for program in [Program::default_simple(), Program::from_tm5_mode(0).unwrap(), Program::from_tm5_mode(1).unwrap(),
                        Program::single(Family::Lines)] {
            for g in (0..16u64).step_by(2) {
                let (p, q) = (program.phase(&epoch, 4, g, Schedule::Rounds, None), program.phase(&epoch, 4, g + 1, Schedule::Rounds, None));
                assert_eq!((p.family, p.key, p.beat), (q.family, q.key, q.beat), "round {g}");
                assert_eq!((p.mask, q.mask), (0, !0));
                for lid in 0..32 {
                    let (a, b) = (p.line(lid), q.line(lid));
                    assert!(a.iter().zip(&b).all(|(x, y)| *x == !*y), "{} line {lid}", p.label());
                }
            }
        }
    }

    /// Every pair is completed (Astra's review: with 3 rounds `switch` got P every cycle and never
    /// ~P). Even rounds a chunk, over any number of its own cycles, end each cycle on whole pairs
    /// and run every family; a verify-only step pairs two suite cycles under one key.
    #[test]
    fn every_pair_is_completed() {
        let program = Program::from_tm5_mode(0).unwrap();
        let epoch = |cycle| StepEpoch { seed: 3, suite_cycle: cycle, step: 1 };
        for wrc in [2u32, 4, 6] {
            for own_cycles in 1..=3u64 {
                // Per family and key: P and ~P written
                let mut written: std::collections::HashMap<(Family, u64), [bool; 2]> = Default::default();
                for cycle in 1..=16 {
                    for g in 0..own_cycles * u64::from(wrc) {
                        let p = program.phase(&epoch(cycle), wrc, g, Schedule::Rounds, None);
                        written.entry((p.family, p.key)).or_default()[usize::from(p.mask != 0)] = true;
                        if g % 2 == 1 {
                            assert_eq!(p.key, program.phase(&epoch(cycle), wrc, g - 1, Schedule::Rounds, None).key, "~P under its P's key");
                        }
                    }
                }
                assert!(written.values().all(|w| w[0] && w[1]), "wrc {wrc}, {own_cycles} cycles: a pair without its ~P");
                let families: std::collections::HashSet<Family> = written.keys().map(|k| k.0).collect();
                assert_eq!(families.len(), 3, "wrc {wrc}: every family of the program runs");
            }
        }
        // Verify-only: suite cycles 1 and 2 are a pair, 3 starts the next
        let once = |cycle| program.phase(&epoch(cycle), 4, 0, Schedule::OncePerRun, None);
        let (p, q) = (once(1), once(2));
        assert_eq!((p.key, p.family, p.mask, q.mask), (q.key, q.family, 0, !0));
        assert_ne!(once(3).key, p.key);
        let families: std::collections::HashSet<Family> = (1..=8).map(|c| once(c).family).collect();
        assert_eq!(families.len(), 3, "verify-only: every family of the program runs");
    }

    #[test]
    fn programs_rotate_over_rounds_and_suite_cycles() {
        // A TM5 mode-0 import: four entries, switch twice
        let program = Program::from_tm5_mode(0).unwrap();
        let at = |cycle, g| program.phase(&StepEpoch { seed: 1, suite_cycle: cycle, step: 0 }, 4, g, Schedule::Rounds, Some(BusBeat::Bits32)).family;
        // A TM5 step: one cycle of its own, rounds 0-3, two pairs a suite cycle
        assert_eq!([at(1, 0), at(1, 2), at(2, 0), at(2, 2), at(3, 0)],
                   [Family::Ident, Family::Switch, Family::Random, Family::Switch, Family::Ident]);
        // Keys differ by suite cycle, step and pair; never by thread
        let key = |cycle, step, g| program.phase(&StepEpoch { seed: 1, suite_cycle: cycle, step }, 4, g, Schedule::Rounds, None).key;
        let keys = [key(1, 0, 0), key(3, 0, 0), key(1, 1, 0), key(1, 0, 4)];
        assert_eq!(keys.iter().collect::<std::collections::HashSet<_>>().len(), keys.len());
    }

    /// Every pattern number is one pattern's: the native families' sit in the numbered modes' groups
    /// of ten (1x identities, 2x in-line arithmetic, 3x switching), and none is a numbered mode or
    /// 12 or 14, the old numbers of 20 and 30 (TODO 97).
    #[test]
    fn pattern_numbers_are_grouped_and_unique() {
        let group = |n: u32| n / 10;
        for f in Family::ALL {
            assert_eq!(Family::from_number(f.number()), Some(f));
            assert!(!crate::pattern_gen::MODES.contains(&f.number()) && ![12, 14].contains(&f.number()), "{}", f.name());
            let want = match f {
                Family::Ident | Family::Random | Family::Linepair => 1,
                Family::Lines => 2,
                Family::Switch | Family::Quiet | Family::Header => 3,
            };
            assert_eq!(group(f.number()), want, "{}", f.name());
        }
        assert_eq!(crate::pattern_gen::MODES.map(group), [0, 0, 0, 1, 1, 1, 2, 3]);
        assert!(is_pattern_mode(31) && is_pattern_mode(20) && !is_pattern_mode(12) && !is_pattern_mode(14));
        assert!(pattern_modes().ends_with("15 ident, 16 random, 17 linepair, 21 lines, 31 switch, 32 quiet, 33 header"), "{}", pattern_modes());
    }

    /// `quiet` holds one pin through each line's transfers, 1 under P, and every other pin flips on
    /// every beat; over lines every pin of the width is held, about equally often (TODO 97).
    #[test]
    fn quiet_holds_one_pin_and_flips_the_rest() {
        for beat in BEATS {
            let (bits, burst) = (beat.bits() as usize, beat.burst_bits() as usize);
            let mut held_count = vec![0usize; bits];
            for lid in 0..4096u64 {
                let line = quiet_line(lid, 0x5EED, beat);
                let bit = |p: usize| (line[p / 64] >> (p % 64)) & 1;
                // Each transfer's bits by pin, within each burst; the last transfer may be partial
                let mut held = None;
                for b in 0..512 / burst {
                    for pin in 0..bits {
                        let places: Vec<usize> = (pin..burst).step_by(bits).map(|q| b * burst + q).collect();
                        let toggles = places.windows(2).all(|w| bit(w[0]) != bit(w[1]));
                        let steady = places.iter().all(|&p| bit(p) == 1);
                        assert!(toggles != steady, "{} bits, line {lid}, burst {b}, pin {pin}: neither held nor flipping", bits);
                        if steady {
                            assert!(held.is_none_or(|h| h == pin), "{} bits, line {lid}: two held pins", bits);
                            held = Some(pin);
                        }
                    }
                }
                held_count[held.expect("a held pin")] += 1;
            }
            let (lo, hi) = (*held_count.iter().min().unwrap(), *held_count.iter().max().unwrap());
            assert!(lo > 0 && hi < 3 * lo, "{} bits: pins held {lo} to {hi} times in 4096 lines", bits);
        }
    }

    /// Every 32 B half of a `header` line differs from every other's in the run, as no `switch`
    /// line's do, and its other six words are `switch`'s (TODO 97).
    #[test]
    fn header_halves_are_distinct_and_the_rest_switches() {
        for beat in BEATS {
            let mut heads = std::collections::HashSet::new();
            for lid in (0..2048u64).chain(line_base(3)..line_base(3) + 2048) {
                let (line, switch) = (header_line(lid, 7, beat), switch_line(lid, 7, beat));
                assert!(heads.insert(line[0]) && heads.insert(line[4]), "{} bits, line {lid}: a repeated head", beat.bits());
                assert!((1..8).filter(|j| j % 4 != 0).all(|j| line[j] == switch[j]));
            }
        }
    }

    /// The default program runs a `random` pair and a `switch` pair every suite cycle at TM5's four
    /// rounds, and every other cycle at the built-in suite's two.
    #[test]
    fn the_default_program_is_random_and_switch() {
        let program = Program::default_simple();
        assert_eq!(program.families(), [Family::Random, Family::Switch]);
        let at = |cycle, wrc, g| program.phase(&StepEpoch { seed: 1, suite_cycle: cycle, step: 0 }, wrc, g, Schedule::Rounds, None).family;
        for cycle in 1..=4 {
            assert_eq!([at(cycle, 4, 0), at(cycle, 4, 2)], [Family::Random, Family::Switch], "cycle {cycle}, 4 rounds");
        }
        assert_eq!([at(1, 2, 0), at(2, 2, 0), at(3, 2, 0)], [Family::Random, Family::Switch, Family::Random]);
    }

    /// Whatever the program's shape, successive `switch` pairs alternate 64 and 32 on an unknown
    /// bus, so neither DDR4 nor DDR5 goes without switching (the independent implementation found
    /// `(pair >> 1) & 1` fixed the width for `[ident, random, switch, switch]`).
    #[test]
    fn unknown_width_alternates_the_switch_profiles() {
        use Family::*;
        let epoch = StepEpoch { seed: 9, suite_cycle: 1, step: 0 };
        for families in [&[Ident, Switch][..], &[Switch], &[Ident, Switch, Random, Switch], &[Ident, Random, Switch, Switch],
                         &[Switch, Ident, Random, Lines], &[Lines, Switch, Random]] {
            let program = Program::new(families).unwrap();
            let beats: Vec<u32> = (0..24u64).filter_map(|pair| {
                let phase = program.phase(&epoch, 4, pair * 2, Schedule::Rounds, None);
                (phase.family == Family::Switch).then(|| phase.beat.bits())
            }).collect();
            assert!(beats.len() >= 4 && beats.chunks(2).all(|p| p.len() < 2 || p == [64, 32]), "{}: {beats:?}", program.name());
        }
    }

    /// Every pin of every burst toggles at every transfer, at every width and polarity; at 16, 32
    /// and 64 bits every byte holds four 1s.
    #[test]
    fn switch_flips_every_beat_and_keeps_bytes_balanced() {
        for beat in BEATS {
            for mask in [0, !0] {
                let phase = Phase { family: Family::Switch, key: 0xABCD, mask, beat };
                for lid in 0..256 {
                    let words = phase.line(lid);
                    assert!(crate::pattern_gen::tests::flips_every_transfer(&words, beat), "{}-bit line {lid}", beat.bits());
                    if !beat.modelled() {
                        assert!(words.iter().all(|w| w.to_le_bytes().iter().all(|b| b.count_ones() == 4)));
                    }
                }
            }
        }
    }

    #[test]
    fn parsing() {
        assert_eq!(Program::parse(&["Ident", "switch"]).unwrap().name(), "ident+switch");
        assert!(Program::parse(&["ident", "nope"]).is_err());
        assert!(Program::parse::<&str>(&[]).is_err());
        assert_eq!(parse_seed("0x10").unwrap(), 16);
        assert_eq!(parse_seed("10").unwrap(), 10);
        assert!(parse_seed("ten").is_err());
        assert_eq!(parse_beat("32").unwrap(), Some(BusBeat::Bits32));
        assert_eq!(parse_beat("auto").unwrap(), None);
        assert_eq!(parse_beat("24").unwrap(), Some(BusBeat::Bits24));
        assert_eq!(parse_beat("DDR6").unwrap(), Some(BusBeat::Bits24));
        assert_eq!(parse_beat("lpddr6").unwrap(), Some(BusBeat::Bits12));
        assert_eq!(parse_beat("lpddr5").unwrap(), Some(BusBeat::Bits16));
        assert_eq!(parse_beat("ddr4").unwrap(), Some(BusBeat::Bits64));
        assert!(parse_beat("20").is_err());
        assert!(parse_beat("ddr7").is_err());
        assert_eq!(Program::from_tm5_mode(2).unwrap().families()[0], Family::Lines);
        assert!(Program::from_tm5_mode(3).is_none());
    }
}
