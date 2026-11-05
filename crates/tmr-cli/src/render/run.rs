//! A run's events as the console shows them (TODO 29): each the print group it replaced, moved here from the engine's `runner.rs` and
//! `layout.rs` with its format strings.

use std::fmt::Write as _;

use tmr::frontend::{CpuPerformanceSummary, NotRun, PageSizeLevel, Run, RunOutcome, RunStatus, SealOp, StepReport};

use super::{report_text, ticker};
use crate::console;

/// How the console writes an event's text: before the suite and after it with a plain `console::print`, a line while the ticker can be up
/// above it (`console::print_above`), a step's report as one block under it (`console::print_block`)
pub(super) enum Written {
    Plain,
    Above,
    Block,
}

/// Shows `run`, with the write primitive its prints had, and returns what it showed (the transcript's, TODO 109)
pub(super) fn show(run: &Run) -> String {
    let (text, written) = text(run);
    match written {
        Written::Plain => console::print(&text),
        Written::Above => console::print_above(&text),
        Written::Block => console::print_block(&text),
    }
    text
}

/// The run's threads and the settings that set them (TODO 7, the owner: which setting controls which): the workers as they started,
/// their count and priority, and their pinning, or each that didn't take, by thread and CPU; the monitor, at the workers' priority; and
/// the engine's thread, at normal. The window adds its own line (`tmr-gui`'s `follow_priority`).
fn workers(report: &tmr::frontend::WorkersReport) -> String {
    let n = report.starts.len();
    let priority = format!("{} priority ({})", report.priority, report.priority.win32_name());
    let unpinned: Vec<String> = report
        .starts
        .iter()
        .filter_map(|s| match &s.pinned {
            Some(Err(e)) => Some(format!("thread {} (CPU {}): {}", s.thread_id, s.cpu_id, e.trim_end_matches('.'))),
            _ => None,
        })
        .collect();
    let unprioritised: Vec<String> = report
        .starts
        .iter()
        .filter_map(|s| s.priority.as_ref().err().map(|e| format!("thread {}: {}", s.thread_id, e.trim_end_matches('.'))))
        .collect();
    let pinning = match (report.pinning, unpinned.len()) {
        (false, _) => "not pinned (--disable-pinning)".to_string(),
        (true, 0) => "each pinned to its CPU".to_string(),
        (true, failed) => format!("{} of {n} pinned; not {}", n - failed, unpinned.join("; ")),
    };
    let mark = if unpinned.is_empty() && unprioritised.is_empty() { "  " } else { "⚠️" };
    let mut text = "⚙  Threads (priority= sets the workers and the monitor; --disable-pinning unpins the workers)\n".to_string();
    text += &format!(" {mark} Workers: {n} at {priority}, {pinning}\n");
    if !unprioritised.is_empty() {
        text += &format!(" ⚠️ Priority not set on {} of {n}: {}\n", unprioritised.len(), unprioritised.join("; "));
    }
    text += "    Monitor, the progress line and the WHEA poll: at the workers' priority\n";
    text += "    Engine, the steps between the workers' runs: normal\n";
    text
}

/// What the console shows for `run`, and how it writes it
pub(super) fn text(run: &Run) -> (String, Written) {
    let line = |text: String| text + "\n";
    match run {
        Run::MemoryPlan(report) => {
            (line(report_text("consolidated memory report", |r| r.report_consolidated_memory(report))), Written::Plain)
        }
        Run::NotRun(why) => (not_run(why), Written::Plain),
        Run::Matchable { tests } => (tests.iter().map(|test| format!("  - {}\n", test)).collect(), Written::Plain),
        Run::LeftOut { tests } => (left_out(tests), Written::Plain),
        Run::Channels { channels, cache_line } => (
            format!("⚙  Memory Channels: {} (affects SimpleTest stride: [channels × param - 1] × {}B cache line)\n", channels, cache_line),
            Written::Plain,
        ),
        Run::TestPlan(report) => (report_text("test configuration report", |r| r.report_test_configuration(report)), Written::Plain),
        Run::Workers(report) => (workers(report), Written::Plain),
        Run::WheaMonitoring(Ok(())) => (line("🔎 WHEA hardware-error monitoring: active".to_string()), Written::Plain),
        Run::WheaMonitoring(Err(reason)) => (line(format!("🔎 WHEA hardware-error monitoring: unavailable ({})", reason)), Written::Plain),
        Run::AllocationStarting { min, max } => (allocation_starting(*min, *max), Written::Plain),
        Run::Allocated(report) => (report_text("the allocation report", |r| r.report_allocation(report)), Written::Plain),
        Run::Calibrated(Ok(now)) => (calibrated(now), Written::Plain),
        Run::Rechecked(Ok(recheck)) => (recheck.text(), Written::Plain),
        Run::Rechecked(Err(e)) => (line(format!("⚠️ The cache calibration's recheck failed ({e}): measured again")), Written::Plain),
        Run::Calibrated(Err(e)) => (line(format!("⚠️ The cache calibration failed ({e}): the tests size from CPUID")), Written::Plain),
        Run::NoMemoryOnCalibrationNode { node, stored } => {
            (line(format!("⚠️ {}", tmr::frontend::no_memory_on_calibration_node(*node, *stored))), Written::Plain)
        }
        // The CLI reads the suite's progress on the monitor's ticks
        Run::SuiteStarted { progress: _, seed, transfer_bits, transfer_source, rescan } => (
            line(format!(
                "🎲 Pattern seed {seed:#018x} (seed= replays it), transfer width {}, rescan {} (rescan=)",
                transfer_bits
                    .map_or("unknown: switch alternates 64 and 32 bits".to_string(), |bits| format!("{bits} bits ({transfer_source})")),
                if *rescan { "on" } else { "off" }
            )),
            Written::Above,
        ),
        Run::CycleStarted { cycle, limit } => (
            line(match limit {
                Some(limit) => format!("\n🔄 Starting test cycle {} of {}", cycle, limit),
                None => format!("\n🔄 Starting test cycle {}", cycle),
            }),
            Written::Above,
        ),
        // No line of its own: the next cycle's start or the verdict follows
        Run::CycleFinished { .. } => (String::new(), Written::Plain),
        Run::SealStage { op, kernel, bytes, errors, bits, duration } => {
            let lead = match op {
                SealOp::SealAll => "🔒 Sealing memory",
                SealOp::FinalCheck => "🔒 Final seal check",
            };
            let gib = *bytes as f64 / (1024.0 * 1024.0 * 1024.0);
            let errors_info = if *op == SealOp::FinalCheck { format!("{errors} errors{}, ", bits.brief()) } else { String::new() };
            // The step report's form for an operation (TODO 90): time, data @ speed
            let text = format!(
                "{lead} ({}): {errors_info}{:.1}s, {gib:.2} GiB @ {:.0} MiB/s",
                kernel,
                duration.as_secs_f64(),
                gib * 1024.0 / duration.as_secs_f64().max(1e-9)
            );
            (line(text), Written::Above)
        }
        Run::StepFinished(report) => (step_report(report), Written::Block),
        Run::LateWhea(events) => (events.iter().map(|event| format!("{}\n", event)).collect(), Written::Plain),
        Run::CpuPerformance(summary) => (cpu_performance(summary), Written::Plain),
        Run::SuiteEnded { outcome, status, errors } => {
            let mut text = String::new();
            // Display completion status
            if *outcome == RunOutcome::Interrupted {
                // The console can't tell which stopped it: CTRL+C, the window's Stop, or the window closed
                text += "\n⚠️  Test suite interrupted before its end (CTRL+C, Stop or the window closed)\n";
            } else if *status == RunStatus::NotRun {
                text += "\n⚠️  No test ran: the cycle or time limit is 0\n";
            } else if *status == RunStatus::Passed {
                text += "\n✅ All test cycles completed successfully!\n";
            }
            if *status == RunStatus::Failed {
                // Same form as the per-test topline. Either count alone fails the run: a WHEA event with `0 errors` is a fault that ECC
                // corrected, or one raised during a test phase that does not verify — still not a stable configuration.
                text += &format!("\n❌ Test suite failed: {errors}\n");
            }
            (text, Written::Plain)
        }
        Run::FinalSummary(report) => (report_text("final summary", |r| r.report_final_summary(report)), Written::Plain),
        // The log file beside it, when there is one (TODO 105): the two files that keep the run
        Run::ResultsSaved { path } => {
            let log = if tmr::logfile::is_open() { format!("📝 Log file: {}\n", tmr::logfile::path().display()) } else { String::new() };
            (format!("💾 Results saved: {path}\n{log}"), Written::Plain)
        }
    }
}

/// Why the run stopped before testing.
fn not_run(why: &NotRun) -> String {
    let mut text = String::new();
    match why {
        NotRun::Config(e) => {
            let _ = writeln!(text, "❌ Config error: {}", e);
        }
        NotRun::Plan(e) => {
            let _ = writeln!(text, "❌ {e}");
        }
        // A filter narrows a named plan: the list (`Run::Matchable`) is what it could have picked from
        NotRun::NoTestMatches { filter, plan: Some(plan) } => {
            let _ = writeln!(text, "❌ No test of plan {} matches filter '{}'. Its tests (plan=all for every test):", plan, filter);
        }
        NotRun::NoTestMatches { filter, plan: None } => {
            let _ = writeln!(text, "❌ No tests match filter '{}'. Available tests:", filter);
        }
        NotRun::NothingLeftToRun => text.push_str("❌ Nothing left to run\n"),
        NotRun::Allocation(e) => {
            let _ = writeln!(text, "❌ Failed to allocate memory blocks: {}\n", e);
        }
    }
    text
}

/// The tests left out, a line a reason (a 512-bit test without AVX-512, the tile test without AMX-INT8), the first few by name: the log
/// lists them all.
fn left_out(tests: &[(String, String)]) -> String {
    const SHOWN: usize = 5;
    let mut reasons: Vec<&str> = Vec::new();
    for (_, lacks) in tests {
        if !reasons.contains(&lacks.as_str()) {
            reasons.push(lacks);
        }
    }
    reasons
        .iter()
        .map(|&lacks| {
            let names: Vec<&str> = tests.iter().filter(|(_, why)| why == lacks).map(|(name, _)| name.as_str()).collect();
            let more = names.len().saturating_sub(SHOWN);
            let more = if more > 0 { format!(" and {more} more (the log lists them)") } else { String::new() };
            let shown = names[..names.len().min(SHOWN)].join(", ");
            format!("ℹ  Left out {} test(s) this CPU can't run ({lacks}): {shown}{more}\n", names.len())
        })
        .collect()
}

/// Allocation's first line, and the page sizes it may take.
fn allocation_starting(min: PageSizeLevel, max: PageSizeLevel) -> String {
    let format_size = |s: PageSizeLevel| -> &'static str {
        match s {
            PageSizeLevel::Regular => "Regular (4KB)",
            PageSizeLevel::Large => "Large (2MB)",
            PageSizeLevel::Huge => "Huge (1GB)",
        }
    };

    let mut text = "Stage 1: Pre-allocating memory blocks...\n".to_string();
    if min == max {
        // Restrictive: only one page size allowed
        let _ = writeln!(text, "⚠️  Page Size Restricted: {} only - may limit available memory", format_size(min));
    } else if min == PageSizeLevel::Large && max == PageSizeLevel::Huge {
        // Default constraints
        let _ = writeln!(text, "⚙️  Page Size Constraints: {} to {} (default)", format_size(min), format_size(max));
    } else {
        // Custom constraints
        let _ = writeln!(text, "⚙️  Page Size Constraints: {} to {}", format_size(min), format_size(max));
    }
    text
}

/// A step's report: its verdict and figures, its config, its threads' table (and a latency step's latencies), and where the suite stood as
/// it ended, then a blank line before the next.
fn step_report(report: &StepReport) -> String {
    let StepReport { cycle, summary: s, wall, timed_secs, cycle_limit, stopped_by_time, sealed: _, config, threads, latency, stamp } =
        report;
    let cycles_completed = s.cycles_completed.1;

    // Format cycle info for logging
    let cycles_info = if cycles_completed > 0 || cycle_limit.is_some() {
        let limit_str = cycle_limit.map_or("∞".to_string(), |l| l.to_string());

        // Cycle limit indicator: RED if cycle limit caused stop, GREEN if didn't reach it
        let stop_indicator_cycles = if let Some(limit) = *cycle_limit {
            if cycles_completed >= limit { "🔴" } else { "🟢" } // RED when hit, GREEN when not
        } else {
            "🟢" // No limit = always green (can't hit what doesn't exist)
        };

        // Time limit indicator: red if time limit stopped us, green otherwise
        let stop_indicator_time = if *stopped_by_time { "🔴" } else { "🟢" };

        format!(", Cycles {} {}/{}, Time Limit {}", stop_indicator_cycles, cycles_completed, limit_str, stop_indicator_time)
    } else {
        String::new()
    };

    // Format operations in human-readable form (B/M/K notation)
    let format_ops = |ops: u64| -> String {
        if ops >= 1_000_000_000 {
            format!("{:.2}B", ops as f64 / 1_000_000_000.0)
        } else if ops >= 1_000_000 {
            format!("{:.2}M", ops as f64 / 1_000_000.0)
        } else if ops >= 1_000 {
            format!("{:.2}K", ops as f64 / 1_000.0)
        } else {
            format!("{}", ops)
        }
    };

    // Calculate operations per second
    let ops_per_sec = if *timed_secs > 0.0 { s.operations as f64 / timed_secs } else { 0.0 };

    // The consolidated P50 and spread of a latency step; a loaded latency test's headline, its rows being a load each (TODO 117)
    let latency_info = match (s.loaded.as_deref(), s.latency_p50_ns, s.latency_spread) {
        (Some(loaded), _, _) => format!(", {}", loaded.headline()),
        (None, Some(p50), Some(spread)) => format!(", P50: {:.1}ns, Spread: {:.2}x", p50, spread),
        _ => String::new(),
    };

    // Pass/fail for this test. Both error sources count: a WHEA event during the test means the hardware reported a fault whether or not a
    // verify read caught it, and either way the system is not stable. This is the only per-test verdict in the output.
    //
    // The ✅/❌ here is a verdict; the 🔴/🟢 in `cycles_info` is not — those mark *which limit ended the test*, so the two glyph pairs are
    // deliberately different.
    // The test's, in a sealed run the seal's (what the checks before this step and its chunks found), and WHEA's, in the one form (TODO
    // 109): WHEA always shown, even at zero, so a clean line still states that we were watching, its C/UC split only when there is
    // something to split
    let errors = report.errors();
    let pass_indicator = if errors.total() == 0 { "✅" } else { "❌" };

    // The step's verdict and wall time, the bits that failed on a line of their own (TODO 109: after each count they cluttered it), then
    // its test's own figures, the work around it by operation (TODO 90, `n/a` where it did none), and its config
    let mut text = String::new();
    let _ = writeln!(
        text,
        "📊 Test report - Cycle {} - Step {} {}: {} {:.1}s, {}{}",
        cycle,
        s.step,
        s.label,
        pass_indicator,
        wall.as_secs_f64(),
        errors,
        cycles_info
    );
    if let Some(bits) = report.failing_bits_line() {
        let _ = writeln!(text, "   {bits}");
    }
    let _ = writeln!(
        text,
        "   Test: {:.1}s, {:.2} GiB @ {:.0} MiB/s, {} ops @ {} ops/s{}",
        timed_secs,
        s.bytes_processed as f64 / (1024.0 * 1024.0 * 1024.0),
        s.throughput_mib_s,
        format_ops(s.operations),
        format_ops(ops_per_sec as u64),
        latency_info
    );
    let _ = writeln!(text, "   Intermediate: {}", s.ops.line());

    let reps_str = if config.test_reps != 1 || config.verify_reps != 1 || config.write_read_cycles != 1 {
        format!(", test_reps={}, verify_reps={}, wrc={}", config.test_reps, config.verify_reps, config.write_read_cycles)
    } else {
        String::new()
    };
    let _ = writeln!(
        text,
        "   Config: parameter={}, mode={}, extent={}, chunk={}{}",
        config.parameter, config.mode, config.extent, config.chunk, reps_str
    );

    // Thread timing deviations, and a latency step's latencies: a loaded latency test's by load, its timing table its loaders', none when
    // it has none (TODO 117)
    if s.loaded.is_none() || !threads.thread_timings.is_empty() {
        text += &report_text("thread timing report", |r| r.report_thread_timing(threads));
    }
    if let Some(loaded) = s.loaded.as_deref() {
        text += &report_text("loaded latency report", |r| r.report_loaded(loaded));
    } else if let Some(latency) = latency {
        text += &report_text("latency report", |r| r.report_latency_summary(latency));
    }

    // Where the suite stood as this test ended. The ticker is down during every report and gone after the run, so this is what stays on
    // screen: one line per test.
    if let Some(stamp) = stamp.as_ref().and_then(|stamp| ticker::test_stamp(stamp, *wall)) {
        let _ = writeln!(text, "{}", stamp);
    }
    // Blank line to separate from the next test
    text.push('\n');
    text
}

/// The run's work by thread, by CPU and by physical core.
fn cpu_performance(summary: &CpuPerformanceSummary) -> String {
    let mut reporter = crate::renderer::create_console_reporter();
    if !summary.by_thread.threads.is_empty() {
        let _ = reporter.report_performance_by_thread(&summary.by_thread);
    }
    if !summary.by_cpu.cpus.is_empty() {
        let _ = reporter.report_performance_by_cpu(&summary.by_cpu);
    }
    if !summary.by_core.cores.is_empty() {
        let _ = reporter.report_performance_by_physical_core(&summary.by_core);
    }
    format!("\n=== CPU Performance Summary ===\n{}", reporter.into_renderer().into_text())
}

/// The cache calibration the run measured before its tests (TODO 22): its table, then where it was saved
fn calibrated(now: &tmr::frontend::CalibratedNow) -> String {
    let mut text = now.table.clone();
    match &now.saved {
        Ok(path) => text.push_str(&format!("💾 Calibration saved to {}\n", path.display())),
        Err(e) => text.push_str(&format!("⚠️ Calibration not saved: {e}\n")),
    }
    text
}
