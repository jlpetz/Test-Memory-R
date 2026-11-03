//! The page exchange's tests (TODO 28): its kernels against the pattern functions, each operation alone and across threads and nodes,
//! the sharing groups, the seal, a bad word found by the next claimer and counted once, a halt, the share of inverting threads, and the
//! arrangement's coverage.

use std::sync::Arc;

use super::*;
use crate::config::TestParameterContext;
use crate::pattern::Family;
use crate::runner::TestFunctionMultiBlock;
use crate::runner::test_support::{KIB, Region};
use crate::seal::SealKernel;
use crate::simd::Width;
use crate::tests::{ChunkMode, ExtentMode};

/// Each operation and its test function
const OPS: [(Op, TestFunctionMultiBlock); 3] =
    [(Op::Copy, page_exchange_multi), (Op::Invert, page_exchange_invert_multi), (Op::Rewrite, page_exchange_rewrite_multi)];

/// Tiles of 64 KiB over a whole span
fn config(cycles: u32) -> TestMemoryConfig {
    TestMemoryConfig::new(ExtentMode::FullAllocation, ChunkMode::Absolute { size_bytes: 64 * KIB })
        .with_timing(TestTiming::cycles_only(cycles))
}

fn with_context(mut config: TestMemoryConfig, ctx: TestParameterContext) -> TestMemoryConfig {
    config.parameter_context = Some(ctx);
    config
}

fn sharing(config: TestMemoryConfig, share: Share) -> TestMemoryConfig {
    with_context(config, TestParameterContext { exchange_share: Some(share), ..Default::default() })
}

/// A sealed step's seal, for `config.seal`
fn sealed() -> crate::seal::SealStep {
    crate::seal::SealStep { kernel: SealKernel::detect(), on: true, expects: true, wrap: true }
}

/// The widths this CPU runs the kernels at
fn widths() -> Vec<Width> {
    let mut out = vec![Width::W256];
    if is_x86_feature_detected!("avx512f") && is_x86_feature_detected!("avx512dq") {
        out.push(Width::W512);
    }
    out
}

/// `test` on one thread over `region`, its span sealed first when `config` wraps the seal.
fn alone(test: TestFunctionMultiBlock, region: &mut Region, config: &TestMemoryConfig) -> TestStats {
    let _beat = crate::pattern::TestBeat::set(Some(BusBeat::Bits32));
    if config.seal.wrap {
        let words = region.words().len();
        // SAFETY: the region's words
        unsafe { config.seal.kernel.fill(region.words().as_mut_ptr(), 0, words) };
    }
    // SAFETY: the region's blocks, live for the call
    let stats = unsafe { test(&region.blocks, 0, crate::ErrorMode::Log, &config.timing, config, None) };
    assert!(region.guards_intact(), "a claim wrote outside its tile");
    stats
}

/// `threads` threads on `nodes`, each with a span of `tiles` tiles, run one step of `test` together (each span sealed first in a sealed
/// step); their stats, the step for its tallies, and the spans.
fn run_step(
    test: TestFunctionMultiBlock, nodes: Vec<u32>, tiles: usize, config: TestMemoryConfig,
) -> (Vec<TestStats>, Arc<Step>, Vec<Region>) {
    let threads = nodes.len();
    let step = Arc::new(Step::new(nodes));
    let config = TestMemoryConfig { exchange: Some(Arc::clone(&step)), ..config };
    let mut regions: Vec<Region> = (0..threads).map(|_| Region::new(&[tiles * 64 * KIB])).collect();
    if config.seal.wrap {
        for region in &mut regions {
            let words = region.words().len();
            // SAFETY: the region's words
            unsafe { config.seal.kernel.fill(region.words().as_mut_ptr(), 0, words) };
        }
    }
    let stats = std::thread::scope(|s| {
        let handles: Vec<_> = regions
            .iter()
            .enumerate()
            .map(|(t, region)| {
                let (blocks, config) = (&region.blocks, &config);
                s.spawn(move || {
                    let _beat = crate::pattern::TestBeat::set(Some(BusBeat::Bits32));
                    // SAFETY: each thread's own region, live for the scope; the exchange shares them by its protocol
                    unsafe { test(blocks, t, crate::ErrorMode::Log, &config.timing, config, None) }
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().expect("a worker of the step")).collect()
    });
    assert!(regions.iter().all(Region::guards_intact), "a claim wrote outside its tile");
    (stats, step, regions)
}

/// Each span's tiles in a step whose checks need every thread to claim in another's span. Which tiles a thread gets rests on how the
/// threads' claims interleave: three claiming in turn (the fourth not yet woken from the step's barrier, on a busy box) each take every
/// third claim. At 4 tiles a span the tests' key put pass 0's claims 5 tiles apart, so a thread's turns were 15 apart, one tile back:
/// claims 1, 4, 7 and 10 took tiles 3, 2, 1 and 0, all thread 0's, and when they were thread 0's turns it claimed in no other span (pass
/// 1's, 11 apart, went one tile on). At 16 a thread's cycle is 16 claims over 64 tiles, which seldom keep to its span: its turns would
/// have to walk neighbouring tiles from the span's edge.
const CROSSING_TILES: usize = 16;

/// The steps `run_step_until` runs at most
const ATTEMPTS: u64 = 8;

/// `run_step` until `check` returns that the step met its precondition: a test's check that rests on how the threads' claims interleaved
/// as well as on the exchange (that every thread claimed in another's span, say). `check` asserts what any step must hold, so a step
/// that misses the precondition still has its images and its seal checked. Each step after the first takes the next seed, for another
/// arrangement of the tiles. After `ATTEMPTS` misses it fails: an exchange that can't meet the precondition misses in every step.
fn run_step_until(
    what: &str, test: TestFunctionMultiBlock, nodes: &[u32], tiles: usize, config: &TestMemoryConfig,
    mut check: impl FnMut(&[TestStats], &Step, &mut [Region]) -> bool,
) {
    let mut last = Vec::new();
    for attempt in 0..ATTEMPTS {
        let epoch = crate::pattern::StepEpoch { seed: config.epoch.seed.wrapping_add(attempt), ..config.epoch };
        let (stats, step, mut regions) = run_step(test, nodes.to_vec(), tiles, TestMemoryConfig { epoch, ..config.clone() });
        if check(&stats, &step, &mut regions) {
            return;
        }
        last = (0..step.threads()).map(|t| step.tally(t)).collect();
    }
    panic!("{what}: the precondition missed in {ATTEMPTS} steps; the last one's tallies {last:?}");
}

/// Every word of the kernels' images is the pattern function's, at the tile's line ids from 0; the check finds a changed word; a copy
/// stores what it read, bad words too; an invert leaves the complement. At each width, family, beat and polarity, both stores.
#[test]
fn the_kernels_write_check_copy_and_invert_the_pattern() {
    const WORDS: usize = 512;
    let mut region = Region::new(&[4 * KIB, 4 * KIB]);
    let base = region.words().as_mut_ptr();
    let (a, b) = (base, base.wrapping_add(WORDS));
    let read = |p: *mut u64| -> Vec<u64> {
        // SAFETY: one of the region's two halves
        unsafe { std::slice::from_raw_parts(p, WORDS).to_vec() }
    };
    let mut cases = 0;
    for width in widths() {
        let kernels = Kernels::at(width);
        for family in Family::ALL {
            for beat in [BusBeat::Bits12, BusBeat::Bits16, BusBeat::Bits24, BusBeat::Bits32, BusBeat::Bits64] {
                for mask in [0, !0] {
                    for stores in [Stores::Cached, Stores::Nt] {
                        let phase = Phase { family, key: 0x0123_4567_89AB_CDEF, mask, beat };
                        let what = format!("{} at {} bits, {} stores", phase.label(), kernels.bits(), stores.name());
                        let want: Vec<u64> = (0..WORDS / LINE_WORDS).flat_map(|l| phase.line(l as u64)).collect();
                        // SAFETY (each): the region's halves, apart, 64-byte aligned, whole lines
                        unsafe {
                            kernels.fill(a, WORDS, phase, stores);
                            assert_eq!(read(a), want, "{what}: the fill");
                            assert_eq!(kernels.verify(a, WORDS, phase), 0, "{what}: the check");
                            assert_eq!(kernels.copy(a, b, WORDS, phase, stores), 0, "{what}: the copy's check");
                            assert_eq!(read(b), want, "{what}: the copy");
                            assert_eq!(kernels.invert(b, WORDS, phase, stores), 0, "{what}: the invert's check");
                            assert_eq!(read(b), want.iter().map(|w| !w).collect::<Vec<_>>(), "{what}: the invert");
                            let complement = Phase { mask: !mask, ..phase };
                            assert_eq!(kernels.verify(b, WORDS, complement), 0, "{what}: the complement's check");
                            *a.add(301) ^= 1 << 31;
                            assert_eq!(kernels.verify(a, WORDS, phase), 1 << 31, "{what}: a changed word");
                            assert_eq!(kernels.copy(a, b, WORDS, phase, stores), 1 << 31, "{what}: a changed word, copied");
                            assert_eq!(*b.add(301), want[301] ^ (1 << 31), "{what}: a copy stores what it read");
                            assert_eq!(kernels.invert(a, WORDS, phase, stores), 1 << 31, "{what}: a changed word, inverted");
                            assert_eq!(*a.add(301), !(want[301] ^ (1 << 31)), "{what}: an invert stores the complement of what it read");
                        }
                        cases += 1;
                    }
                }
            }
        }
    }
    assert!(cases >= 7 * 5 * 2 * 2);
    assert!(region.guards_intact());
}

#[test]
fn a_rewrite_alone_checks_and_rewrites_its_tiles() {
    let mut region = Region::new(&[256 * KIB]);
    let stats = alone(page_exchange_rewrite_multi, &mut region, &config(3));
    assert_eq!((stats.error_count, stats.seal_errors), (0, 0));
    assert_eq!(stats.cycles_completed, 3);
    // 4 tiles a cycle: the first written, the next two checked and written, then each checked once more at the end
    assert_eq!(stats.bytes_processed, (4 + 2 * 4 * 2 + 4) * 64 * KIB);
}

/// Each operation alone, with each store, over every family, and over a span whose last tile is short (a copy rewrites it in place)
#[test]
fn each_operation_alone_holds_its_images() {
    for (op, test) in OPS {
        for stores in [Stores::Cached, Stores::Nt] {
            for family in Family::ALL {
                for span in [256 * KIB, 288 * KIB] {
                    let mut region = Region::new(&[span]);
                    let ctx = TestParameterContext { exchange_stores: Some(stores), ..Default::default() };
                    let config = with_context(config(4), ctx).with_program(Program::single(family));
                    let stats = alone(test, &mut region, &config);
                    let what = format!("{op:?}, {} stores, {}, {} KiB", stores.name(), family.name(), span / KIB);
                    assert_eq!((stats.error_count, stats.seal_errors), (0, 0), "{what}");
                    assert_eq!(stats.cycles_completed, 4, "{what}");
                }
            }
        }
    }
}

/// In a sealed step the first claim of a tile checks the seal (and a copy its first held tile), and the step leaves every tile resealed,
/// whether it ran one cycle (each tile resealed after its last check) or more (the whole extent, as the test finishes).
#[test]
fn a_sealed_step_checks_the_seal_first_and_leaves_it_resealed() {
    for (op, test) in OPS {
        for cycles in [1, 3] {
            let mut region = Region::new(&[256 * KIB]);
            let config = TestMemoryConfig { seal: sealed(), ..config(cycles) };
            let stats = alone(test, &mut region, &config);
            assert_eq!((stats.error_count, stats.seal_errors), (0, 0), "{op:?}, {cycles} cycles");
            let words = region.words().len();
            // SAFETY: the region's words
            let left = unsafe { config.seal.kernel.check(region.words().as_mut_ptr(), 0, words, "test") };
            assert_eq!(left, 0, "{op:?}, {cycles} cycles: left words unsealed");
        }
    }
}

/// A word that changed since the seal is a seal error, found by the tile's first claim (or by taking it to copy into)
#[test]
fn a_bad_word_under_the_seal_is_a_seal_error() {
    for (op, test) in OPS {
        for tile in [0, 3] {
            let mut region = Region::new(&[256 * KIB]);
            let config = TestMemoryConfig { seal: sealed(), ..config(1) };
            let words = region.words().len();
            // SAFETY: the region's words
            unsafe { config.seal.kernel.fill(region.words().as_mut_ptr(), 0, words) };
            region.words()[tile * 8 * KIB + 5] ^= 1 << 17;
            // SAFETY: the region's blocks, live for the call
            let stats = unsafe { test(&region.blocks, 0, crate::ErrorMode::Log, &config.timing, &config, None) };
            assert_eq!((stats.error_count, stats.seal_errors), (0, 1), "{op:?}, tile {tile}");
        }
    }
}

/// Four threads on two nodes: every thread claims other threads' tiles and other nodes', and every image checks out; copies cross nodes.
/// Every step's images check out, and a step that isn't a copy copies nothing. Whose tiles a thread claimed, and how many of its claims
/// found their tile busy, rest on how the threads' claims interleaved as well as on the exchange, so a step in which a thread kept to its
/// span or its node, too few claims worked, or a copy's copies kept to one node, is run again (`run_step_until`): an exchange that doesn't
/// cross misses in every step.
#[test]
fn tiles_cross_threads_and_nodes_and_every_image_checks_out() {
    for (op, test) in OPS {
        run_step_until(&format!("{op:?}"), test, &[0, 0, 1, 1], CROSSING_TILES, &config(6), |stats, step, _| {
            for (t, s) in stats.iter().enumerate() {
                assert_eq!((s.error_count, s.seal_errors), (0, 0), "{op:?}, thread {t}");
            }
            let tallies: Vec<Tally> = (0..4).map(|t| step.tally(t).expect("each thread's tally")).collect();
            let (copies, across) = tallies.iter().fold((0, 0), |a, x| (a.0 + x.copies, a.1 + x.copies_across));
            if op != Op::Copy {
                assert_eq!(copies, 0, "{op:?}");
            }
            // 64 tiles (a copy's 4 held), 16 claims a cycle a thread: about every tile 6 times
            let claims: u64 = tallies.iter().map(|x| x.claims).sum();
            tallies.iter().all(|x| x.others > 0 && x.other_nodes > 0)
                && claims >= (4 * CROSSING_TILES as u64 - 4) * 5
                && (op != Op::Copy || copies > 0 && across > 0)
        });
    }
}

/// A sealed step of four threads: first claims check the seal in each other's spans, and every span ends resealed, one cycle or more.
/// That every thread claimed in another's span is those checks' precondition, not what they check: it rests on how the threads' claims
/// interleaved, so a step in which one didn't is run again (`run_step_until`), its seal checked all the same.
#[test]
fn a_sealed_step_across_threads_leaves_every_span_resealed() {
    for (op, test) in OPS {
        for cycles in [1, 4] {
            let what = format!("{op:?}, {cycles} cycles");
            let config = TestMemoryConfig { seal: sealed(), ..config(cycles) };
            run_step_until(&what, test, &[0, 0, 1, 1], CROSSING_TILES, &config, |stats, step, regions| {
                for (t, s) in stats.iter().enumerate() {
                    assert_eq!((s.error_count, s.seal_errors), (0, 0), "{what}, thread {t}");
                }
                let kernel = sealed().kernel;
                for (t, region) in regions.iter_mut().enumerate() {
                    let words = region.words().len();
                    // SAFETY: the region's words
                    let left = unsafe { kernel.check(region.words().as_mut_ptr(), 0, words, "test") };
                    assert_eq!(left, 0, "{what}: span {t} not resealed");
                }
                (0..stats.len()).all(|t| step.tally(t).is_some_and(|x| x.others > 0))
            });
        }
    }
}

/// `node` keeps to the claimer's node, `cross` to the others
#[test]
fn the_sharing_groups_keep_to_their_nodes() {
    for (op, test) in [OPS[0], OPS[2]] {
        let (stats, step, _) = run_step(test, vec![0, 0, 1, 1], 4, sharing(config(4), Share::Node));
        for (t, s) in stats.iter().enumerate() {
            assert_eq!((s.error_count, s.seal_errors), (0, 0));
            let tally = step.tally(t).expect("each thread's tally");
            assert!(tally.claims > 0 && tally.other_nodes == 0 && tally.copies_across == 0, "{op:?} node, thread {t}: {tally:?}");
        }
        let (stats, step, _) = run_step(test, vec![0, 0, 1, 1], 4, sharing(config(4), Share::Cross));
        for (t, s) in stats.iter().enumerate() {
            assert_eq!((s.error_count, s.seal_errors), (0, 0));
            let tally = step.tally(t).expect("each thread's tally");
            assert!(tally.claims > 0 && tally.other_nodes == tally.claims, "{op:?} cross, thread {t}: {tally:?}");
        }
    }
}

/// `cross` on one node is `any`: each thread claims, none on another node, and each in the other's span. That last rests on how the two
/// threads' claims interleaved, so a step in which one kept to its own span is run again (`run_step_until`).
#[test]
fn cross_on_one_node_is_any() {
    let config = sharing(config(3), Share::Cross);
    run_step_until("cross on one node", page_exchange_rewrite_multi, &[0, 0], CROSSING_TILES, &config, |stats, step, _| {
        for (t, s) in stats.iter().enumerate() {
            assert_eq!(s.error_count, 0);
            let tally = step.tally(t).expect("each thread's tally");
            assert!(tally.claims > 0 && tally.other_nodes == 0, "thread {t}: {tally:?}");
        }
        (0..2).all(|t| step.tally(t).is_some_and(|x| x.others > 0))
    });
}

/// `exchange_invert` makes that share of a copy's threads invert, spread over them: at 50%, every other thread
#[test]
fn a_share_of_the_copys_threads_invert() {
    let ctx = TestParameterContext { exchange_invert: Some(50), ..Default::default() };
    let (stats, step, _) = run_step(page_exchange_multi, vec![0, 0, 1, 1], 4, with_context(config(6), ctx));
    let copying: Vec<bool> = (0..4).map(|t| step.tally(t).expect("each thread's tally").copies > 0).collect();
    assert_eq!(copying, [true, false, true, false]);
    for (t, s) in stats.iter().enumerate() {
        assert_eq!((s.error_count, s.seal_errors), (0, 0), "thread {t}");
        assert!(step.tally(t).is_some_and(|x| x.claims > 0), "thread {t}");
    }
}

/// A step of one thread run claim by claim: its runner, its worker and its span's first word.
struct Manual {
    region: Region,
    step: Step,
}

impl Manual {
    fn new() -> Self {
        Manual { region: Region::new(&[256 * KIB]), step: Step::new(vec![0]) }
    }

    /// The tiles published, and a worker for `op` on them (a copy holding tile 0), with its runner
    fn start<'a>(
        &'a mut self, op: Op, error_mode: crate::ErrorMode, config: &'a TestMemoryConfig,
    ) -> (Worker<'a>, TestRunner<'a>, *mut u64) {
        let base = self.region.words().as_mut_ptr();
        let (runner, extent) =
            TestRunner::new(&self.region.blocks, 0, error_mode, &config.timing, config, None, op.test_name(), TestAction::ReadWrite);
        let tile_bytes = 64 * KIB;
        let tiles = (0..extent.test_size / tile_bytes).map(|_| Tile::default()).collect();
        let home = Home { base: extent.ptr as usize, bytes: extent.test_size, tile_bytes, tiles };
        assert!(self.step.homes[0].set(home).is_ok());
        let held = (op == Op::Copy).then(|| {
            self.step.home(0).tiles[0].state.store(BUSY, Ordering::Relaxed);
            Held { at: Spot { home: 0, k: 0 }, image: Some(0) }
        });
        let worker = Worker {
            step: &self.step,
            me: 0,
            op,
            name: op.test_name(),
            stores: op.default_stores(),
            pool: Pool::new(&self.step, 0, Share::Any),
            key: 7,
            program: Program::default_exchange(),
            beat: Some(BusBeat::Bits32),
            kernels: Kernels::at(crate::simd::test_width()),
            sealed: false,
            pass: (0, 0, 0),
            held,
            tally: Tally::default(),
        };
        (worker, runner, base)
    }
}

/// `n` claims; the bad words they found
fn claims(worker: &mut Worker, runner: &mut TestRunner, n: usize) -> u64 {
    (0..n)
        .map(|_| match unsafe { worker.claim(runner, 0) } {
            Claim::Worked(found) => found,
            Claim::Busy => 0,
            Claim::Halted(_) => panic!("no halt under errors=log"),
        })
        .sum()
}

/// A word changed after a claim wrote it is found by the next claim of its tile, and counted once: the claim that found it leaves the
/// image whole (a rewrite's new image, a copy's held tile and an invert's tile written whole)
#[test]
fn a_bad_word_is_found_by_the_next_claimer_and_counted_once() {
    for (op, _) in OPS {
        let mut manual = Manual::new();
        let config = config(1);
        let (mut worker, mut runner, base) = manual.start(op, crate::ErrorMode::Log, &config);
        let tiles = worker.pool.tiles;
        // SAFETY (each): the region's tiles, this thread's alone
        assert_eq!(claims(&mut worker, &mut runner, tiles), 0, "{op:?}: the first pass");
        assert_eq!(claims(&mut worker, &mut runner, tiles), 0, "{op:?}: the second pass");
        // A tile that holds an image now and isn't held
        let k = (1..tiles).find(|&k| manual_state(&worker, k) & BUSY == 0 && manual_state(&worker, k) != 0).expect("a free tile");
        // SAFETY: a word of the region, which no claim holds now
        unsafe { *base.add(k * 8 * KIB + 100) ^= 1 << 40 };
        assert_eq!(claims(&mut worker, &mut runner, tiles), 1, "{op:?}: the next pass's claim of the tile finds the bad word");
        assert_eq!(claims(&mut worker, &mut runner, 2 * tiles), 0, "{op:?}: the claim that found it left the image whole");
    }
}

fn manual_state(worker: &Worker, k: usize) -> u64 {
    worker.step.home(0).tiles[k].state.load(Ordering::Relaxed)
}

/// Under errors=halt the claim that finds a bad word leaves it where it was found, and halts the step: a rewrite's tile unwritten, a
/// copy's source as it was, an invert's tile inverted with the bad word in it
#[test]
fn a_halt_leaves_the_bad_word_where_it_was_found() {
    for (op, _) in OPS {
        let mut manual = Manual::new();
        let config = config(1);
        let (mut worker, mut runner, base) = manual.start(op, crate::ErrorMode::Halt, &config);
        let tiles = worker.pool.tiles;
        for _ in 0..2 * tiles {
            // SAFETY: the region's tiles, this thread's alone
            assert!(matches!(unsafe { worker.claim(&mut runner, 0) }, Claim::Worked(0) | Claim::Busy));
        }
        let k = (1..tiles).find(|&k| manual_state(&worker, k) & BUSY == 0 && manual_state(&worker, k) != 0).expect("a free tile");
        let state = manual_state(&worker, k);
        let bad = k * 8 * KIB + 100;
        // SAFETY: a word of the region, which no claim holds now
        let corrupted = unsafe {
            *base.add(bad) ^= 1 << 40;
            *base.add(bad)
        };
        let mut halted = None;
        for _ in 0..tiles {
            // SAFETY: as above
            if let Claim::Halted(found) = unsafe { worker.claim(&mut runner, 0) } {
                halted = Some(found);
                break;
            }
        }
        assert_eq!(halted, Some(1), "{op:?}: the claim of the bad tile halts");
        // SAFETY: as above
        let now = unsafe { *base.add(bad) };
        let (want_word, want_state) = if op == Op::Invert { (!corrupted, state ^ INVERT) } else { (corrupted, state) };
        assert_eq!(now, want_word, "{op:?}: the bad word stays as found");
        assert_eq!(manual_state(&worker, k), want_state, "{op:?}: the tile is released as found");
        assert_ne!(manual.step.halted.0.load(Ordering::Relaxed), 0, "{op:?}: the step halts");
    }
}

/// Each pass's arrangement visits every tile once
#[test]
fn a_pass_covers_every_tile_once() {
    for n in [1usize, 2, 3, 4, 6, 12, 16, 97, 1000, 1024] {
        for pass in 0..5 {
            let (s, o) = arrangement(n, 0x1234_5678, pass);
            assert_eq!(gcd(s, n as u64), 1);
            let mut seen = vec![false; n];
            for t in 0..n as u64 {
                let i = ((t as u128 * s as u128 + o as u128) % n as u128) as usize;
                assert!(!seen[i], "n {n}, pass {pass}: tile {i} twice");
                seen[i] = true;
            }
        }
    }
}

#[test]
fn a_state_word_names_its_image() {
    for (number, family, writer_thread, invert) in [(0, 0, 0, false), (1, 6, 5, true), (IMAGE_BITS, 7, WRITER_BITS as usize - 1, true)] {
        let state = image(number, family, writer_thread, invert);
        assert_ne!(state, 0, "an image is never the no-image state");
        assert_eq!(state & BUSY, 0);
        assert_eq!(writer(state), writer_thread);
        assert_eq!(family_at(state), family);
        assert_eq!(state & INVERT != 0, invert);
        assert_eq!(state & IMAGE_BITS, number & IMAGE_BITS);
    }
}

/// A new image takes the program's families in turn, and its complement every other lap
#[test]
fn new_images_take_every_family_in_both_polarities() {
    let mut manual = Manual::new();
    let config = config(1);
    let (worker, _runner, _) = manual.start(Op::Rewrite, crate::ErrorMode::Log, &config);
    let len = worker.program.families().len() as u64;
    let mut seen = std::collections::HashSet::new();
    for t in 0..2 * len {
        let phase = worker.phase(worker.new_image(t));
        seen.insert((phase.family, phase.mask));
    }
    assert_eq!(seen.len(), 2 * Family::ALL.len());
}
