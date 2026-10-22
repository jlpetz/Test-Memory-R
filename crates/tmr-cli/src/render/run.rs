//! A run's events as the console shows them (TODO 29): each the print group it replaced, moved here
//! from the engine's `runner.rs` and `layout.rs` with its format strings.

use std::fmt::Write as _;

use tmr::frontend::{CpuPerformanceSummary, NotRun, PageSizeLevel, Run, RunOutcome, RunStatus, SealOp, StepReport, WheaCounts};

use super::{report_text, ticker};
use crate::console;

/// Shows `run`, with the write primitive its prints had.
pub(super) fn show(run: Run) {
    match run {
        Run::MemoryPlan(report) => {
            println!("{}", report_text("consolidated memory report", |r| r.report_consolidated_memory(&report)));
        }
        Run::NotRun(why) => print!("{}", not_run(&why)),
        Run::Matchable { tests } => {
            for test in tests {
                println!("  - {}", test);
            }
        }
        Run::LeftOut { tests } => print!("{}", left_out(&tests)),
        Run::Channels { channels, cache_line } => {
            println!("⚙  Memory Channels: {} (affects SimpleTest stride: [channels × param - 1] × {}B cache line)",
                channels, cache_line);
        }
        Run::TestPlan(report) => {
            print!("{}", report_text("test configuration report", |r| r.report_test_configuration(&report)));
        }
        Run::WheaMonitoring(Ok(())) => println!("🔎 WHEA hardware-error monitoring: active"),
        Run::WheaMonitoring(Err(reason)) => println!("🔎 WHEA hardware-error monitoring: unavailable ({})", reason),
        Run::AllocationStarting { min, max } => print!("{}", allocation_starting(min, max)),
        Run::Allocated(report) => print!("{}", report_text("the allocation report", |r| r.report_allocation(&report))),
        // The CLI reads the suite's progress on the monitor's ticks
        Run::SuiteStarted { progress: _, seed, transfer_bits, transfer_source } => {
            console::print_above(&format!("🎲 Pattern seed {seed:#018x} (seed= replays it), transfer width {}",
                transfer_bits.map_or("unknown: switch alternates 64 and 32 bits".to_string(), |bits| format!("{bits} bits ({transfer_source})"))));
        }
        Run::CycleStarted { cycle, limit } => console::print_above(&match limit {
            Some(limit) => format!("\n🔄 Starting test cycle {} of {}", cycle, limit),
            None => format!("\n🔄 Starting test cycle {}", cycle),
        }),
        // No line of its own: the next cycle's start or the verdict follows
        Run::CycleFinished { .. } => {}
        Run::SealStage { op, kernel, bytes, errors, bits, duration } => {
            let lead = match op {
                SealOp::SealAll => "🔒 Sealing memory",
                SealOp::FinalCheck => "🔒 Final seal check",
            };
            let gib = bytes as f64 / (1024.0 * 1024.0 * 1024.0);
            let errors_info = if op == SealOp::FinalCheck { format!("{errors} errors{}, ", bits.brief()) } else { String::new() };
            // The step report's form for an operation (TODO 90): time, data @ speed
            console::print_above(&format!("{lead} ({}): {errors_info}{:.1}s, {gib:.2} GiB @ {:.0} MiB/s",
                kernel, duration.as_secs_f64(), gib * 1024.0 / duration.as_secs_f64().max(1e-9)));
        }
        Run::StepFinished(report) => console::print_block(&step_report(&report)),
        Run::LateWhea(events) => {
            for event in events {
                println!("{}", event);
            }
        }
        Run::CpuPerformance(summary) => print!("{}", cpu_performance(&summary)),
        Run::SuiteEnded { outcome, status, memory_errors, whea } => {
            // Display completion status
            if outcome == RunOutcome::Interrupted {
                println!("\n⚠️  Test suite interrupted by user (CTRL+C)");
            } else if status == RunStatus::NotRun {
                println!("\n⚠️  No test ran: the cycle or time limit is 0");
            } else if status == RunStatus::Passed {
                println!("\n✅ All test cycles completed successfully!");
            }
            if status == RunStatus::Failed {
                // Same form as the per-test topline. Either count alone fails the run: a WHEA event
                // with `0 errors` is a fault that ECC corrected, or one raised during a test phase
                // that does not verify — still not a stable configuration.
                println!(
                    "\n❌ Test suite failed: {} errors + {} WHEA{}",
                    memory_errors, whea.total, whea.split_suffix()
                );
            }
        }
        Run::FinalSummary(report) => print!("{}", report_text("final summary", |r| r.report_final_summary(&report))),
        Run::ResultsSaved { path } => println!("💾 Results saved: {}", path),
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
        // A filter narrows a named plan: the list (`Run::Matchable`) is what it could have picked
        // from
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

/// The tests left out, the first few by name: the log lists them all.
fn left_out(tests: &[(String, String)]) -> String {
    const SHOWN: usize = 5;
    let Some((_, lacks)) = tests.first() else {
        return String::new();
    };
    let names: Vec<&str> = tests.iter().take(SHOWN).map(|(name, _)| name.as_str()).collect();
    let more = tests.len().saturating_sub(SHOWN);
    let more = if more > 0 { format!(" and {more} more (the log lists them)") } else { String::new() };
    format!("ℹ  Left out {} test(s) this CPU can't run ({}): {}{more}\n", tests.len(), lacks, names.join(", "))
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

/// A step's report: its verdict and figures, its config, its threads' table (and a latency step's
/// latencies), and where the suite stood as it ended, then a blank line before the next.
fn step_report(report: &StepReport) -> String {
    let StepReport { cycle, summary: s, wall, timed_secs, cycle_limit, stopped_by_time, sealed, config, threads, latency, stamp } = report;
    let cycles_completed = s.cycles_completed.1;

    // Format cycle info for logging
    let cycles_info = if cycles_completed > 0 || cycle_limit.is_some() {
        let limit_str = cycle_limit.map_or("∞".to_string(), |l| l.to_string());

        // Cycle limit indicator: RED if cycle limit caused stop, GREEN if didn't reach it
        let stop_indicator_cycles = if let Some(limit) = *cycle_limit {
            if cycles_completed >= limit { "🔴" } else { "🟢" }  // RED when hit, GREEN when not
        } else {
            "🟢"  // No limit = always green (can't hit what doesn't exist)
        };

        // Time limit indicator: red if time limit stopped us, green otherwise
        let stop_indicator_time = if *stopped_by_time { "🔴" } else { "🟢" };

        format!(", Cycles {} {}/{}, Time Limit {}",
                stop_indicator_cycles, cycles_completed, limit_str, stop_indicator_time)
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
    let ops_per_sec = if *timed_secs > 0.0 {
        s.operations as f64 / timed_secs
    } else {
        0.0
    };

    // The consolidated P50 and spread of a latency step
    let latency_info = match (s.latency_p50_ns, s.latency_spread) {
        (Some(p50), Some(spread)) => format!(", P50: {:.1}ns, Spread: {:.2}x", p50, spread),
        _ => String::new(),
    };

    // Pass/fail for this test. Both error sources count: a WHEA event during the test means the
    // hardware reported a fault whether or not a verify read caught it, and either way the
    // system is not stable. This is the only per-test verdict in the output.
    //
    // The ✅/❌ here is a verdict; the 🔴/🟢 in `cycles_info` is not — those mark *which limit
    // ended the test*, so the two glyph pairs are deliberately different.
    let test_passed = s.errors == 0 && s.seal_errors == 0 && s.whea_total == 0;
    let pass_indicator = if test_passed { "✅" } else { "❌" };
    // The seal's errors, in a sealed run: what the checks before this step's chunks found
    let seal_info = if *sealed { format!(" + {} seal{}", s.seal_errors, s.seal_failing_bits.brief()) } else { String::new() };
    let whea = WheaCounts { total: s.whea_total, corrected: s.whea_corrected };

    // WHEA is always shown, even at zero, so a clean line still states that we were watching;
    // the C/UC split only appears when there is something to split. "errors" is the word every
    // table uses for these (the `Errors` column) — "data" would collide with the `Data` column,
    // which is bytes processed.
    // The bits that failed follow each count: ` (bit 31)`
    // The step's verdict and wall time, then its test's own figures, the work around it by
    // operation (TODO 90, `n/a` where it did none), and its config
    let mut text = String::new();
    let _ = writeln!(text, "📊 Test report - Cycle {} - Step {} {}: {} {:.1}s, {} errors{}{} + {} WHEA{}{}",
             cycle,
             s.step,
             s.label,
             pass_indicator,
             wall.as_secs_f64(),
             s.errors,
             s.failing_bits.brief(),
             seal_info,
             whea.total,
             whea.split_suffix(),
             cycles_info);
    let _ = writeln!(text, "   Test: {:.1}s, {:.2} GiB @ {:.0} MiB/s, {} ops @ {} ops/s{}",
             timed_secs,
             s.bytes_processed as f64 / (1024.0 * 1024.0 * 1024.0),
             s.throughput_mib_s,
             format_ops(s.operations),
             format_ops(ops_per_sec as u64),
             latency_info);
    let _ = writeln!(text, "   Intermediate: {}", s.ops.line());

    let reps_str = if config.test_reps != 1 || config.verify_reps != 1 || config.write_read_cycles != 1 {
        format!(", test_reps={}, verify_reps={}, wrc={}", config.test_reps, config.verify_reps, config.write_read_cycles)
    } else {
        String::new()
    };
    let _ = writeln!(text, "   Config: parameter={}, mode={}, extent={}, chunk={}{}",
             config.parameter,
             config.mode,
             config.extent,
             config.chunk,
             reps_str);

    // Thread timing deviations, and a latency step's latencies
    text += &report_text("thread timing report", |r| r.report_thread_timing(threads));
    if let Some(latency) = latency {
        text += &report_text("latency report", |r| r.report_latency_summary(latency));
    }

    // Where the suite stood as this test ended. The ticker is down during every report and
    // gone after the run, so this is what stays on screen: one line per test.
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
