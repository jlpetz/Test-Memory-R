//! Mem-PageExchange's tests (TODO 28): one thread alone, threads crossing tiles and nodes, the sharing groups, the seal, a bad word found
//! by the next claimer, and the arrangement's coverage.

use std::sync::Arc;

use super::*;
use crate::config::TestParameterContext;
use crate::runner::test_support::{KIB, Region};
use crate::tests::{ChunkMode, ExtentMode};

/// Tiles of 64 KiB over a whole span
fn config(cycles: u32) -> TestMemoryConfig {
    TestMemoryConfig::new(ExtentMode::FullAllocation, ChunkMode::Absolute { size_bytes: 64 * KIB })
        .with_timing(TestTiming::cycles_only(cycles))
}

fn sharing(mut config: TestMemoryConfig, share: Share) -> TestMemoryConfig {
    config.parameter_context = Some(TestParameterContext { exchange_share: Some(share), ..Default::default() });
    config
}

/// A sealed step's seal, for `config.seal`
fn sealed() -> crate::seal::SealStep {
    crate::seal::SealStep { kernel: SealKernel::detect(), on: true, expects: true, wrap: true }
}

/// `threads` threads on `nodes`, each with a span of `tiles` tiles, run one step together; their stats, and the step for its tallies.
fn run_step(nodes: Vec<u32>, tiles: usize, config: TestMemoryConfig) -> (Vec<TestStats>, Arc<Step>) {
    let threads = nodes.len();
    let step = Arc::new(Step::new(nodes));
    let config = TestMemoryConfig { exchange: Some(Arc::clone(&step)), ..config };
    let regions: Vec<Region> = (0..threads).map(|_| Region::new(&[tiles * 64 * KIB])).collect();
    let stats = std::thread::scope(|s| {
        let handles: Vec<_> = regions
            .iter()
            .enumerate()
            .map(|(t, region)| {
                let (blocks, config) = (&region.blocks, &config);
                // SAFETY: each thread's own region, live for the scope; the exchange shares them by its protocol
                s.spawn(move || unsafe { page_exchange_multi(blocks, t, crate::ErrorMode::Log, &config.timing, config, None) })
            })
            .collect();
        handles.into_iter().map(|h| h.join().expect("a worker of the step")).collect()
    });
    assert!(regions.iter().all(Region::guards_intact), "a claim wrote outside its tile");
    (stats, step)
}

#[test]
fn a_thread_alone_exchanges_its_tiles_with_itself() {
    let region = Region::new(&[256 * KIB]);
    let config = config(3);
    // SAFETY: the region's blocks, live for the call
    let stats = unsafe { page_exchange_multi(&region.blocks, 0, crate::ErrorMode::Log, &config.timing, &config, None) };
    assert_eq!((stats.error_count, stats.seal_errors), (0, 0));
    assert_eq!(stats.cycles_completed, 3);
    // 4 tiles a cycle: the first written, the next two checked and written, then each checked once more at the end
    assert_eq!(stats.bytes_processed, (4 + 2 * 4 * 2 + 4) * 64 * KIB);
    assert!(region.guards_intact());
}

/// In a sealed step the first claim of a tile checks the seal, and the step leaves every tile resealed, whether it ran one cycle (each tile
/// resealed after its last check) or more (the whole extent, as the test finishes).
#[test]
fn a_sealed_step_checks_the_seal_first_and_leaves_it_resealed() {
    for cycles in [1, 3] {
        let mut region = Region::new(&[256 * KIB]);
        let config = TestMemoryConfig { seal: sealed(), ..config(cycles) };
        let words = region.words().len();
        let base = region.words().as_mut_ptr();
        // SAFETY: the region's words
        unsafe { config.seal.kernel.fill(base, 0, words) };
        // SAFETY: as above
        let stats = unsafe { page_exchange_multi(&region.blocks, 0, crate::ErrorMode::Log, &config.timing, &config, None) };
        assert_eq!((stats.error_count, stats.seal_errors), (0, 0), "{cycles} cycles");
        // SAFETY: as above
        assert_eq!(unsafe { config.seal.kernel.check(base, 0, words, "test") }, 0, "{cycles} cycles: left words unsealed");
    }
}

/// A word that changed since the seal is a seal error, found by the tile's first claim
#[test]
fn a_bad_word_under_the_seal_is_a_seal_error() {
    let mut region = Region::new(&[256 * KIB]);
    let config = TestMemoryConfig { seal: sealed(), ..config(1) };
    let words = region.words().len();
    let base = region.words().as_mut_ptr();
    // SAFETY: the region's words
    unsafe { config.seal.kernel.fill(base, 0, words) };
    region.words()[3 * 8 * KIB + 5] ^= 1 << 17;
    // SAFETY: as above
    let stats = unsafe { page_exchange_multi(&region.blocks, 0, crate::ErrorMode::Log, &config.timing, &config, None) };
    assert_eq!((stats.error_count, stats.seal_errors), (0, 1));
}

/// Four threads on two nodes: every thread claims other threads' tiles and other nodes', and every write checks out
#[test]
fn tiles_cross_threads_and_nodes_and_every_write_checks_out() {
    let (stats, step) = run_step(vec![0, 0, 1, 1], 4, config(6));
    for (t, s) in stats.iter().enumerate() {
        assert_eq!((s.error_count, s.seal_errors), (0, 0), "thread {t}");
        let tally = step.tally(t).expect("each thread's tally");
        assert!(tally.claims > 0 && tally.others > 0 && tally.other_nodes > 0, "thread {t}: {tally:?}");
    }
    // 16 tiles, 4 claims a cycle a thread: about every tile 6 times
    let claims: u64 = (0..4).map(|t| step.tally(t).map_or(0, |x| x.claims)).sum();
    assert!(claims >= 16 * 5, "{claims} claims");
}

/// `node` keeps to the claimer's node, `cross` to the others
#[test]
fn the_sharing_groups_keep_to_their_nodes() {
    let (stats, step) = run_step(vec![0, 0, 1, 1], 4, sharing(config(4), Share::Node));
    for (t, s) in stats.iter().enumerate() {
        assert_eq!((s.error_count, s.seal_errors), (0, 0));
        let tally = step.tally(t).expect("each thread's tally");
        assert!(tally.claims > 0 && tally.other_nodes == 0, "node, thread {t}: {tally:?}");
    }
    let (stats, step) = run_step(vec![0, 0, 1, 1], 4, sharing(config(4), Share::Cross));
    for (t, s) in stats.iter().enumerate() {
        assert_eq!((s.error_count, s.seal_errors), (0, 0));
        let tally = step.tally(t).expect("each thread's tally");
        assert!(tally.claims > 0 && tally.other_nodes == tally.claims, "cross, thread {t}: {tally:?}");
    }
}

/// `cross` on one node is `any`
#[test]
fn cross_on_one_node_is_any() {
    let (stats, step) = run_step(vec![0, 0], 4, sharing(config(3), Share::Cross));
    for (t, s) in stats.iter().enumerate() {
        assert_eq!(s.error_count, 0);
        let tally = step.tally(t).expect("each thread's tally");
        assert!(tally.claims > 0 && tally.others > 0 && tally.other_nodes == 0, "thread {t}: {tally:?}");
    }
}

/// A word changed after a claim wrote it is found by the next claim of its tile, and counted once
#[test]
fn a_bad_word_is_found_by_the_next_claimer() {
    let mut region = Region::new(&[256 * KIB]);
    let base = region.words().as_mut_ptr();
    let config = config(1);
    let step = Step::new(vec![0]);
    let (mut runner, extent) =
        TestRunner::new(&region.blocks, 0, crate::ErrorMode::Log, &config.timing, &config, None, TEST_NAME, TestAction::ReadWrite);
    let tile_bytes = 64 * KIB;
    let tiles = extent.test_size / tile_bytes;
    let states = (0..tiles).map(|_| Slot::default()).collect();
    assert!(step.homes[0].set(Home { base: extent.ptr as usize, bytes: extent.test_size, tile_bytes, states }).is_ok());
    step.barrier.wait();
    let mut worker = Worker {
        step: &step,
        me: 0,
        pool: Pool::new(&step, 0, Share::Any),
        key: 7,
        width: config.seal.kernel.width,
        sealed: false,
        pass: (0, 0, 0),
        tally: Tally::default(),
    };
    // SAFETY: the region's tiles, this thread's alone
    let first: u64 = (0..tiles).map(|_| unsafe { worker.claim(&mut runner) }.expect("no tile busy")).sum();
    assert_eq!(first, 0);
    // SAFETY: a word of the region, which no claim holds now
    unsafe { *base.add(2 * 8 * KIB + 100) ^= 1 << 40 };
    // SAFETY: as above
    let second: u64 = (0..tiles).map(|_| unsafe { worker.claim(&mut runner) }.expect("no tile busy")).sum();
    assert_eq!(second, 1, "the next pass's claim of the tile finds the bad word");
    // SAFETY: as above
    let third: u64 = (0..tiles).map(|_| unsafe { worker.claim(&mut runner) }.expect("no tile busy")).sum();
    assert_eq!(third, 0, "the claim that found it wrote the tile anew");
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
fn a_state_word_names_its_writer_and_polarity() {
    for (claim, writer_thread, invert) in [(0, 0, false), (1, 5, true), (CLAIM_BITS, WRITER_BITS as usize - 1, true)] {
        let state = written(claim, writer_thread, invert);
        assert_ne!(state, 0, "a write is never the no-write state");
        assert_eq!(state & BUSY, 0);
        assert_eq!(writer(state), writer_thread);
        assert_eq!(state & INVERT != 0, invert);
        assert_eq!(state & CLAIM_BITS, claim & CLAIM_BITS);
    }
}
