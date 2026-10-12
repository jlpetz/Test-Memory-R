//! Isolated code-generation experiment. Does not change TMR.
//! Compare the original expression, the current shared helper, and explicit
//! 16/32-bit truncation. Bounded tests use ordinary aligned heap memory.
#![feature(portable_simd)]

#[path = "../../../src/pattern_gen.rs"]
mod pattern_gen;

use std::simd::{num::SimdUint, u64x4};

#[inline(always)]
fn old_word(lid: u64, key: u64) -> u64 {
    const LOW: u64 = 0x0f0f_0f0f_0f0f_0f0f;
    let mut x = pattern_gen::pattern_mode13(lid, key) & 0xffff_ffff;
    x = (x | x << 16) & 0x0000_ffff_0000_ffff;
    x = (x | x << 8) & 0x00ff_00ff_00ff_00ff;
    x = (x | x << 4) & LOW;
    let a = x | ((!x & LOW) << 4);
    let pair = (a & 0xffff) | ((!a & 0xffff) << 16);
    pair | pair << 32
}

#[inline(always)]
fn shared_word(lid: u64, key: u64) -> u64 {
    pattern_gen::mode14_line(lid as usize, key, pattern_gen::BusBeat::Bits16)[0]
}

#[inline(always)]
fn cast_word(lid: u64, key: u64) -> u64 {
    let a = pattern_gen::balanced_bytes(pattern_gen::pattern_mode13(lid, key)) as u16;
    let pair = u32::from(a) | (u32::from(!a) << 16);
    u64::from(pair) | (u64::from(pair) << 32)
}

#[inline(always)]
fn masked_word(lid: u64, key: u64) -> u64 {
    let h = pattern_gen::pattern_mode13(lid, key);
    let a = pattern_gen::balanced_bytes(h & 255);
    pattern_gen::bus_flip_line(a, pattern_gen::BusBeat::Bits16)[0]
}

macro_rules! kernels {
    ($fill:ident, $verify:ident, $word:ident) => {
        #[unsafe(no_mangle)]
        #[inline(never)]
        #[target_feature(enable = "avx2")]
        pub unsafe fn $fill(ptr: *mut u64, words: usize, key: u64) {
            for off in (0..words).step_by(8) {
                let value = u64x4::splat($word((off / 8) as u64, key));
                unsafe {
                    *(ptr.add(off) as *mut u64x4) = value;
                    *(ptr.add(off + 4) as *mut u64x4) = value;
                }
            }
        }

        #[unsafe(no_mangle)]
        #[inline(never)]
        #[target_feature(enable = "avx2")]
        pub unsafe fn $verify(ptr: *const u64, words: usize, key: u64) -> u64 {
            let mut acc = [u64x4::splat(0); 4];
            for off in (0..words).step_by(16) {
                let a = u64x4::splat($word((off / 8) as u64, key));
                let b = u64x4::splat($word((off / 8 + 1) as u64, key));
                unsafe {
                    acc[0] |= *(ptr.add(off) as *const u64x4) ^ a;
                    acc[1] |= *(ptr.add(off + 4) as *const u64x4) ^ a;
                    acc[2] |= *(ptr.add(off + 8) as *const u64x4) ^ b;
                    acc[3] |= *(ptr.add(off + 12) as *const u64x4) ^ b;
                }
            }
            ((acc[0] | acc[1]) | (acc[2] | acc[3])).reduce_or()
        }
    };
}

kernels!(fill_old, verify_old, old_word);
kernels!(fill_shared, verify_shared, shared_word);
kernels!(fill_cast, verify_cast, cast_word);
kernels!(fill_masked, verify_masked, masked_word);

#[cfg(test)]
mod checks {
    use super::*;
    use std::hint::black_box;
    use std::time::{Duration, Instant};

    #[repr(align(64))]
    #[derive(Clone)]
    struct Line([u64; 8]);

    #[test]
    fn review_perf16_expressions_preserve_images_and_checks() {
        for lid in 0..100_000 {
            let expected = old_word(lid, 0x1234_5678_9abc_def0);
            assert_eq!(shared_word(lid, 0x1234_5678_9abc_def0), expected);
            assert_eq!(cast_word(lid, 0x1234_5678_9abc_def0), expected);
            assert_eq!(masked_word(lid, 0x1234_5678_9abc_def0), expected);
        }
        let mut data = vec![Line([0; 8]); 128];
        let ptr = data.as_mut_ptr().cast::<u64>();
        unsafe {
            fill_old(ptr, 1024, 17);
            assert_eq!(verify_shared(ptr, 1024, 17), 0);
            assert_eq!(verify_cast(ptr, 1024, 17), 0);
            assert_eq!(verify_masked(ptr, 1024, 17), 0);
            data[15].0[7] ^= 1 << 51;
            assert_eq!(verify_old(ptr, 1024, 17), 1 << 51);
            assert_eq!(verify_shared(ptr, 1024, 17), 1 << 51);
            assert_eq!(verify_cast(ptr, 1024, 17), 1 << 51);
            assert_eq!(verify_masked(ptr, 1024, 17), 1 << 51);
        }
    }

    // Only an isolated CPU-cost sample on this VM, not LPDDR hardware validation.
    // No assertion on speed. Fixed-size heap, one thread, roughly two seconds.
    #[test]
    #[ignore = "run explicitly after inspecting the assembly"]
    fn review_perf16_bounded_cpu_sample() {
        type Verify = unsafe fn(*const u64, usize, u64) -> u64;
        let variants: [(&str, Verify); 4] = [
            ("old", verify_old), ("shared", verify_shared), ("cast", verify_cast),
            ("masked", verify_masked),
        ];
        for kib in [8, 256] {
            let mut data = vec![Line([0; 8]); kib * 16];
            let ptr = data.as_mut_ptr().cast::<u64>();
            let words = kib * 128;
            unsafe { fill_old(ptr, words, 17); }
            for block in 0..9 {
                // Rotate order to avoid always timing a variant first.
                for at in 0..4 {
                    let (name, verify) = variants[(at + block) % 4];
                    let start = Instant::now();
                    let mut iterations = 0u64;
                    while start.elapsed() < Duration::from_millis(15) {
                        for _ in 0..16 {
                            let diff = unsafe { verify(black_box(ptr), black_box(words), black_box(17)) };
                            assert_eq!(black_box(diff), 0);
                        }
                        iterations += 16;
                    }
                    let nanos = start.elapsed().as_nanos() as f64;
                    println!("sample,{kib},{block},{name},{:.6}",
                             nanos / (iterations as f64 * (words / 8) as f64));
                }
            }
        }
    }
}
