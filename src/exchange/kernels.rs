//! The page exchange's kernels (TODO 28): a tile's fill, check, copy and invert, at 256 and 512 bits (`width=128` runs at 256, as
//! Mem-Sequence: the CPU gate requires AVX2), the stores cached or non-temporal. Each pattern family and beat has a function of its own
//! for each (`with_phase_vectors!`, matched once a call, outside the loop). A tile's line ids count from 0 at its start, so its image is
//! a function of its family, key and polarity and of each word's place in the tile, not its address: a copy holds where it lands.
//!
//! The cached fill is native SimpleTest's sequential write (`native_write_seq!`), and the NT fill walks the same way, a line a step: a
//! pointer stepped through `opaque_ptr`, every access at a fixed displacement from it. The check, the copy and the invert walk so too, by
//! steps spelled out at each width (`check_and_store!`, as Mem-Sequence's): 4 vectors into 4 accumulators for the families computed as
//! vectors (Rules 6.3 and 6.12 of `doc/simd_codegen_rules.md`), a line for the switching ones. Left to the compiler to unroll, the
//! heavier families' steps at 512 bits stayed a loop of four, the accumulators an array on the stack indexed by it (the first build's
//! asm). A copy stores what it loaded and an invert the inverse of it, as GSAT's do; the check rides along, so neither collapses into a
//! `memcpy`.

use std::simd::num::SimdUint;
use std::simd::{u64x4, u64x8};

use super::Stores;
use crate::pattern::{LINE_WORDS, Phase, with_phase_vectors};
use crate::tests::{NativeChunk, native_write_seq, opaque_ptr};

/// Vector `$x` stored at `$at`: `cached`, or `nt` with `$stream` on the width's `$arch` type.
macro_rules! store {
    (cached, $simd:ty, $arch:ty, $stream:path, $at:expr, $x:expr) => {
        *($at as *mut $simd) = $x
    };
    (nt, $simd:ty, $arch:ty, $stream:path, $at:expr, $x:expr) => {
        $stream($at as *mut $arch, std::mem::transmute::<$simd, $arch>($x))
    };
}

/// After a kernel's stores: an SFENCE after non-temporal ones, so they come before the tile's release (a Release store orders cached ones).
macro_rules! fence {
    (cached) => {};
    (nt) => {
        std::arch::x86_64::_mm_sfence()
    };
}

/// The non-temporal fill of `$words` words from `$ptr`, each line `$line`'s: `native_write_seq!`'s walk, a line a step.
macro_rules! fill_nt {
    ([$simd:ty, $w:tt], $arch:ty, $stream:path, $ptr:expr, $words:expr, $line:expr) => {{
        let line = $line;
        let (mut p, stop) = ($ptr, $ptr.wrapping_add($words));
        let mut lid = 0u64;
        while p < stop {
            for (v, x) in line(lid).into_iter().enumerate() {
                store!(nt, $simd, $arch, $stream, p.add(v * $w), x);
            }
            lid += 1;
            p = opaque_ptr(p.wrapping_add(LINE_WORDS));
        }
        fence!(nt);
    }};
}

/// The words of `$words` from `$src` checked against `$line`'s: `verify` alone, `copy` storing each as loaded at `$dst`, `invert`
/// storing its inverse in place (`$dst` the same pointer); their differences, ORed: 0 when every word held. A step takes `$step`,
/// `[line: [vector accumulator, ...], ...]`, each line computed and checked before the next, then the lines past the last whole step one at
/// a time. The step is the width's group of 4 vectors for the families whose words are computed as vectors (`ident`, `random`, `lines`,
/// `linepair`), or one line for the switching ones (`switch`, `quiet`, `header`), whose words come from a scalar hash a line: at a step of
/// two or four of their lines the hashes' values went to the stack (the first builds' asm: SIMD round trips at 256 bits, and at 512 with
/// NT stores), and those loops are bound by the hashing, not by the accumulators' chains.
macro_rules! check_and_store {
    ($op:ident, $kind:ident, [$simd:ty, $w:tt], $arch:ty, $stream:path, $src:expr, $dst:expr, $words:expr, $line:expr,
     [$($l:literal: [$($v:literal $a:literal),*]),*]) => {{
        const VPL: usize = LINE_WORDS / $w;
        const LPS: usize = [$($l),*].len();
        const _: () = assert!(LPS * VPL == 4 || LPS == 1, "a step is four vectors, or one line");
        let line = $line;
        let mut acc = [<$simd>::splat(0); 4];
        let (mut s, mut d) = ($src, $dst);
        let stop = s.wrapping_add($words);
        // The last step's first word: the bound the step loop runs to (test memory sits far above address 0, so this never wraps)
        let last = stop.wrapping_sub(LPS * LINE_WORDS);
        let mut lid = 0u64;
        while s <= last {
            $({
                let want = line(lid + $l);
                $(
                    let x = *(s.add(($l * VPL + $v) * $w) as *const $simd);
                    acc[$a] |= x ^ want[$v];
                    check_and_store!(@store $op, $kind, $simd, $arch, $stream, d.add(($l * VPL + $v) * $w), x);
                )*
            })*
            lid += LPS as u64;
            s = opaque_ptr(s.wrapping_add(LPS * LINE_WORDS));
            d = check_and_store!(@next $op, s, d.wrapping_add(LPS * LINE_WORDS));
        }
        while s < stop {
            let want = line(lid);
            for v in 0..VPL {
                let x = *(s.add(v * $w) as *const $simd);
                acc[v % 4] |= x ^ want[v];
                check_and_store!(@store $op, $kind, $simd, $arch, $stream, d.add(v * $w), x);
            }
            lid += 1;
            s = opaque_ptr(s.wrapping_add(LINE_WORDS));
            d = check_and_store!(@next $op, s, d.wrapping_add(LINE_WORDS));
        }
        check_and_store!(@fence $op, $kind);
        ((acc[0] | acc[1]) | (acc[2] | acc[3])).reduce_or()
    }};
    (@store verify, $kind:ident, $simd:ty, $arch:ty, $stream:path, $at:expr, $x:expr) => {
        let _ = $at;
    };
    (@store copy, $kind:ident, $simd:ty, $arch:ty, $stream:path, $at:expr, $x:expr) => {
        store!($kind, $simd, $arch, $stream, $at, $x)
    };
    (@store invert, $kind:ident, $simd:ty, $arch:ty, $stream:path, $at:expr, $x:expr) => {
        store!($kind, $simd, $arch, $stream, $at, !$x)
    };
    // A copy steps its own destination pointer; an invert's is its source's, and a verify has none
    (@next copy, $s:expr, $d:expr) => {
        opaque_ptr($d)
    };
    (@next $op:ident, $s:expr, $d:expr) => {
        $s
    };
    (@fence verify, $kind:ident) => {};
    (@fence $op:ident, $kind:ident) => {
        fence!($kind)
    };
}

/// A kernel function of `check_and_store!`: `$op` with `$kind` stores at the width, its step `$step`, over the pointers `$args`.
macro_rules! check_fn {
    ($name:ident, $op:ident, $kind:ident, [$simd:ty, $w:tt], $arch:ty, $stream:path, $tf:literal, $step:tt, ($($arg:ident),*), $src:expr,
     $dst:expr) => {
        #[inline(never)]
        #[target_feature(enable = $tf)]
        unsafe fn $name<L: Fn(u64) -> [$simd; LINE_WORDS / $w]>($($arg: *mut u64,)* words: usize, line: L) -> u64 {
            check_and_store!($op, $kind, [$simd, $w], $arch, $stream, $src, $dst, words, line, $step)
        }
    };
}

/// The kernels at one width, as module `$m`: `$group` its step of 4 vectors, `$line` its step of one line (`check_and_store!`).
macro_rules! exchange_kernels {
    ($m:ident, [$simd:ty, $w:tt], $arch:ty, $stream:path, $group:tt, $line:tt, $tf:literal) => {
        pub(super) mod $m {
            use super::*;

            /// A line's vectors at this width
            type Line = [$simd; LINE_WORDS / $w];

            /// Writes `phase`'s image over the `words` words from `ptr`.
            #[inline]
            #[target_feature(enable = $tf)]
            pub(crate) unsafe fn fill(ptr: *mut u64, words: usize, phase: Phase, stores: Stores) {
                with_phase_vectors!($w, phase, |line| {
                    match stores {
                        Stores::Cached => fill_cached(ptr, words, phase, line),
                        Stores::Nt => fill_nt(ptr, words, line),
                    }
                })
            }

            /// The differences of the `words` words from `ptr` from `phase`'s image, ORed: 0 when every word holds. A step of 4 vectors, or
            /// of a line for the switching families (`check_and_store!`).
            #[inline]
            #[target_feature(enable = $tf)]
            pub(crate) unsafe fn verify(ptr: *mut u64, words: usize, phase: Phase) -> u64 {
                with_phase_vectors!(
                    $w,
                    phase,
                    |line| { verify_group(ptr, words, line) },
                    switching | line | { verify_line(ptr, words, line) }
                )
            }

            /// Copies the `words` words from `src` to `dst`, checking them against `phase`'s image on the way: the differences, ORed.
            #[inline]
            #[target_feature(enable = $tf)]
            pub(crate) unsafe fn copy(src: *mut u64, dst: *mut u64, words: usize, phase: Phase, stores: Stores) -> u64 {
                with_phase_vectors!(
                    $w,
                    phase,
                    |line| {
                        match stores {
                            Stores::Cached => copy_cached_group(src, dst, words, line),
                            Stores::Nt => copy_nt_group(src, dst, words, line),
                        }
                    },
                    switching | line | {
                        match stores {
                            Stores::Cached => copy_cached_line(src, dst, words, line),
                            Stores::Nt => copy_nt_line(src, dst, words, line),
                        }
                    }
                )
            }

            /// Inverts the `words` words from `ptr` in place, checking them against `phase`'s image first: the differences, ORed.
            #[inline]
            #[target_feature(enable = $tf)]
            pub(crate) unsafe fn invert(ptr: *mut u64, words: usize, phase: Phase, stores: Stores) -> u64 {
                with_phase_vectors!(
                    $w,
                    phase,
                    |line| {
                        match stores {
                            Stores::Cached => invert_cached_group(ptr, words, line),
                            Stores::Nt => invert_nt_group(ptr, words, line),
                        }
                    },
                    switching | line | {
                        match stores {
                            Stores::Cached => invert_cached_line(ptr, words, line),
                            Stores::Nt => invert_nt_line(ptr, words, line),
                        }
                    }
                )
            }

            #[inline(never)]
            #[target_feature(enable = $tf)]
            unsafe fn fill_cached<L: Fn(u64) -> Line>(ptr: *mut u64, words: usize, phase: Phase, line: L) {
                native_write_seq!([$simd, $w], NativeChunk { base: ptr, start: 0, end: words, lid0: 0, period: 0, phase }, line)
            }

            #[inline(never)]
            #[target_feature(enable = $tf)]
            unsafe fn fill_nt<L: Fn(u64) -> Line>(ptr: *mut u64, words: usize, line: L) {
                fill_nt!([$simd, $w], $arch, $stream, ptr, words, line)
            }

            check_fn!(verify_group, verify, cached, [$simd, $w], $arch, $stream, $tf, $group, (ptr), ptr, ptr);
            check_fn!(verify_line, verify, cached, [$simd, $w], $arch, $stream, $tf, $line, (ptr), ptr, ptr);
            check_fn!(copy_cached_group, copy, cached, [$simd, $w], $arch, $stream, $tf, $group, (src, dst), src, dst);
            check_fn!(copy_cached_line, copy, cached, [$simd, $w], $arch, $stream, $tf, $line, (src, dst), src, dst);
            check_fn!(copy_nt_group, copy, nt, [$simd, $w], $arch, $stream, $tf, $group, (src, dst), src, dst);
            check_fn!(copy_nt_line, copy, nt, [$simd, $w], $arch, $stream, $tf, $line, (src, dst), src, dst);
            check_fn!(invert_cached_group, invert, cached, [$simd, $w], $arch, $stream, $tf, $group, (ptr), ptr, ptr);
            check_fn!(invert_cached_line, invert, cached, [$simd, $w], $arch, $stream, $tf, $line, (ptr), ptr, ptr);
            check_fn!(invert_nt_group, invert, nt, [$simd, $w], $arch, $stream, $tf, $group, (ptr), ptr, ptr);
            check_fn!(invert_nt_line, invert, nt, [$simd, $w], $arch, $stream, $tf, $line, (ptr), ptr, ptr);
        }
    };
}

exchange_kernels!(k256, [u64x4, 4], std::arch::x86_64::__m256i, std::arch::x86_64::_mm256_stream_si256, [0: [0 0, 1 1], 1: [0 2, 1 3]],
                  [0: [0 0, 1 1]], "avx2,avx,fma,bmi1,bmi2");
exchange_kernels!(k512, [u64x8, 8], std::arch::x86_64::__m512i, std::arch::x86_64::_mm512_stream_si512, [0: [0 0], 1: [0 1], 2: [0 2], 3: [0 3]],
                  [0: [0 0]], "avx512f,avx512bw,avx512cd,avx512dq,avx512vl,avx2,avx,fma,bmi1,bmi2");

/// The kernels at a step's width: 512 bits where the run's width is, else 256 (no 128-bit kernels: `width=128` runs at 256).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Kernels {
    wide: bool,
}

impl Kernels {
    pub(super) fn at(width: crate::simd::Width) -> Self {
        Kernels { wide: width == crate::simd::Width::W512 }
    }

    /// The width it runs at, in bits
    pub(super) fn bits(self) -> u32 {
        if self.wide { 512 } else { 256 }
    }

    /// # Safety
    /// `words` words from `ptr` are valid for writes, 64-byte aligned, a whole number of lines; this thread holds them.
    pub(super) unsafe fn fill(self, ptr: *mut u64, words: usize, phase: Phase, stores: Stores) {
        if self.wide { k512::fill(ptr, words, phase, stores) } else { k256::fill(ptr, words, phase, stores) }
    }

    /// # Safety
    /// As `fill`, for reads.
    pub(super) unsafe fn verify(self, ptr: *mut u64, words: usize, phase: Phase) -> u64 {
        if self.wide { k512::verify(ptr, words, phase) } else { k256::verify(ptr, words, phase) }
    }

    /// # Safety
    /// As `fill`, for both, which don't overlap.
    pub(super) unsafe fn copy(self, src: *mut u64, dst: *mut u64, words: usize, phase: Phase, stores: Stores) -> u64 {
        if self.wide { k512::copy(src, dst, words, phase, stores) } else { k256::copy(src, dst, words, phase, stores) }
    }

    /// # Safety
    /// As `fill`.
    pub(super) unsafe fn invert(self, ptr: *mut u64, words: usize, phase: Phase, stores: Stores) -> u64 {
        if self.wide { k512::invert(ptr, words, phase, stores) } else { k256::invert(ptr, words, phase, stores) }
    }
}
