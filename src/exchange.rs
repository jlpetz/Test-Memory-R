//! Mem-PageExchange (TODO 28): a GSAT-style (stressapptest) page exchange, the first test whose memory crosses threads. Each worker's
//! extent is cut into tiles of the test's chunk size (2 MiB by default, a large page), apart and never overlapping (tile mode,
//! `doc/memory_system_design.md` §4.7), the last one shorter when the chunk doesn't divide the extent: tile mode's equal tiles would leave
//! the rest untested, and a tile's state word doesn't need them equal. Every thread then claims tiles from every thread's extent, checks the
//! pattern its last writer left,
//! writes its own and releases it, so tiles cross threads, CCDs and NUMA nodes on purpose: the coherence fabric's and the links' traffic,
//! which no other test makes.
//!
//! **The claim protocol**, which makes another thread's memory safe to write (`memory/buffer.rs`'s `Sync` note). Each tile has a state
//! word, on a cache line of its own, which its home thread allocates: free, with its last writer and claim, or busy. A thread claims a
//! tile with one compare-and-swap, free to busy (Acquire: it sees the last writer's stores), works it, and releases it with one store
//! (Release, after the fill's own SFENCE: its stores are non-temporal). So a tile has one thread at a time, and each sees the one before.
//! Nobody waits: a tile found busy is skipped, and with far more tiles than threads that is rare. No message is sent: the state word is the
//! signal.
//!
//! **Which tile.** A pool's claims are numbered by one counter, and claim `t` takes the pool's tile `(t * s + o) mod N` for its pass
//! `t / N`, `s` coprime with `N` and both drawn from the pass. So each pass covers every tile once, and each pass arranges them anew, which
//! hands a tile to different threads even when the threads run in step. The pool is the sharing group's (`exchange_share`): every thread's
//! tiles (`any`, the default), those of the claimer's node (`node`), or those of the other nodes (`cross`: the links between them).
//!
//! **The pattern** is the seal's `tmr` (`seal.rs`): a hash of each word's address and a key, here from the step's key and the tile's state
//! word, so whoever claims a tile next can check it from that word alone. Alternate passes invert it, so every bit takes both values. The
//! seal's fill writes it (non-temporal: the next claimer reads it from DRAM, over the links when it's on another node), and its verify checks
//! it, at the seal's width.
//!
//! **The seal and the end.** In a sealed step a tile's first claim checks the seal (those are seal errors). Once every thread has stopped
//! claiming (a barrier), each checks its own tiles' last writes, and they are resealed (`TestRunner::reseal`, or the whole extent as the
//! test finishes), so the cycle's final check finds the seal. Unsealed, a first claim only writes. A claim's errors are the claimer's: it
//! found them; one more line says whose tile it was and who wrote it last.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Barrier, OnceLock};

use crate::seal::{SealKernel, SealPattern, SealWidth};
use crate::test_scaffolding::TestRunner;
use crate::tests::{TestAction, TestMemoryConfig, TestProgress, TestStats, TestTiming};

pub const TEST_NAME: &str = "Mem-PageExchange";

/// Whose tiles a thread claims (`exchange_share`): the test's sharing group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Share {
    /// Any thread's (the default)
    #[default]
    Any,
    /// Only the threads' on its own node: the exchange without the links between nodes
    Node,
    /// Only the threads' on other nodes: every claim crosses a link. On one node, `Any`.
    Cross,
}

impl Share {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "any" => Ok(Share::Any),
            "node" => Ok(Share::Node),
            "cross" => Ok(Share::Cross),
            other => Err(format!("\"exchange_share\" is any, node or cross, not \"{other}\"")),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Share::Any => "any",
            Share::Node => "node",
            Share::Cross => "cross",
        }
    }
}

/// A state word or a pool's counter, on a cache line of its own: two threads claiming neighbouring tiles would otherwise contend for one
/// line.
#[repr(align(64))]
#[derive(Default)]
struct Slot(AtomicU64);

/// A tile's state word: 0 is no write this step (the seal in a sealed step, else nothing known); a write is its writer (thread + 1), its
/// claim and its polarity; `BUSY` while a thread works it.
const BUSY: u64 = 1 << 63;
const INVERT: u64 = 1 << 62;
const WRITER_SHIFT: u32 = 47;
const WRITER_BITS: u64 = (1 << 15) - 1;
const CLAIM_BITS: u64 = (1 << WRITER_SHIFT) - 1;

/// The state a write leaves: `claim` by thread `writer`, inverted on odd passes.
fn written(claim: u64, writer: usize, invert: bool) -> u64 {
    ((writer as u64 + 1) << WRITER_SHIFT) | (claim & CLAIM_BITS) | if invert { INVERT } else { 0 }
}

/// The thread that left a write's state
fn writer(state: u64) -> usize {
    (((state >> WRITER_SHIFT) & WRITER_BITS) - 1) as usize
}

/// One thread's tiles, published at the step's start.
struct Home {
    /// The extent's first byte, as an address: the thread's span, which only claims touch during the step
    base: usize,
    /// The extent's length, and its tiles' (the last one shorter when this doesn't divide that)
    bytes: usize,
    tile_bytes: usize,
    states: Box<[Slot]>,
}

impl Home {
    /// Tile `k`: its first word and its length in bytes
    fn tile(&self, k: usize) -> (*mut u64, usize) {
        let at = k * self.tile_bytes;
        ((self.base + at) as *mut u64, self.tile_bytes.min(self.bytes - at))
    }
}

/// What one thread did in a step, for the tests and its debug line.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Tally {
    /// Tiles it worked
    pub claims: u64,
    /// Claims that found the tile busy
    pub busy: u64,
    /// Of its claims, tiles in another thread's extent, and on another node
    pub others: u64,
    pub other_nodes: u64,
}

/// One step of the exchange, shared by its workers through the step's `TestMemoryConfig::exchange`: each worker's node and tiles, the pools'
/// counters, the barrier between the phases, and what each did.
pub struct Step {
    /// Each thread's node, its CPU's (`ThreadPool::cpu_assignments`); the memory a worker fills is its node's first
    nodes: Box<[u32]>,
    homes: Box<[OnceLock<Home>]>,
    /// A pool's claims: `[0]` every thread's tiles, then `1 + n` node `n`'s for its own threads, then `1 + NODES + n` the other nodes'
    /// for node `n`'s threads
    counters: Box<[Slot]>,
    /// Between publishing the tiles and claiming them, and between claiming and the end
    barrier: Barrier,
    tallies: Box<[OnceLock<Tally>]>,
}

/// The nodes a step keeps counters for; a node beyond them shares the last one's (Windows has at most 64 on a processor group)
const NODES: usize = 64;

impl Step {
    /// A step for threads on `nodes` (thread `i` on `nodes[i]`), one entry a thread.
    pub fn new(nodes: Vec<u32>) -> Self {
        let threads = nodes.len().max(1);
        assert!(threads as u64 <= WRITER_BITS, "{threads} threads: a state word names at most {WRITER_BITS}");
        Step {
            nodes: if nodes.is_empty() { vec![0].into() } else { nodes.into() },
            homes: (0..threads).map(|_| OnceLock::new()).collect(),
            counters: (0..1 + 2 * NODES).map(|_| Slot::default()).collect(),
            barrier: Barrier::new(threads),
            tallies: (0..threads).map(|_| OnceLock::new()).collect(),
        }
    }

    pub fn threads(&self) -> usize {
        self.nodes.len()
    }

    fn node(&self, thread: usize) -> usize {
        (self.nodes[thread] as usize).min(NODES - 1)
    }

    fn home(&self, thread: usize) -> &Home {
        self.homes[thread].get().expect("every thread publishes its tiles before the barrier")
    }

    /// What thread `thread` did in the step, once it has finished
    #[cfg(test)]
    pub fn tally(&self, thread: usize) -> Option<Tally> {
        self.tallies.get(thread).and_then(|t| t.get().copied())
    }
}

impl std::fmt::Debug for Step {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Step {{ threads: {} }}", self.threads())
    }
}

/// The tiles a thread claims from: its sharing group's homes, in order, numbered through.
struct Pool {
    homes: Vec<usize>,
    /// Each home's first tile in the pool's numbering
    starts: Vec<usize>,
    tiles: usize,
    /// The threads that draw on its counter
    claimers: usize,
    counter: usize,
}

impl Pool {
    fn new(step: &Step, me: usize, share: Share) -> Self {
        let mine = step.node(me);
        let threads = 0..step.threads();
        let crosses = threads.clone().any(|t| step.node(t) != mine);
        let (members, claimers, counter): (Vec<usize>, usize, usize) = match share {
            Share::Node => {
                let same: Vec<usize> = threads.filter(|&t| step.node(t) == mine).collect();
                let n = same.len();
                (same, n, 1 + mine)
            }
            Share::Cross if crosses => {
                let claimers = threads.clone().filter(|&t| step.node(t) == mine).count();
                (threads.filter(|&t| step.node(t) != mine).collect(), claimers, 1 + NODES + mine)
            }
            // `cross` on one node: every thread's
            _ => (threads.collect(), step.threads(), 0),
        };
        let (mut homes, mut starts, mut tiles) = (Vec::new(), Vec::new(), 0);
        for t in members {
            let n = step.home(t).states.len();
            if n > 0 {
                homes.push(t);
                starts.push(tiles);
                tiles += n;
            }
        }
        Pool { homes, starts, tiles, claimers: claimers.max(1), counter }
    }

    /// The pool's tile `i`: its home thread and its index there.
    fn at(&self, i: usize) -> (usize, usize) {
        let h = self.starts.partition_point(|&s| s <= i) - 1;
        (self.homes[h], i - self.starts[h])
    }
}

fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

/// A pass's arrangement of `tiles` tiles: a step `s` coprime with `tiles` and an offset, from the step's key and the pass.
fn arrangement(tiles: usize, key: u64, pass: u64) -> (u64, u64) {
    let n = tiles as u64;
    let h = crate::pattern::fmix(key ^ crate::pattern::fmix(pass.wrapping_add(0xE8C4_A9E1)));
    let mut s = (h % n) | 1;
    while gcd(s, n) != 1 {
        s += 2;
    }
    (s, crate::pattern::fmix(h) % n)
}

/// One thread's part in the step.
struct Worker<'s> {
    step: &'s Step,
    me: usize,
    pool: Pool,
    key: u64,
    width: SealWidth,
    /// The step wraps its chunks in the seal: a first claim checks it
    sealed: bool,
    /// The pass the last claim's arrangement is for
    pass: (u64, u64, u64),
    tally: Tally,
}

/// Why the claims stopped
enum Stop {
    /// The step's time or cycles are done
    Done,
    /// Ctrl+C, or errors under `errors=halt`: this cycle's errors
    Aborted(u64),
}

impl<'s> Worker<'s> {
    /// The pattern a tile in state `state` holds (a write's state: not 0)
    fn kernel(&self, state: u64) -> SealKernel {
        let state = state & !BUSY;
        SealKernel {
            pattern: SealPattern::Tmr,
            width: self.width,
            key: crate::pattern::fmix(self.key ^ crate::pattern::fmix(state)),
            invert: state & INVERT != 0,
        }
    }

    /// The pool's tile claim `t` takes, by its pass's arrangement.
    fn tile(&mut self, t: u64) -> (usize, usize) {
        let n = self.pool.tiles as u64;
        let pass = t / n;
        if self.pass.0 != pass || self.pass.1 == 0 {
            let (s, o) = arrangement(self.pool.tiles, self.key, pass);
            self.pass = (pass, s, o);
        }
        let (_, s, o) = self.pass;
        let i = ((t % n) as u128 * s as u128 + o as u128) % n as u128;
        self.pool.at(i as usize)
    }

    /// One claim: the bad words its check found, or `None` when the tile was busy.
    ///
    /// # Safety
    /// Every home's tiles are inside its thread's extent, committed and live for the step.
    unsafe fn claim(&mut self, runner: &mut TestRunner) -> Option<u64> {
        let step = self.step;
        let t = step.counters[self.pool.counter].0.fetch_add(1, Ordering::Relaxed);
        let (home_thread, k) = self.tile(t);
        let home = step.home(home_thread);
        let slot = &home.states[k].0;
        let was = slot.load(Ordering::Relaxed);
        if was & BUSY != 0 || slot.compare_exchange(was, was | BUSY, Ordering::Acquire, Ordering::Relaxed).is_err() {
            self.tally.busy += 1;
            return None;
        }
        let (ptr, len) = home.tile(k);
        let mut found = 0;
        if was != 0 {
            // SAFETY: the tile, held (busy) from the CAS above until the release below
            found = unsafe { self.kernel(was).verify(ptr, 0, len / 8, "page exchange's check") };
            if found > 0 {
                self.bad_tile(found, home_thread, k, was);
            }
            runner.add_bytes(len);
        } else if self.sealed {
            // SAFETY: as above
            unsafe { runner.check_seal_of(ptr, len) };
        }
        let pass = t / self.pool.tiles as u64;
        let now = written(t, self.me, pass % 2 == 1);
        // SAFETY: as above; the fill ends with an SFENCE, so its stores come before the release
        unsafe { self.kernel(now).fill(ptr, 0, len / 8) };
        runner.add_bytes(len);
        slot.store(now, Ordering::Release);
        self.tally.claims += 1;
        self.tally.others += u64::from(home_thread != self.me);
        self.tally.other_nodes += u64::from(step.node(home_thread) != step.node(self.me));
        Some(found)
    }

    /// The tile's story, for the bad words the check before it logged
    #[cold]
    #[inline(never)]
    fn bad_tile(&self, found: u64, home_thread: usize, k: usize, was: u64) {
        let step = self.step;
        log::error!(
            "{}: those {found} words are tile {k} of thread {home_thread} (node {}), last written by thread {} (node {}), and checked by \
             thread {} (node {})",
            crate::error_context::found_by(TEST_NAME),
            step.node(home_thread),
            writer(was),
            step.node(writer(was)),
            self.me,
            step.node(self.me),
        );
    }

    /// Claim tiles until the step's time or cycles are done: a cycle is this thread's share of a pass.
    ///
    /// # Safety
    /// As `claim`.
    unsafe fn claims(&mut self, runner: &mut TestRunner) -> Stop {
        if self.pool.tiles == 0 {
            return Stop::Done;
        }
        let share = self.pool.tiles.div_ceil(self.pool.claimers);
        loop {
            runner.begin_cycle();
            let mut cycle_errors = 0;
            for _ in 0..share {
                if runner.shutdown_requested() {
                    return Stop::Aborted(cycle_errors);
                }
                // SAFETY: the caller's
                cycle_errors += unsafe { self.claim(runner) }.unwrap_or(0);
                if runner.should_halt(cycle_errors) {
                    return Stop::Aborted(cycle_errors);
                }
                runner.update_progress_in_cycle();
            }
            runner.commit_cycle_errors(cycle_errors);
            runner.update_progress();
            if !runner.should_continue() {
                return Stop::Done;
            }
        }
    }

    /// After every thread has stopped: check this thread's own tiles' last writes (or the seal, on a tile no one claimed), and reseal them.
    ///
    /// # Safety
    /// As `claim`; no thread claims any more.
    unsafe fn finish_home(&mut self, runner: &mut TestRunner) -> u64 {
        let home = self.step.home(self.me);
        let mut errors = 0;
        for k in 0..home.states.len() {
            let state = home.states[k].0.load(Ordering::Acquire);
            let (ptr, len) = home.tile(k);
            if state != 0 {
                // SAFETY: the thread's own tile, which no thread claims any more
                let found = unsafe { self.kernel(state).verify(ptr, 0, len / 8, "page exchange's last check") };
                if found > 0 {
                    self.bad_tile(found, self.me, k, state);
                    errors += found;
                }
                runner.add_bytes(len);
            } else if self.sealed {
                // SAFETY: as above
                unsafe { runner.check_seal_of(ptr, len) };
            }
            // A no-op past the first cycle: finishing then reseals the whole extent
            runner.reseal(k * home.tile_bytes, len);
        }
        errors
    }
}

/// Mem-PageExchange (TODO 28). In a run, every worker of the step runs it, sharing `config.exchange` (`ThreadPool::execute_test`); called
/// without one (a unit test, the seal's), the thread exchanges its tiles with itself.
#[doc = include_str!("test_fn_safety.md")]
pub unsafe fn page_exchange_multi(
    blocks: &[crate::runner::AllocationBlock], thread_id: usize, error_mode: crate::ErrorMode, timing: &TestTiming,
    config: &TestMemoryConfig, progress: Option<&TestProgress>,
) -> TestStats {
    let (mut runner, extent) = TestRunner::new(blocks, thread_id, error_mode, timing, config, progress, TEST_NAME, TestAction::ReadWrite);
    let alone;
    let (step, me) = match config.exchange.as_deref() {
        Some(step) => (step, thread_id),
        None => {
            alone = Step::new(vec![0]);
            (&alone, 0)
        }
    };
    assert!(me < step.threads(), "thread {me} in a page exchange of {} threads", step.threads());
    // Tiles of the chunk's size from the extent's start, apart, the last one what is left (both whole granules)
    let tile_bytes = runner.chunk_bytes().max(crate::test_memory::GRANULE);
    let tiles = if extent.ptr.is_null() { 0 } else { extent.test_size.div_ceil(tile_bytes) };
    let states = (0..tiles).map(|_| Slot::default()).collect();
    let home = Home { base: extent.ptr as usize, bytes: extent.test_size, tile_bytes, states };
    if step.homes[me].set(home).is_err() {
        panic!("thread {me} published its tiles twice in one page exchange step");
    }
    step.barrier.wait();

    let share = config.parameter_context.as_ref().and_then(|c| c.exchange_share).unwrap_or_default();
    let mut worker = Worker {
        step,
        me,
        pool: Pool::new(step, me, share),
        key: config.epoch.key(),
        width: config.seal.kernel.width,
        sealed: config.seal.wrap,
        pass: (0, 0, 0),
        tally: Tally::default(),
    };
    runner.restart_clock();
    // SAFETY: every home's tiles are its thread's extent, which the step's caller vouches for, and the claim protocol gives each claim its
    // tile alone
    let stop = unsafe { worker.claims(&mut runner) };
    // Every thread waits here, however it stopped, so none checks its own tiles while another still claims
    step.barrier.wait();
    let ops = |runner: &TestRunner| (runner.bytes_processed() / 8) as u64;
    let stats = match stop {
        Stop::Done => {
            // SAFETY: the thread's own tiles; no thread claims any more
            let errors = unsafe { worker.finish_home(&mut runner) };
            runner.commit_cycle_errors(errors);
            runner.finish_completed(ops(&runner))
        }
        Stop::Aborted(errors) => runner.finish_aborted(errors, ops(&runner)),
    };
    let t = worker.tally;
    log::debug!(
        "[Thread {me}] {TEST_NAME} ({}): {} claims, {} of other threads' tiles, {} on other nodes; {} found busy",
        share.name(),
        t.claims,
        t.others,
        t.other_nodes,
        t.busy
    );
    let _ = step.tallies[me].set(t);
    stats
}

#[cfg(test)]
mod tests;
