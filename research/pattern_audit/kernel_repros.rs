//! Added as a child of tests.rs in the isolated source copy.

use super::*;

#[test]
fn audit_simd_positional_counts_batches_not_bad_words() {
    #[repr(align(64))]
    struct Aligned([u64; 512]);
    let mut data = Aligned([0; 512]);
    let base = 0xDEAD_BEEF_DEAD_BEEF;
    for (i, value) in data.0.iter_mut().enumerate() { *value = i as u64 ^ base; }
    for i in [0, 5, 511] { data.0[i] ^= 1; }
    let ctx = ChunkCtx {
        ptr: data.0.as_mut_ptr(), chunk_start: 0, chunk_end: 512,
        cycle: 1, thread_id: 0, check_mask: None,
    };
    #[target_feature(enable = "avx2")]
    unsafe fn check(ctx: &ChunkCtx, base: u64) -> u64 {
        let offsets = u64x4::from_array([0, 1, 2, 3]);
        let bv = u64x4::splat(base);
        let zero = u64x4::splat(0);
        simple_verify_positional_simd!(u64x4, 4, ctx, bv, offsets, zero)
    }
    let simd = unsafe { check(&ctx, base) };
    let scalar = unsafe { verify_words(ctx.ptr, 0, 512, "audit", |i| i as u64 ^ base) };
    assert_eq!((simd, scalar), (1, 3));
    println!("ERROR_UNITS: three corrupt words -> sequential SIMD {simd}, scalar {scalar}");
}

#[test]
fn audit_check_interval_changes_with_vector_width() {
    #[repr(align(64))]
    struct Aligned([u64; 512]);
    let mut data = Aligned([0; 512]);
    let base = 0xDEAD_BEEF_DEAD_BEEF;
    for (i, value) in data.0.iter_mut().enumerate() { *value = i as u64 ^ base; }
    for i in [0, 20] { data.0[i] ^= 1; }
    let ctx = ChunkCtx {
        ptr: data.0.as_mut_ptr(), chunk_start: 0, chunk_end: 512,
        cycle: 1, thread_id: 0, check_mask: Some(7),
    };
    #[target_feature(enable = "avx2")]
    unsafe fn check(ctx: &ChunkCtx, base: u64) -> (u64, u64) {
        let n128 = simple_verify_positional_simd!(
            u64x2, 2, ctx, u64x2::splat(base), u64x2::from_array([0, 1]), u64x2::splat(0)
        );
        let n256 = simple_verify_positional_simd!(
            u64x4, 4, ctx, u64x4::splat(base), u64x4::from_array([0, 1, 2, 3]), u64x4::splat(0)
        );
        (n128, n256)
    }
    let counts = unsafe { check(&ctx, base) };
    assert_eq!(counts, (2, 1));
    println!("CHECK_INTERVAL: identical two faults, mask 7 -> 128-bit {} vs 256-bit {}", counts.0, counts.1);
}

#[test]
fn audit_mode2_low_bits_degrade_before_total_collapse() {
    let mut trailing = [0usize; 65];
    let mut equal = 0;
    for page in 0..256 {
        let (mut seed, mut step) = pattern_gen::mode2_page_start(0x1E980000000 + page * 4096, 0, 1);
        for _ in 0..64 {
            trailing[step.trailing_zeros() as usize] += 1;
            if step == 0 { equal += 1; }
            (seed, step) = pattern_gen::mode2_evolve(seed, step, 0x77777777, 0x33333333);
        }
    }
    let high_only: usize = trailing[32..].iter().sum();
    assert!(high_only > equal);
    println!("MODE2: {equal}/16384 lines fully collapse; {high_only}/16384 have at least 32 shared low bits");
}
