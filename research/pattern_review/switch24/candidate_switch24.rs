//! SIMD encoder for the existing 24-DQ contiguous-payload model.
//!
//! The scalar oracle remains `bus_flip_from_hash(h, BusBeat::Bits24)`. This encoder
//! produces exactly those bytes: a three-byte seed, then its three complemented
//! bytes, repeated across the 64-byte payload. No hash, seed, or phase changes.
//! Macros place the width-specific operations inside the caller's hot loop.

use std::arch::x86_64::*;

/// Expand the low three hash nibbles into the low three balanced bytes.
#[inline(always)]
pub(crate) fn seed(h: u64) -> __m128i {
    const BALANCED: [u8; 16] = [
        0xf0, 0xe1, 0xd2, 0xc3, 0xb4, 0xa5, 0x96, 0x87, 0x78, 0x69, 0x5a, 0x4b, 0x3c, 0x2d, 0x1e, 0x0f,
    ];
    // SAFETY: TMR's x86-64-v3 baseline includes SSSE3. The bitcast keeps 128 bits.
    unsafe {
        let x = _mm_cvtsi32_si128(h as i32);
        let pairs = _mm_unpacklo_epi8(x, _mm_srli_epi16::<4>(x));
        let indices = _mm_and_si128(pairs, _mm_set1_epi8(15));
        _mm_shuffle_epi8(std::mem::transmute::<[u8; 16], __m128i>(BALANCED), indices)
    }
}

/// Each PSHUFB lane selects from its copy of the first three seed bytes.
pub(crate) const fn selectors<const B: usize>(offset: usize) -> [u8; B] {
    let mut out = [0; B];
    let mut i = 0;
    while i < B {
        out[i] = ((offset + i) % 3) as u8;
        i += 1;
    }
    out
}

pub(crate) const fn inversion<const B: usize>(offset: usize) -> [u8; B] {
    let mut out = [0; B];
    let mut i = 0;
    while i < B {
        out[i] = if ((offset + i) / 3) % 2 == 1 { 255 } else { 0 };
        i += 1;
    }
    out
}

macro_rules! layout {
    ($source:expr, $bytes:literal, $lanes:literal, $vectors:literal,
     $raw:ty, $shuffle:path) => {{
        const SELECT: [[u8; $bytes]; $vectors] = {
            let mut out = [[0; $bytes]; $vectors];
            let mut v = 0;
            while v < $vectors {
                out[v] = $crate::pattern_gen::switch24::selectors::<$bytes>(v * $bytes);
                v += 1;
            }
            out
        };
        const INVERSE: [[u8; $bytes]; $vectors] = {
            let mut out = [[0; $bytes]; $vectors];
            let mut v = 0;
            while v < $vectors {
                out[v] = $crate::pattern_gen::switch24::inversion::<$bytes>(v * $bytes);
                v += 1;
            }
            out
        };
        // SAFETY: The caller has the selected width's target features, including
        // AVX-512BW for 512 bits. Every bitcast preserves the vector's bit size.
        unsafe {
            let source = $source;
            std::array::from_fn::<std::simd::Simd<u64, $lanes>, $vectors, _>(|v| {
                let select: $raw = std::mem::transmute(SELECT[v]);
                let result: std::simd::Simd<u64, $lanes> = std::mem::transmute($shuffle(source, select));
                let inverse: std::simd::Simd<u64, $lanes> = std::mem::transmute(INVERSE[v]);
                result ^ inverse
            })
        }
    }};
}
pub(crate) use layout;

macro_rules! vectors {
    (2, $hash:expr) => {{
        let seed = $crate::pattern_gen::switch24::seed($hash);
        $crate::pattern_gen::switch24::layout!(seed, 16, 2, 4, std::arch::x86_64::__m128i, std::arch::x86_64::_mm_shuffle_epi8)
    }};
    (4, $hash:expr) => {{
        let seed = $crate::pattern_gen::switch24::seed($hash);
        $crate::pattern_gen::switch24::layout!(
            std::arch::x86_64::_mm256_broadcastsi128_si256(seed),
            32,
            4,
            2,
            std::arch::x86_64::__m256i,
            std::arch::x86_64::_mm256_shuffle_epi8
        )
    }};
    (8, $hash:expr) => {{
        let seed = $crate::pattern_gen::switch24::seed($hash);
        $crate::pattern_gen::switch24::layout!(
            std::arch::x86_64::_mm512_broadcast_i32x4(seed),
            64,
            8,
            1,
            std::arch::x86_64::__m512i,
            std::arch::x86_64::_mm512_shuffle_epi8
        )
    }};
}
pub(crate) use vectors;

#[cfg(test)]
mod tests {
    macro_rules! image {
        ($name:ident, $lanes:tt, $features:literal) => {
            #[target_feature(enable = $features)]
            unsafe fn $name(h: u64) -> [u64; 8] {
                let packed = super::vectors!($lanes, h);
                std::array::from_fn(|i| packed[i / $lanes][i % $lanes])
            }
        };
    }
    image!(image128, 2, "ssse3");
    image!(image256, 4, "avx2");
    image!(image512, 8, "avx512f,avx512bw");

    // Byte reference independent of the scalar packer and SIMD selector tables.
    fn expected(h: u64) -> [u64; 8] {
        let mut words = [0; 8];
        for i in 0..64 {
            let nibble = ((h >> (4 * (i % 3))) & 15) as u8;
            let byte = (nibble | ((nibble ^ 15) << 4)) ^ if (i / 3) % 2 == 1 { 255 } else { 0 };
            words[i / 8] |= u64::from(byte) << (8 * (i % 8));
        }
        words
    }

    #[test]
    fn switch24_simd_covers_every_seed_and_ignores_unused_hash_bits() {
        let has512 = is_x86_feature_detected!("avx512f") && is_x86_feature_detected!("avx512bw");
        for h in 0..4096 {
            let want = expected(h);
            for high in [0, 0xfedc_ba98_7654_3000] {
                // SAFETY: The baseline provides SSSE3 and AVX2; AVX-512 is checked.
                unsafe {
                    assert_eq!(image128(h | high), want);
                    assert_eq!(image256(h | high), want);
                    if has512 {
                        assert_eq!(image512(h | high), want);
                    }
                }
            }
        }
    }
}
