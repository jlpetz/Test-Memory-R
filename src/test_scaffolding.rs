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

use std::time::Instant;
use std::sync::atomic::Ordering;

use crate::ErrorMode;
use crate::tests::{TestBlock, TestStats, TestAction, TestTiming, TestProgress, TestMemoryConfig};

use crate::runner::{AllocationBlock, SHUTDOWN_REQUESTED};

/// Per-test bookkeeping shared by the loop-owning v1 tests.
///
/// Construct with [`TestRunner::new`], which also returns the prepared
/// [`TestBlock`] list (kept as a local in the test so the test can hold an
/// immutable borrow of it while calling `&mut self` methods on the runner).
pub struct TestRunner<'a> {
    test_name: &'static str,
    action: TestAction,
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &'a TestTiming,
    progress: Option<&'a TestProgress>,

    /// The test's chunk, resolved once from its extent (TODO 76). Every chunk is exactly this,
    /// spread evenly over each piece (`chunks`); a piece shorter than it is one chunk.
    chunk: usize,
    start: Instant,
    cycle: u32,
    total_error_count: u64,
    total_bytes_processed: usize,
    last_progress_update: Instant,
}

impl<'a> TestRunner<'a> {
    /// Prepare blocks for the extent and start the timers.
    ///
    /// Returns `(runner, test_blocks)`. The test keeps `test_blocks` as a local
    /// `Vec` (exactly as the v1 tests do today) so iterating it doesn't conflict
    /// with `&mut self` accumulator calls on the runner.
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
    ) -> (Self, Vec<TestBlock<'a>>) {
        let total_allocated: usize = blocks.iter().map(|b| b.buffer.size()).sum();
        let extent_size = config.calculate_extent_size(test_name, total_allocated);
        // The extent is the first `extent_size` bytes of the thread's span (TODO 76).
        let mut test_blocks = crate::test_memory::extent_pieces(blocks, extent_size);
        if test_blocks.is_empty() && let Some(first) = blocks.first() {
            log::error!("{}: extent of {} bytes is below 4 KiB; testing 4 KiB instead", test_name, extent_size);
            test_blocks.push(TestBlock::at_block(first, crate::test_memory::GRANULE.min(first.buffer.size())));
        }
        let total_test_size: usize = test_blocks.iter().map(|b| b.test_size).sum();
        let chunk = config.calculate_chunk_size(test_name, total_test_size);

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
            cycle: 0,
            total_error_count: 0,
            total_bytes_processed: 0,
            last_progress_update: now,
        };

        // Log the registration name (carries the `_A` auto-dispatch suffix and distinguishes
        // registrations that share one test fn, e.g. Mem-StuckBit-Flush128); fall back to the
        // fn's baked-in name. Sizing above still keys off `test_name` — do not swap that.
        // Each piece is listed with its page sizes and chunks (Mem-Random's chunks are RNG
        // iterations, not these byte ranges).
        let log_name = config.display_name.as_deref().unwrap_or(test_name);
        log::info!(
            "[Thread {}] Running {} on {} (extent {}): {}",
            thread_id, log_name,
            crate::test_memory::size_str(total_test_size),
            crate::test_memory::size_str(extent_size),
            crate::test_memory::describe_pieces(blocks, &test_blocks, |piece| runner.chunks(piece)),
        );
        (runner, test_blocks)
    }

    /// The chunks of a piece `len` bytes long: each exactly the test's chunk (or the piece, when
    /// it is shorter), spread evenly from 0 to `len` (TODO 76). The test converts the byte
    /// offsets to its own element/operation units.
    #[inline]
    pub fn chunks(&self, len: usize) -> crate::test_memory::ChunkSpread {
        crate::test_memory::ChunkSpread::new(len, self.chunk)
    }

    /// Half-chunks over the first half of a piece, for tests that copy it to the second half:
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
                progress.errors_found.store(self.total_error_count, Ordering::Relaxed);
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

    /// Build `TestStats` for a mid-cycle abort (Halt on error, or shutdown).
    /// `error_count = running total + this cycle's errors`, `stopped_by_time_limit = false`.
    #[inline]
    pub fn finish_aborted(&self, cycle_errors: u64, total_operations: u64) -> TestStats {
        TestStats {
            name: self.test_name,
            action: self.action,
            bytes_processed: self.total_bytes_processed,
            elapsed_ms: self.start.elapsed().as_millis(),
            thread_id: self.thread_id,
            error_count: self.total_error_count + cycle_errors,
            total_operations,
            cycles_completed: self.cycle,
            cycles_planned: self.timing.cycles,
            stopped_by_time_limit: false,
        }
    }

    /// Build `TestStats` for a normal completion (timing/cycle limit reached).
    /// Assumes the final cycle's errors were already folded via `commit_cycle_errors`.
    #[inline]
    pub fn finish_completed(&self, total_operations: u64) -> TestStats {
        TestStats {
            name: self.test_name,
            action: self.action,
            bytes_processed: self.total_bytes_processed,
            elapsed_ms: self.start.elapsed().as_millis(),
            thread_id: self.thread_id,
            error_count: self.total_error_count,
            total_operations,
            cycles_completed: self.cycle,
            cycles_planned: self.timing.cycles,
            stopped_by_time_limit: self.timing.cycles.is_none_or(|limit| self.cycle < limit),
        }
    }
}
