//! The seal (TODO 74): TMR's take on TM5's test 0.
//!
//! **What it is for.** Every test checks its own data almost as soon as it writes it. The seal
//! checks data that has sat untouched for a long time while heavy traffic hit the rest of memory:
//! retention under load, sensitive to refresh timings, heat and neighbouring-row disturbance. It
//! also catches a test that wrote outside its own chunk. TM5 calls these errors "test 0".
//!
//! **How a run uses it.** At the start of each cycle every thread seals its whole span. Then for
//! each step, around each chunk of a correctness test, the harness checks the seal before the test
//! works the chunk and reseals it after; the chunk then sits sealed until the next step reaches it,
//! about one step later. A mirror test works on the sealed data itself and the seal check after it
//! is its verify (TM5's `Capable_UseTst0ForGenAndCheck`). Bandwidth, latency, Bench and Mem-Random
//! steps never take it: the worker checks what they will overwrite before they run and reseals it
//! before the next sealed step. The cycle ends with a final check of all memory.
//!
//! **The patterns.** Both depend only on the word's address and the cycle, so any chunk layout
//! works, and in both each 8 B word is followed by its inverse. On a 64-bit bus that flips every
//! data line on every other transfer. On DDR5's 32-bit beats it flips about half, as random data
//! does:
//! - `tmr` (the default): each 16 B holds `[v, !v]`, where `v` is a 63-bit bijective hash of the
//!   16 B unit's address and the cycle's key, so `v` has its top bit clear and `!v` set: no two
//!   words anywhere hold the same value, and a write that lands at the wrong address always shows.
//! - `tm5`: TM5's `RS_GeneratePattern`, bit for bit in odd cycles (64-bit bus): per 4 KiB page a
//!   16-bit word from the page number, written as a 48 B motif `[q0, !q0, q1, !q1, q2, !q2]`
//!   repeated across the page. Different pages can share it (page 1 and page 65,536 do), and inside
//!   a page it repeats every 48 B.
//!
//! **Cycles.** Each cycle seals with its own kernel (`SealKernel::for_cycle`). Cycles go in pairs:
//! `tmr` takes a key per pair, and the pair's second cycle inverts every word (`tm5` too). So every
//! sealed bit holds 1 for one cycle's dwell and 0 for the other's, and memory still holding the
//! last cycle's seal fails this cycle's check: inverted within a pair, another key across pairs.
//! One kernel serves a whole cycle, from the seal at its start to the final check at its end, so
//! each check expects what the step before it sealed.
//!
//! **The kernels.** Fill (non-temporal stores, so the seal lands in DRAM rather than waiting in
//! cache) and check (XOR/OR into accumulators; a hit rescans to count and log each bad word),
//! stamped per width (128/256/512) with `macro_rules!`. One call per chunk, never per word.

use std::simd::num::SimdUint;
use std::simd::{u64x2, u64x4, u64x8};

/// Which seal pattern a run writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SealPattern {
    #[default]
    Tmr,
    Tm5,
}

/// The seal kernels' SIMD width; `Auto` is the widest the CPU has.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SealWidth {
    #[default]
    Auto,
    W128,
    W256,
    W512,
}

impl SealWidth {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_lowercase().as_str() {
            "auto" => Ok(SealWidth::Auto),
            "128" => Ok(SealWidth::W128),
            "256" => Ok(SealWidth::W256),
            "512" => Ok(SealWidth::W512),
            other => Err(format!("seal width '{other}': use auto, 128, 256 or 512")),
        }
    }

    /// The width this CPU runs: `Auto` resolved, or an error if the CPU lacks the asked-for one.
    ///
    /// `Auto` is 256, not 512, where AVX2 is there: on the dev box (Xeon 6975P, 2026-10-06, 28 GiB,
    /// 4 threads) every width fills at ~90 GiB/s (the non-temporal store ceiling), but the 512-bit
    /// check ran at 52-55 GiB/s against 58-63 for 128 and 256. The Bench-*-Seal-* pairs measure it.
    pub fn resolve(self) -> Result<Self, String> {
        let avx512 = is_x86_feature_detected!("avx512f");
        let avx2 = is_x86_feature_detected!("avx2");
        match self {
            SealWidth::Auto if avx2 => Ok(SealWidth::W256),
            SealWidth::Auto => Ok(SealWidth::W128),
            SealWidth::W512 if !avx512 => Err("seal width 512 needs AVX-512, which this CPU lacks".to_string()),
            SealWidth::W256 if !avx2 => Err("seal width 256 needs AVX2, which this CPU lacks".to_string()),
            w => Ok(w),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            SealWidth::Auto => "auto",
            SealWidth::W128 => "128",
            SealWidth::W256 => "256",
            SealWidth::W512 => "512",
        }
    }
}

impl SealPattern {
    pub fn name(self) -> &'static str {
        match self {
            SealPattern::Tmr => "tmr",
            SealPattern::Tm5 => "tm5",
        }
    }
}

/// A seal pattern at a resolved width, for one cycle: what one fill or check call runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SealKernel {
    pub pattern: SealPattern,
    /// Never `Auto`: resolved when the run starts.
    pub width: SealWidth,
    /// The cycle's `tmr` key (`tm5` has none), added to each unit's hash.
    pub key: u64,
    /// The cycle's polarity: every word inverted.
    pub invert: bool,
}

impl SealKernel {
    /// The default kernel on this CPU: `tmr` at the widest width, key 0, not inverted.
    pub fn detect() -> Self {
        SealKernel::new(SealPattern::Tmr, SealWidth::Auto.resolve().unwrap_or(SealWidth::W128))
    }

    /// A pattern at a width, key 0 and not inverted: the kernel outside a run (the Bench pairs,
    /// plan-time steps).
    pub fn new(pattern: SealPattern, width: SealWidth) -> Self {
        SealKernel { pattern, width, key: 0, invert: false }
    }

    /// This kernel for suite cycle `cycle` (from 1) of a run seeded `seed`: cycles `2n - 1` and
    /// `2n` share a key, and the even one is inverted, so each bit takes both values over the pair.
    pub fn for_cycle(self, seed: u64, cycle: u64) -> Self {
        let pair = cycle.div_ceil(2);
        let key = crate::pattern::fmix(seed ^ crate::pattern::fmix(pair.wrapping_add(0x5EA1)));
        SealKernel { key, invert: cycle.is_multiple_of(2), ..self }
    }

    /// The seal word at byte address `addr` (a multiple of 8).
    #[inline(always)]
    pub fn word(self, addr: usize) -> u64 {
        let w = match self.pattern {
            SealPattern::Tmr => tmr_word(addr, self.key),
            SealPattern::Tm5 => tm5_word(addr),
        };
        if self.invert { !w } else { w }
    }

    pub fn name(self) -> String {
        format!("{}-{}", self.pattern.name(), self.width.name())
    }

    /// Seal the words `[start, end)` of `ptr`. Both ends must be 64 B aligned addresses.
    ///
    /// # Safety
    /// `ptr.add(start)..ptr.add(end)` must be valid for writes, and the width's instructions
    /// available (`SealWidth::resolve`).
    pub unsafe fn fill(self, ptr: *mut u64, start: usize, end: usize) {
        debug_assert!((ptr.add(start) as usize).is_multiple_of(64) && (end - start).is_multiple_of(8), "unaligned seal range");
        let (key, invert) = (self.key, self.invert_mask());
        match (self.pattern, self.width) {
            (SealPattern::Tmr, SealWidth::W512) => tmr_fill_512(ptr, start, end, key, invert),
            (SealPattern::Tmr, SealWidth::W256) => tmr_fill_256(ptr, start, end, key, invert),
            (SealPattern::Tmr, _) => tmr_fill_128(ptr, start, end, key, invert),
            (SealPattern::Tm5, SealWidth::W512) => tm5_fill_512(ptr, start, end, invert),
            (SealPattern::Tm5, SealWidth::W256) => tm5_fill_256(ptr, start, end, invert),
            (SealPattern::Tm5, _) => tm5_fill_128(ptr, start, end, invert),
        }
    }

    /// The polarity as a mask XORed into every word.
    fn invert_mask(self) -> u64 {
        if self.invert { !0 } else { 0 }
    }

    /// Check the words `[start, end)` of `ptr` hold the seal; returns the bad words, the first 10
    /// logged as found by `what` (e.g. "seal check before") and the thread's error context. The
    /// seal's own check: its errors are seal errors, not in the test's alarms.
    ///
    /// # Safety
    /// As `fill`, for reads.
    pub unsafe fn check(self, ptr: *const u64, start: usize, end: usize, what: &str) -> u64 {
        self.check_as(ptr, start, end, what, false)
    }

    /// `check` as a test's own verify (a mirror's, Seal-Check's, Bench-Verify-Seal's): a failed
    /// check is one of the test's alarms.
    ///
    /// # Safety
    /// As `check`.
    pub unsafe fn verify(self, ptr: *const u64, start: usize, end: usize, what: &str) -> u64 {
        self.check_as(ptr, start, end, what, true)
    }

    unsafe fn check_as(self, ptr: *const u64, start: usize, end: usize, what: &str, count: bool) -> u64 {
        debug_assert!((ptr.add(start) as usize).is_multiple_of(64) && (end - start).is_multiple_of(8), "unaligned seal range");
        let (key, invert) = (self.key, self.invert_mask());
        let seen = match (self.pattern, self.width) {
            (SealPattern::Tmr, SealWidth::W512) => tmr_check_512(ptr, start, end, key, invert),
            (SealPattern::Tmr, SealWidth::W256) => tmr_check_256(ptr, start, end, key, invert),
            (SealPattern::Tmr, _) => tmr_check_128(ptr, start, end, key, invert),
            (SealPattern::Tm5, SealWidth::W512) => tm5_check_512(ptr, start, end, invert),
            (SealPattern::Tm5, SealWidth::W256) => tm5_check_256(ptr, start, end, invert),
            (SealPattern::Tm5, _) => tm5_check_128(ptr, start, end, invert),
        };
        if seen == 0 {
            return 0;
        }
        rescan(self, ptr, start, end, what, seen, count)
    }
}

/// The seal's width setting: `seal_width` when given (`seal-width=`, else a config's
/// `system.seal_width`), else the run's `width=` (`width`, `crate::simd::width_given`), else
/// `auto`, the seal's own rule (`SealWidth::resolve`).
pub fn width_setting(seal_width: Option<&str>, width: Option<crate::simd::Width>) -> &str {
    seal_width.unwrap_or_else(|| width.map_or("auto", crate::simd::Width::name))
}

/// The run's seal setting (`seal=` / `system.seal`, `seal-width=` / `system.seal_width`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SealSettings {
    /// `false` for `off`: no step is wrapped, and the run neither seals nor checks memory.
    pub on: bool,
    pub pattern: SealPattern,
    pub width: SealWidth,
}

impl SealSettings {
    /// `seal` is `tmr`, `tm5` or `off`; `width` is `auto`, `128`, `256` or `512`. `off` keeps the
    /// `tmr` pattern for the mirror tests, whose data it is either way.
    pub fn parse(seal: &str, width: &str) -> Result<Self, String> {
        let (on, pattern) = match seal.trim().to_lowercase().as_str() {
            "tmr" => (true, SealPattern::Tmr),
            "tm5" => (true, SealPattern::Tm5),
            "off" => (false, SealPattern::Tmr),
            other => return Err(format!("seal '{other}': use tmr, tm5 or off")),
        };
        Ok(SealSettings { on, pattern, width: SealWidth::parse(width)? })
    }

    /// The kernel this CPU runs, or why the asked-for width can't run.
    pub fn kernel(&self) -> Result<SealKernel, String> {
        Ok(SealKernel::new(self.pattern, self.width.resolve()?))
    }
}

/// The seal as one step runs it, set per step by the runner (TODO 74).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SealStep {
    /// The run's seal pattern and width. A mirror test's data is the seal even in an unsealed run.
    pub kernel: SealKernel,
    /// The run is sealed.
    pub on: bool,
    /// The step's memory holds the seal when it starts: the worker reseals what an unsealed step
    /// before it left. Every step but one that never takes the seal, in a sealed run.
    pub expects: bool,
    /// Check each chunk before the test works it and reseal it after (a correctness test in a
    /// sealed run that hasn't opted out).
    pub wrap: bool,
}

impl Default for SealStep {
    /// Unsealed, with the default kernel: how a test runs outside a sealed run (unit tests).
    fn default() -> Self {
        SealStep { kernel: SealKernel::detect(), on: false, expects: false, wrap: false }
    }
}

// ─── The patterns, one word at a time (the reference the kernels must match) ─────────────────────

/// Odd, so `k * SEAL_MUL + key` is a bijection mod 2^63; `x ^ (x >> 32)` is one too on 63 bits.
const SEAL_MUL: u64 = 0x9E37_79B9_7F4A_7C15;
/// `tmr`'s hash keeps 63 bits, so `v` has its top bit clear and `!v` set and the two never meet.
const LOW63: u64 = u64::MAX >> 1;

/// The `tmr` seal word at byte address `addr` (a multiple of 8) under `key`, before the cycle's
/// inversion: `v` for the unit's first 8 B, `!v` for its second.
#[inline(always)]
pub fn tmr_word(addr: usize, key: u64) -> u64 {
    let x = ((addr >> 4) as u64).wrapping_mul(SEAL_MUL).wrapping_add(key) & LOW63;
    let v = x ^ (x >> 32);
    if addr & 8 == 0 { v } else { !v }
}

/// TM5's 48 B motif for the 4 KiB page at `page` (`RS_GeneratePattern`, `mtests0.asm` ~1714,
/// 64-bit bus): a 16-bit word `d` from the page number, then words `rol16(d, n)` for n = 0..11,
/// four to a qword, each qword followed by its inverse.
#[inline(always)]
pub fn tm5_motif(page: usize) -> [u64; 6] {
    let d = ((page >> 12).wrapping_add(page >> 28)) as u16;
    let q = |k: u32| (0..4).fold(0u64, |acc, j| acc | (d.rotate_left(4 * k + j) as u64) << (16 * j));
    let (q0, q1, q2) = (q(0), q(1), q(2));
    [q0, !q0, q1, !q1, q2, !q2]
}

/// The `tm5` seal word at byte address `addr` (a multiple of 8).
#[inline(always)]
pub fn tm5_word(addr: usize) -> u64 {
    tm5_motif(addr & !0xFFF)[((addr & 0xFFF) >> 3) % 6]
}

/// Counts and logs the words of `[start, end)` that don't hold the seal, once a kernel's
/// accumulator has seen one (the tests' cold path, `crate::tests::Rescan`). A fault gone by the
/// rescan still counts once.
#[cold]
#[inline(never)]
unsafe fn rescan(kernel: SealKernel, ptr: *const u64, start: usize, end: usize, what: &str, seen: u64, count: bool) -> u64 {
    let lead = crate::error_context::found_by_the(what);
    if !crate::pattern::rescan_enabled() {
        return crate::tests::unlocalised_as(lead, format_args!("in idx {start}..{end}"), seen, count);
    }
    let mut scan = crate::tests::Rescan::begin(ptr, lead, count).seen(seen);
    for idx in start..end {
        scan.word(idx, kernel.word(ptr.add(idx) as usize));
    }
    scan.finish(format_args!("in idx {start}..{end}"))
}

// ─── The kernels ─────────────────────────────────────────────────────────────────────────────────

/// `tmr` at one width: lane j of the vector at address `a` is the word at `a + 8j`, so the lanes
/// hold the 16 B units `a/16 + j/2`, odd lanes inverted. `x = unit * SEAL_MUL` steps by a constant,
/// so the hash costs an add, a shift and two XORs per vector, no multiply.
///
/// Both walk a pointer over the range's vectors, four a step, then the rest, each access at the
/// pointer plus a fixed displacement (TODO 99). Counting words from `start` instead, the check's
/// loads were `(base,index,8)` for the index its tail continued from, and on Intel a load with an
/// index folded into a three-operand vector op splits in two (Rule 6.3).
macro_rules! tmr_kernels {
    ($fill:ident, $check:ident, $t:ty, $lanes:literal, $arch:ty, $stream:path, $tf:literal) => {
        #[target_feature(enable = $tf)]
        unsafe fn $fill(ptr: *mut u64, start: usize, end: usize, key: u64, invert: u64) {
            let (mut x, step, inv): ($t, u64, $t) = tmr_start::<$lanes>(ptr.add(start) as usize, key, invert);
            let (step, thirty_two, low63) = (<$t>::splat(step), <$t>::splat(32), <$t>::splat(LOW63));
            let vectors = (end - start).div_ceil($lanes);
            let mut p = ptr.add(start) as *mut $arch;
            for _ in 0..vectors / 4 {
                for u in 0..4 {
                    let v = x ^ (x >> thirty_two) ^ inv;
                    $stream(p.add(u), std::mem::transmute::<$t, $arch>(v));
                    x = (x + step) & low63;
                }
                p = p.wrapping_add(4);
            }
            for _ in 0..vectors % 4 {
                let v = x ^ (x >> thirty_two) ^ inv;
                $stream(p, std::mem::transmute::<$t, $arch>(v));
                x = (x + step) & low63;
                p = p.wrapping_add(1);
            }
            std::arch::x86_64::_mm_sfence();
        }

        /// The differences from the seal, ORed: 0 when every word holds it.
        #[target_feature(enable = $tf)]
        unsafe fn $check(ptr: *const u64, start: usize, end: usize, key: u64, invert: u64) -> u64 {
            let (mut x, step, inv): ($t, u64, $t) = tmr_start::<$lanes>(ptr.add(start) as usize, key, invert);
            let (step, thirty_two, low63) = (<$t>::splat(step), <$t>::splat(32), <$t>::splat(LOW63));
            let zero = <$t>::splat(0);
            let mut acc = [zero; 4];
            let vectors = (end - start).div_ceil($lanes);
            let mut p = ptr.add(start) as *const $t;
            for _ in 0..vectors / 4 {
                for (u, a) in acc.iter_mut().enumerate() {
                    let v = x ^ (x >> thirty_two) ^ inv;
                    *a |= *p.add(u) ^ v;
                    x = (x + step) & low63;
                }
                p = p.wrapping_add(4);
            }
            for _ in 0..vectors % 4 {
                let v = x ^ (x >> thirty_two) ^ inv;
                acc[0] |= *p ^ v;
                x = (x + step) & low63;
                p = p.wrapping_add(1);
            }
            ((acc[0] | acc[1]) | (acc[2] | acc[3])).reduce_or()
        }
    };
}

/// The first vector's `x` lanes, the per-vector step and the inversion mask for `tmr` at `LANES`
/// lanes from byte address `addr` (16 B aligned), under `key` and the cycle's `invert` mask. `x`
/// stays below 2^63: the kernels mask each step's sum.
#[inline(always)]
fn tmr_start<const LANES: usize>(addr: usize, key: u64, invert: u64) -> (std::simd::Simd<u64, LANES>, u64, std::simd::Simd<u64, LANES>)
{
    let unit = (addr >> 4) as u64;
    let x = std::simd::Simd::from_array(std::array::from_fn(|j| (unit + j as u64 / 2).wrapping_mul(SEAL_MUL).wrapping_add(key) & LOW63));
    let inv = std::simd::Simd::from_array(std::array::from_fn(|j| if j % 2 == 1 { !invert } else { invert }));
    (x, (LANES as u64 / 2).wrapping_mul(SEAL_MUL), inv)
}

/// `tm5` at one width. The motif is 6 words, so at any width it repeats every 3 vectors: per page
/// (or the part of one in range) the kernel builds the 3 from the page's motif and the range's
/// phase in it, then stores or checks them in turn.
///
/// Per page, both walk a pointer over its vectors, three a step, then the rest (`tmr_kernels`: no
/// index in the check's loads, TODO 99).
macro_rules! tm5_kernels {
    ($fill:ident, $check:ident, $t:ty, $lanes:literal, $arch:ty, $stream:path, $tf:literal) => {
        #[target_feature(enable = $tf)]
        unsafe fn $fill(ptr: *mut u64, start: usize, end: usize, invert: u64) {
            let mut i = start;
            while i < end {
                let (v, page_end): ([$t; 3], usize) = tm5_vectors::<$lanes>(ptr, i, end, invert);
                let vectors = (page_end - i).div_ceil($lanes);
                let mut p = ptr.add(i) as *mut $arch;
                for _ in 0..vectors / 3 {
                    for (u, x) in v.iter().enumerate() {
                        $stream(p.add(u), std::mem::transmute::<$t, $arch>(*x));
                    }
                    p = p.wrapping_add(3);
                }
                // The rest, at most 2 (a loop over `v` here kept it on the stack)
                let rest = vectors % 3;
                if rest > 0 {
                    $stream(p, std::mem::transmute::<$t, $arch>(v[0]));
                }
                if rest > 1 {
                    $stream(p.add(1), std::mem::transmute::<$t, $arch>(v[1]));
                }
                i += vectors * $lanes;
            }
            std::arch::x86_64::_mm_sfence();
        }

        /// The differences from the seal, ORed: 0 when every word holds it.
        #[target_feature(enable = $tf)]
        unsafe fn $check(ptr: *const u64, start: usize, end: usize, invert: u64) -> u64 {
            let zero = <$t>::splat(0);
            let mut acc = [zero; 3];
            let mut i = start;
            while i < end {
                let (v, page_end): ([$t; 3], usize) = tm5_vectors::<$lanes>(ptr as *mut u64, i, end, invert);
                let vectors = (page_end - i).div_ceil($lanes);
                let mut p = ptr.add(i) as *const $t;
                for _ in 0..vectors / 3 {
                    for (u, a) in acc.iter_mut().enumerate() {
                        *a |= *p.add(u) ^ v[u];
                    }
                    p = p.wrapping_add(3);
                }
                // The rest, at most 2
                let rest = vectors % 3;
                if rest > 0 {
                    acc[0] |= *p ^ v[0];
                }
                if rest > 1 {
                    acc[1] |= *p.add(1) ^ v[1];
                }
                i += vectors * $lanes;
            }
            (acc[0] | acc[1] | acc[2]).reduce_or()
        }
    };
}

/// The 3 vectors of `tm5` from word `i` of `ptr` to the end of its 4 KiB page (or `end`), and that
/// end: vector `u`'s lane `j` is the motif word `(m + u * LANES + j) % 6` XOR the cycle's `invert`
/// mask, where `m` is `i`'s word in the page.
#[inline(always)]
fn tm5_vectors<const LANES: usize>(ptr: *mut u64, i: usize, end: usize, invert: u64) -> ([std::simd::Simd<u64, LANES>; 3], usize)
{
    let addr = ptr.wrapping_add(i) as usize;
    let motif = tm5_motif(addr & !0xFFF);
    let m = (addr & 0xFFF) >> 3;
    let page_end = (i + (512 - m)).min(end);
    let vectors = std::array::from_fn(|u| std::simd::Simd::from_array(std::array::from_fn(|j| motif[(m + u * LANES + j) % 6] ^ invert)));
    (vectors, page_end)
}

tmr_kernels!(tmr_fill_128, tmr_check_128, u64x2, 2, std::arch::x86_64::__m128i, std::arch::x86_64::_mm_stream_si128,
             "sse4.2,sse4.1,ssse3,sse3,sse2,popcnt");
tmr_kernels!(tmr_fill_256, tmr_check_256, u64x4, 4, std::arch::x86_64::__m256i, std::arch::x86_64::_mm256_stream_si256,
             "avx2,avx,fma,bmi1,bmi2");
tmr_kernels!(tmr_fill_512, tmr_check_512, u64x8, 8, std::arch::x86_64::__m512i, std::arch::x86_64::_mm512_stream_si512,
             "avx512f,avx512bw,avx512cd,avx512dq,avx512vl,avx2,avx,fma,bmi1,bmi2");
tm5_kernels!(tm5_fill_128, tm5_check_128, u64x2, 2, std::arch::x86_64::__m128i, std::arch::x86_64::_mm_stream_si128,
             "sse4.2,sse4.1,ssse3,sse3,sse2,popcnt");
tm5_kernels!(tm5_fill_256, tm5_check_256, u64x4, 4, std::arch::x86_64::__m256i, std::arch::x86_64::_mm256_stream_si256,
             "avx2,avx,fma,bmi1,bmi2");
tm5_kernels!(tm5_fill_512, tm5_check_512, u64x8, 8, std::arch::x86_64::__m512i, std::arch::x86_64::_mm512_stream_si512,
             "avx512f,avx512bw,avx512cd,avx512dq,avx512vl,avx2,avx,fma,bmi1,bmi2");

#[cfg(test)]
mod tests {
    use super::*;

    /// TM5's `RS_GeneratePattern` as the binary runs it (`MT0.cxx` `sub_100021A9`, bus width 8):
    /// the 48 B buffer it loads into xmm0-2, built byte for byte.
    fn tm5_reference(page: u32) -> [u64; 6] {
        let mut buf = [0u8; 128];
        let mut dx = ((page >> 28).wrapping_add(page >> 12)) as u16;
        let mut ax = !dx;
        let (bus, mut at, mut left) = (8usize, 0usize, 24i32);
        loop {
            for _ in 0..bus / 2 {
                buf[at..at + 2].copy_from_slice(&dx.to_le_bytes());
                buf[at + bus..at + bus + 2].copy_from_slice(&ax.to_le_bytes());
                at += 2;
                dx = dx.rotate_left(1);
                ax = ax.rotate_left(1);
            }
            at += bus;
            let done = left <= bus as i32;
            left -= bus as i32;
            if done {
                break;
            }
        }
        std::array::from_fn(|k| u64::from_le_bytes(buf[8 * k..8 * k + 8].try_into().unwrap()))
    }

    fn kernels() -> Vec<SealKernel> {
        let mut widths = vec![SealWidth::W128];
        if is_x86_feature_detected!("avx2") { widths.push(SealWidth::W256); }
        if is_x86_feature_detected!("avx512f") { widths.push(SealWidth::W512); }
        // Each pattern and width at the plan-time kernel and two cycles' (inverted, and a key)
        widths.iter().flat_map(|&width| [SealPattern::Tmr, SealPattern::Tm5].map(|pattern| SealKernel::new(pattern, width)))
            .flat_map(|k| [k, k.for_cycle(7, 2), k.for_cycle(7, 3)]).collect()
    }

    #[test]
    fn tm5_motif_is_tm5s_bits() {
        for page in [0u32, 0x1000, 0x7FFF_F000, 0x1234_5000, 0xFFFF_F000, 0x1000_0000, 0xA5A5_A000] {
            assert_eq!(tm5_motif(page as usize), tm5_reference(page), "page {page:#x}");
        }
        // TM5's 16-bit weakness, kept: page 1 and page 65,536 share their motif
        assert_eq!(tm5_motif(1 << 12), tm5_motif(65_536 << 12));
    }

    #[test]
    fn tmr_words_differ_everywhere_and_alternate() {
        for key in [0, 0x0123_4567_89AB_CDEF, u64::MAX] {
            let mut seen = std::collections::HashSet::new();
            for unit in 0..100_000usize {
                let (a, b) = (tmr_word(unit * 16, key), tmr_word(unit * 16 + 8, key));
                assert_eq!(b, !a);
                assert!(seen.insert(a) && seen.insert(b), "unit {unit} repeats a value");
            }
        }
        // The audit's pair: one unit's v met another's !v before the hash kept 63 bits
        assert_ne!(tmr_word(0x6B39_4CE3_7B10, 0), tmr_word(0x6B49_4CE3_7B18, 0));
    }

    /// TODO 99: the seal takes `width=` unless `seal-width=` (or a config's `seal_width`) sets its
    /// own, `auto` included; with neither it keeps its own auto rule.
    #[test]
    fn seal_width_is_its_own_then_the_runs_then_auto() {
        use crate::simd::Width;
        let width = |seal_width, width| SealSettings::parse("tmr", width_setting(seal_width, width)).unwrap().width;
        assert_eq!(width(None, None), SealWidth::Auto);
        assert_eq!(width(None, Some(Width::W128)), SealWidth::W128);
        assert_eq!(width(None, Some(Width::W256)), SealWidth::W256);
        assert_eq!(width(None, Some(Width::W512)), SealWidth::W512);
        assert_eq!(width(Some("256"), Some(Width::W128)), SealWidth::W256);
        assert_eq!(width(Some("128"), Some(Width::W512)), SealWidth::W128);
        assert_eq!(width(Some("auto"), Some(Width::W512)), SealWidth::Auto);
        assert_eq!(width(Some("512"), None), SealWidth::W512);
    }




    /// Known answers from an independent implementation of `doc/pattern_spec.md`'s seal section
    /// (Python, from the spec's text alone, 2026-10-11).
    #[test]
    fn independent_known_answers() {
        let keys: &[(u64, u64, u64, bool)] = &[
            (0x5, 1, 0x6bee77a5232abdd9, false),
            (0x5, 2, 0x6bee77a5232abdd9, true),
            (0x5, 3, 0x55ca380d4216c135, false),
            (0x5, 4, 0x55ca380d4216c135, true)
        ];
        for &(seed, cycle, key, invert) in keys {
            let k = SealKernel::detect().for_cycle(seed, cycle);
            assert_eq!((k.key, k.invert), (key, invert), "seed {seed} cycle {cycle}");
        }
        let words: &[(usize, u64, bool, u64)] = &[
            (0x0, 0x6bee77a5232abdd9, false, 0x6bee77a548c4ca7c),
            (0x8, 0x6bee77a5232abdd9, false, 0x9411885ab73b3583),
            (0x10, 0x6bee77a5232abdd9, false, 0xa25f15ea850c8b0),
            (0x18, 0x6bee77a5232abdd9, false, 0xf5da0ea157af374f),
            (0x6b394ce37b10, 0x6bee77a5232abdd9, false, 0x2c49399a0f7032c4),
            (0x6b494ce37b18, 0x6bee77a5232abdd9, false, 0xd46c4a50f755410e),
            (0x0, 0x6bee77a5232abdd9, true, 0x9411885ab73b3583),
            (0x8, 0x6bee77a5232abdd9, true, 0x6bee77a548c4ca7c),
            (0x10, 0x6bee77a5232abdd9, true, 0xf5da0ea157af374f),
            (0x18, 0x6bee77a5232abdd9, true, 0xa25f15ea850c8b0),
            (0x6b394ce37b10, 0x6bee77a5232abdd9, true, 0xd3b6c665f08fcd3b),
            (0x6b494ce37b18, 0x6bee77a5232abdd9, true, 0x2b93b5af08aabef1)
        ];
        for &(addr, key, invert, word) in words {
            let k = SealKernel { key, invert, ..SealKernel::new(SealPattern::Tmr, SealWidth::W128) };
            assert_eq!(k.word(addr), word, "addr {addr:#x} key {key:#x} invert {invert}");
        }
    }

    /// The seal's own checks are seal errors, never the test's alarms; a test that verifies with
    /// the seal (a mirror, Seal-Check) counts its failed checks as alarms. With rescan on or off.
    #[test]
    fn only_a_tests_own_seal_checks_are_its_alarms() {
        let mut buf = vec![u64x8::splat(0); 64];
        let p = buf.as_mut_ptr() as *mut u64;
        let kernel = SealKernel::detect();
        unsafe {
            kernel.fill(p, 0, 512);
            *p.add(77) ^= 1 << 5;
            crate::tests::reset_rescan_counts();
            assert_eq!(kernel.check(p, 0, 512, "test"), 1);
            assert_eq!(crate::tests::rescan_counts().alarms, 0, "the seal's own check");
            assert_eq!(kernel.verify(p, 0, 512, "test"), 1);
            let counts = crate::tests::rescan_counts();
            assert_eq!((counts.alarms, counts.not_reproduced), (1, 0), "a test's verify");
            // Their failing bits likewise: bit 5 in one check and one word each (TODO 97)
            let bits = crate::tests::take_failing_bits();
            let way = if kernel.word(p.add(77) as usize) & 1 << 5 == 0 { "(1 + 0)" } else { "(0 + 1)" };
            assert_eq!((bits.test.detail(), bits.seal.detail()), (format!("5: 1 / 1 {way}"), format!("5: 1 / 1 {way}")));
            assert!(crate::tests::take_failing_bits().test.is_empty(), "a take leaves none");
        }
    }

    #[test]
    fn cycles_change_the_seal() {
        let k = SealKernel::detect();
        let (c1, c2, c3) = (k.for_cycle(5, 1), k.for_cycle(5, 2), k.for_cycle(5, 3));
        assert!(!c1.invert && c2.invert && !c3.invert);
        for addr in (0..4096).step_by(8) {
            // A pair of cycles: every bit both ways; the next pair: another key
            assert_eq!(c1.word(addr), !c2.word(addr));
            assert_ne!(c1.word(addr), c3.word(addr));
            assert_ne!(c2.word(addr), c3.word(addr));
        }
        let t = SealKernel::new(SealPattern::Tm5, k.width);
        assert_eq!(t.for_cycle(5, 1).word(0x3000), tm5_word(0x3000));
        assert_eq!(t.for_cycle(5, 2).word(0x3000), !tm5_word(0x3000));
    }

    /// Every kernel writes exactly the reference words over ranges that start and end mid-page,
    /// checks clean on them, and counts each bad word (the first and last of the range too).
    #[test]
    fn kernels_match_the_reference_and_count_each_bad_word() {
        let words = 3 * 512 + 64;
        // A guard page either side
        let mut buf = vec![u64x8::splat(0); words / 8 + 2 * 64];
        let p = unsafe { (buf.as_mut_ptr() as *mut u64).add(512) };
        for kernel in kernels() {
            let word = |addr: usize| kernel.word(addr);
            for (start, end) in [(0, words), (8, 512), (256, 1024 + 8), (512, 1024), (1528, words)] {
                unsafe {
                    std::ptr::write_bytes(p, 0x5A, words);
                    kernel.fill(p, start, end);
                    for i in 0..words {
                        let want = if (start..end).contains(&i) { word(p.add(i) as usize) } else { 0x5A5A_5A5A_5A5A_5A5A };
                        assert_eq!(*p.add(i), want, "{} word {i} of {start}..{end}", kernel.name());
                    }
                    assert_eq!(kernel.check(p, start, end, "test"), 0, "{} {start}..{end}", kernel.name());
                    *p.add(start) ^= 1;
                    *p.add(end - 1) ^= 1 << 63;
                    *p.add((start + end) / 2) = 0;
                    assert_eq!(kernel.check(p, start, end, "test"), 3, "{} {start}..{end}", kernel.name());
                    let guard = |i: usize| *(buf.as_ptr() as *const u64).add(i);
                    assert!((0..512).chain(512 + words..1024 + words).all(|i| guard(i) == 0), "{} wrote outside {start}..{end}", kernel.name());
                }
            }
        }
    }

    /// The raw fill at a kernel's pattern and width, past `fill`'s 64 B contract: any whole number
    /// of vectors, so a test reaches the tails a 64 B-aligned range can't (at 128 bits a line is
    /// exactly one step).
    unsafe fn raw_fill(k: SealKernel, p: *mut u64, start: usize, end: usize) {
        let (key, invert) = (k.key, k.invert_mask());
        match (k.pattern, k.width) {
            (SealPattern::Tmr, SealWidth::W512) => tmr_fill_512(p, start, end, key, invert),
            (SealPattern::Tmr, SealWidth::W256) => tmr_fill_256(p, start, end, key, invert),
            (SealPattern::Tmr, _) => tmr_fill_128(p, start, end, key, invert),
            (SealPattern::Tm5, SealWidth::W512) => tm5_fill_512(p, start, end, invert),
            (SealPattern::Tm5, SealWidth::W256) => tm5_fill_256(p, start, end, invert),
            (SealPattern::Tm5, _) => tm5_fill_128(p, start, end, invert),
        }
    }

    /// The raw check (as `raw_fill`): the differences ORed, before any rescan.
    unsafe fn raw_check(k: SealKernel, p: *const u64, start: usize, end: usize) -> u64 {
        let (key, invert) = (k.key, k.invert_mask());
        match (k.pattern, k.width) {
            (SealPattern::Tmr, SealWidth::W512) => tmr_check_512(p, start, end, key, invert),
            (SealPattern::Tmr, SealWidth::W256) => tmr_check_256(p, start, end, key, invert),
            (SealPattern::Tmr, _) => tmr_check_128(p, start, end, key, invert),
            (SealPattern::Tm5, SealWidth::W512) => tm5_check_512(p, start, end, invert),
            (SealPattern::Tm5, SealWidth::W256) => tm5_check_256(p, start, end, invert),
            (SealPattern::Tm5, _) => tm5_check_128(p, start, end, invert),
        }
    }

    /// TODO 99: the kernels' walks write exactly the seal's words (`SealKernel::word`, the pattern
    /// one word at a time) and nothing else, check clean, and see a bad bit in every word of the
    /// range: each lane of each accumulator, each tail (tmr: 1-3 vectors after its steps of 4; tm5:
    /// 1-2 after a page's steps of 3) and the steps the compiler's unrolling leaves to a remainder.
    /// Ranges from 1 vector to over two pages, starting at and inside a page, at every width.
    #[test]
    fn kernel_walks_write_the_seal_and_see_every_bad_word() {
        let words: usize = 3 * 512 + 64;
        // A guard page either side, in 4 KiB pages: a `Vec<u64x8>` is only 64 B aligned, so where
        // its pages began was the heap's choice (`p` 0xf40 into one on the dev box), and with it
        // which tails a range's first and last page took (round 3's review; TODO 99)
        #[derive(Clone, Copy)]
        #[repr(C, align(4096))]
        struct Page([u64; 512]);
        let mut buf = vec![Page([0; 512]); (words + 2 * 512).div_ceil(512)];
        let p = unsafe { (buf.as_mut_ptr() as *mut u64).add(512) };
        assert!(p.addr().is_multiple_of(4096), "the range's word 0 a page's first");
        const FILLER: u64 = 0x5A5A_5A5A_5A5A_5A5A;
        for kernel in kernels() {
            let lanes = match kernel.width { SealWidth::W512 => 8, SealWidth::W256 => 4, _ => 2 };
            // Starts at a page, one vector in, mid-page, and 16 words before a page's end; lengths
            // in vectors from one to 25, then over two pages (a full page is 21 steps at 512 bits,
            // 42 at 256 and 85 at 128)
            let starts = [0, lanes, 40, 496];
            let lengths = (1..=9).chain([15, 22, 23, 24, 25]).chain([1100 / lanes]);
            let ranges: Vec<(usize, usize)> = lengths.flat_map(|n| starts.map(|s| (s, s + n * lanes))).collect();
            for (start, end) in ranges {
                let name = format!("{} {start}..{end}", kernel.name());
                unsafe {
                    for i in 0..words + 1024 {
                        *p.wrapping_sub(512).add(i) = FILLER;
                    }
                    raw_fill(kernel, p, start, end);
                    for i in 0..words + 1024 {
                        let at = p.wrapping_sub(512).wrapping_add(i);
                        let word = i.wrapping_sub(512);
                        let want = if (start..end).contains(&word) { kernel.word(at as usize) } else { FILLER };
                        assert_eq!(*at, want, "{name}: word {word}");
                    }
                    assert_eq!(raw_check(kernel, p, start, end), 0, "{name}: clean");
                    for i in start..end {
                        *p.add(i) ^= 1 << (i % 64);
                        assert_ne!(raw_check(kernel, p, start, end), 0, "{name}: missed word {i}");
                        *p.add(i) ^= 1 << (i % 64);
                    }
                }
            }
        }
    }
}
