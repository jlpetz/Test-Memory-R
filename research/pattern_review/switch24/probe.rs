//! Bounded CPU and code-generation experiments; no TMR allocator or application.
#![feature(portable_simd)]

#[path = "../../../src/pattern_gen.rs"]
pub mod pattern_gen;

use std::simd::{Simd, num::SimdUint};

/// Byte selectors for a repeated three-byte seed, within each PSHUFB 128-bit lane.
pub const fn selectors<const B: usize>(offset: usize) -> [u8; B] {
    let mut out = [0; B];
    let mut i = 0;
    while i < B {
        out[i] = ((offset + i) % 3) as u8;
        i += 1;
    }
    out
}

/// Complement every other three-byte transfer in the modeled contiguous payload.
pub const fn inversion<const B: usize>(offset: usize) -> [u8; B] {
    let mut out = [0; B];
    let mut i = 0;
    while i < B {
        out[i] = if ((offset + i) / 3) % 2 == 1 { 255 } else { 0 };
        i += 1;
    }
    out
}

const INVERT: [u64; 8] = {
    let bytes = inversion::<64>(0);
    let mut words = [0; 8];
    let mut i = 0;
    while i < 64 {
        words[i / 8] |= (bytes[i] as u64) << (8 * (i % 8));
        i += 1;
    }
    words
};

/// Form the three phases of a 24-bit repetition using only 64-bit operations.
#[inline(always)]
pub fn scalar_repeat(s: u64) -> [u64; 8] {
    let a = (s & 0xff_ffff).wrapping_mul(0x0001_0000_0100_0001);
    let b = (a >> 16) | (a << 32);
    let c = (a >> 8) | (a << 40);
    let words = [a, b, c, a, b, c, a, b];
    std::array::from_fn(|i| words[i] ^ INVERT[i])
}

/// Form a six-byte seed/complement motif, then its three 64-bit phases.
#[inline(always)]
pub fn scalar_motif(s: u64) -> [u64; 8] {
    let motif = (s & 0xff_ffff).wrapping_mul(0x0100_0001) ^ 0xffff_ff00_0000;
    let a = motif | (motif << 48);
    let b = (motif >> 16) | (motif << 32);
    let c = (motif >> 32) | (motif << 16);
    [a, b, c, a, b, c, a, b]
}

macro_rules! shuffle_vectors {
    ($source:expr, $bytes:literal, $lanes:literal, $vectors:literal,
     $raw:ty, $shuffle:path) => {{
        use std::mem::transmute;
        const SELECT: [[u8; $bytes]; $vectors] = {
            let mut out = [[0; $bytes]; $vectors];
            let mut v = 0;
            while v < $vectors {
                out[v] = selectors::<$bytes>(v * $bytes);
                v += 1;
            }
            out
        };
        const INVERSE: [[u8; $bytes]; $vectors] = {
            let mut out = [[0; $bytes]; $vectors];
            let mut v = 0;
            while v < $vectors {
                out[v] = inversion::<$bytes>(v * $bytes);
                v += 1;
            }
            out
        };
        let source = $source;
        // SAFETY: Expanded inside a function with the required target features.
        // All transmuted arrays and vectors have exactly the same bit size.
        unsafe {
            std::array::from_fn::<Simd<u64, $lanes>, $vectors, _>(|v| {
                let select: $raw = transmute(SELECT[v]);
                let result: Simd<u64, $lanes> = transmute($shuffle(source, select));
                let inverse: Simd<u64, $lanes> = transmute(INVERSE[v]);
                result ^ inverse
            })
        }
    }};
}

macro_rules! shuffle_body {
    ($seed:expr, $bytes:literal, $lanes:literal, $vectors:literal,
     $raw:ty, $broadcast:path, $shuffle:path) => {{
        // Expanded inside the corresponding target-feature function.
        let source = $broadcast($seed as i32);
        shuffle_vectors!(source, $bytes, $lanes, $vectors, $raw, $shuffle)
    }};
}

macro_rules! shuffle_line {
    (2, $seed:expr) => {
        shuffle_body!($seed, 16, 2, 4, std::arch::x86_64::__m128i,
            std::arch::x86_64::_mm_set1_epi32, std::arch::x86_64::_mm_shuffle_epi8)
    };
    (4, $seed:expr) => {
        shuffle_body!($seed, 32, 4, 2, std::arch::x86_64::__m256i,
            std::arch::x86_64::_mm256_set1_epi32, std::arch::x86_64::_mm256_shuffle_epi8)
    };
    (8, $seed:expr) => {
        shuffle_body!($seed, 64, 8, 1, std::arch::x86_64::__m512i,
            std::arch::x86_64::_mm512_set1_epi32, std::arch::x86_64::_mm512_shuffle_epi8)
    };
}

/// Build the same three balanced bytes directly in SIMD registers.
macro_rules! lookup_seed {
    ($hash:expr) => {{
        use std::arch::x86_64::*;
        const LUT: [u8; 16] = [
            0xf0, 0xe1, 0xd2, 0xc3, 0xb4, 0xa5, 0x96, 0x87,
            0x78, 0x69, 0x5a, 0x4b, 0x3c, 0x2d, 0x1e, 0x0f,
        ];
        // SAFETY: SSSE3 is part of the declared AVX2 baseline; same-sized transmute.
        unsafe {
            let input = _mm_cvtsi32_si128($hash as i32);
            let nibbles = _mm_unpacklo_epi8(input, _mm_srli_epi16::<4>(input));
            let indices = _mm_and_si128(nibbles, _mm_set1_epi8(15));
            _mm_shuffle_epi8(std::mem::transmute::<[u8; 16], __m128i>(LUT), indices)
        }
    }};
}

macro_rules! lookup_line {
    (2, $h:expr) => {{
        let seed = lookup_seed!($h);
        shuffle_vectors!(seed, 16, 2, 4, std::arch::x86_64::__m128i,
            std::arch::x86_64::_mm_shuffle_epi8)
    }};
    (4, $h:expr) => {{
        let seed = lookup_seed!($h);
        let source = std::arch::x86_64::_mm256_broadcastsi128_si256(seed);
        shuffle_vectors!(source, 32, 4, 2, std::arch::x86_64::__m256i,
            std::arch::x86_64::_mm256_shuffle_epi8)
    }};
    (8, $h:expr) => {{
        let seed = lookup_seed!($h);
        let source = std::arch::x86_64::_mm512_broadcast_i32x4(seed);
        shuffle_vectors!(source, 64, 8, 1, std::arch::x86_64::__m512i,
            std::arch::x86_64::_mm512_shuffle_epi8)
    }};
}

macro_rules! make_hash_line {
    (lookup, $n:tt, $h:expr) => { lookup_line!($n, $h) };
    ($candidate:ident, $n:tt, $h:expr) => {{
        let seed = pattern_gen::balanced_bytes($h & 0xfff);
        make_line!($candidate, $n, seed)
    }};
}

macro_rules! word_vectors {
    ($n:literal, $words:expr) => {{
        let words = $words;
        std::array::from_fn::<Simd<u64, $n>, { 8 / $n }, _>(|v| {
            Simd::from_array(std::array::from_fn(|lane| words[v * $n + lane]))
        })
    }};
}

macro_rules! make_line {
    (shared, $n:tt, $seed:expr) => {
        word_vectors!($n, pattern_gen::bus_flip_line($seed, pattern_gen::BusBeat::Bits24))
    };
    (repeat, $n:tt, $seed:expr) => {
        word_vectors!($n, scalar_repeat($seed))
    };
    (motif, $n:tt, $seed:expr) => {
        word_vectors!($n, scalar_motif($seed))
    };
    (shuffle, $n:tt, $seed:expr) => { shuffle_line!($n, $seed) };
}

macro_rules! kernels {
    ($fill:ident, $verify:ident, $image:ident, $candidate:ident, $n:tt, $features:literal) => {
        /// # Safety
        /// Requires the declared CPU features. `ptr` is 64-byte aligned and writable for `words` u64s; `words` is a multiple of 32.
        #[unsafe(no_mangle)]
        #[inline(never)]
        #[target_feature(enable = $features)]
        pub unsafe fn $fill(ptr: *mut u64, words: usize, key: u64, mask: u64) {
            for off in (0..words).step_by(8) {
                let h = pattern_gen::pattern_mode13((off / 8) as u64, key);
                let image = make_hash_line!($candidate, $n, h);
                for (v, expected) in image.into_iter().enumerate() {
                    unsafe {
                        *ptr.add(off + v * $n).cast::<Simd<u64, $n>>() =
                            expected ^ Simd::splat(mask);
                    }
                }
            }
        }

        /// # Safety
        /// Requires the declared CPU features. `ptr` is 64-byte aligned and readable for `words` u64s; `words` is a multiple of 32.
        #[unsafe(no_mangle)]
        #[inline(never)]
        #[target_feature(enable = $features)]
        pub unsafe fn $verify(ptr: *const u64, words: usize, key: u64, mask: u64) -> u64 {
            let mut acc = [Simd::<u64, $n>::splat(0); 4];
            const VECTORS: usize = 8 / $n;
            const LINES: usize = 4 / VECTORS;
            for off in (0..words).step_by(LINES * 8) {
                for line in 0..LINES {
                    let h = pattern_gen::pattern_mode13((off / 8 + line) as u64, key);
                    let image = make_hash_line!($candidate, $n, h);
                    for (v, expected) in image.into_iter().enumerate() {
                        unsafe {
                            acc[line * VECTORS + v] |=
                                *ptr.add(off + line * 8 + v * $n).cast::<Simd<u64, $n>>()
                                ^ expected ^ Simd::splat(mask);
                        }
                    }
                }
            }
            ((acc[0] | acc[1]) | (acc[2] | acc[3])).reduce_or()
        }

        /// # Safety
        /// Requires the declared CPU features.
        #[target_feature(enable = $features)]
        pub unsafe fn $image(hash: u64) -> [u64; 8] {
            let vectors = make_hash_line!($candidate, $n, hash);
            std::array::from_fn(|i| vectors[i / $n][i % $n])
        }
    };
}

kernels!(fill_shared_128, verify_shared_128, image_shared_128, shared, 2, "avx2");
kernels!(fill_repeat_128, verify_repeat_128, image_repeat_128, repeat, 2, "avx2");
kernels!(fill_motif_128, verify_motif_128, image_motif_128, motif, 2, "avx2");
kernels!(fill_shuffle_128, verify_shuffle_128, image_shuffle_128, shuffle, 2, "avx2");
kernels!(fill_lookup_128, verify_lookup_128, image_lookup_128, lookup, 2, "avx2");
kernels!(fill_shared_256, verify_shared_256, image_shared_256, shared, 4, "avx2");
kernels!(fill_repeat_256, verify_repeat_256, image_repeat_256, repeat, 4, "avx2");
kernels!(fill_motif_256, verify_motif_256, image_motif_256, motif, 4, "avx2");
kernels!(fill_shuffle_256, verify_shuffle_256, image_shuffle_256, shuffle, 4, "avx2");
kernels!(fill_lookup_256, verify_lookup_256, image_lookup_256, lookup, 4, "avx2");
kernels!(fill_shared_512, verify_shared_512, image_shared_512, shared, 8, "avx512f,avx512bw");
kernels!(fill_repeat_512, verify_repeat_512, image_repeat_512, repeat, 8, "avx512f,avx512bw");
kernels!(fill_motif_512, verify_motif_512, image_motif_512, motif, 8, "avx512f,avx512bw");
kernels!(fill_shuffle_512, verify_shuffle_512, image_shuffle_512, shuffle, 8, "avx512f,avx512bw");
kernels!(fill_lookup_512, verify_lookup_512, image_lookup_512, lookup, 8, "avx512f,avx512bw");

#[cfg(test)]
mod checks {
    use super::*;
    use std::hint::black_box;
    use std::time::{Duration, Instant};

    #[repr(align(64))]
    #[derive(Clone)]
    struct Line([u64; 8]);

    type Fill = unsafe fn(*mut u64, usize, u64, u64);
    type Verify = unsafe fn(*const u64, usize, u64, u64) -> u64;
    type Image = unsafe fn(u64) -> [u64; 8];

    struct Candidate {
        width: usize,
        name: &'static str,
        fill: Fill,
        verify: Verify,
        image: Image,
    }

    fn candidates() -> Vec<Candidate> {
        assert!(is_x86_feature_detected!("avx2"));
        let mut out = vec![];
        macro_rules! add {
            ($width:literal, $name:literal, $fill:ident, $verify:ident, $image:ident) => {
                out.push(Candidate { width: $width, name: $name, fill: $fill, verify: $verify, image: $image });
            };
        }
        add!(128, "shared", fill_shared_128, verify_shared_128, image_shared_128);
        add!(128, "repeat", fill_repeat_128, verify_repeat_128, image_repeat_128);
        add!(128, "motif", fill_motif_128, verify_motif_128, image_motif_128);
        add!(128, "shuffle", fill_shuffle_128, verify_shuffle_128, image_shuffle_128);
        add!(128, "lookup", fill_lookup_128, verify_lookup_128, image_lookup_128);
        add!(256, "shared", fill_shared_256, verify_shared_256, image_shared_256);
        add!(256, "repeat", fill_repeat_256, verify_repeat_256, image_repeat_256);
        add!(256, "motif", fill_motif_256, verify_motif_256, image_motif_256);
        add!(256, "shuffle", fill_shuffle_256, verify_shuffle_256, image_shuffle_256);
        add!(256, "lookup", fill_lookup_256, verify_lookup_256, image_lookup_256);
        if is_x86_feature_detected!("avx512f") && is_x86_feature_detected!("avx512bw") {
            add!(512, "shared", fill_shared_512, verify_shared_512, image_shared_512);
            add!(512, "repeat", fill_repeat_512, verify_repeat_512, image_repeat_512);
            add!(512, "motif", fill_motif_512, verify_motif_512, image_motif_512);
            add!(512, "shuffle", fill_shuffle_512, verify_shuffle_512, image_shuffle_512);
            add!(512, "lookup", fill_lookup_512, verify_lookup_512, image_lookup_512);
        }
        out
    }

    // Independent byte-level reference, not the production packer or its tables.
    fn oracle(seed: u64) -> [u64; 8] {
        let mut out = [0; 8];
        for i in 0..64 {
            let byte = ((seed >> (8 * (i % 3))) as u8)
                ^ if (i / 3) % 2 == 1 { 255 } else { 0 };
            out[i / 8] |= u64::from(byte) << (8 * (i % 8));
        }
        out
    }

    #[test]
    fn review_switch24_all_balanced_seeds_and_affine_basis() {
        let candidates = candidates();
        // The packers use only bit placement and XOR: zero and all 24 input basis vectors also check arbitrary (not just balanced) 24-bit
        // seeds.
        for seed in std::iter::once(0).chain((0..24).map(|bit| 1u64 << bit)) {
            let expected = oracle(seed);
            assert_eq!(scalar_repeat(seed), expected);
            assert_eq!(scalar_motif(seed), expected);
        }
        for h in 0..4096 {
            let seed = pattern_gen::balanced_bytes(h) & 0xff_ffff;
            let expected = oracle(seed);
            assert_eq!(scalar_repeat(seed), expected);
            assert_eq!(scalar_motif(seed), expected);
            for candidate in &candidates {
                assert_eq!(unsafe { (candidate.image)(h) }, expected,
                    "{} {}", candidate.width, candidate.name);
            }
        }
    }

    #[test]
    fn review_switch24_every_bit_fault_and_pair_polarities() {
        let candidates = candidates();
        let mut data = vec![Line([0; 8]); 8];
        let ptr = data.as_mut_ptr().cast::<u64>();
        let words = data.len() * 8;
        for mask in [0, u64::MAX] {
            unsafe { fill_shared_256(ptr, words, 0x1234_5678_9abc_def0, mask); }
            let reference: Vec<_> = data.iter().map(|line| line.0).collect();
            for candidate in &candidates {
                unsafe {
                    (candidate.fill)(ptr, words, 0x1234_5678_9abc_def0, mask);
                    assert!(data.iter().zip(&reference).all(|(a,b)| a.0 == *b));
                    assert_eq!((candidate.verify)(ptr, words, 0x1234_5678_9abc_def0, mask), 0);
                    for word in 0..words {
                        for bit in 0..64 {
                            *ptr.add(word) ^= 1 << bit;
                            assert_eq!((candidate.verify)(ptr, words, 0x1234_5678_9abc_def0, mask), 1 << bit);
                            *ptr.add(word) ^= 1 << bit;
                        }
                    }
                    // Identical bit errors in separate accumulator groups cannot cancel.
                    *ptr.add(0) ^= 1 << 47;
                    *ptr.add(32) ^= 1 << 47;
                    assert_eq!((candidate.verify)(ptr, words, 0x1234_5678_9abc_def0, mask), 1 << 47);
                    *ptr.add(0) ^= 1 << 47;
                    *ptr.add(32) ^= 1 << 47;
                }
            }
        }
    }

    #[test]
    #[ignore = "explicit bounded CPU sample after inspecting assembly; not DRAM throughput"]
    fn review_switch24_cpu_sample() {
        let candidates = candidates();
        for kib in [8, 256] {
            let mut data = vec![Line([0; 8]); kib * 16];
            let ptr = data.as_mut_ptr().cast::<u64>();
            let words = data.len() * 8;
            for block in 0..9 {
                for at in 0..candidates.len() {
                    let candidate = &candidates[(at + block * 5) % candidates.len()];
                    let key = 0x1234_5678_9abc_def0u64.wrapping_add(block as u64);
                    let mask = if block % 2 == 1 { u64::MAX } else { 0 };
                    unsafe { fill_shared_256(ptr, words, key, mask); }
                    for action in ["verify", "fill"] {
                        let start = Instant::now();
                        let mut repeats = 0;
                        let mut sink = 0u64;
                        while start.elapsed() < Duration::from_millis(12) {
                            for _ in 0..8 {
                                if action == "verify" {
                                    sink = sink.wrapping_add(unsafe {
                                        (candidate.verify)(black_box(ptr), black_box(words),
                                            black_box(key), black_box(mask))
                                    });
                                } else {
                                    unsafe {
                                        (candidate.fill)(black_box(ptr), black_box(words),
                                            black_box(key), black_box(mask));
                                    }
                                }
                            }
                            repeats += 8;
                        }
                        let elapsed = start.elapsed().as_nanos() as f64;
                        assert_eq!(black_box(sink), 0);
                        println!("sample,{kib},{block},{},{},{action},{:.6}",
                            candidate.width, candidate.name, elapsed / (repeats * data.len()) as f64);
                    }
                }
            }
        }
    }
}
