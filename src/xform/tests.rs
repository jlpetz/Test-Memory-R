//! Mem-TileTransform's tests (TODO 102), ported from plumb's `bench/src/tests_xform.rs`. The construction and the stored checksum are
//! checked on any CPU; everything that runs the kernels needs AMX-INT8 and returns early without it (CI's runners, the AMD hosts). These run
//! unoptimised under `cargo test`, so the exhaustive sweeps plumb runs (131,072 cases each) are `#[ignore]`d measurements, run by name in
//! release; the default runs sample them.

use super::*;
use crate::runner::test_support::{KIB, Region};

/// Whether this CPU runs the kernels; says so when it doesn't, and the test returns.
fn amx() -> bool {
    let has = crate::amx::has_amx_int8();
    if !has {
        eprintln!("skip: no AMX-INT8 on this CPU/OS");
    }
    has
}

/// Bits a tile has.
const TILE_BITS: usize = TILE_U64 * 64;

/// `n` zeroed tiles of 64-byte aligned memory, as a span's.
fn tiles(n: usize) -> Vec<Tile> {
    vec![Tile([0; TILE_U64]); n]
}

/// The run's set at `X`, with `r` and `layout`.
fn set<const X: usize>(r: usize, layout: u64) -> PatternSet<X> {
    // SAFETY: the callers checked AMX-INT8 (`amx`)
    unsafe { PatternSet::new(Matrices::generate(BUILTIN_SEED), r, layout) }
}

fn get_bit(words: &[u64], bit: usize) -> bool {
    (words[bit / 64] >> (bit % 64)) & 1 == 1
}

fn flip_bit(words: &mut [u64], bit: usize) {
    words[bit / 64] ^= 1 << (bit % 64);
}

/// Tile `t` of the buffer at `base`, as words.
fn tile_words<'a>(base: *mut u64, t: usize) -> &'a mut [u64] {
    // SAFETY: the callers' buffers hold tile `t`, and nothing else holds a reference into it while this one lives
    unsafe { std::slice::from_raw_parts_mut(base.add(t * TILE_U64), TILE_U64) }
}

/// W on tiles `first..end`, as a chunk of the test.
fn write<const X: usize>(base: *mut u64, first: usize, end: usize, set: &PatternSet<X>, pass: usize) {
    // SAFETY: AMX-INT8 (the callers'); the buffer holds the tiles
    unsafe { write_tiles::<X>(base, first, end, set.m.patterns.as_ptr(), set.layout, pass) }
}

/// RW on tiles `first..end`.
fn transform<const X: usize>(base: *mut u64, first: usize, end: usize, set: &PatternSet<X>) {
    // SAFETY: as `write`
    unsafe { transform_tiles(base.add(first * TILE_U64), end - first, &raw const set.m.transform) }
}

/// R on tiles `first..end` as `run` does it, without the runner: the partial groups word by word, the whole groups by their checksums,
/// each failure rescanned. The errors it counts.
fn verify<const X: usize>(base: *mut u64, first: usize, end: usize, set: &PatternSet<X>, pass: usize) -> u64 {
    let mut errors = 0;
    let (g_from, g_to) = (first.div_ceil(X), end / X);
    let (head_end, tail_start) = if g_from < g_to { (g_from * X, g_to * X) } else { (end, end) };
    // SAFETY: AMX-INT8 (the callers'); the buffer holds the tiles
    unsafe {
        for (from, to) in [(first, head_end), (tail_start, end)] {
            if from < to && verify_part::<X>(base, from, to, set, pass) != 0 {
                errors += rescan_tiles::<X>(base, from, to, set, pass, "partial group");
            }
        }
        let mut g = g_from;
        while g < g_to {
            let bad = verify_groups::<X>(base, g, g_to, set.check(), pass);
            if bad == NONE {
                break;
            }
            errors += rescan_group::<X>(base, bad, set, pass);
            g = bad + 1;
        }
    }
    errors
}

/// The bad tiles tier 2 found on this thread since the last take, and its last checksum recheck.
fn take_found() -> (Vec<BadTile>, Option<bool>) {
    (FOUND.with_borrow_mut(std::mem::take), RECHECKED.with(|r| r.take()))
}

// ─── The scalar reference ────────────────────────────────────────────────────────────────────────

/// Byte `i` of a tile's words, little-endian.
fn byte(words: &[u64; TILE_U64], i: usize) -> u8 {
    (words[i / 8] >> (i % 8 * 8)) as u8
}

/// `A x B` as TDPBUUD defines it, `C[m][n] = sum(j < 64) A[m][j] * B[j][n]` (u8 x u8 into u32, wrapping), in plain scalar code that never
/// touches AMX: the independent computation. `A` is 16 rows of 64 bytes, `B` the VNNI-packed matrix, `C` in TILESTORED's layout.
fn scalar_multiply(a: &[u64; TILE_U64], b: &Matrix) -> Tile {
    let c: [u32; 2 * TILE_U64] = std::array::from_fn(|mn| {
        let (m, n) = (mn / 16, mn % 16);
        (0..64).fold(0u32, |s, j| s.wrapping_add(u32::from(byte(a, m * 64 + j)) * u32::from(b.get(j, n))))
    });
    Tile(std::array::from_fn(|w| u64::from(c[2 * w]) | (u64::from(c[2 * w + 1]) << 32)))
}

/// Two words of u32 lanes combined lane by lane with `f`.
fn lanes(a: u64, b: u64, f: impl Fn(u32, u32) -> u32) -> u64 {
    u64::from(f(a as u32, b as u32)) | (u64::from(f((a >> 32) as u32, (b >> 32) as u32)) << 32)
}

/// `n` times a checksum, lane by lane, wrapping: what `n` repeats of every product add up to.
fn scale(e: &Tile, n: usize) -> Tile {
    let n = n as u64 | ((n as u64) << 32);
    Tile(e.0.map(|w| lanes(w, n, u32::wrapping_mul)))
}

/// `T(P_i)` for each pattern and the group checksum at r = 0, by the scalar reference.
fn scalar_values<const X: usize>(m: &Matrices<X>) -> (Vec<Tile>, Tile) {
    let t: Vec<Tile> = m.patterns.iter().map(|p| scalar_multiply(&p.0, &m.transform)).collect();
    let mut sum = [0u64; TILE_U64];
    for (ti, k) in t.iter().zip(m.keys.iter()) {
        for (s, q) in sum.iter_mut().zip(scalar_multiply(&ti.0, k).0) {
            *s = lanes(*s, q, u32::wrapping_add);
        }
    }
    (t, Tile(sum))
}

// ─── The set and its expected values ─────────────────────────────────────────────────────────────

/// What the guarantees rest on (`Matrices::generate`): every byte odd, the 16-byte rows distinct across the set's matrices, the
/// transform's rows distinct.
fn assert_odd_and_distinct<const X: usize>(m: &Matrices<X>, what: &str) {
    let odd = |m: &Matrix| m.0.iter().all(|w| w & 0x0101_0101_0101_0101 == 0x0101_0101_0101_0101);
    assert!(odd(&m.transform), "{what}: the transform has an even byte");
    assert_eq!((0..64).map(|j| m.transform.row(j)).collect::<HashSet<_>>().len(), 64, "{what}: the transform's rows repeat");
    let mut rows = HashSet::new();
    for (i, k) in m.keys.iter().enumerate() {
        assert!(odd(k), "{what}: pattern {i}'s matrix has an even byte");
        for j in 0..64 {
            assert!(rows.insert(k.row(j)), "{what}: row {j} of pattern {i}'s matrix repeats");
        }
    }
}

/// TODO 102: every matrix byte is odd and every row distinct, at every set size and several seeds.
#[test]
fn the_matrices_are_odd_with_distinct_rows() {
    for seed in [0u64, 1, BUILTIN_SEED, u64::MAX] {
        assert_odd_and_distinct(&Matrices::<16>::generate(seed), &format!("X = 16, seed {seed:#x}"));
        assert_odd_and_distinct(&Matrices::<32>::generate(seed), &format!("X = 32, seed {seed:#x}"));
        assert_odd_and_distinct(&Matrices::<64>::generate(seed), &format!("X = 64, seed {seed:#x}"));
    }
}

/// TODO 102: the checksum `self_test` compares with is the scalar reference's for the built-in set.
#[test]
fn the_built_in_sets_stored_checksum_is_the_scalar_references() {
    let (_, e) = scalar_values(&Matrices::<16>::generate(BUILTIN_SEED));
    assert_eq!(e.0, BUILTIN_CHECKSUM, "xform_builtin.rs is stale: regenerate it (builtin_regenerate)");
}

/// Writes `src/xform_builtin.rs`, the built-in set's checksum, from the scalar reference (the seed decides the set; no value comes from the
/// tiles).
#[test]
#[ignore = "writes src/xform_builtin.rs: run it by name after changing how a set is generated"]
fn builtin_regenerate() {
    let (_, e) = scalar_values(&Matrices::<16>::generate(BUILTIN_SEED));
    let mut s = String::from(
        "// Generated by `cargo test --release --lib xform::tests::builtin_regenerate -- --ignored` from the tests' scalar reference, never the \
         tiles:\n// the built-in set's group checksum at r = 0 (X = 16, BUILTIN_SEED), as TILESTORED lays it out. The same as plumb's (its \
         generator is\n// this one). Don't edit by hand.\n\n/// The built-in set's group checksum at r = 0 (`self_test`).\nconst BUILTIN_CHECKSUM: \
         [u64; TILE_U64] = [\n",
    );
    for line in e.0.chunks(4) {
        let words: Vec<String> = line.iter().map(|w| format!("{w:#018x}")).collect();
        s.push_str(&format!("    {},\n", words.join(", ")));
    }
    s.push_str("];\n");
    std::fs::write(concat!(env!("CARGO_MANIFEST_DIR"), "/src/xform_builtin.rs"), s).expect("write xform_builtin.rs");
}

fn assert_matches_scalar<const X: usize>(r: usize, seed: u64) {
    // SAFETY: the caller checked AMX-INT8
    let set = unsafe { PatternSet::<X>::new(Matrices::generate(seed), r, 7) };
    let (t, e) = scalar_values(&set.m);
    let what = format!("X = {X}, seed {seed:#x}, r = {r}");
    assert_eq!(set.expected_t.as_slice(), t.as_slice(), "{what}: T(P_i)");
    assert_eq!(set.checksum, e, "{what}: the checksum");
    assert_eq!(set.expected_sum, scale(&e, 1 + r), "{what}: the expected sum");
}

/// TODO 102: the values computed on the tiles are the scalar reference's, at every set size and load setting, and the matrix unit passes
/// the self-test.
#[test]
fn the_expected_values_match_an_independent_computation() {
    if !amx() {
        return;
    }
    for r in [0, 1, 4, AHEAD_FROM_R, 255] {
        for seed in [0u64, BUILTIN_SEED, u64::MAX] {
            assert_matches_scalar::<16>(r, seed);
            assert_matches_scalar::<32>(r, seed);
            assert_matches_scalar::<64>(r, seed);
        }
    }
    // SAFETY: AMX-INT8
    assert!(unsafe { self_test() }, "the built-in set's checksum on this CPU's matrix unit");
}

// ─── The rotation ────────────────────────────────────────────────────────────────────────────────

fn every_pattern_each_cycle<const X: usize>() {
    for g in 0..64 {
        for cycle in 0..3 {
            let seen: HashSet<usize> = (0..X).map(|p| rotation::<X>(0xC1C1E, g, cycle * X + p)).collect();
            assert_eq!(seen.len(), X, "X = {X}, group {g}, cycle {cycle}");
        }
    }
}

/// TODO 102: over each cycle of `X` passes, every tile holds every pattern once.
#[test]
fn every_tile_holds_every_pattern_each_cycle() {
    every_pattern_each_cycle::<16>();
    every_pattern_each_cycle::<32>();
    every_pattern_each_cycle::<64>();
}

/// TODO 102: groups an addressing fault would pair (one address bit apart: neighbours and every power-of-two distance) hold the same data
/// for a whole cycle about 1 time in `X^4 / 4` (16,384 at X = 16), never two cycles running; and the layout changes the arrangement.
#[test]
fn the_rotation_differs_between_groups_and_layouts() {
    let cycle = |layout: u64, g: usize, c: usize| -> [usize; 16] { std::array::from_fn(|p| rotation::<16>(layout, g, c * 16 + p)) };
    let (mut pairs, mut blind) = (0, 0);
    for g in 0..4096usize {
        for k in 0..12 {
            let h = g ^ (1 << k);
            if h < g {
                continue;
            }
            pairs += 1;
            let same = [0, 1].map(|c| cycle(0x5EED, g, c) == cycle(0x5EED, h, c));
            blind += same.iter().filter(|&&s| s).count();
            assert!(!(same[0] && same[1]), "groups {g} and {h}: the same data two cycles running");
        }
    }
    // 24,576 pairs x 2 cycles / 16,384: about 3 expected
    assert!(blind <= 12, "{blind} of {pairs} pairs x 2 cycles held the same data for a cycle");
    let differ = (0..64).filter(|&g| cycle(0x5EED, g, 0) != cycle(0x5EEE, g, 0)).count();
    assert!(differ >= 60, "only {differ} of 64 groups' arrangements changed with the layout");
}

// ─── Clean chunks ────────────────────────────────────────────────────────────────────────────────

/// A chunk cut out of a buffer of 3 groups: it starts 4 tiles before group 1 and ends 4 tiles before group 3, so it has a partial group
/// at each end around a whole one (chunks start on 4 KiB, 4 tiles).
fn chunk_of<const X: usize>() -> (usize, usize) {
    (X - 4, 3 * X - 4)
}

fn clean_chunks<const X: usize>(r: usize) {
    let set = set::<X>(r, 0xC1EA);
    let mut buf = tiles(3 * X);
    let base = buf.as_mut_ptr().cast::<u64>();
    let (first, end) = chunk_of::<X>();
    for pass in 0..2 * X + 1 {
        write(base, first, end, &set, pass);
        transform(base, first, end, &set);
        let errors = verify(base, first, end, &set, pass);
        assert_eq!((errors, take_found()), (0, (vec![], None)), "X = {X}, r = {r}, pass {pass}");
        for t in first..end {
            assert_eq!(tile_words(base, t), set.expected_t[set.pattern_at(t, pass)].0, "X = {X} pass {pass} tile {t}");
        }
    }
    assert!(buf[..first].iter().chain(&buf[end..]).all(|t| t.0 == [0; TILE_U64]), "X = {X}: wrote outside the chunk");
}

/// TODO 102: a chunk whose ends cut groups runs clean at every set size, at load settings on both of R's loops, over two cycles of passes;
/// each tile holds its rotated pattern's transform, and nothing outside the chunk is written.
#[test]
fn chunks_cut_anywhere_run_clean_and_hold_the_rotated_transforms() {
    if !amx() {
        return;
    }
    for r in [0, 3, AHEAD_FROM_R, 9] {
        clean_chunks::<16>(r);
        clean_chunks::<32>(r);
        clean_chunks::<64>(r);
    }
}

// ─── Fault injection ─────────────────────────────────────────────────────────────────────────────

/// The bits of a tile the sampled tests flip: a spread over every row and byte, with each end.
fn sampled_bits() -> impl Iterator<Item = usize> {
    (0..TILE_BITS).step_by(509).chain([TILE_BITS - 1])
}

fn stored_bits<const X: usize>(r: usize) {
    let set = set::<X>(r, 0x5106);
    let mut buf = tiles(3 * X);
    let base = buf.as_mut_ptr().cast::<u64>();
    let (first, end) = chunk_of::<X>();
    let pass = 3;
    for t in first..end {
        let whole = (X..2 * X).contains(&t);
        for bit in sampled_bits() {
            write(base, first, end, &set, pass);
            transform(base, first, end, &set);
            flip_bit(tile_words(base, t), bit);
            crate::tests::reset_rescan_counts();
            let errors = verify(base, first, end, &set, pass);
            let what = format!("X = {X}, r = {r}, tile {t}, bit {bit}");
            let (found, recheck) = take_found();
            let signature = Signature::Stored { byte: bit / 8, bit: (bit % 8) as u32 };
            assert_eq!(found, [BadTile { tile: t, pattern: set.pattern_at(t, pass), bad_words: 1, signature }], "{what}");
            assert_eq!((errors, recheck), (1, whole.then_some(true)), "{what}: errors, and the tiles' recheck of a whole group");
            let counts = crate::tests::rescan_counts();
            assert_eq!((counts.alarms, counts.not_reproduced), (1, 0), "{what}");
        }
    }
}

/// TODO 102: one flipped bit in the stored result, anywhere in a chunk (the partial groups at its ends and a whole group), on both of R's
/// loops: caught, counted once, and located by tier 2 to its tile, byte and bit; a whole group's checksum is still wrong through the
/// tiles. A sample; `every_stored_bit_is_caught_and_located` is every bit.
#[test]
fn one_flipped_bit_in_the_stored_result_is_caught_and_located() {
    if !amx() {
        return;
    }
    for r in [3, AHEAD_FROM_R] {
        stored_bits::<16>(r);
        stored_bits::<64>(r);
    }
}

fn bits_before_the_transform<const X: usize>() {
    let set = set::<X>(0, 0xBEF0);
    let mut buf = tiles(3 * X);
    let base = buf.as_mut_ptr().cast::<u64>();
    let (first, end) = chunk_of::<X>();
    let pass = 5;
    for t in first..end {
        let pattern = set.pattern_at(t, pass);
        for bit in sampled_bits() {
            write(base, first, end, &set, pass);
            flip_bit(tile_words(base, t), bit);
            transform(base, first, end, &set);
            let errors = verify(base, first, end, &set, pass);
            let rose = !get_bit(&set.m.patterns[pattern].0, bit);
            let signature = Signature::BeforeTransform { byte: bit / 8, bit: (bit % 8) as u32, rose };
            let what = format!("X = {X}, tile {t}, bit {bit}");
            assert_eq!(take_found().0, [BadTile { tile: t, pattern, bad_words: 8, signature }], "{what}");
            assert_eq!(errors, 8, "{what}: the row's 8 words");
        }
    }
}

/// TODO 102: one bit of a pattern flipped between W and RW (memory that went bad before the transform), anywhere in a chunk: caught, and
/// tier 2 works back to the pattern's byte and bit and the way it flipped. A sample; `every_bit_before_the_transform_is_caught_and_located`
/// is every bit of the built-in set.
#[test]
fn one_bit_flipped_before_the_transform_is_caught_and_located() {
    if !amx() {
        return;
    }
    bits_before_the_transform::<16>();
    bits_before_the_transform::<32>();
}

/// The two flips at bit `b` of data row `m` that could cancel, in tiles `t1` and `t2` of the group at `base`'s tiles `g0..`: a 0 set in
/// `t1` and a 1 cleared in `t2`, as bit indices in their tiles; the same byte where their bits allow (two patterns sharing a matrix would
/// collide there), two bytes of one tile. `None` if the stored data has no such pair.
fn opposite_flips(base: *mut u64, t1: usize, t2: usize, m: usize, b: usize, start: usize) -> Option<(usize, usize)> {
    let at = |j: usize| (m * 64 + j) * 8 + b;
    let from = |s: usize| (0..64).map(move |j| (s + j) % 64);
    let rises = |j| !get_bit(tile_words(base, t1), at(j));
    let falls = |j| get_bit(tile_words(base, t2), at(j));
    let same = (t1 != t2).then(|| from(start).find(|&j| rises(j) && falls(j))).flatten();
    let (j1, j2) = match same {
        Some(j) => (j, j),
        None => {
            let j1 = from(start).find(|&j| rises(j))?;
            (j1, from(start + 32).find(|&j| (t1, j1) != (t2, j) && falls(j))?)
        }
    };
    Some((at(j1), at(j2)))
}

/// TODO 102: two flipped bits can only cancel at the same bit of the same data row, in opposite directions (`Matrices::generate`); the
/// distinct rows rule that out. A sample of tile pairs of a group (the same tile twice among them), rows and bits, at r = 1 (the change
/// doubled).
#[test]
fn two_flips_that_could_cancel_are_caught() {
    if !amx() {
        return;
    }
    let set = set::<16>(1, 0x2B17);
    let mut buf = tiles(16);
    let base = buf.as_mut_ptr().cast::<u64>();
    let mut injected = 0;
    for (t1, t2) in [(0, 1), (0, 15), (3, 3), (5, 9), (14, 2), (7, 8)] {
        for m in [0, 9, 15] {
            for b in [0, 4, 7] {
                write(base, 0, 16, &set, 7);
                transform(base, 0, 16, &set);
                let Some((bit1, bit2)) = opposite_flips(base, t1, t2, m, b, (t1 * 7 + m * 3 + b) % 64) else { continue };
                flip_bit(tile_words(base, t1), bit1);
                flip_bit(tile_words(base, t2), bit2);
                injected += 1;
                assert!(verify(base, 0, 16, &set, 7) >= 1, "tiles {t1}, {t2}, row {m}, bit {b}: not caught");
                take_found();
            }
        }
    }
    assert!(injected > 40, "only {injected} cases found a pair to flip");
}

/// A seeded xorshift for the random faults.
struct TestRng(u64);

impl TestRng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// TODO 102: three or more flips could cancel in principle, so this measures it: random faults in a group's stored result, each either 3
/// to 16 distinct flipped bits anywhere in it or 1 to 8 words XORed with random masks, all caught.
#[test]
fn three_or_more_flips_are_caught_in_random_injection() {
    if !amx() {
        return;
    }
    let set = set::<16>(0, 0x3F11);
    let mut buf = tiles(16);
    let base = buf.as_mut_ptr().cast::<u64>();
    for c in 0..200u64 {
        write(base, 0, 16, &set, 11);
        transform(base, 0, 16, &set);
        // SAFETY: the buffer is 16 tiles, and nothing else refers to it while `group` lives
        let group = unsafe { std::slice::from_raw_parts_mut(base, 16 * TILE_U64) };
        let mut rng = TestRng(crate::pattern::fmix(c) | 1);
        if c % 4 == 0 {
            for _ in 0..1 + rng.below(8) {
                let w = rng.below(group.len());
                group[w] ^= rng.next() | 1;
            }
        } else {
            let mut bits = HashSet::new();
            while bits.len() < 3 + rng.below(14) {
                bits.insert(rng.below(group.len() * 64));
            }
            bits.into_iter().for_each(|bit| flip_bit(group, bit));
        }
        assert!(verify(base, 0, 16, &set, 11) >= 1, "case {c}: not caught");
        take_found();
    }
}

/// TODO 102: each product goes into the checksum `1 + r` times, so a fault changes it by `1 + r` times its change at r = 0, which can't
/// wrap to zero up to `MAX_R`: a single bit and two flips that could cancel are caught at load settings up to the largest, with `1 + r` a
/// power of two (the tightest case for wrapping) and at the cap; the expected sum is the scalar one scaled.
#[test]
fn detection_holds_at_any_load_setting() {
    if !amx() {
        return;
    }
    let (_, e) = scalar_values(&Matrices::<16>::generate(BUILTIN_SEED));
    let mut buf = tiles(16);
    let base = buf.as_mut_ptr().cast::<u64>();
    for r in [1, 7, 255, 32767, MAX_R as usize] {
        let set = set::<16>(r, 0xA11);
        assert_eq!(set.expected_sum, scale(&e, 1 + r), "r = {r}");
        write(base, 0, 16, &set, r);
        transform(base, 0, 16, &set);
        flip_bit(tile_words(base, 5), 1031);
        assert!(verify(base, 0, 16, &set, r) >= 1, "r = {r}: a single bit");
        write(base, 0, 16, &set, r);
        transform(base, 0, 16, &set);
        let (bit1, bit2) = opposite_flips(base, 2, 11, 6, 3, 17).expect("a pair to flip");
        flip_bit(tile_words(base, 2), bit1);
        flip_bit(tile_words(base, 11), bit2);
        assert!(verify(base, 0, 16, &set, r) >= 1, "r = {r}: two flips that could cancel");
        take_found();
    }
}

/// TODO 102: a group whose checksum disagreed while every word reads right counts one error, not reproduced, as TMR's rescan reports it;
/// the tiles' recheck says the checksum is right.
#[test]
fn a_check_the_rescan_cant_reproduce_counts_one_error_not_reproduced() {
    if !amx() {
        return;
    }
    let set = set::<16>(2, 0x7E57);
    let mut buf = tiles(32);
    let base = buf.as_mut_ptr().cast::<u64>();
    write(base, 0, 32, &set, 4);
    transform(base, 0, 32, &set);
    crate::tests::reset_rescan_counts();
    // SAFETY: AMX-INT8; group 1 of the buffer
    let errors = unsafe { rescan_group::<16>(base, 1, &set, 4) };
    let counts = crate::tests::rescan_counts();
    assert_eq!((errors, counts.alarms, counts.not_reproduced), (1, 1, 1));
    assert_eq!(take_found(), (vec![], Some(false)));
}

// ─── The test function ───────────────────────────────────────────────────────────────────────────

/// TODO 102: the test function runs clean on three joined blocks at every set size and on both of R's loops, chunks cutting groups (68
/// KiB), two cycles: no errors or alarms, every byte counted four times.
#[test]
fn the_test_runs_clean_at_every_set_size_and_load() {
    if !amx() {
        return;
    }
    let region = Region::new(&[256 * KIB, 256 * KIB, 128 * KIB]);
    for x in [16, 32, 64] {
        for r in [0, AHEAD_FROM_R as u32] {
            let context = crate::config::TestParameterContext { tile_patterns: Some(x), tile_load: Some(r), ..Default::default() };
            let config =
                TestMemoryConfig::new(crate::tests::ExtentMode::FullAllocation, crate::tests::ChunkMode::Absolute { size_bytes: 68 * KIB })
                    .with_timing(TestTiming::cycles_only(2))
                    .with_parameter_context(context);
            // SAFETY: the region is live, 64-byte aligned and the test's alone; AMX-INT8
            let stats = unsafe { tile_transform_multi(&region.blocks, 0, crate::ErrorMode::Log, &config.timing, &config, None) };
            let what = format!("X = {x}, r = {r}");
            assert_eq!((stats.error_count, stats.alarms, stats.cycles_completed), (0, 0, 2), "{what}");
            assert!(stats.bytes_processed >= 2 * 4 * 640 * KIB, "{what}: {} bytes", stats.bytes_processed);
        }
    }
}

// ─── The exhaustive sweeps (measurements: run them by name, in release) ──────────────────────────────────────────────────

/// Every case of a sweep, one a group, `BATCH` groups at a time: `inject(group's words, case, before_transform)` corrupts each group,
/// after W (`true`) or after RW (`false`); `check(case, group's first tile, its bad tiles)` gets each.
fn sweep(set: &PatternSet<16>, cases: usize, before: bool, mut check: impl FnMut(usize, usize, Vec<BadTile>)) {
    const BATCH: usize = 512;
    let pass = 3;
    for start in (0..cases).step_by(BATCH) {
        let n = BATCH.min(cases - start);
        let mut buf = tiles(16 * n);
        let base = buf.as_mut_ptr().cast::<u64>();
        write(base, 0, 16 * n, set, pass);
        let inject = |base: *mut u64, g: usize| {
            let (t, bit) = ((start + g) / TILE_BITS % 16, (start + g) % TILE_BITS);
            flip_bit(tile_words(base, 16 * g + t), bit);
        };
        if before {
            (0..n).for_each(|g| inject(base, g));
        }
        transform(base, 0, 16 * n, set);
        if !before {
            (0..n).for_each(|g| inject(base, g));
        }
        verify(base, 0, 16 * n, set, pass);
        let found = take_found().0;
        for g in 0..n {
            check(start + g, 16 * g, found.iter().filter(|b| b.tile / 16 == g).copied().collect());
        }
    }
}

/// TODO 102, a measurement: every one of the 131,072 single-bit errors a group's stored result can have, at r = 3: caught and located.
#[test]
#[ignore = "a measurement: run it by name, in release"]
fn every_stored_bit_is_caught_and_located() {
    if !amx() {
        return;
    }
    let set = set::<16>(3, 0x5106);
    sweep(&set, 16 * TILE_BITS, false, |c, g0, found| {
        let (t, bit) = (c / TILE_BITS, c % TILE_BITS);
        let want = BadTile {
            tile: g0 + t,
            pattern: set.pattern_at(g0 + t, 3),
            bad_words: 1,
            signature: Signature::Stored { byte: bit / 8, bit: (bit % 8) as u32 },
        };
        assert_eq!(found, [want], "tile {t}, bit {bit}");
    });
}

/// TODO 102, a measurement: every single-bit error in the built-in set's patterns before the transform (16 patterns x 8,192 bits, the
/// design's exhaustive check): caught, and worked back to the pattern's byte and bit. Not guaranteed by construction: a property of the
/// built-in set's matrices.
#[test]
#[ignore = "a measurement: run it by name, in release"]
fn every_bit_before_the_transform_is_caught_and_located() {
    if !amx() {
        return;
    }
    let set = set::<16>(0, 0xBEF0);
    sweep(&set, 16 * TILE_BITS, true, |c, g0, found| {
        let (t, bit) = (c / TILE_BITS, c % TILE_BITS);
        let pattern = set.pattern_at(g0 + t, 3);
        let rose = !get_bit(&set.m.patterns[pattern].0, bit);
        let signature = Signature::BeforeTransform { byte: bit / 8, bit: (bit % 8) as u32, rose };
        assert_eq!(found, [BadTile { tile: g0 + t, pattern, bad_words: 8, signature }], "tile {t}, bit {bit}");
    });
}
