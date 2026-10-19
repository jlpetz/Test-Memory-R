//! Shared, zero-cost scaffolding for the loop-owning correctness tests.
//!
//! This is **not** an orchestrator (that mistake — a generic `TestPattern` trait +
//! `run_interleaved_test()` — was tried and deleted 2026-04-13 because
//! `#[target_feature]` does not propagate through trait/closure calls and SIMD
//! silently dropped to baseline). Instead `TestRunner` is a bag of bookkeeping
//! utilities the test composes while owning its entire hot inner loop.
//!
//! Every method here runs **between** chunks/blocks, never inside the SIMD loop:
//! they are simple field reads/writes that inline trivially. The hot path stays
//! byte-identical to the hand-written v1 tests — only the surrounding boilerplate
//! (extent sizing, block prep, timer/cycle loop, shutdown checks, throttled
//! progress, error-mode dispatch, `TestStats` construction) moves in here.
//!
//! See TODO #19 Part A for the design and the three-tier execution model.

use std::time::{Duration, Instant};
use std::sync::atomic::Ordering;

use crate::ErrorMode;
use crate::tests::{Stage, TestBlock, TestStats, TestAction, TestTiming, TestProgress, TestMemoryConfig};

use crate::runner::{AllocationBlock, SHUTDOWN_REQUESTED};

/// Per-test bookkeeping shared by the loop-owning v1 tests.
///
/// Construct with [`TestRunner::new`], which also returns the test's extent as a
/// [`TestBlock`] (kept as a local in the test, so the test can hold it while calling
/// `&mut self` methods on the runner).
pub struct TestRunner<'a> {
    test_name: &'static str,
    action: TestAction,
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &'a TestTiming,
    progress: Option<&'a TestProgress>,

    /// The test's chunk, resolved once from its extent (TODO 76). Every chunk is exactly this,
    /// spread evenly over the extent (`chunks`).
    chunk: usize,
    start: Instant,
    /// The work around the test sequence, by operation (TODO 90)
    ops: crate::ops::OpTally,
    /// The part of it inside the clock (the first cycle's seal checks and reseals, a sealed step's
    /// per-chunk fill): left out of the test's time when it finishes
    inside: Duration,
    cycle: u32,
    total_error_count: u64,
    total_bytes_processed: usize,
    last_progress_update: Instant,

    /// The seal as this step runs it (TODO 74), and the extent, which its kernels index from
    seal: crate::seal::SealStep,
    seal_base: *mut u64,
    seal_words: usize,
    /// Bad words the seal checks before chunks found: not this test's errors, but they halt it
    seal_errors: u64,
}

/// The extent a test of this config covers on these blocks: the first bytes of the thread's span
/// (TODO 76), at least one granule when there is memory. Also the worker's view of what a step
/// that never takes the seal will overwrite (TODO 74).
pub fn test_extent<'a>(blocks: &'a [AllocationBlock], config: &TestMemoryConfig, test_name: &str) -> (TestBlock<'a>, usize) {
    let total_allocated: usize = blocks.iter().map(|b| b.buffer.size()).sum();
    let extent_size = config.calculate_extent_size(test_name, total_allocated);
    let mut extent = crate::test_memory::extent(blocks, extent_size);
    if extent.test_size == 0 && total_allocated > 0 {
        extent = crate::test_memory::extent(blocks, crate::test_memory::GRANULE);
    }
    (extent, extent_size)
}

impl<'a> TestRunner<'a> {
    /// Size the extent and the chunk, and start the timers.
    ///
    /// Returns `(runner, extent)`: the first bytes of the thread's span (TODO 76).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        blocks: &'a [AllocationBlock],
        thread_id: usize,
        error_mode: ErrorMode,
        timing: &'a TestTiming,
        config: &'a TestMemoryConfig,
        progress: Option<&'a TestProgress>,
        test_name: &'static str,
        action: TestAction,
    ) -> (Self, TestBlock<'a>) {
        // The extent is the first `extent_size` bytes of the thread's span (TODO 76).
        let (extent, extent_size) = test_extent(blocks, config, test_name);
        if extent.test_size > extent_size {
            log::error!("{}: extent of {} bytes is below 4 KiB; testing 4 KiB instead", test_name, extent_size);
        }
        let total_test_size = extent.test_size;
        let chunk = config.calculate_chunk_size(test_name, total_test_size);

        // This test's failed checks, counted from here by the verifies' cold path (TODO 27)
        crate::tests::reset_rescan_counts();
        let now = Instant::now();
        let runner = TestRunner {
            test_name,
            action,
            thread_id,
            error_mode,
            timing,
            progress,
            chunk,
            start: now,
            ops: Default::default(),
            inside: Duration::ZERO,
            cycle: 0,
            total_error_count: 0,
            total_bytes_processed: 0,
            last_progress_update: now,
            seal: config.seal,
            seal_base: extent.ptr as *mut u64,
            seal_words: extent.test_size / 8,
            seal_errors: 0,
        };
        // The untimed setup a latency or bandwidth test does before `restart_clock`, for the ticker
        match action {
            TestAction::Latency => runner.set_stage(Stage::BuildingPointerChain),
            _ if test_name.starts_with("Spd-") => runner.set_stage(Stage::FillingMemory),
            _ => {}
        }

        // Log the registration name (carries the `_A` auto-dispatch suffix and distinguishes
        // registrations that share one test fn, e.g. Mem-StuckBit-Flush128); fall back to the
        // fn's baked-in name. Sizing above still keys off `test_name` — do not swap that.
        // What it covers, the extent asked for, and the chunks (Mem-Random's chunks are RNG
        // iterations, not these byte ranges).
        let log_name = config.display_name.as_deref().unwrap_or(test_name);
        log::info!(
            "[Thread {}] Running {} on {} (extent {}), {}",
            thread_id, log_name,
            crate::test_memory::size_str(total_test_size),
            crate::test_memory::size_str(extent_size),
            crate::test_memory::describe_chunks(runner.chunks(total_test_size)),
        );
        (runner, extent)
    }

    /// The chunks of an extent `len` bytes long: each exactly the test's chunk (or the extent,
    /// when it is shorter), spread evenly from 0 to `len` (TODO 76). The test converts the byte
    /// offsets to its own element/operation units.
    #[inline]
    pub fn chunks(&self, len: usize) -> crate::test_memory::ChunkSpread {
        crate::test_memory::ChunkSpread::new(len, self.chunk)
    }

    /// Half-chunks over the first half of an extent, for tests that copy it to the second half:
    /// `chunks(len)` halved, on a 2 KiB granule.
    #[inline]
    pub fn half_chunks(&self, len: usize) -> crate::test_memory::ChunkSpread {
        crate::test_memory::ChunkSpread::with_granule(len / 2, self.chunk.min(len) / 2, crate::test_memory::GRANULE / 2)
    }

    /// The test's chunk in bytes, for tests whose chunks aren't byte ranges (Mem-Random's are
    /// batches of random reads).
    #[inline]
    pub fn chunk_bytes(&self) -> usize {
        self.chunk
    }

    /// Start the clock again, leaving out what came before, with no record of it. A test with
    /// setup to leave out says what it was: `setup_done`. Call it before counting any bytes.
    #[inline]
    pub fn restart_clock(&mut self) {
        self.set_stage(Stage::Testing);
        let now = Instant::now();
        self.start = now;
        self.inside = Duration::ZERO;
        self.last_progress_update = now;
    }

    /// Setup since the clock started is done (TODO 90): recorded as `op` over `bytes`, and the
    /// clock starts again, so it counts toward neither the test's time nor its throughput. A fill
    /// (Mem-Random's, a bandwidth test's first touch) or a latency test's chain build.
    #[inline]
    pub fn setup_done(&mut self, op: crate::ops::Op, bytes: u64) {
        self.ops.add(op, self.start.elapsed(), bytes);
        self.restart_clock();
    }

    /// Run `f`, setup inside the test's clock (TODO 90): recorded as `op` over `bytes` and left
    /// out of the test's time, as a sealed step's per-chunk fill. Two clock reads a call, so for
    /// per-chunk work, not per word.
    #[inline]
    pub fn untimed<R>(&mut self, op: crate::ops::Op, bytes: u64, f: impl FnOnce() -> R) -> R {
        let at = Instant::now();
        let r = f();
        let time = at.elapsed();
        self.ops.add(op, time, bytes);
        self.inside += time;
        r
    }

    /// Whether this step wraps each chunk in the seal (TODO 74): `check_seal` before the test works
    /// it, `reseal` after.
    #[inline]
    pub fn seal_wrap(&self) -> bool {
        self.seal.wrap
    }

    /// Check the seal on bytes `[start, start + len)` of the extent before the test works them
    /// (TODO 74); a no-op unless the step wraps, and after the step's first cycle. Bad words count
    /// as seal errors, not the test's, and halt it under `errors=halt`.
    ///
    /// Only the first cycle: a later one finds the chunks as the test left them, so a test that
    /// runs many cycles of its own (a duration) pays for the wrap once, and its extent is resealed
    /// once, when it finishes.
    #[inline]
    pub fn check_seal(&mut self, start: usize, len: usize) {
        if !self.seal.wrap || self.cycle > 1 {
            return;
        }
        self.set_stage(Stage::CheckingSeal);
        let at = Instant::now();
        let (from, to) = (start / 8, (start + len) / 8);
        // SAFETY: the range is inside the extent, which the test is about to work
        self.seal_errors += unsafe { self.seal.kernel.check(self.seal_base, from, to, "seal check before") };
        // The seal's work, not the test's (TODO 90)
        let time = at.elapsed();
        self.ops.add(crate::ops::Op::SealCheck, time, len as u64);
        self.inside += time;
        self.set_stage(Stage::Testing);
    }

    /// Reseal bytes `[start, start + len)` of the extent once the test is done with them (TODO 74);
    /// a no-op unless the step wraps, and after its first cycle (see `check_seal`).
    #[inline]
    pub fn reseal(&mut self, start: usize, len: usize) {
        if !self.seal.wrap || self.cycle > 1 {
            return;
        }
        self.set_stage(Stage::Resealing);
        let at = Instant::now();
        // SAFETY: as `check_seal`
        unsafe { self.seal.kernel.fill(self.seal_base, start / 8, (start + len) / 8) };
        let time = at.elapsed();
        self.ops.add(crate::ops::Op::SealFill, time, len as u64);
        self.inside += time;
        self.set_stage(Stage::Testing);
    }

    /// Name what the worker is doing, for the ticker.
    #[inline]
    pub fn set_stage(&self, stage: Stage) {
        if let Some(progress) = self.progress {
            progress.set_stage(stage);
        }
    }

    /// Increment and return the new cycle number. Call at the top of each outer cycle.
    #[inline]
    pub fn begin_cycle(&mut self) -> u32 {
        self.cycle += 1;
        self.cycle
    }

    /// Accumulate processed bytes (called per block, between chunks).
    #[inline]
    pub fn add_bytes(&mut self, n: usize) {
        self.total_bytes_processed = self.total_bytes_processed.saturating_add(n);
    }

    #[inline]
    pub fn bytes_processed(&self) -> usize { self.total_bytes_processed }

    /// Fold this cycle's error count into the running total. Call once at cycle end.
    #[inline]
    pub fn commit_cycle_errors(&mut self, cycle_errors: u64) {
        self.total_error_count += cycle_errors;
    }

    /// Error-mode dispatch for a mid-chunk error check.
    ///
    /// - `Panic`: panics immediately (debug builds / strict runs).
    /// - `Halt`: returns `true` when `cycle_errors > 0` so the test can break its
    ///   own (possibly labeled) loop and `finish_aborted`.
    /// - `Log`: returns `false` (errors already logged in the hot loop).
    #[inline]
    pub fn should_halt(&self, cycle_errors: u64) -> bool {
        let cycle_errors = cycle_errors + self.seal_errors;
        if cycle_errors == 0 {
            return false;
        }
        match self.error_mode {
            ErrorMode::Panic => panic!(
                "{}: {} memory errors detected in cycle {} (see logs above)",
                self.test_name, cycle_errors, self.cycle
            ),
            ErrorMode::Halt => true,
            ErrorMode::Log => false,
        }
    }

    /// Cooperative shutdown check (Relaxed, between chunks — never in the SIMD loop).
    #[inline]
    pub fn shutdown_requested(&self) -> bool {
        SHUTDOWN_REQUESTED.load(Ordering::Relaxed)
    }

    /// Throttled (250 ms) progress publish at the end of a cycle. No-op if no progress sink.
    #[inline]
    pub fn update_progress(&mut self) {
        self.publish_progress(self.cycle);
    }

    /// The same from inside a cycle, between chunks: the current cycle isn't complete yet.
    #[inline]
    pub fn update_progress_in_cycle(&mut self) {
        self.publish_progress(self.cycle.saturating_sub(1));
    }

    #[inline]
    fn publish_progress(&mut self, cycles_completed: u32) {
        if let Some(progress) = self.progress {
            let now = Instant::now();
            if now.duration_since(self.last_progress_update).as_millis() >= 250 {
                progress.cycles_completed.store(cycles_completed, Ordering::Relaxed);
                progress.bytes_processed.store(self.total_bytes_processed as u64, Ordering::Relaxed);
                progress.errors_found.store(self.total_error_count + self.seal_errors, Ordering::Relaxed);
                progress.last_update_ms.store(self.start.elapsed().as_millis() as u64, Ordering::Relaxed);
                self.last_progress_update = now;
            }
        }
    }

    /// Timing/cycle gate. `true` => run another cycle.
    #[inline]
    pub fn should_continue(&self) -> bool {
        let elapsed_secs = self.start.elapsed().as_secs() as u32;
        self.timing.should_continue(self.cycle, elapsed_secs)
    }

    /// A step that wrapped its chunks in the seal reseals its whole extent as it finishes when it
    /// ran past its first cycle (TODO 74, `check_seal`), or was cut short (`aborted`), which can
    /// leave a chunk it was working unsealed. Returns the test's time, taken before, without its
    /// setup (TODO 90).
    ///
    /// Not when an error halts it: the bad words stay in memory as the test found them, for a
    /// look after the run, and the run stops at this step anyway.
    fn reseal_extent(&mut self, aborted: bool, cycle_errors: u64) -> std::time::Duration {
        let timed = self.start.elapsed().saturating_sub(self.inside);
        let halted = matches!(self.error_mode, ErrorMode::Halt) && self.total_error_count + cycle_errors + self.seal_errors > 0;
        if halted {
            log::info!("[Thread {}] {}: halted on an error, so the extent keeps what the test found (no reseal)",
                       self.thread_id, self.test_name);
        }
        if self.seal.wrap && (self.cycle > 1 || aborted) && !halted {
            self.set_stage(Stage::Resealing);
            let at = Instant::now();
            // SAFETY: the extent the test has just finished with
            unsafe { self.seal.kernel.fill(self.seal_base, 0, self.seal_words) };
            // After the test's time: the seal's work, on its own record (TODO 90)
            self.ops.add(crate::ops::Op::SealFill, at.elapsed(), (self.seal_words * 8) as u64);
            self.set_stage(Stage::Testing);
        }
        timed
    }

    /// Build `TestStats` for a mid-cycle abort (Halt on error, or shutdown).
    /// `error_count = running total + this cycle's errors`, `stopped_by_time_limit = false`.
    #[inline]
    pub fn finish_aborted(&mut self, cycle_errors: u64, total_operations: u64) -> TestStats {
        let timed = Some(self.reseal_extent(true, cycle_errors));
        let rescans = crate::tests::rescan_counts();
        TestStats {
            name: self.test_name,
            action: self.action,
            bytes_processed: self.total_bytes_processed,
            timed,
            thread_id: self.thread_id,
            error_count: self.total_error_count + cycle_errors,
            total_operations,
            cycles_completed: self.cycle,
            cycles_planned: self.timing.cycles,
            stopped_by_time_limit: false,
            seal_errors: self.seal_errors,
            alarms: rescans.alarms,
            not_reproduced: rescans.not_reproduced,
            ops: self.ops,
        }
    }

    /// Build `TestStats` for a normal completion (timing/cycle limit reached).
    /// Assumes the final cycle's errors were already folded via `commit_cycle_errors`.
    #[inline]
    pub fn finish_completed(&mut self, total_operations: u64) -> TestStats {
        let timed = Some(self.reseal_extent(false, 0));
        let rescans = crate::tests::rescan_counts();
        TestStats {
            name: self.test_name,
            action: self.action,
            bytes_processed: self.total_bytes_processed,
            timed,
            thread_id: self.thread_id,
            error_count: self.total_error_count,
            total_operations,
            cycles_completed: self.cycle,
            cycles_planned: self.timing.cycles,
            stopped_by_time_limit: self.timing.cycles.is_none_or(|limit| self.cycle < limit),
            seal_errors: self.seal_errors,
            alarms: rescans.alarms,
            not_reproduced: rescans.not_reproduced,
            ops: self.ops,
        }
    }
}
