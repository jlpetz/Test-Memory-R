use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};
use std::thread;

use crate::constants::{BYTES_PER_GIB_F64, BYTES_PER_MIB_F64};
use crate::tests::TestProgress;

// Live suite position for the progress ticker, plus the run's error tallies. Per-cycle and per-test
// results are not kept here: `TestRunResult` (results.rs) owns those, and the final summary and
// the JSON file are both built from it.
pub struct ProgressTracker {
    // What the suite was asked to do, fixed by `begin_suite`.
    suite: OnceLock<SuitePlan>,
    position: Mutex<Position>,
    finished: AtomicBool,

    // Hardware errors the OS saw that our verify reads cannot (DDR5 on-die ECC corrections, and
    // any fault landing during a non-verifying test like bandwidth/latency). Counted separately
    // from the thread-reported errors because these are OS-reported. See whea.rs.
    pub whea: crate::whea::WheaMonitor,
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
    errors_by_test: HashMap<String, u64>,
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

#[derive(Debug, Clone)]
pub struct TestSummary {
    /// The step's 1-based position in the cycle (TODO 74)
    pub step: usize,
    /// Which of the plan's tests it ran: steps of one test share it
    pub test_index: usize,
    /// The test's config id (a TM5 import's test number)
    pub id: Option<String>,
    /// The step's name in reports, e.g. `Mem-SimpleV2_A (Test 12)`
    pub label: String,
    /// What the seal checks before its chunks found (TODO 74): not the test's errors
    pub seal_errors: u64,
    pub name: String,
    pub duration_ms: u128,
    pub bytes_processed: u64,
    pub throughput_mib_s: f64,
    pub errors: u64,
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
}

/// The running test's figures, summed over the workers that have published.
#[derive(Default)]
struct LiveFigures {
    published: bool,
    bytes: u64,
    errors: u64,
    bytes_per_sec: f64,
    /// What the first worker not just testing is doing (TODO 74), e.g. "Checking seal"
    stage: Option<crate::tests::Stage>,
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
                errors_by_test: HashMap::new(),
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
    pub fn begin_suite(
        &self,
        start: Instant,
        cycle_limit: Option<u32>,
        time_limit: Option<Duration>,
        tests_per_cycle: u64,
        workers: Arc<[TestProgress]>,
    ) {
        let _ = self.suite.set(SuitePlan { start, cycle_limit, time_limit, tests_per_cycle, workers });
    }

    pub fn start_new_cycle(&self, cycle_number: u32) {
        let mut position = self.position();
        position.cycle = cycle_number;
        position.cycle_start = Instant::now();
        position.tests_done = 0;
    }

    /// `number` is the test's 1-based position in the cycle; `time_limit` is the test's own
    /// duration limit, if it has one; `publishes` is whether the test reports live figures.
    /// Must be called before the test is dispatched: it zeroes the workers' live slots, which is
    /// only safe while they are idle.
    pub fn start_test(
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
    pub fn complete_test(&self, test_name: &str, errors: u64) {
        let mut position = self.position();
        position.tests_done += 1;
        position.errors += errors;
        if errors > 0 {
            *position.errors_by_test.entry(test_name.to_string()).or_insert(0) += errors;
        }
        if let Some(test) = position.test.as_mut() {
            test.finished = true;
        }
    }

    /// Errors a final seal check found (TODO 74): counted for the run, no test finished.
    pub fn add_seal_errors(&self, errors: u64) {
        let mut position = self.position();
        position.errors += errors;
        *position.errors_by_test.entry("Final seal check".to_string()).or_insert(0) += errors;
    }

    /// Tests finished in the current cycle (reset at each cycle's start).
    pub fn tests_done(&self) -> u64 {
        self.position().tests_done
    }

    /// Thread-reported errors from every finished test.
    pub fn total_errors(&self) -> u64 {
        self.position().errors
    }

    /// The suite is over: the reporter thread takes the ticker down and exits. There is no final
    /// ticker line, because the final summary reports the same figures.
    pub fn finish(&self) {
        self.finished.store(true, Ordering::Relaxed);
    }

    /// The ticker, or `None` before the suite has begun. It starts with a blank line, which sets
    /// it off from the output above, and has two lines under that:
    ///
    /// ```text
    ///
    /// Runtime: 00:41:10 (remaining 00:18:50) | Cycle 4/8 (00:12:34, 40%) | 0 errors + 0 WHEA
    /// Test 2/6 Mem-Refresh128 (00:05:12, 53%) | 285.00 GiB @ 19108.7 MiB/s (18.66 GiB/s)
    /// ```
    ///
    /// Each bracketed part appears only when the matching limit exists. Errors are always shown,
    /// the running test's live ones included; the WHEA C/UC split only when there is one.
    fn render(&self) -> Option<String> {
        let plan = self.suite.get()?;
        let position = self.position();

        // The running test's figures, while there is one. After it finishes they stay readable
        // until the next test starts, but its errors are then in `position.errors` instead.
        let live = position.test.as_ref().map(|_| live_figures(&plan.workers));
        let live_errors = match (&position.test, &live) {
            (Some(test), Some(live)) if !test.finished => live.errors,
            _ => 0,
        };
        let mut line = format!("\n{}", self.suite_line(plan, &position, live_errors));

        if let (Some(test), Some(live)) = (&position.test, live) {
            let _ = write!(line, "\nTest {}/{} {} ({}", test.number, plan.tests_per_cycle, test.name, hms(test.start.elapsed()));
            if let Some(limit) = test.time_limit {
                let _ = write!(line, ", {}%", percent(test.start.elapsed().as_millis(), limit.as_millis()));
            }
            line.push(')');
            if let Some(stage) = live.stage {
                let _ = write!(line, " | {}", stage.label());
            }
            if live.published {
                let _ = write!(
                    line,
                    " | {:.2} GiB @ {:.1} MiB/s ({:.2} GiB/s)",
                    live.bytes as f64 / BYTES_PER_GIB_F64,
                    live.bytes_per_sec / BYTES_PER_MIB_F64,
                    live.bytes_per_sec / BYTES_PER_GIB_F64,
                );
            } else if test.publishes {
                line.push_str(" | pending");
            } else {
                line.push_str(" | no live data");
            }
        }
        Some(line)
    }

    /// The ticker's first line, then the test that just ended. It is the last line of that test's
    /// report, so a record of the suite's progress stays on screen after the ticker is gone:
    ///
    /// ```text
    /// Runtime: 00:03:03 | Cycle 2/2 (00:01:31, 100%) | 0 errors + 0 WHEA | Test 6/6 Mem-Refresh512 (00:00:15)
    /// ```
    ///
    /// Call after `complete_test`, so the error total includes this test. `duration` is the test's
    /// own measured time, the one its report shows. `None` before the first test.
    pub fn test_stamp(&self, duration: Duration) -> Option<String> {
        let plan = self.suite.get()?;
        let position = self.position();
        let test = position.test.as_ref()?;
        let mut line = self.suite_line(plan, &position, 0);
        let _ = write!(line, " | Test {}/{} {} ({})", test.number, plan.tests_per_cycle, test.name, hms(duration));
        Some(line)
    }

    /// `Runtime | Cycle | errors`: the ticker's first line, and the start of each test's stamp.
    /// `live_errors` is what the running test has found so far, not yet in `position.errors`.
    fn suite_line(&self, plan: &SuitePlan, position: &Position, live_errors: u64) -> String {
        let tests = plan.tests_per_cycle;
        let cycle = u64::from(position.cycle);

        let runtime = plan.start.elapsed();
        let mut line = format!("Runtime: {}", hms(runtime));
        if let Some(limit) = plan.time_limit {
            match limit.checked_sub(runtime) {
                Some(left) if !left.is_zero() => {
                    let _ = write!(line, " (remaining {})", hms(left));
                }
                _ => line.push_str(" (limit reached, finishing cycle)"),
            }
        }

        match plan.cycle_limit {
            Some(limit) => {
                let _ = write!(line, " | Cycle {}/{} ({}", cycle, limit, hms(position.cycle_start.elapsed()));
                // Whole-run completion, counting finished tests only: Cycle 4/8 with 1 of 6 tests
                // done is (3*6 + 1) / (8*6) = 40%.
                let done = cycle.saturating_sub(1) * tests + position.tests_done;
                let _ = write!(line, ", {}%)", percent(done as u128, u128::from(limit) * tests as u128));
            }
            None => {
                let _ = write!(line, " | Cycle {} ({})", cycle, hms(position.cycle_start.elapsed()));
            }
        }

        // Same form as the per-test topline: always shown, the C/UC split only when non-zero.
        let errors = position.errors + live_errors;
        let whea = self.whea.counts();
        let flag = if errors > 0 || whea.total > 0 { "⚠️ " } else { "" };
        let _ = write!(line, " | {}{} errors + {} WHEA{}", flag, errors, whea.total, whea.split_suffix());
        line
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

/// `part` as a whole percentage of `whole`, capped at 100; 0 when `whole` is 0.
fn percent(part: u128, whole: u128) -> u128 {
    (part * 100).checked_div(whole).map_or(0, |p| p.min(100))
}

fn hms(d: Duration) -> String {
    let secs = d.as_secs();
    format!("{:02}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60)
}

pub fn progress_reporter(progress: Arc<ProgressTracker>) {
    let mut last_draw = Instant::now();

    loop {
        thread::sleep(Duration::from_millis(500));

        // Drain any WHEA events the OS queued for us. Costs a single zero-timeout wait on a local
        // event handle when nothing has happened, so it is safe to do on every 500ms tick — the
        // descriptions are cached and only printed at the display interval below (TODO #63).
        progress.whea.poll();

        if progress.finished.load(Ordering::Relaxed) {
            break;
        }

        // Redraw every 2 seconds.
        if last_draw.elapsed() < Duration::from_secs(2) {
            continue;
        }
        // `None` until the suite has begun.
        let Some(ticker) = progress.render() else {
            continue;
        };

        // Anything WHEA reported since the last draw is printed above the ticker. These name the
        // failing component, which is what tells a marginal DIMM from a marginal NIC. While a test
        // report holds the console this draws nothing and leaves the WHEA events queued, so try
        // again at the next tick.
        if crate::console::draw_ticker(ticker, || progress.whea.take_pending()) {
            last_draw = Instant::now();
        }
    }

    // Take the ticker down for good. Everything from here on prints with plain `println!`.
    crate::console::hold();

    // Events the OS queued since the last draw (the poll above ran on this tick), not printed yet.
    // Events already shown live are not reprinted: they are in the log file. No run tally here
    // either: the final summary's `Total WHEA` row prints it a screen later, and saying it twice is
    // noise. The runner flushes anything later still, once monitoring stops.
    for event in progress.whea.take_pending() {
        println!("{}", event);
    }

    // Print per-test error summary if there were any errors
    let error_summary = progress.position().errors_by_test.clone();
    if !error_summary.is_empty() {
        println!("\nError Summary by Test:");
        for (test_name, error_count) in error_summary {
            println!("  {}: {} errors", test_name, error_count);
        }
    }
}
