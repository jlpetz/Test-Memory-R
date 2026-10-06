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
//! **The patterns.** Both depend only on the word's address, so any chunk layout works, and both
//! alternate a value and its inverse between consecutive 8 B bus transfers, so every data line
//! toggles on every transfer:
//! - `tmr` (the default): each 16 B holds `[v, !v]`, where `v` is a 64-bit bijective hash of the
//!   16 B unit's address, so no two units anywhere hold the same value and a write that lands at
//!   the wrong address always shows.
//! - `tm5`: TM5's `RS_GeneratePattern`, bit for bit (64-bit bus): per 4 KiB page a 16-bit word
//!   from the page number, written as a 48 B motif `[q0, !q0, q1, !q1, q2, !q2]` repeated across
//!   the page. Different pages can share it (page 1 and page 65,536 do), and inside a page it
//!   repeats every 48 B.
//!
//! **The kernels.** Fill (non-temporal stores, so the seal lands in DRAM rather than waiting in
//! cache) and check (XOR/OR into accumulators; a hit rescans to count and log each bad word),
//! stamped per width (128/256/512) with `macro_rules!`. One call per chunk, never per word.

use std::simd::cmp::SimdPartialEq;
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
    pub fn resolve(self) -> Result<Self, String> {
        let avx512 = is_x86_feature_detected!("avx512f");
        let avx2 = is_x86_feature_detected!("avx2");
        match self {
            SealWidth::Auto if avx512 => Ok(SealWidth::W512),
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

/// A seal pattern at a resolved width: what one fill or check call runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SealKernel {
    pub pattern: SealPattern,
    /// Never `Auto`: resolved when the run starts.
    pub width: SealWidth,
}

impl SealKernel {
    /// The default kernel on this CPU: `tmr` at the widest width.
    pub fn detect() -> Self {
        SealKernel { pattern: SealPattern::Tmr, width: SealWidth::Auto.resolve().unwrap_or(SealWidth::W128) }
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
        match (self.pattern, self.width) {
            (SealPattern::Tmr, SealWidth::W512) => tmr_fill_512(ptr, start, end),
            (SealPattern::Tmr, SealWidth::W256) => tmr_fill_256(ptr, start, end),
            (SealPattern::Tmr, _) => tmr_fill_128(ptr, start, end),
            (SealPattern::Tm5, SealWidth::W512) => tm5_fill_512(ptr, start, end),
            (SealPattern::Tm5, SealWidth::W256) => tm5_fill_256(ptr, start, end),
            (SealPattern::Tm5, _) => tm5_fill_128(ptr, start, end),
        }
    }

    /// Check the words `[start, end)` of `ptr` hold the seal; returns the bad words, the first 10
    /// logged as found by `what` (e.g. "seal check before") and the thread's error context.
    ///
    /// # Safety
    /// As `fill`, for reads.
    pub unsafe fn check(self, ptr: *const u64, start: usize, end: usize, what: &str) -> u64 {
        debug_assert!((ptr.add(start) as usize).is_multiple_of(64) && (end - start).is_multiple_of(8), "unaligned seal range");
        let clean = match (self.pattern, self.width) {
            (SealPattern::Tmr, SealWidth::W512) => tmr_check_512(ptr, start, end),
            (SealPattern::Tmr, SealWidth::W256) => tmr_check_256(ptr, start, end),
            (SealPattern::Tmr, _) => tmr_check_128(ptr, start, end),
            (SealPattern::Tm5, SealWidth::W512) => tm5_check_512(ptr, start, end),
            (SealPattern::Tm5, SealWidth::W256) => tm5_check_256(ptr, start, end),
            (SealPattern::Tm5, _) => tm5_check_128(ptr, start, end),
        };
        if clean {
            return 0;
        }
        rescan(self.pattern, ptr, start, end, what)
    }
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
        Ok(SealKernel { pattern: self.pattern, width: self.width.resolve()? })
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

/// Odd, so `k * SEAL_MUL` is a bijection on u64; `x ^ (x >> 32)` is one too.
const SEAL_MUL: u64 = 0x9E37_79B9_7F4A_7C15;

/// The `tmr` seal word at byte address `addr` (a multiple of 8).
#[inline(always)]
pub fn tmr_word(addr: usize) -> u64 {
    let x = ((addr >> 4) as u64).wrapping_mul(SEAL_MUL);
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
/// accumulator has seen one. A fault gone by the reread still counts once.
#[cold]
#[inline(never)]
unsafe fn rescan(pattern: SealPattern, ptr: *const u64, start: usize, end: usize, what: &str) -> u64 {
    let word = match pattern {
        SealPattern::Tmr => tmr_word,
        SealPattern::Tm5 => tm5_word,
    };
    let lead = crate::error_context::found_by_the(what);
    let mut errors = 0u64;
    for idx in start..end {
        let (want, actual) = (word(ptr.add(idx) as usize), *ptr.add(idx));
        if actual != want {
            errors += 1;
            if errors <= 10 {
                log::error!("{lead}, idx {idx}: expected {want:#x}, got {actual:#x}");
            }
        }
    }
    if errors == 0 {
        log::error!("{lead} in idx {start}..{end} that a reread no longer shows (transient)");
        errors = 1;
    }
    errors
}

// ─── The kernels ─────────────────────────────────────────────────────────────────────────────────

/// `tmr` at one width: lane j of the vector at address `a` is the word at `a + 8j`, so the lanes
/// hold the 16 B units `a/16 + j/2`, odd lanes inverted. `x = unit * SEAL_MUL` steps by a constant,
/// so the hash costs an add, a shift and two XORs per vector, no multiply.
macro_rules! tmr_kernels {
    ($fill:ident, $check:ident, $t:ty, $lanes:literal, $arch:ty, $stream:path, $tf:literal) => {
        #[target_feature(enable = $tf)]
        unsafe fn $fill(ptr: *mut u64, start: usize, end: usize) {
            let (mut x, step, inv): ($t, u64, $t) = tmr_start::<$lanes>(ptr.add(start) as usize);
            let (step, thirty_two) = (<$t>::splat(step), <$t>::splat(32));
            let mut i = start;
            let unrolled = start + (end - start) / (4 * $lanes) * (4 * $lanes);
            while i < unrolled {
                for u in 0..4 {
                    let v = x ^ (x >> thirty_two) ^ inv;
                    $stream(ptr.add(i + u * $lanes) as *mut $arch, std::mem::transmute::<$t, $arch>(v));
                    x += step;
                }
                i += 4 * $lanes;
            }
            while i < end {
                let v = x ^ (x >> thirty_two) ^ inv;
                $stream(ptr.add(i) as *mut $arch, std::mem::transmute::<$t, $arch>(v));
                x += step;
                i += $lanes;
            }
            std::arch::x86_64::_mm_sfence();
        }

        /// `true` when every word holds the seal.
        #[target_feature(enable = $tf)]
        unsafe fn $check(ptr: *const u64, start: usize, end: usize) -> bool {
            let (mut x, step, inv): ($t, u64, $t) = tmr_start::<$lanes>(ptr.add(start) as usize);
            let (step, thirty_two) = (<$t>::splat(step), <$t>::splat(32));
            let zero = <$t>::splat(0);
            let mut acc = [zero; 4];
            let mut i = start;
            let unrolled = start + (end - start) / (4 * $lanes) * (4 * $lanes);
            while i < unrolled {
                for (u, a) in acc.iter_mut().enumerate() {
                    let v = x ^ (x >> thirty_two) ^ inv;
                    *a |= *(ptr.add(i + u * $lanes) as *const $t) ^ v;
                    x += step;
                }
                i += 4 * $lanes;
            }
            while i < end {
                let v = x ^ (x >> thirty_two) ^ inv;
                acc[0] |= *(ptr.add(i) as *const $t) ^ v;
                x += step;
                i += $lanes;
            }
            ((acc[0] | acc[1]) | (acc[2] | acc[3])).simd_eq(zero).all()
        }
    };
}

/// The first vector's `x` lanes, the per-vector step and the inversion mask for `tmr` at `LANES`
/// lanes from byte address `addr` (16 B aligned).
#[inline(always)]
fn tmr_start<const LANES: usize>(addr: usize) -> (std::simd::Simd<u64, LANES>, u64, std::simd::Simd<u64, LANES>)
{
    let unit = (addr >> 4) as u64;
    let x = std::simd::Simd::from_array(std::array::from_fn(|j| (unit + j as u64 / 2).wrapping_mul(SEAL_MUL)));
    let inv = std::simd::Simd::from_array(std::array::from_fn(|j| if j % 2 == 1 { !0 } else { 0 }));
    (x, (LANES as u64 / 2).wrapping_mul(SEAL_MUL), inv)
}

/// `tm5` at one width. The motif is 6 words, so at any width it repeats every 3 vectors: per page
/// (or the part of one in range) the kernel builds the 3 from the page's motif and the range's
/// phase in it, then stores or checks them in turn.
macro_rules! tm5_kernels {
    ($fill:ident, $check:ident, $t:ty, $lanes:literal, $arch:ty, $stream:path, $tf:literal) => {
        #[target_feature(enable = $tf)]
        unsafe fn $fill(ptr: *mut u64, start: usize, end: usize) {
            let mut i = start;
            while i < end {
                let (v, page_end): ([$t; 3], usize) = tm5_vectors::<$lanes>(ptr, i, end);
                let mut r = 0;
                while i + 3 * $lanes <= page_end {
                    for (u, x) in v.iter().enumerate() {
                        $stream(ptr.add(i + u * $lanes) as *mut $arch, std::mem::transmute::<$t, $arch>(*x));
                    }
                    i += 3 * $lanes;
                }
                while i < page_end {
                    $stream(ptr.add(i) as *mut $arch, std::mem::transmute::<$t, $arch>(v[r]));
                    r += 1;
                    i += $lanes;
                }
            }
            std::arch::x86_64::_mm_sfence();
        }

        /// `true` when every word holds the seal.
        #[target_feature(enable = $tf)]
        unsafe fn $check(ptr: *const u64, start: usize, end: usize) -> bool {
            let zero = <$t>::splat(0);
            let mut acc = [zero; 3];
            let mut i = start;
            while i < end {
                let (v, page_end): ([$t; 3], usize) = tm5_vectors::<$lanes>(ptr as *mut u64, i, end);
                while i + 3 * $lanes <= page_end {
                    for (u, a) in acc.iter_mut().enumerate() {
                        *a |= *(ptr.add(i + u * $lanes) as *const $t) ^ v[u];
                    }
                    i += 3 * $lanes;
                }
                let mut r = 0;
                while i < page_end {
                    acc[r] |= *(ptr.add(i) as *const $t) ^ v[r];
                    r += 1;
                    i += $lanes;
                }
            }
            (acc[0] | acc[1] | acc[2]).simd_eq(zero).all()
        }
    };
}

/// The 3 vectors of `tm5` from word `i` of `ptr` to the end of its 4 KiB page (or `end`), and that
/// end: vector `u`'s lane `j` is the motif word `(m + u * LANES + j) % 6`, where `m` is `i`'s word
/// in the page.
#[inline(always)]
fn tm5_vectors<const LANES: usize>(ptr: *mut u64, i: usize, end: usize) -> ([std::simd::Simd<u64, LANES>; 3], usize)
{
    let addr = ptr.wrapping_add(i) as usize;
    let motif = tm5_motif(addr & !0xFFF);
    let m = (addr & 0xFFF) >> 3;
    let page_end = (i + (512 - m)).min(end);
    let vectors = std::array::from_fn(|u| std::simd::Simd::from_array(std::array::from_fn(|j| motif[(m + u * LANES + j) % 6])));
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
        widths.iter().flat_map(|&width| [SealPattern::Tmr, SealPattern::Tm5].map(|pattern| SealKernel { pattern, width })).collect()
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
        let mut seen = std::collections::HashSet::new();
        for unit in 0..100_000usize {
            let (a, b) = (tmr_word(unit * 16), tmr_word(unit * 16 + 8));
            assert_eq!(b, !a);
            assert!(seen.insert(a), "unit {unit} repeats a value");
        }
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
            let word = match kernel.pattern { SealPattern::Tmr => tmr_word, SealPattern::Tm5 => tm5_word };
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
}
