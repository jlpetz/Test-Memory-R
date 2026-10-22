use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};
use std::thread;

use crate::frontend::Frontend;
use crate::tests::TestProgress;

// Live suite position for the progress ticker, plus the run's error tallies. Per-cycle and per-test
// results are not kept here: `TestRunResult` (results.rs) owns those, and the final summary and
// the JSON file are both built from it. A frontend reads it with `snapshot` (TODO 29).
pub struct ProgressTracker {
    // What the suite was asked to do, fixed by `begin_suite`.
    suite: OnceLock<SuitePlan>,
    position: Mutex<Position>,
    finished: AtomicBool,

    // Hardware errors the OS saw that our verify reads cannot (DDR5 on-die ECC corrections, and
    // any fault landing during a non-verifying test like bandwidth/latency). Counted separately
    // from the thread-reported errors because these are OS-reported. See whea.rs.
    pub(crate) whea: crate::whea::WheaMonitor,
}

/// What the suite was asked to do, fixed when it starts.
struct SuitePlan {
    start: Instant,
    cycle_limit: Option<u32>,
    time_limit: Option<Duration>,
    tests_per_cycle: u64,
    /// The workers' live-progress slots (thread_pool.rs), which carry the running test's figures.
    workers: Arc<[TestProgress]>,
}

/// Where the suite is now, and the errors of the tests already finished. One lock, so a render
/// never sees a half-made step: in particular a test's errors counted both in `errors` and still in
/// its workers' live figures.
struct Position {
    cycle: u32, // 1-based, 0 = not started
    cycle_start: Instant,
    tests_done: u64, // tests finished in this cycle
    test: Option<RunningTest>,
    errors: u64, // from finished tests, whole run
}

/// The test the workers are running now, or the one that just finished.
struct RunningTest {
    number: u64, // 1-based position in the cycle
    name: String,
    start: Instant,
    time_limit: Option<Duration>,
    /// Whether this kind of test publishes live figures. Every built-in does since TODO 76, when
    /// the latency tests moved onto `TestRunner`.
    publishes: bool,
    /// Results are in and its errors are in `Position::errors`, so its workers' live errors must
    /// not be added again.
    finished: bool,
}

/// How the suite ended. Drives the run verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunOutcome {
    /// The cycle limit was reached.
    Completed,
    /// The suite time limit expired. It is checked between cycles, so the last cycle always
    /// finishes.
    TimeLimit,
    /// Ctrl+C.
    Interrupted,
    /// `ErrorMode::Halt` stopped the suite on an error.
    Halted,
}

impl RunOutcome {
    /// Its key in the results file (`OverallStats::outcome`)
    pub fn key(self) -> &'static str {
        match self {
            RunOutcome::Completed => "completed",
            RunOutcome::TimeLimit => "time_limit",
            RunOutcome::Interrupted => "interrupted",
            RunOutcome::Halted => "halted",
        }
    }
}

#[derive(Debug, Clone)]
pub struct TestSummary {
    /// The step's 1-based position in the cycle (TODO 74)
    pub step: usize,
    /// Which of the plan's tests it ran: steps of one test share it
    pub test_index: usize,
    /// The test's config id (a TM5 import's test number)
    pub id: Option<String>,
    /// The step's name in reports, e.g. `Mem-SimpleV2-512_A (Test 12)`
    pub label: String,
    /// What the seal checks before its chunks found (TODO 74): not the test's errors
    pub seal_errors: u64,
    pub name: String,
    pub duration_ms: u128,
    /// The test's own time, its slowest thread's, which its speed divides by (TODO 90)
    pub timed_ms: f64,
    pub bytes_processed: u64,
    /// The operations its threads counted
    pub operations: u64,
    /// Passes over its extent the step's threads completed, the fewest and the most (threads on a
    /// time limit can differ), and what ended it: `"cycles"` (its cycle limit), `"time"` (its time
    /// limit) or `"stopped"` (errors=halt, Ctrl+C, or a test that ends without either). `None`
    /// for a step that didn't run.
    pub cycles_completed: (u32, u32),
    pub stopped_by: Option<&'static str>,
    /// The work around its test sequence, by operation (TODO 90): the seal's between the steps
    /// and around its chunks, its fills
    pub ops: crate::ops::OpTally,
    pub throughput_mib_s: f64,
    pub errors: u64,
    /// The test's failed checks, and of those the ones its rescans didn't reproduce (TODO 27):
    /// `errors` counts the bad words the rescans found, plus one for each not reproduced.
    pub alarms: u64,
    pub not_reproduced: u64,
    /// The bits that failed the step's own checks, and its seal checks' (TODO 97)
    pub failing_bits: crate::failing_bits::FailingBits,
    pub seal_failing_bits: crate::failing_bits::FailingBits,
    // Hardware errors WHEA reported to the OS while this test was running (all severities), and
    // the subset the OS reported as corrected. Tracked apart from `errors` because they are not
    // thread-detected: see whea.rs.
    pub whea_total: u64,
    pub whea_corrected: u64,
    // Latency metrics (Some for latency tests, None for other tests)
    pub latency_samples: Option<u64>,
    pub latency_p5_ns: Option<f64>,
    pub latency_p10_ns: Option<f64>,
    pub latency_p25_ns: Option<f64>,
    pub latency_p50_ns: Option<f64>,
    pub latency_p75_ns: Option<f64>,
    pub latency_p90_ns: Option<f64>,
    pub latency_p95_ns: Option<f64>,
    pub latency_p99_ns: Option<f64>,
    pub latency_p99_9_ns: Option<f64>,
    pub latency_spread: Option<f64>,
    /// Each thread's part of the step, by thread (TODO 72); none for a step that didn't run
    pub threads: Vec<crate::results::ThreadStepRecord>,
}

/// The running test's figures, summed over the workers that have published.
#[derive(Debug, Clone, Default)]
pub struct LiveFigures {
    pub published: bool,
    pub bytes: u64,
    /// The running test's errors so far, also `ProgressSnapshot::live_errors` until it finishes
    pub errors: u64,
    pub bytes_per_sec: f64,
    /// What the first worker not just testing is doing (TODO 74), e.g. "Checking seal"
    pub stage: Option<crate::tests::Stage>,
}

/// Where the suite is, read at once (TODO 29): what the ticker shows, and what a test's report
/// ends with. `ProgressTracker::snapshot` takes it.
#[derive(Debug, Clone)]
pub struct ProgressSnapshot {
    /// Since the suite began (`begin_suite`), and its time limit
    pub runtime: Duration,
    pub time_limit: Option<Duration>,
    /// The cycle, 1-based (0 before the first), its limit and how long it has run
    pub cycle: u32,
    pub cycle_limit: Option<u32>,
    pub cycle_elapsed: Duration,
    /// Tests finished in this cycle, of the cycle's
    pub tests_done: u64,
    pub tests_per_cycle: u64,
    /// The errors of the tests finished, the whole run's, and what the running test has found so
    /// far, not yet in them (0 once it has finished)
    pub finished_errors: u64,
    pub live_errors: u64,
    pub whea: crate::whea::WheaCounts,
    /// The test running, or the one that just finished
    pub test: Option<TestSnapshot>,
}

/// The running test, or the one that just finished, in a `ProgressSnapshot`.
#[derive(Debug, Clone)]
pub struct TestSnapshot {
    /// Its 1-based position in the cycle, and its name as the reports give it
    pub number: u64,
    pub name: String,
    pub elapsed: Duration,
    /// Its own time limit, when it has one
    pub time_limit: Option<Duration>,
    /// Its results are in (`complete_test`): its errors are then in `finished_errors`
    pub finished: bool,
    /// Whether it publishes live figures (`RunningTest::publishes`)
    pub publishes: bool,
    /// Its workers' last published figures
    pub live: LiveFigures,
}

impl Default for ProgressTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl ProgressTracker {
    pub fn new() -> Self {
        Self {
            suite: OnceLock::new(),
            position: Mutex::new(Position {
                cycle: 0,
                cycle_start: Instant::now(),
                tests_done: 0,
                test: None,
                errors: 0,
            }),
            finished: AtomicBool::new(false),
            // Inert until `whea.start()` is called by the runner — constructing a tracker must not
            // have the side effect of opening an event-log subscription.
            whea: crate::whea::WheaMonitor::new(),
        }
    }

    fn position(&self) -> MutexGuard<'_, Position> {
        self.position.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Start the suite clock. The progress line draws nothing until this is called, so Runtime is
    /// measured from the first test cycle — the same span the final summary reports — rather than
    /// from before allocation.
    pub(crate) fn begin_suite(
        &self,
        start: Instant,
        cycle_limit: Option<u32>,
        time_limit: Option<Duration>,
        tests_per_cycle: u64,
        workers: Arc<[TestProgress]>,
    ) {
        let _ = self.suite.set(SuitePlan { start, cycle_limit, time_limit, tests_per_cycle, workers });
    }

    pub(crate) fn start_new_cycle(&self, cycle_number: u32) {
        let mut position = self.position();
        position.cycle = cycle_number;
        position.cycle_start = Instant::now();
        position.tests_done = 0;
    }

    /// `number` is the test's 1-based position in the cycle; `time_limit` is the test's own
    /// duration limit, if it has one; `publishes` is whether the test reports live figures.
    /// Must be called before the test is dispatched: it zeroes the workers' live slots, which is
    /// only safe while they are idle.
    pub(crate) fn start_test(
        &self,
        number: usize,
        name: &str,
        start: Instant,
        time_limit: Option<Duration>,
        publishes: bool,
    ) {
        let mut position = self.position();
        if let Some(plan) = self.suite.get() {
            for slot in plan.workers.iter() {
                slot.reset();
            }
        }
        position.test = Some(RunningTest {
            number: number as u64,
            name: name.to_string(),
            start,
            time_limit,
            publishes,
            finished: false,
        });
    }

    /// Record a finished test's errors, reported by its workers' results.
    pub(crate) fn complete_test(&self, errors: u64) {
        let mut position = self.position();
        position.tests_done += 1;
        position.errors += errors;
        if let Some(test) = position.test.as_mut() {
            test.finished = true;
        }
    }

    /// Errors a final seal check found (TODO 74): counted for the run, no test finished.
    pub(crate) fn add_seal_errors(&self, errors: u64) {
        self.add_errors(errors);
    }

    /// Errors found outside a finished test: counted for the run, no test finished. Where they
    /// were is the final summary's to say (its Errors by Step table).
    pub(crate) fn add_errors(&self, errors: u64) {
        self.position().errors += errors;
    }

    /// Tests finished in the current cycle (reset at each cycle's start).
    pub(crate) fn tests_done(&self) -> u64 {
        self.position().tests_done
    }

    /// Thread-reported errors from every finished test.
    pub(crate) fn total_errors(&self) -> u64 {
        self.position().errors
    }

    /// The suite is over: the monitor thread makes its frontend's last call (`ticks_done`) and
    /// exits. There is no final ticker line, because the final summary reports the same figures.
    pub(crate) fn finish(&self) {
        self.finished.store(true, Ordering::Relaxed);
    }

    /// Where the suite is now, or `None` before it has begun: one read of the position, under its
    /// lock, and of the workers' live figures. The running test's live errors are 0 once its
    /// results are in, as they are then in `finished_errors`.
    pub fn snapshot(&self) -> Option<ProgressSnapshot> {
        let plan = self.suite.get()?;
        let position = self.position();

        // The running test's figures, while there is one. After it finishes they stay readable
        // until the next test starts, but its errors are then in `position.errors` instead.
        let test = position.test.as_ref().map(|test| TestSnapshot {
            number: test.number,
            name: test.name.clone(),
            elapsed: test.start.elapsed(),
            time_limit: test.time_limit,
            finished: test.finished,
            publishes: test.publishes,
            live: live_figures(&plan.workers),
        });
        let live_errors = test.as_ref().filter(|test| !test.finished).map_or(0, |test| test.live.errors);
        Some(ProgressSnapshot {
            runtime: plan.start.elapsed(),
            time_limit: plan.time_limit,
            cycle: position.cycle,
            cycle_limit: plan.cycle_limit,
            cycle_elapsed: position.cycle_start.elapsed(),
            tests_done: position.tests_done,
            tests_per_cycle: plan.tests_per_cycle,
            finished_errors: position.errors,
            live_errors,
            whea: self.whea.counts(),
            test,
        })
    }

    /// What WHEA reported since the last call, one line each: they name the failing component,
    /// which is what tells a marginal DIMM from a marginal NIC. The monitor thread's poll queues
    /// them (`whea.rs`).
    pub fn take_whea_descriptions(&self) -> Vec<String> {
        self.whea.take_pending()
    }
}

/// Sum the workers' last published figures. Workers publish at most every 250 ms, at a cycle
/// boundary of their own loop, so this lags the work by up to one publish.
fn live_figures(workers: &[TestProgress]) -> LiveFigures {
    let mut live = LiveFigures {
        stage: workers.iter()
            .map(|slot| crate::tests::Stage::from_u8(slot.stage.load(Ordering::Relaxed)))
            .find(|&stage| stage != crate::tests::Stage::Testing),
        ..Default::default()
    };
    for slot in workers {
        let ms = slot.last_update_ms.load(Ordering::Relaxed);
        if ms == 0 {
            continue; // this worker has not published yet
        }
        let bytes = slot.bytes_processed.load(Ordering::Relaxed);
        live.published = true;
        live.bytes += bytes;
        live.errors += slot.errors_found.load(Ordering::Relaxed);
        // Each worker's own average since the test began, as of its publish. Summing these rather
        // than dividing the summed bytes by the wall clock keeps the rate from sagging between
        // publishes.
        live.bytes_per_sec += bytes as f64 * 1000.0 / ms as f64;
    }
    live
}

/// The engine's monitor thread while a suite runs (TODO 29): every 500 ms it drains what WHEA
/// queued, then hands `fe` its tick, which the CLI draws its ticker on; when the suite is over,
/// `fe`'s last call. The WHEA drain is the engine's, whatever the frontend draws.
pub fn monitor(progress: Arc<ProgressTracker>, fe: Arc<dyn Frontend>) {
    loop {
        thread::sleep(Duration::from_millis(500));

        // Drain any WHEA events the OS queued for us. Costs a single zero-timeout wait on a local
        // event handle when nothing has happened, so it is safe to do on every 500ms tick — the
        // descriptions are cached and only printed when the frontend draws (TODO 63).
        progress.whea.poll();

        if progress.finished.load(Ordering::Relaxed) {
            break;
        }
        fe.tick(&progress);
    }
    // The poll above ran on this tick, so the frontend gets everything the OS queued until now
    fe.ticks_done(&progress);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::Stage;

    /// A snapshot as a frontend reads it (TODO 29): nothing before the suite begins; while a test
    /// runs, the figures and errors of its workers that have published, live, apart from the
    /// finished tests' errors; once its results are in, its errors with the finished tests' and
    /// none live, its figures still readable until the next test starts.
    #[test]
    fn a_snapshot_has_a_tests_errors_live_until_its_results_are_in() {
        let ago = |secs: u64| Instant::now().checked_sub(Duration::from_secs(secs)).expect("a clock that has run a minute");
        let progress = ProgressTracker::new();
        assert!(progress.snapshot().is_none(), "before the suite begins");
        let workers: Arc<[TestProgress]> = Arc::from([TestProgress::new(), TestProgress::new()]);
        progress.begin_suite(ago(60), Some(2), Some(Duration::from_secs(600)), 4, Arc::clone(&workers));
        progress.start_new_cycle(1);
        progress.start_test(3, "Mem-StuckBit256", ago(5), Some(Duration::from_secs(60)), true);
        // The first worker has published 3 errors in 1 MiB half a second in; the second, checking
        // the seal, nothing yet, so its errors aren't read
        workers[0].bytes_processed.store(1 << 20, Ordering::Relaxed);
        workers[0].errors_found.store(3, Ordering::Relaxed);
        workers[0].last_update_ms.store(500, Ordering::Relaxed);
        workers[1].errors_found.store(5, Ordering::Relaxed);
        workers[1].set_stage(Stage::CheckingSeal);

        let running = progress.snapshot().expect("the suite has begun");
        assert_eq!((running.cycle, running.cycle_limit, running.time_limit, running.tests_done, running.tests_per_cycle),
                   (1, Some(2), Some(Duration::from_secs(600)), 0, 4));
        assert_eq!((running.finished_errors, running.live_errors, running.whea.total), (0, 3, 0));
        let test = running.test.as_ref().expect("a test is running");
        assert_eq!((test.number, test.name.as_str(), test.time_limit, test.finished, test.publishes),
                   (3, "Mem-StuckBit256", Some(Duration::from_secs(60)), false, true));
        assert!(running.runtime >= Duration::from_secs(60) && (Duration::from_secs(5)..running.runtime).contains(&test.elapsed),
                "the suite's time and the test's own: {:?}, {:?}", running.runtime, test.elapsed);
        assert_eq!((test.live.published, test.live.bytes, test.live.errors, test.live.stage), (true, 1 << 20, 3, Some(Stage::CheckingSeal)));
        assert_eq!(test.live.bytes_per_sec, 2.0 * (1 << 20) as f64, "1 MiB in half a second");

        progress.complete_test(3);
        let finished = progress.snapshot().expect("the suite has begun");
        assert_eq!((finished.tests_done, finished.finished_errors, finished.live_errors), (1, 3, 0));
        let test = finished.test.as_ref().expect("the test that finished");
        assert_eq!((test.finished, test.live.errors), (true, 3));
    }
}
