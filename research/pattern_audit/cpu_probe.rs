//! Bounded CPU-cost experiment, not an integrated TMR or DRAM-bandwidth benchmark.
//! Compile with:
//! rustc -O -C target-cpu=x86-64-v3 --edition=2024 --emit=link,asm cpu_probe.rs
//!
//! Single thread, ordinary heap, at most 32 MiB. Each kernel has a statically
//! inlined line generator; dispatch occurs once per buffer, never once per line.
//! Four independent portable-SIMD accumulators verify 128 bytes per iteration.
#![feature(portable_simd)]

use std::hint::black_box;
use std::simd::u64x4;
use std::time::Instant;

const G: u64 = 0x9E37_79B9_7F4A_7C15;
const KEY: u64 = 0x243F_6A88_85A3_08D3;
const MASK63: u64 = (1 << 63) - 1;

#[inline(always)]
fn mix(x: u64) -> u64 { x ^ (x >> 32) }

#[inline(always)]
fn splitmix(mut x: u64) -> u64 {
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

#[inline(always)]
fn balanced(h: u64) -> u64 {
    let mut x = h & 0xFFFF_FFFF;
    x = (x | (x << 16)) & 0x0000_FFFF_0000_FFFF;
    x = (x | (x << 8)) & 0x00FF_00FF_00FF_00FF;
    x = (x | (x << 4)) & 0x0F0F_0F0F_0F0F_0F0F;
    x | ((!x & 0x0F0F_0F0F_0F0F_0F0F) << 4)
}

#[inline(always)]
fn counter_line(addr: usize, _: usize) -> [u64; 8] {
    std::array::from_fn(|j| ((addr >> 3) as u64 + j as u64) ^ KEY)
}

#[inline(always)]
fn current0_line(_: usize, index: usize) -> [u64; 8] {
    std::array::from_fn(|j| {
        let i = (index + j) as u64;
        KEY ^ i.wrapping_mul(G) ^ 0u64.wrapping_sub((i >> 9) & 1)
    })
}

#[inline(always)]
fn strong_line(addr: usize, _: usize) -> [u64; 8] {
    std::array::from_fn(|j| splitmix(((addr >> 3) as u64 + j as u64).wrapping_add(KEY)))
}

#[inline(always)]
fn cheap_line(addr: usize, _: usize) -> [u64; 8] {
    let first = ((addr >> 3) as u64).wrapping_mul(G).wrapping_add(KEY);
    std::array::from_fn(|j| mix(first.wrapping_add((j as u64).wrapping_mul(G))))
}

#[inline(always)]
fn seal_line(addr: usize, _: usize) -> [u64; 8] {
    let first = ((addr >> 4) as u64).wrapping_mul(G);
    std::array::from_fn(|j| {
        let v = mix(first.wrapping_add((j as u64 / 2).wrapping_mul(G)));
        if j & 1 == 0 { v } else { !v }
    })
}

#[inline(always)]
fn tagged_line(addr: usize, _: usize) -> [u64; 8] {
    let first = ((addr >> 4) as u64).wrapping_mul(G).wrapping_add(KEY);
    std::array::from_fn(|j| {
        let v = mix(first.wrapping_add((j as u64 / 2).wrapping_mul(G)) & MASK63);
        if j & 1 == 0 { v } else { !v }
    })
}

#[inline(always)]
fn flip32_line(addr: usize, _: usize) -> [u64; 8] {
    let a = balanced(splitmix((addr as u64).wrapping_add(KEY))) & 0xFFFF_FFFF;
    [a | ((!a & 0xFFFF_FFFF) << 32); 8]
}

#[inline(always)]
fn triple_line(addr: usize, _: usize) -> [u64; 8] {
    let b = balanced(splitmix(((addr >> 6) as u64).wrapping_add(KEY)));
    let (a, c) = (b & 0xFFFF, (b >> 16) & 0xFFFF);
    let na = a ^ 0xFFFF;
    let w0 = a | na << 16 | c << 32 | a << 48;
    let w1 = na | c << 16 | a << 32 | na << 48;
    let w2 = c | a << 16 | na << 32 | c << 48;
    [w0, w1, w2, w0, w1, w2, w0, w1]
}

#[inline(always)]
fn near32_line(addr: usize, _: usize) -> [u64; 8] {
    let seed = splitmix(((addr >> 6) as u64).wrapping_add(KEY));
    let (mut payload, mut control) = (seed & 0xFFFF, seed >> 16);
    let mut words = [0u64; 8];
    for word in &mut words {
        let lo = balanced(payload) & 0xFFFF_FFFF;
        payload ^= 0xFFFF ^ (1 << (control & 15));
        control >>= 4;
        let hi = balanced(payload) & 0xFFFF_FFFF;
        *word = lo | hi << 32;
        payload ^= 0xFFFF ^ (1 << (control & 15));
        control >>= 4;
    }
    words
}

#[inline(always)]
fn near64_line(addr: usize, _: usize) -> [u64; 8] {
    let seed = splitmix(((addr >> 6) as u64).wrapping_add(KEY));
    let (mut payload, mut control) = (seed & 0xFFFF_FFFF, seed >> 32);
    let mut words = [0u64; 8];
    for word in &mut words {
        *word = balanced(payload);
        payload ^= 0xFFFF_FFFF ^ (1 << (control & 31));
        control >>= 5;
    }
    words
}

#[inline(always)]
fn header32_line(addr: usize, _: usize) -> [u64; 8] {
    let seed = splitmix(((addr >> 6) as u64).wrapping_add(KEY));
    let a = balanced(seed);
    let b = balanced(seed >> 32);
    let d = b >> 32;
    let tail = (!d & 0xFFFF_FFFF) | (d << 32);
    [a, b, tail, tail, tail, tail, tail, tail]
}

#[inline(always)]
fn header64_line(addr: usize, _: usize) -> [u64; 8] {
    let seed = splitmix(((addr >> 6) as u64).wrapping_add(KEY));
    let a = balanced(seed);
    let b = balanced(seed >> 32);
    [a, b, !b, b, !b, b, !b, b]
}

type Fill = fn(&mut [u64]);
type Verify = fn(&[u64]) -> bool;

macro_rules! kernels {
    ($fill:ident, $verify:ident, $line:ident) => {
        #[inline(never)]
        fn $fill(buf: &mut [u64]) {
            let base = buf.as_ptr() as usize;
            for (line_index, chunk) in buf.chunks_exact_mut(8).enumerate() {
                chunk.copy_from_slice(&$line(base + 64 * line_index, 8 * line_index));
            }
        }

        #[inline(never)]
        fn $verify(buf: &[u64]) -> bool {
            let base = buf.as_ptr() as usize;
            let mut a0 = u64x4::splat(0);
            let mut a1 = u64x4::splat(0);
            let mut a2 = u64x4::splat(0);
            let mut a3 = u64x4::splat(0);
            for (pair, chunk) in buf.chunks_exact(16).enumerate() {
                let w0 = $line(base + pair * 128, pair * 16);
                let w1 = $line(base + pair * 128 + 64, pair * 16 + 8);
                a0 |= u64x4::from_slice(&chunk[0..4]) ^ u64x4::from_slice(&w0[0..4]);
                a1 |= u64x4::from_slice(&chunk[4..8]) ^ u64x4::from_slice(&w0[4..8]);
                a2 |= u64x4::from_slice(&chunk[8..12]) ^ u64x4::from_slice(&w1[0..4]);
                a3 |= u64x4::from_slice(&chunk[12..16]) ^ u64x4::from_slice(&w1[4..8]);
            }
            (a0 | a1 | a2 | a3).to_array() == [0; 4]
        }
    };
}

kernels!(fill_counter, verify_counter, counter_line);
kernels!(fill_current0, verify_current0, current0_line);
kernels!(fill_strong, verify_strong, strong_line);
kernels!(fill_cheap, verify_cheap, cheap_line);
kernels!(fill_seal, verify_seal, seal_line);
kernels!(fill_tagged, verify_tagged, tagged_line);
kernels!(fill_flip32, verify_flip32, flip32_line);
kernels!(fill_triple, verify_triple, triple_line);
kernels!(fill_near32, verify_near32, near32_line);
kernels!(fill_near64, verify_near64, near64_line);
kernels!(fill_header32, verify_header32, header32_line);
kernels!(fill_header64, verify_header64, header64_line);

fn cpu_name() -> String {
    let mut bytes = Vec::new();
    for leaf in 0x8000_0002..=0x8000_0004 {
        // These leaves only read CPU identification.
        let value = std::arch::x86_64::__cpuid(leaf);
        for word in [value.eax, value.ebx, value.ecx, value.edx] {
            bytes.extend_from_slice(&word.to_le_bytes());
        }
    }
    String::from_utf8_lossy(&bytes).trim_matches('\0').trim().to_string()
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

fn main() {
    assert!(std::is_x86_feature_detected!("avx2"));
    if std::env::args().any(|a| a == "--dump-lines") {
        for address in [0x1E980000000usize, 0x1E980000040, 0x1E980001000] {
            for (name, line) in [
                ("cheap", cheap_line as fn(usize, usize) -> [u64; 8]),
                ("seal", seal_line),
                ("tagged", tagged_line),
                ("near32", near32_line),
                ("near64", near64_line),
                ("triple", triple_line),
                ("header32", header32_line),
                ("header64", header64_line),
            ] {
                println!("{name} {address:x} {:x?}", line(address, 0));
            }
        }
        return;
    }
    let candidates: [(&str, Fill, Verify); 12] = [
        ("Counter", fill_counter, verify_counter),
        ("Current mode 0 formula", fill_current0, verify_current0),
        ("SplitMix per word", fill_strong, verify_strong),
        ("Cheap full-address permutation", fill_cheap, verify_cheap),
        ("Current seal formula", fill_seal, verify_seal),
        ("Tagged 63-bit pair seal", fill_tagged, verify_tagged),
        ("Mode 14 / 32 formula", fill_flip32, verify_flip32),
        ("Triple 16 balanced", fill_triple, verify_triple),
        ("Balanced near-flip / 32", fill_near32, verify_near32),
        ("Balanced near-flip / 64", fill_near64, verify_near64),
        ("Identity header + flip tail / 32", fill_header32, verify_header32),
        ("Identity header + flip tail / 64", fill_header64, verify_header64),
    ];
    println!("CPU: {}", cpu_name());
    println!("One thread, x86-64-v3; temporal stores. Not pinned. Not TMR throughput.");
    println!("Median of five rounds; rotated/reversed candidate order. GiB/s.");
    println!("Working sets 256 KiB and 32 MiB; neither is asserted to bypass the LLC.");
    for (bytes, passes) in [(256 * 1024, 256usize), (32 * 1024 * 1024, 4)] {
        let mut buffer = vec![0u64; bytes / 8];
        let mut samples = vec![([0.0; 5], [0.0; 5]); candidates.len()];
        for round in 0..5 {
            for position in 0..candidates.len() {
                let direction = if round & 1 == 0 { position } else { candidates.len() - 1 - position };
                let which = (direction + round * 3) % candidates.len();
                let (name, fill, verify) = candidates[which];
                fill(black_box(&mut buffer));
                assert!(verify(black_box(&buffer)), "{name}: initial verify");
                buffer[17] ^= 1;
                assert!(!verify(black_box(&buffer)), "{name}: injected fault missed");
                buffer[17] ^= 1;
                let start = Instant::now();
                for _ in 0..passes { fill(black_box(&mut buffer)); }
                samples[which].0[round] = bytes as f64 * passes as f64 / start.elapsed().as_secs_f64() / (1u64 << 30) as f64;
                let start = Instant::now();
                let mut ok = true;
                for _ in 0..passes { ok &= black_box(verify(black_box(&buffer))); }
                samples[which].1[round] = bytes as f64 * passes as f64 / start.elapsed().as_secs_f64() / (1u64 << 30) as f64;
                assert!(ok, "{name}: measured verify");
            }
        }
        println!("\nBuffer {} KiB:", bytes / 1024);
        for ((name, _, _), (mut fills, mut verifies)) in candidates.into_iter().zip(samples) {
            let fill = median(&mut fills);
            let verify = median(&mut verifies);
            println!("{name:32} fill {fill:7.2} verify {verify:7.2}  verify range {:.2}..{:.2}", verifies[0], verifies[4]);
        }
    }
}
