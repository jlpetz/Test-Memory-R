//! Mem-CoherenceStorm (TODO 28): GSAT's cache coherency test (stressapptest's `--cc_test`), cache lines that every thread writes at
//! once. A few shared structures (`storm_lines`, 2 by default as GSAT's) hold a byte for each thread; a thread bumps its own byte in a
//! structure picked at random, `storm_increments` times (1000, GSAT's), then checks its bytes. No thread writes another's byte, but they
//! share the lines, and a store needs its line to itself, so each line is pulled from core to core (across CCDs and nodes when its
//! sharers are) with almost no DRAM traffic: the coherence fabric's load (FCLK on Ryzen), which no other test makes. A byte that doesn't
//! hold what its thread put there is a write lost or changed between the cores: a coherence failure, or a line corrupted in transit.
//!
//! **The layout.** A structure is `group` cache lines, `group` the threads over 64 rounded up, and thread `t`'s byte is byte `t / group`
//! of its line `t % group`: so a line's (up to) 64 sharers are spread over the thread range (`t`, `t + group`, ...), not its neighbours,
//! which on a big machine sit on one CCD. GSAT gives each thread an int, in order, 16 to a line.
//!
//! **The check.** GSAT sums a thread's bytes and compares the sum's low byte with its increments. Here each thread replays its random
//! sequence after the bumps (the hot loop keeps no tally) and checks each byte against its own count, so a change anywhere shows, named
//! by its structure; a wrong byte is counted once (the next check expects what it found).
//!
//! The test writes none of its memory span: its lines are the step's (`Step`, shared through `TestMemoryConfig::storm`), so it takes no
//! seal (`runner::seal_use`) and its extent is one granule.

use std::sync::Barrier;
use std::sync::atomic::{AtomicU8, Ordering};

use crate::test_scaffolding::TestRunner;
use crate::tests::{TestAction, TestMemoryConfig, TestProgress, TestStats, TestTiming};

pub const TEST_NAME: &str = "Mem-CoherenceStorm";
/// GSAT's: two structures, 1000 increments a check
pub const DEFAULT_LINES: u32 = 2;
pub const DEFAULT_INCREMENTS: u32 = 1000;
/// The most structures and increments a check a config may set
pub const MAX_LINES: u32 = 4096;
pub const MAX_INCREMENTS: u32 = 1 << 24;
/// Threads a cache line holds a byte of
const SHARERS: usize = 64;
/// Checks a cycle
const CHECKS_A_CYCLE: u32 = 256;

/// A cache line of bytes
#[repr(align(64))]
struct Line([AtomicU8; SHARERS]);

/// One step of the storm, shared by its workers: their nodes, the lines and the barrier they start at.
pub struct Step {
    nodes: Box<[u32]>,
    /// `structures` structures of `group` lines each
    lines: Box<[Line]>,
    structures: usize,
    group: usize,
    barrier: Barrier,
}

impl Step {
    /// A step for threads on `nodes` (thread `i` on `nodes[i]`), with `config`'s structures (`storm_lines`).
    pub fn new(nodes: Vec<u32>, config: &TestMemoryConfig) -> Self {
        let threads = nodes.len().max(1);
        let structures =
            config.parameter_context.as_ref().and_then(|c| c.storm_lines).unwrap_or(DEFAULT_LINES).clamp(1, MAX_LINES) as usize;
        let group = threads.div_ceil(SHARERS);
        Step {
            nodes: if nodes.is_empty() { vec![0].into() } else { nodes.into() },
            lines: (0..structures * group).map(|_| Line(std::array::from_fn(|_| AtomicU8::new(0)))).collect(),
            structures,
            group,
            barrier: Barrier::new(threads),
        }
    }

    pub fn threads(&self) -> usize {
        self.nodes.len()
    }

    /// Thread `t`'s byte of structure `s`: its line's index and its place in the line
    fn place(&self, t: usize, s: usize) -> (usize, usize) {
        (s * self.group + t % self.group, t / self.group)
    }

    fn byte(&self, t: usize, s: usize) -> &AtomicU8 {
        let (line, at) = self.place(t, s);
        &self.lines[line].0[at]
    }
}

impl std::fmt::Debug for Step {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Step {{ threads: {}, structures: {} }}", self.threads(), self.structures)
    }
}

/// xorshift64: the next of a nonzero sequence
#[inline(always)]
fn next(mut r: u64) -> u64 {
    r ^= r << 13;
    r ^= r >> 7;
    r ^ (r << 17)
}

/// The structure `r` picks of `n`: its top bits scaled, below `n` with no division
#[inline(always)]
fn pick(r: u64, n: usize) -> usize {
    ((u128::from(r) * n as u128) >> 64) as usize
}

/// The hot loop: `n` bumps of this thread's byte in a structure picked from the sequence after `r`, each a plain load and store (as
/// GSAT's `++`), so each takes its line from whichever core wrote it last. Returns the sequence's last value.
///
/// # Safety
/// `bytes[s]` is this thread's byte of structure `s`, which no other thread writes; `bytes` isn't empty.
#[inline(never)]
unsafe fn bump(bytes: &[&AtomicU8], mut r: u64, n: u32) -> u64 {
    let count = bytes.len();
    for _ in 0..n {
        r = next(r);
        // SAFETY: `pick` is below `count`
        let byte = unsafe { bytes.get_unchecked(pick(r, count)) };
        byte.store(byte.load(Ordering::Relaxed).wrapping_add(1), Ordering::Relaxed);
    }
    r
}

/// One thread's part in the step: its bytes, what each should hold, and the counts a replay gives.
struct Worker<'s> {
    step: &'s Step,
    me: usize,
    bytes: Vec<&'s AtomicU8>,
    expected: Vec<u8>,
    counts: Vec<u32>,
}

impl<'s> Worker<'s> {
    fn new(step: &'s Step, me: usize) -> Self {
        let bytes: Vec<&AtomicU8> = (0..step.structures).map(|s| step.byte(me, s)).collect();
        let expected = bytes.iter().map(|b| b.load(Ordering::Relaxed)).collect();
        Worker { step, me, bytes, expected, counts: vec![0; step.structures] }
    }

    /// The bytes that don't hold what the `n` bumps from `start` should have left, logged and counted, each expected as found from now.
    #[inline(never)]
    fn check(&mut self, start: u64, n: u32) -> u64 {
        self.counts.fill(0);
        let (mut r, structures) = (start, self.counts.len());
        for _ in 0..n {
            r = next(r);
            self.counts[pick(r, structures)] += 1;
        }
        let mut bad = 0;
        for s in 0..self.bytes.len() {
            let want = self.expected[s].wrapping_add(self.counts[s] as u8);
            let got = self.bytes[s].load(Ordering::Relaxed);
            if got != want {
                bad += 1;
                self.bad_byte(s, want, got);
            }
            self.expected[s] = got;
        }
        bad
    }

    #[cold]
    #[inline(never)]
    fn bad_byte(&self, s: usize, want: u8, got: u8) {
        let step = self.step;
        let (line, at) = step.place(self.me, s);
        let sharers = (0..step.threads()).filter(|t| t % step.group == self.me % step.group).count();
        let nodes: std::collections::BTreeSet<u32> =
            (0..step.threads()).filter(|t| t % step.group == self.me % step.group).map(|t| step.nodes[t]).collect();
        log::error!(
            "{}: thread {} (node {})'s byte of shared structure {s} (line {line}, byte {at}, address {:p}): expected {want:#04x}, got \
             {got:#04x}, XOR {:#04x}: a write lost or changed between the {sharers} threads that share the line (nodes {nodes:?})",
            crate::error_context::found_by(TEST_NAME),
            self.me,
            step.nodes[self.me],
            self.bytes[s],
            want ^ got,
        );
    }
}

/// Mem-CoherenceStorm (TODO 28). In a run, every worker of the step runs it, sharing `config.storm` (`ThreadPool::execute_test`); called
/// without one (a unit test), the thread storms alone.
#[doc = include_str!("test_fn_safety.md")]
pub unsafe fn coherence_storm_multi(
    blocks: &[crate::runner::AllocationBlock], thread_id: usize, error_mode: crate::ErrorMode, timing: &TestTiming,
    config: &TestMemoryConfig, progress: Option<&TestProgress>,
) -> TestStats {
    let (mut runner, _) = TestRunner::new(blocks, thread_id, error_mode, timing, config, progress, TEST_NAME, TestAction::ReadWrite);
    let alone;
    let (step, me) = match config.storm.as_deref() {
        Some(step) => (step, thread_id),
        None => {
            alone = Step::new(vec![0], config);
            (&alone, 0)
        }
    };
    assert!(me < step.threads(), "thread {me} in a coherence storm of {} threads", step.threads());
    let increments =
        config.parameter_context.as_ref().and_then(|c| c.storm_increments).unwrap_or(DEFAULT_INCREMENTS).clamp(1, MAX_INCREMENTS);
    let mut worker = Worker::new(step, me);
    // Each thread's own sequence, from the step's key
    let mut r = crate::pattern::fmix(config.epoch.key() ^ crate::pattern::fmix(me as u64 + 1)) | 1;
    // Every thread starts together, so the lines are contended from the first bump
    step.barrier.wait();
    runner.restart_clock();
    let ops = |runner: &TestRunner| runner.bytes_processed() as u64;
    loop {
        runner.begin_cycle();
        let mut cycle_errors = 0;
        for _ in 0..CHECKS_A_CYCLE {
            if runner.shutdown_requested() {
                return runner.finish_aborted(cycle_errors, ops(&runner));
            }
            let start = r;
            // SAFETY: the worker's bytes are its own of each structure
            r = unsafe { bump(&worker.bytes, r, increments) };
            cycle_errors += worker.check(start, increments);
            // A bump is a byte stored
            runner.add_bytes(increments as usize);
            if runner.should_halt(cycle_errors) {
                return runner.finish_aborted(cycle_errors, ops(&runner));
            }
            runner.update_progress_in_cycle();
        }
        runner.commit_cycle_errors(cycle_errors);
        runner.update_progress();
        if !runner.should_continue() {
            break;
        }
    }
    log::debug!(
        "[Thread {me}] {TEST_NAME}: {} bumps over {} structures of {} lines, {increments} a check",
        runner.bytes_processed(),
        step.structures,
        step.group
    );
    runner.finish_completed(ops(&runner))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::config::TestParameterContext;
    use crate::runner::test_support::{KIB, Region};
    use crate::tests::{ChunkMode, ExtentMode};

    fn config(cycles: u32, lines: Option<u32>) -> TestMemoryConfig {
        let mut config = TestMemoryConfig::new(ExtentMode::Absolute { size_bytes: 4 * KIB }, ChunkMode::Absolute { size_bytes: 4 * KIB })
            .with_timing(TestTiming::cycles_only(cycles));
        config.parameter_context = Some(TestParameterContext { storm_lines: lines, storm_increments: Some(200), ..Default::default() });
        config
    }

    #[test]
    fn a_thread_alone_storms_clean() {
        let region = Region::new(&[64 * KIB]);
        let config = config(2, None);
        // SAFETY: the region's blocks, live for the call
        let stats = unsafe { coherence_storm_multi(&region.blocks, 0, crate::ErrorMode::Log, &config.timing, &config, None) };
        assert_eq!((stats.error_count, stats.cycles_completed), (0, 2));
        assert_eq!(stats.bytes_processed, 2 * CHECKS_A_CYCLE as usize * 200);
        assert!(region.guards_intact());
    }

    /// Threads on two nodes bump their bytes of the same lines at once, and every byte holds its thread's count
    #[test]
    fn threads_sharing_lines_keep_their_counts() {
        for lines in [1, 2, 5] {
            let nodes = vec![0, 0, 1, 1, 0, 1];
            let step = Arc::new(Step::new(nodes.clone(), &config(3, Some(lines))));
            let config = TestMemoryConfig { storm: Some(Arc::clone(&step)), ..config(3, Some(lines)) };
            let regions: Vec<Region> = nodes.iter().map(|_| Region::new(&[64 * KIB])).collect();
            let stats: Vec<TestStats> = std::thread::scope(|s| {
                let handles: Vec<_> = regions
                    .iter()
                    .enumerate()
                    .map(|(t, region)| {
                        let (blocks, config) = (&region.blocks, &config);
                        // SAFETY: each thread's own region, live for the scope
                        s.spawn(move || unsafe { coherence_storm_multi(blocks, t, crate::ErrorMode::Log, &config.timing, config, None) })
                    })
                    .collect();
                handles.into_iter().map(|h| h.join().expect("a worker of the step")).collect()
            });
            for (t, s) in stats.iter().enumerate() {
                assert_eq!((s.error_count, s.cycles_completed), (0, 3), "{lines} structures, thread {t}");
            }
        }
    }

    /// A byte changed under its thread is found by its next check and counted once
    #[test]
    fn a_changed_byte_is_found_once() {
        let step = Step::new(vec![0, 0], &config(1, Some(3)));
        let mut worker = Worker::new(&step, 1);
        let mut r = 0x9E37_79B9_7F4A_7C15;
        for round in 0..4 {
            let start = r;
            // SAFETY: thread 1's bytes, which nothing else writes here
            r = unsafe { bump(&worker.bytes, r, 500) };
            if round == 2 {
                let b = step.byte(1, 1);
                b.store(b.load(Ordering::Relaxed) ^ 0x10, Ordering::Relaxed);
            }
            assert_eq!(worker.check(start, 500), u64::from(round == 2), "round {round}");
        }
        assert_eq!(step.byte(0, 1).load(Ordering::Relaxed), 0, "thread 0's byte untouched");
    }

    /// Every thread's byte of every structure is its own, and a line's sharers are spread over the threads
    #[test]
    fn the_layout_gives_each_thread_its_own_byte_spread_over_lines() {
        for threads in [1, 2, 64, 65, 130, 256] {
            let step = Step::new(vec![0; threads], &config(1, Some(2)));
            assert_eq!(step.group, threads.div_ceil(SHARERS));
            let mut seen = std::collections::HashSet::new();
            for t in 0..threads {
                for s in 0..2 {
                    let (line, at) = step.place(t, s);
                    assert!(line < step.lines.len() && at < SHARERS, "{threads} threads: thread {t}");
                    assert!(seen.insert((line, at)), "{threads} threads: thread {t}'s byte of structure {s} twice");
                }
            }
            // Thread 0's line holds threads 0, group, 2 * group, ...
            let sharers: Vec<usize> = (0..threads).filter(|&t| step.place(t, 0).0 == 0).collect();
            assert!(sharers.windows(2).all(|w| w[1] - w[0] == step.group), "{threads} threads: {sharers:?}");
        }
    }

    #[test]
    fn a_pick_stays_below_its_count() {
        let mut r = 1u64;
        for n in [1usize, 2, 3, 7, 4096] {
            let mut hits = vec![0u32; n];
            for _ in 0..100 * n.max(200) {
                r = next(r);
                hits[pick(r, n)] += 1;
            }
            assert!(hits.iter().all(|&h| h > 0), "{n}: {hits:?}");
        }
    }
}
