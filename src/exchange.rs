//! The page exchange (TODO 28): GSAT-style (stressapptest) tests whose memory crosses threads, three operations on one engine. Each
//! worker's extent is cut into tiles of the test's chunk size (2 MiB by default, a large page), apart and never overlapping (tile mode,
//! `doc/memory_system_design.md` §4.7), the last one shorter when the chunk doesn't divide the extent: tile mode's equal tiles would leave
//! the rest untested, and a tile's state word doesn't need them equal. Every thread then claims tiles from every thread's extent, checks
//! the image its last writer left and works it, so tiles cross threads, CCDs and NUMA nodes on purpose: the coherence fabric's and the
//! links' traffic, which no other test makes. The operations, a test each:
//! - **Mem-PageExchange** copies, GSAT's default: each thread holds one tile, and a claim copies its tile into the held one, checking it on
//!   the way (each word stored as loaded); the held tile goes back to the pool with the image, and the claimed one is held next, still
//!   holding it. GSAT keeps 2/5 of its pages empty for copies; holding one tile a thread, a copy takes one claim, and nearly all memory
//!   holds a checked image. A short last tile can't be copied into a full one, so it is checked and rewritten in place.
//! - **Mem-PageExchange-Invert** checks each word and writes back its inverse in place, GSAT's invert threads' operation in one checked
//!   pass (GSAT's are four unchecked ones, up, down, down, up).
//! - **Mem-PageExchange-Rewrite** checks the tile, then writes a new image over it.
//!
//! A tile's first claim fills it (after the seal's check, in a sealed step), whatever the operation.
//!
//! **The claim protocol**, which makes another thread's memory safe to write (`memory/buffer.rs`'s `Sync` note). Each tile has a state
//! word, on a cache line of its own, which its home thread allocates: free, with the image it holds, or busy. A thread claims a tile with
//! one compare-and-swap, free to busy (Acquire: it sees the last writer's stores), works it, and releases it with one store (Release,
//! after an SFENCE when its stores were non-temporal). So a tile has one thread at a time, and each sees the one before; a copy's held
//! tile stays busy while it is held. Nobody waits: a tile found busy is skipped, and with far more tiles than threads that is rare. No
//! message is sent: the state word is the signal.
//!
//! **Which tile.** A pool's claims are numbered by one counter, and claim `t` takes the pool's tile `(t * s + o) mod N` for its pass
//! `t / N`, `s` coprime with `N` and both drawn from the pass. So each pass covers every tile once, and each pass arranges them anew, which
//! hands a tile to different threads even when the threads run in step. The pool is the sharing group's (`exchange_share`): every thread's
//! tiles (`any`, the default), those of the claimer's node (`node`), or those of the other nodes (`cross`: the links between them).
//!
//! **The patterns** are the native families (`crate::pattern`), the step's `program` (all seven when it names none). A new image takes
//! the program's family at its claim's number, and its polarity a lap of the program in turn, so every family is written as itself and
//! as its complement; its key comes from the step's key and the image's number and writer. The state word names all of it, so whoever
//! claims the tile next can check it from that word alone. A word's value comes from its place in its tile (`kernels`), so a copied
//! image holds where it lands. A copy moves an image as it is, and an invert flips its polarity.
//!
//! **Stores** (`exchange_stores`): cached, as GSAT's copy and invert (the default for both), or non-temporal (the rewrite's default: the
//! next claimer reads the tile from DRAM, over the links when it's on another node).
//!
//! **The seal and the end.** In a sealed step a tile's first claim checks the seal (those are seal errors), as does taking a copy's first
//! held tile. Once every thread has stopped claiming (a barrier), each checks its own tiles' images, and they are resealed
//! (`TestRunner::reseal`, or the whole extent as the test finishes), so the cycle's final check finds the seal. A claim's errors are the
//! claimer's: it found them; one more line says whose tile it was, its image and who wrote it there last (or that it held the seal). A
//! claim that finds bad words writes the image whole again (the copy into the held tile, the inverted tile), so each is counted once.
//!
//! **A halt** (`errors=halt`) keeps the bad words where they were found, as every test does: the claim that found them releases its tile
//! as it found it (a copy's source, a rewrite's tile unwritten; an invert's tile inverted, the bad words with it), and since the memory is
//! shared, the whole step halts: every thread stops claiming, and none rechecks or reseals its tiles.

mod kernels;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Barrier, OnceLock};

use kernels::Kernels;

use crate::pattern::{LINE_WORDS, Phase, Program};
use crate::pattern_gen::BusBeat;
use crate::test_scaffolding::TestRunner;
use crate::tests::{TestAction, TestMemoryConfig, TestProgress, TestStats, TestTiming};

pub const TEST_NAME: &str = "Mem-PageExchange";
pub const INVERT_NAME: &str = "Mem-PageExchange-Invert";
pub const REWRITE_NAME: &str = "Mem-PageExchange-Rewrite";
/// The page exchange's tests, an operation each
pub const NAMES: [&str; 3] = [TEST_NAME, INVERT_NAME, REWRITE_NAME];

/// Whether `name` is one of the page exchange's tests
pub fn is_exchange(name: &str) -> bool {
    NAMES.contains(&name)
}

/// What a claim does to a tile that holds an image: the test's operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Copy,
    Invert,
    Rewrite,
}

impl Op {
    fn test_name(self) -> &'static str {
        match self {
            Op::Copy => TEST_NAME,
            Op::Invert => INVERT_NAME,
            Op::Rewrite => REWRITE_NAME,
        }
    }

    /// GSAT's stores for its copy and invert, NT for the rewrite (as built first)
    fn default_stores(self) -> Stores {
        match self {
            Op::Copy | Op::Invert => Stores::Cached,
            Op::Rewrite => Stores::Nt,
        }
    }
}

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

/// How a claim stores (`exchange_stores`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stores {
    /// Through the caches: a store usually reads its line first, and the cache writes it back when it evicts it
    Cached,
    /// Non-temporal, straight to DRAM
    Nt,
}

impl Stores {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "cached" => Ok(Stores::Cached),
            "nt" => Ok(Stores::Nt),
            other => Err(format!("\"exchange_stores\" is cached or nt, not \"{other}\"")),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Stores::Cached => "cached",
            Stores::Nt => "nt",
        }
    }
}

/// A pool's counter or the step's halt flag, on a cache line of its own.
#[repr(align(64))]
#[derive(Default)]
struct Slot(AtomicU64);

/// A tile's state, on a cache line of its own: two threads claiming neighbouring tiles would otherwise contend for one line.
#[repr(align(64))]
#[derive(Default)]
struct Tile {
    /// The state word: 0 is no image this step (the seal in a sealed step, else nothing known); an image is its writer (thread + 1), its
    /// family's place in the program, its number and its polarity; `BUSY` while a thread works it or a copy holds it.
    state: AtomicU64,
    /// The thread that last wrote the tile's bytes, + 1 (0: none this step): stored before the state's release, read after its claim, for
    /// a bad word's story.
    wrote: AtomicU64,
}

const BUSY: u64 = 1 << 63;
const INVERT: u64 = 1 << 62;
const WRITER_SHIFT: u32 = 47;
const WRITER_BITS: u64 = (1 << 15) - 1;
const FAMILY_SHIFT: u32 = 44;
const FAMILY_BITS: u64 = (1 << (WRITER_SHIFT - FAMILY_SHIFT)) - 1;
const IMAGE_BITS: u64 = (1 << FAMILY_SHIFT) - 1;

/// The state of image `image` of the program's family `family`, written by thread `writer`, as the complement when `invert`.
fn image(image: u64, family: usize, writer: usize, invert: bool) -> u64 {
    ((writer as u64 + 1) << WRITER_SHIFT)
        | ((family as u64 & FAMILY_BITS) << FAMILY_SHIFT)
        | (image & IMAGE_BITS)
        | if invert { INVERT } else { 0 }
}

/// The thread that wrote an image's state
fn writer(state: u64) -> usize {
    (((state >> WRITER_SHIFT) & WRITER_BITS) - 1) as usize
}

/// An image's family, as its place in the program
fn family_at(state: u64) -> usize {
    ((state >> FAMILY_SHIFT) & FAMILY_BITS) as usize
}

/// One thread's tiles, published at the step's start.
struct Home {
    /// The extent's first byte, as an address: the thread's span, which only claims touch during the step
    base: usize,
    /// The extent's length, and its tiles' (the last one shorter when this doesn't divide that)
    bytes: usize,
    tile_bytes: usize,
    tiles: Box<[Tile]>,
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
    /// Of its claims, the copies (into its held tile), and those whose held tile was on another node than the tile copied
    pub copies: u64,
    pub copies_across: u64,
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
    /// A thread halted on an error (`errors=halt`): every thread stops, and the memory keeps what was found
    halted: Slot,
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
            halted: Slot::default(),
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
    /// The group in effect: `cross` on one node is `any`
    share: Share,
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
        let share = if share == Share::Cross && !crosses { Share::Any } else { share };
        let (members, claimers, counter): (Vec<usize>, usize, usize) = match share {
            Share::Node => {
                let same: Vec<usize> = threads.filter(|&t| step.node(t) == mine).collect();
                let n = same.len();
                (same, n, 1 + mine)
            }
            Share::Cross => {
                let claimers = threads.clone().filter(|&t| step.node(t) == mine).count();
                (threads.filter(|&t| step.node(t) != mine).collect(), claimers, 1 + NODES + mine)
            }
            Share::Any => (threads.collect(), step.threads(), 0),
        };
        let (mut homes, mut starts, mut tiles) = (Vec::new(), Vec::new(), 0);
        for t in members {
            let n = step.home(t).tiles.len();
            if n > 0 {
                homes.push(t);
                starts.push(tiles);
                tiles += n;
            }
        }
        Pool { share, homes, starts, tiles, claimers: claimers.max(1), counter }
    }

    /// The pool's tile `i`: its home thread and its index there.
    fn at(&self, i: usize) -> Spot {
        let h = self.starts.partition_point(|&s| s <= i) - 1;
        Spot { home: self.homes[h], k: i - self.starts[h] }
    }
}

/// A tile: its home thread and its index there
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Spot {
    home: usize,
    k: usize,
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

/// The words of a tile that differ from `phase`'s image, named and counted, once a kernel's check has seen one (`seen`, its differences
/// ORed): the cold path (`crate::tests::Rescan`).
#[cold]
#[inline(never)]
unsafe fn rescan(ptr: *const u64, words: usize, phase: Phase, name: &str, seen: u64) -> u64 {
    let label = format!("{}, key {:#018x}", phase.label(), phase.key);
    if !crate::pattern::rescan_enabled() {
        return crate::tests::unlocalised(crate::error_context::found_by(name), format_args!("in idx 0..{words} ({label})"), seen);
    }
    let mut scan = crate::tests::Rescan::for_test(ptr, name).seen(seen);
    for line in 0..words / LINE_WORDS {
        scan.line(line * LINE_WORDS, &phase.line(line as u64));
    }
    scan.finish(format_args!("in idx 0..{words} ({label})"))
}

/// A copy's held tile, and what it holds: an image's state, 0 for nothing written this step (the seal in a sealed step), `None` for
/// words that don't hold one (a copy that carried bad words, which the held tile got whole instead of).
#[derive(Debug, Clone, Copy)]
struct Held {
    at: Spot,
    image: Option<u64>,
}

/// One thread's part in the step.
struct Worker<'s> {
    step: &'s Step,
    me: usize,
    op: Op,
    /// The test's name, for its error lines
    name: &'static str,
    stores: Stores,
    pool: Pool,
    key: u64,
    program: Program,
    /// The transfer width the switching families flip at; unknown, images alternate the 64- and 32-bit profiles
    beat: Option<BusBeat>,
    kernels: Kernels,
    /// The step wraps its chunks in the seal: a first claim checks it
    sealed: bool,
    /// The pass the last claim's arrangement is for
    pass: (u64, u64, u64),
    held: Option<Held>,
    tally: Tally,
}

/// What a claim did
enum Claim {
    /// The tile was busy: nothing
    Busy,
    /// Checked and worked: the bad words found
    Worked(u64),
    /// Found bad words under `errors=halt`: the tile released as found, and the step halted
    Halted(u64),
}

/// Why the claims stopped
enum Stop {
    /// The step's time or cycles are done
    Done,
    /// Ctrl+C, or errors under `errors=halt`: this cycle's errors
    Aborted(u64),
}

impl<'s> Worker<'s> {
    /// The pattern of the image in state `state` (an image's: not 0)
    fn phase(&self, state: u64) -> Phase {
        let state = state & !BUSY;
        let id = state & !INVERT;
        let beat = self.beat.unwrap_or(if id & 1 == 0 { BusBeat::Bits64 } else { BusBeat::Bits32 });
        Phase {
            family: self.program.families()[family_at(state)],
            key: crate::pattern::fmix(self.key ^ crate::pattern::fmix(id)),
            mask: if state & INVERT != 0 { !0 } else { 0 },
            beat,
        }
    }

    /// The image claim `t` writes: the program's family at `t`, its complement every other lap of the program
    fn new_image(&self, t: u64) -> u64 {
        let len = self.program.families().len() as u64;
        image(t, (t % len) as usize, self.me, (t / len) % 2 == 1)
    }

    /// The pool's tile claim `t` takes, by its pass's arrangement.
    fn tile(&mut self, t: u64) -> Spot {
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

    fn bytes(&self, at: Spot) -> (*mut u64, usize) {
        self.step.home(at.home).tile(at.k)
    }

    /// Releases tile `at` with `state`, its bytes written by this thread when `wrote`.
    fn release(&self, at: Spot, state: u64, wrote: bool) {
        let tile = &self.step.home(at.home).tiles[at.k];
        if wrote {
            tile.wrote.store(self.me as u64 + 1, Ordering::Relaxed);
        }
        tile.state.store(state, Ordering::Release);
    }

    /// Releases tile `at` as `state` and halts the step (`errors=halt`): the memory is shared, so every thread stops.
    fn halt(&self, at: Spot, state: u64, wrote: bool, found: u64) -> Claim {
        self.release(at, state, wrote);
        self.step.halted.0.store(1, Ordering::Relaxed);
        Claim::Halted(found)
    }

    /// The bad words of tile `at`'s image `state`, named and counted (0 when every word holds it).
    ///
    /// # Safety
    /// The tile is this thread's: claimed, or its own after the claims.
    unsafe fn check(&self, at: Spot, state: u64) -> u64 {
        let (ptr, len) = self.bytes(at);
        let phase = self.phase(state);
        // SAFETY: the caller's
        let seen = unsafe { self.kernels.verify(ptr, len / 8, phase) };
        if seen == 0 {
            return 0;
        }
        // SAFETY: as above
        let found = unsafe { rescan(ptr, len / 8, phase, self.name, seen) };
        self.bad_tile(found, at, Some(state));
        found
    }

    /// One claim, `cycle_errors` already found this cycle.
    ///
    /// # Safety
    /// Every home's tiles are inside its thread's extent, committed and live for the step.
    unsafe fn claim(&mut self, runner: &mut TestRunner, cycle_errors: u64) -> Claim {
        let step = self.step;
        let t = step.counters[self.pool.counter].0.fetch_add(1, Ordering::Relaxed);
        let at = self.tile(t);
        let tile = &step.home(at.home).tiles[at.k];
        let was = tile.state.load(Ordering::Relaxed);
        if was & BUSY != 0 || tile.state.compare_exchange(was, was | BUSY, Ordering::Acquire, Ordering::Relaxed).is_err() {
            self.tally.busy += 1;
            return Claim::Busy;
        }
        // A copy needs its held tile as long as the claimed one: the short last tile is rewritten in place
        let copies = self.held.is_some_and(|h| self.bytes(h.at).1 == self.bytes(at).1);
        // SAFETY: the tile, held (busy) from the CAS above until its release
        let claim = unsafe {
            match self.op {
                _ if was == 0 => self.first(runner, cycle_errors, at, t),
                Op::Copy if copies => self.copy(runner, cycle_errors, at, was),
                Op::Invert => self.invert(runner, cycle_errors, at, was),
                _ => self.rewrite(runner, cycle_errors, at, was, t),
            }
        };
        if let Claim::Worked(_) = claim {
            self.tally.claims += 1;
            self.tally.others += u64::from(at.home != self.me);
            self.tally.other_nodes += u64::from(step.node(at.home) != step.node(self.me));
        }
        claim
    }

    /// A tile's first claim this step: the seal's check in a sealed step, then a new image.
    ///
    /// # Safety
    /// As `claim`; the tile is claimed.
    unsafe fn first(&mut self, runner: &mut TestRunner, cycle_errors: u64, at: Spot, t: u64) -> Claim {
        let (ptr, len) = self.bytes(at);
        if self.sealed {
            // SAFETY: the caller's
            let seal = unsafe { runner.check_seal_of(ptr, len) };
            if seal > 0 {
                self.bad_tile(seal, at, None);
            }
        }
        // Seal errors count towards a halt (`should_halt`)
        if runner.should_halt(cycle_errors) {
            return self.halt(at, 0, false, 0);
        }
        let now = self.new_image(t);
        // SAFETY: as above
        unsafe { self.kernels.fill(ptr, len / 8, self.phase(now), self.stores) };
        runner.add_bytes(len);
        self.release(at, now, true);
        Claim::Worked(0)
    }

    /// The rewrite: check the image, then write a new one.
    ///
    /// # Safety
    /// As `first`.
    unsafe fn rewrite(&mut self, runner: &mut TestRunner, cycle_errors: u64, at: Spot, was: u64, t: u64) -> Claim {
        let (ptr, len) = self.bytes(at);
        // SAFETY: the caller's
        let found = unsafe { self.check(at, was) };
        runner.add_bytes(len);
        if runner.should_halt(cycle_errors + found) {
            return self.halt(at, was, false, found);
        }
        let now = self.new_image(t);
        // SAFETY: as above
        unsafe { self.kernels.fill(ptr, len / 8, self.phase(now), self.stores) };
        runner.add_bytes(len);
        self.release(at, now, true);
        Claim::Worked(found)
    }

    /// The invert: each word checked and written back inverted, so the tile holds the image's complement.
    ///
    /// # Safety
    /// As `first`.
    unsafe fn invert(&mut self, runner: &mut TestRunner, cycle_errors: u64, at: Spot, was: u64) -> Claim {
        let (ptr, len) = self.bytes(at);
        let now = was ^ INVERT;
        // SAFETY: the caller's
        let seen = unsafe { self.kernels.invert(ptr, len / 8, self.phase(was), self.stores) };
        runner.add_bytes(2 * len);
        let mut found = 0;
        if seen != 0 {
            // The bad words were inverted with the rest, so they differ from the complement as they did from the image
            // SAFETY: as above
            found = unsafe { rescan(ptr, len / 8, self.phase(now), self.name, seen) };
            self.bad_tile(found, at, Some(was));
        }
        if runner.should_halt(cycle_errors + found) {
            return self.halt(at, now, true, found);
        }
        if found > 0 {
            // SAFETY: as above
            unsafe { self.kernels.fill(ptr, len / 8, self.phase(now), self.stores) };
        }
        self.release(at, now, true);
        Claim::Worked(found)
    }

    /// The copy: the tile copied into the held one, checked on the way; the held one released with the image, and the tile held next.
    ///
    /// # Safety
    /// As `first`; the held tile is as long as the tile.
    unsafe fn copy(&mut self, runner: &mut TestRunner, cycle_errors: u64, at: Spot, was: u64) -> Claim {
        let held = self.held.expect("a copy holds a tile");
        let ((src, len), (dst, _)) = (self.bytes(at), self.bytes(held.at));
        let phase = self.phase(was);
        // SAFETY: the caller's; the held tile is this thread's, busy since it took it
        let seen = unsafe { self.kernels.copy(src, dst, len / 8, phase, self.stores) };
        runner.add_bytes(2 * len);
        let mut found = 0;
        if seen != 0 {
            // A copy reads its source and nothing more: its words are as the copy found them
            // SAFETY: as above
            found = unsafe { rescan(src, len / 8, phase, self.name, seen) };
            self.bad_tile(found, at, Some(was));
        }
        if runner.should_halt(cycle_errors + found) {
            // The source keeps the bad words; the held tile, holding their copy, stays held
            self.held = Some(Held { at: held.at, image: Some(was) });
            return self.halt(at, was, false, found);
        }
        if found > 0 {
            // The copy carried the bad words: the held tile gets the image whole, and the source holds none now
            // SAFETY: as above
            unsafe { self.kernels.fill(dst, len / 8, phase, self.stores) };
        }
        self.release(held.at, was, true);
        self.held = Some(Held { at, image: if found > 0 { None } else { Some(was) } });
        self.tally.copies += 1;
        self.tally.copies_across += u64::from(self.step.node(held.at.home) != self.step.node(at.home));
        Claim::Worked(found)
    }

    /// The held tile at the step's end, released with the image it holds; holding none (its words don't, or it was never copied into,
    /// as in a step of one pass), with a new one, which the end's check then checks; but as it is when the step halted.
    ///
    /// # Safety
    /// As `claim`.
    unsafe fn release_held(&mut self, runner: &mut TestRunner) {
        let Some(held) = self.held.take() else { return };
        match held.image {
            Some(state) if state != 0 => self.release(held.at, state, false),
            _ if self.step.halted.0.load(Ordering::Relaxed) != 0 => self.release(held.at, 0, false),
            _ => {
                let (ptr, len) = self.bytes(held.at);
                let now = self.new_image(IMAGE_BITS - held.at.k as u64);
                // SAFETY: the held tile, this thread's
                unsafe { self.kernels.fill(ptr, len / 8, self.phase(now), self.stores) };
                runner.add_bytes(len);
                self.release(held.at, now, true);
            }
        }
    }

    /// The tile's story, for the bad words the check before it logged: whose tile, its image and who wrote it there last (`None`: it
    /// held the seal)
    #[cold]
    #[inline(never)]
    fn bad_tile(&self, found: u64, at: Spot, image: Option<u64>) {
        let step = self.step;
        let held = match image {
            Some(state) => {
                let wrote = step.home(at.home).tiles[at.k].wrote.load(Ordering::Relaxed);
                let by = match wrote {
                    0 => "no one this step".to_string(),
                    w => format!("thread {} (node {})", w - 1, step.node(w as usize - 1)),
                };
                format!("holding {} (thread {}'s image), last written by {by}", self.phase(state).label(), writer(state))
            }
            None => "which held the seal".to_string(),
        };
        log::error!(
            "{}: those {found} words are tile {} of thread {} (node {}), {held}, and checked by thread {} (node {})",
            crate::error_context::found_by(self.name),
            at.k,
            at.home,
            step.node(at.home),
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
                if runner.shutdown_requested() || self.step.halted.0.load(Ordering::Relaxed) != 0 {
                    return Stop::Aborted(cycle_errors);
                }
                // SAFETY: the caller's
                match unsafe { self.claim(runner, cycle_errors) } {
                    Claim::Busy => {}
                    Claim::Worked(found) => cycle_errors += found,
                    Claim::Halted(found) => return Stop::Aborted(cycle_errors + found),
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

    /// After every thread has stopped: check this thread's own tiles' images (or the seal, on a tile no one claimed), and reseal them.
    ///
    /// # Safety
    /// As `claim`; no thread claims any more.
    unsafe fn finish_home(&mut self, runner: &mut TestRunner) -> u64 {
        let home = self.step.home(self.me);
        let mut errors = 0;
        for k in 0..home.tiles.len() {
            let at = Spot { home: self.me, k };
            let state = home.tiles[k].state.load(Ordering::Acquire);
            debug_assert_eq!(state & BUSY, 0, "tile {k} still busy after the claims");
            let (ptr, len) = home.tile(k);
            if state != 0 {
                // SAFETY: the thread's own tile, which no thread claims any more
                errors += unsafe { self.check(at, state) };
                runner.add_bytes(len);
            } else if self.sealed {
                // SAFETY: as above
                let seal = unsafe { runner.check_seal_of(ptr, len) };
                if seal > 0 {
                    self.bad_tile(seal, at, None);
                }
            }
            // A no-op past the first cycle: finishing then reseals the whole extent
            runner.reseal(k * home.tile_bytes, len);
        }
        errors
    }
}

/// Mem-PageExchange (TODO 28): the copy. In a run, every worker of the step runs it, sharing `config.exchange` (`ThreadPool::execute_test`);
/// called without one (a unit test, the seal's), the thread exchanges its tiles with itself.
#[doc = include_str!("test_fn_safety.md")]
pub unsafe fn page_exchange_multi(
    blocks: &[crate::runner::AllocationBlock], thread_id: usize, error_mode: crate::ErrorMode, timing: &TestTiming,
    config: &TestMemoryConfig, progress: Option<&TestProgress>,
) -> TestStats {
    // SAFETY: the caller's
    unsafe { run(Op::Copy, blocks, thread_id, error_mode, timing, config, progress) }
}

/// Mem-PageExchange-Invert (TODO 28), as `page_exchange_multi`.
#[doc = include_str!("test_fn_safety.md")]
pub unsafe fn page_exchange_invert_multi(
    blocks: &[crate::runner::AllocationBlock], thread_id: usize, error_mode: crate::ErrorMode, timing: &TestTiming,
    config: &TestMemoryConfig, progress: Option<&TestProgress>,
) -> TestStats {
    // SAFETY: the caller's
    unsafe { run(Op::Invert, blocks, thread_id, error_mode, timing, config, progress) }
}

/// Mem-PageExchange-Rewrite (TODO 28), as `page_exchange_multi`.
#[doc = include_str!("test_fn_safety.md")]
pub unsafe fn page_exchange_rewrite_multi(
    blocks: &[crate::runner::AllocationBlock], thread_id: usize, error_mode: crate::ErrorMode, timing: &TestTiming,
    config: &TestMemoryConfig, progress: Option<&TestProgress>,
) -> TestStats {
    // SAFETY: the caller's
    unsafe { run(Op::Rewrite, blocks, thread_id, error_mode, timing, config, progress) }
}

/// One thread's part in a step of the page exchange's `op`.
///
/// # Safety
/// As the test functions'.
unsafe fn run(
    op: Op, blocks: &[crate::runner::AllocationBlock], thread_id: usize, error_mode: crate::ErrorMode, timing: &TestTiming,
    config: &TestMemoryConfig, progress: Option<&TestProgress>,
) -> TestStats {
    let name = op.test_name();
    let (mut runner, extent) = TestRunner::new(blocks, thread_id, error_mode, timing, config, progress, name, TestAction::ReadWrite);
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
    let home =
        Home { base: extent.ptr as usize, bytes: extent.test_size, tile_bytes, tiles: (0..tiles).map(|_| Tile::default()).collect() };
    if step.homes[me].set(home).is_err() {
        panic!("thread {me} published its tiles twice in one page exchange step");
    }
    let home = step.home(me);
    let ctx = config.parameter_context.as_ref();
    // Under `exchange_invert`, that share of a copy's threads invert, spread evenly over the threads, as GSAT's copy and invert threads
    let invert = u64::from(ctx.and_then(|c| c.exchange_invert).unwrap_or(0).min(100));
    let at = |t: usize| t as u64 * invert / 100;
    let op = if op == Op::Copy && at(me + 1) > at(me) { Op::Invert } else { op };
    // A copy holds one of its own full tiles from the start, its seal checked first in a sealed step: busy before the barrier, so no
    // claim takes it
    let held = (op == Op::Copy).then(|| (0..tiles).find(|&k| home.tile(k).1 == tile_bytes)).flatten().map(|k| {
        home.tiles[k].state.store(BUSY, Ordering::Relaxed);
        let (ptr, len) = home.tile(k);
        // SAFETY: the thread's own tile, which no claim takes
        let seal = unsafe { runner.check_seal_of(ptr, len) };
        if seal > 0 {
            log::error!(
                "{}: those {seal} words are tile {k} of thread {me}, which held the seal, taken to copy into",
                crate::error_context::found_by(name)
            );
        }
        // Words that don't hold the seal hold nothing known: counted once, here
        Held { at: Spot { home: me, k }, image: (seal == 0).then_some(0) }
    });
    step.barrier.wait();

    let share = ctx.and_then(|c| c.exchange_share).unwrap_or_default();
    let stores = ctx.and_then(|c| c.exchange_stores).unwrap_or(op.default_stores());
    let program = config.program.unwrap_or_else(Program::default_exchange);
    let mut worker = Worker {
        step,
        me,
        op,
        name,
        stores,
        pool: Pool::new(step, me, share),
        key: config.epoch.key(),
        program,
        beat: crate::pattern::transfer_beat().0,
        kernels: Kernels::at(crate::simd::test_width()),
        sealed: config.seal.wrap,
        pass: (0, 0, 0),
        held,
        tally: Tally::default(),
    };
    runner.restart_clock();
    // SAFETY: every home's tiles are its thread's extent, which the step's caller vouches for, and the claim protocol gives each claim its
    // tile alone
    let stop = unsafe { worker.claims(&mut runner) };
    // SAFETY: as above
    unsafe { worker.release_held(&mut runner) };
    // Every thread waits here, however it stopped, so none checks its own tiles while another still claims; after it, each sees whether
    // one halted (the barrier orders the flag's store before)
    step.barrier.wait();
    let halted = step.halted.0.load(Ordering::Relaxed) != 0;
    if halted {
        runner.keep_memory();
    }
    let ops = |runner: &TestRunner| (runner.bytes_processed() / 8) as u64;
    let stats = match stop {
        Stop::Done if !halted => {
            // SAFETY: the thread's own tiles; no thread claims any more
            let errors = unsafe { worker.finish_home(&mut runner) };
            runner.commit_cycle_errors(errors);
            runner.finish_completed(ops(&runner))
        }
        Stop::Done => runner.finish_aborted(0, ops(&runner)),
        Stop::Aborted(errors) => runner.finish_aborted(errors, ops(&runner)),
    };
    let t = worker.tally;
    log::debug!(
        "[Thread {me}] {name} ({}{}, {} stores, {}, {} bits): {} claims, {} of other threads' tiles, {} on other nodes, {} copies ({} across \
         nodes); {} found busy",
        worker.pool.share.name(),
        if op.test_name() == name { "" } else { ", inverting" },
        stores.name(),
        program.name(),
        worker.kernels.bits(),
        t.claims,
        t.others,
        t.other_nodes,
        t.copies,
        t.copies_across,
        t.busy
    );
    let _ = step.tallies[me].set(t);
    stats
}

#[cfg(test)]
mod tests;
