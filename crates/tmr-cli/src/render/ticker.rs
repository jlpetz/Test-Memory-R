//! The ticker as the CLI draws it, and the stamp each step's report ends with (TODO 29: moved from `progress.rs`, as functions of a
//! `ProgressSnapshot`).

use std::fmt::Write as _;
use std::time::Duration;

use tmr::frontend::{ErrorCounts, ProgressSnapshot};

/// Bytes in a MiB and a GiB, for the ticker's figures (the engine's `constants` has the GiB's too)
const BYTES_PER_MIB_F64: f64 = 1024.0 * 1024.0;
const BYTES_PER_GIB_F64: f64 = 1024.0 * 1024.0 * 1024.0;

/// The ticker. It starts with a blank line, which sets it off from the output above, and has two lines under that:
///
/// ```text
///
/// Runtime: 00:41:10 (remaining 00:18:50) | Cycle 4/8 (00:12:34, 40%) | 0 errors + 0 WHEA
/// Test 2/6 Mem-Refresh128 (00:05:12, 53%) | 285.00 GiB @ 19108.7 MiB/s (18.66 GiB/s)
/// ```
///
/// Each bracketed part appears only when the matching limit exists. Errors are always shown, the running test's live ones included; the
/// WHEA C/UC split only when there is one.
pub(super) fn render(snapshot: &ProgressSnapshot) -> String {
    let mut line = format!("\n{}", suite_line(snapshot, snapshot.errors()));

    if let Some(test) = &snapshot.test {
        let live = &test.live;
        let _ = write!(line, "\nTest {}/{} {} ({}", test.number, snapshot.tests_per_cycle, test.name, hms(test.elapsed));
        if let Some(limit) = test.time_limit {
            let _ = write!(line, ", {}%", percent(test.elapsed.as_millis(), limit.as_millis()));
        }
        line.push(')');
        if let Some(stage) = live.stage {
            let _ = write!(line, " | {}", stage.label());
            // Mem-Refresh's wait (TODO 112): a bit fade's is minutes
            if let Some(left) = live.wait_left {
                let _ = write!(line, ", {} left", hms(left));
            }
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
    line
}

/// The ticker's first line, then the test that just ended. It is the last line of that test's report, so a record of the suite's progress
/// stays on screen after the ticker is gone:
///
/// ```text
/// Runtime: 00:03:03 | Cycle 2/2 (00:01:31, 100%) | 0 errors + 0 WHEA | Test 6/6 Mem-Refresh512 (00:00:15)
/// ```
///
/// From a snapshot taken after the test's results are in, so the error total includes this test. `duration` is the test's own measured
/// time, the one its report shows. `None` before the first test.
pub(super) fn test_stamp(snapshot: &ProgressSnapshot, duration: Duration) -> Option<String> {
    let test = snapshot.test.as_ref()?;
    let mut line = suite_line(snapshot, snapshot.errors_of_finished_tests());
    let _ = write!(line, " | Test {}/{} {} ({})", test.number, snapshot.tests_per_cycle, test.name, hms(duration));
    Some(line)
}

/// `Runtime | Cycle | errors`: the ticker's first line, and the start of each test's stamp: `errors`, the ticker's the running test's too,
/// a stamp's the finished tests' alone.
fn suite_line(snapshot: &ProgressSnapshot, errors: ErrorCounts) -> String {
    let tests = snapshot.tests_per_cycle;
    let cycle = u64::from(snapshot.cycle);

    let runtime = snapshot.runtime;
    let mut line = format!("Runtime: {}", hms(runtime));
    if let Some(limit) = snapshot.time_limit {
        match limit.checked_sub(runtime) {
            Some(left) if !left.is_zero() => {
                let _ = write!(line, " (remaining {})", hms(left));
            }
            _ => line.push_str(" (limit reached, finishing cycle)"),
        }
    }

    match snapshot.cycle_limit {
        Some(limit) => {
            let _ = write!(line, " | Cycle {}/{} ({}", cycle, limit, hms(snapshot.cycle_elapsed));
            // Whole-run completion, counting finished tests only: Cycle 4/8 with 1 of 6 tests done is (3*6 + 1) / (8*6) = 40%.
            let done = cycle.saturating_sub(1) * tests + snapshot.tests_done;
            let _ = write!(line, ", {}%)", percent(done as u128, u128::from(limit) * tests as u128));
        }
        None => {
            let _ = write!(line, " | Cycle {} ({})", cycle, hms(snapshot.cycle_elapsed));
        }
    }

    // The one form, the per-test topline's too (TODO 109): always shown, the test and seal parts and the C/UC split only when non-zero
    let flag = if errors.total() > 0 { "⚠️ " } else { "" };
    let _ = write!(line, " | {flag}{errors}");
    line
}

/// `part` as a whole percentage of `whole`, capped at 100; 0 when `whole` is 0.
fn percent(part: u128, whole: u128) -> u128 {
    (part * 100).checked_div(whole).map_or(0, |p| p.min(100))
}

fn hms(d: Duration) -> String {
    let secs = d.as_secs();
    format!("{:02}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60)
}
