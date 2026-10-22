//! Goldens of the CLI's frontend (TODO 29), from fixed payloads: each event the console goldens of
//! a run don't show (`testdata/console/{cycle,plan_only,not_run}.txt`), through `CliFrontend::event`
//! as a run sends it, and the ticker; and the ticker's clock. `Run::Allocated` and
//! `Run::FinalSummary` have none of their own: each shows one report, whose golden is the
//! Reporter's (`report_allocation_*`, `report_final_summary`).

use std::time::{Duration, Instant};

use super::{draw_when_due, ticker, CliFrontend};
use crate::failing_bits::FailingBits;
use crate::frontend::{CpuPerformanceSummary, Frontend, NotRun, Run, StepConfigLine, StepReport};
use crate::golden::{capture, check};
use crate::memory::allocator::PageSizeLevel;
use crate::progress::{LiveFigures, ProgressSnapshot, RunOutcome, TestSnapshot, TestSummary};
use crate::reporting::models::{
    CpuPerformance, PerformanceByCpuReport, PerformanceByPhysicalCoreReport, PerformanceByThreadReport,
    PhysicalCorePerformance, ThreadPerformance,
};
use crate::runner::RunStatus;
use crate::thread_pool::SealOp;
use crate::whea::WheaCounts;

const GIB: u64 = 1 << 30;

/// The run's events the console goldens of a run don't reach, each after a `## case` line.
#[test]
fn events_print_as_their_golden() {
    let fe = CliFrontend::new();
    let left_out = |names: &[&str]| Run::LeftOut {
        tests: names.iter().map(|name| (name.to_string(), "no AVX-512F".to_string())).collect(),
    };
    let verdict = |outcome, status, memory_errors, whea| Run::SuiteEnded { outcome, status, memory_errors, whea };
    let no_whea = WheaCounts::default();
    let cases = vec![
        ("a plan the suite can't give", Run::NotRun(NotRun::Plan("plan stability: bug: no test Mem-Nope in the suite".to_string()))),
        ("every test left out", Run::NotRun(NotRun::NothingLeftToRun)),
        ("allocation failed", Run::NotRun(NotRun::Allocation("thread 2: no memory left for its span".to_string()))),
        ("two tests left out", left_out(&["Mem-SimpleV2-512", "Mem-MirrorV2-512"])),
        ("seven tests left out", left_out(&["Lat-L1-Read-512", "Lat-L1-Write-512", "Lat-L1-Copy-512", "Lat-L2-Read-512",
                                            "Lat-L2-Write-512", "Lat-L2-Copy-512", "Spd-Read-512"])),
        ("WHEA monitored", Run::WheaMonitoring(Ok(()))),
        ("WHEA not monitored", Run::WheaMonitoring(Err("the System event log can't be read".to_string()))),
        ("allocation between the default page sizes", Run::AllocationStarting { min: PageSizeLevel::Large, max: PageSizeLevel::Huge }),
        ("allocation of one page size", Run::AllocationStarting { min: PageSizeLevel::Regular, max: PageSizeLevel::Regular }),
        ("allocation between other page sizes", Run::AllocationStarting { min: PageSizeLevel::Regular, max: PageSizeLevel::Huge }),
        ("the suite starts", Run::SuiteStarted { seed: 0x0123_4567_89ab_cdef, transfer_bits: Some(64), transfer_source: "SMBIOS" }),
        ("the suite starts, its transfer width unknown", Run::SuiteStarted { seed: 7, transfer_bits: None, transfer_source: "none" }),
        ("a cycle of a limit", Run::CycleStarted { cycle: 2, limit: Some(3) }),
        ("a cycle with no limit", Run::CycleStarted { cycle: 41, limit: None }),
        ("memory sealed", Run::SealStage { op: SealOp::SealAll, kernel: "tm5-512".to_string(), bytes: 24 * GIB, errors: 0,
                                           bits: FailingBits::default(), duration: Duration::from_millis(1250) }),
        ("a final check that found errors", Run::SealStage { op: SealOp::FinalCheck, kernel: "tmr-256".to_string(), bytes: 24 * GIB,
                                                             errors: 2, bits: bits(1 << 5), duration: Duration::from_millis(1180) }),
        ("a step unsealed, ended by its time limit, with a WHEA event and repetitions", Run::StepFinished(Box::new(step()))),
        ("WHEA after the monitor's last tick", Run::LateWhea(vec![
            "⚠️  WHEA: corrected Memory error on DIMM_A1 (Event 47)".to_string(),
            "⚠️  WHEA: corrected Memory error on DIMM_A1 (Event 47)".to_string(),
        ])),
        ("the CPU performance summary", Run::CpuPerformance(Box::new(cpu_performance()))),
        ("the CPU performance summary, no rows", Run::CpuPerformance(Box::new(CpuPerformanceSummary {
            by_thread: PerformanceByThreadReport { threads: Vec::new(), pinned: true, whea: no_whea },
            by_cpu: PerformanceByCpuReport { cpus: Vec::new(), whea: no_whea },
            by_core: PerformanceByPhysicalCoreReport { cores: Vec::new(), whea: no_whea },
        }))),
        ("passed", verdict(RunOutcome::Completed, RunStatus::Passed, 0, no_whea)),
        ("passed on the time limit", verdict(RunOutcome::TimeLimit, RunStatus::Passed, 0, no_whea)),
        ("errors", verdict(RunOutcome::Completed, RunStatus::Failed, 3, no_whea)),
        ("halted on a WHEA event", verdict(RunOutcome::Halted, RunStatus::Failed, 0, WheaCounts { total: 1, corrected: 0 })),
        ("interrupted", verdict(RunOutcome::Interrupted, RunStatus::Interrupted, 0, no_whea)),
        ("interrupted with errors", verdict(RunOutcome::Interrupted, RunStatus::Failed, 1, WheaCounts { total: 2, corrected: 2 })),
        ("interrupted before any test", verdict(RunOutcome::Interrupted, RunStatus::NotRun, 0, no_whea)),
        ("no test ran", verdict(RunOutcome::Completed, RunStatus::NotRun, 0, no_whea)),
        ("the results saved", Run::ResultsSaved { path: "results/tmr_2026-10-22_19-30-01.json".to_string() }),
    ];
    let mut text = String::new();
    for (case, run) in cases {
        let ((), printed) = capture(|| fe.event(run.into()));
        text += &format!("## {case}\n{printed}");
    }
    check("events", &text);
}

/// The ticker in each of its forms, and a step's stamp, each after a `## case` line.
#[test]
fn the_ticker_draws_as_its_golden() {
    let mut text = String::new();
    let mut case = |name: &str, drawn: String| text += &format!("## {name}\n{drawn}\n");
    let running = snapshot();
    case("limits, a test on its own time limit, its figures in", ticker::render(&running));
    case("the stamp it ends with", ticker::test_stamp(&running, Duration::from_secs(15)).unwrap());

    let mut pending = snapshot();
    (pending.time_limit, pending.cycle_limit, pending.cycle) = (None, None, 12);
    (pending.finished_errors, pending.live_errors, pending.whea) = (2, 1, WheaCounts { total: 3, corrected: 1 });
    let test = pending.test.as_mut().unwrap();
    test.time_limit = None;
    test.live = LiveFigures { stage: Some(crate::tests::Stage::CheckingSeal), ..Default::default() };
    case("no limits, errors and WHEA, a stage, no figures yet", ticker::render(&pending));
    case("its stamp: the finished tests' errors only", ticker::test_stamp(&pending, Duration::from_secs(15)).unwrap());

    let mut limit_reached = snapshot();
    limit_reached.runtime = Duration::from_secs(3600);
    let test = limit_reached.test.as_mut().unwrap();
    (test.publishes, test.live) = (false, LiveFigures::default());
    case("the time limit reached, a test with no live figures", ticker::render(&limit_reached));

    let before = ProgressSnapshot { test: None, cycle: 1, tests_done: 0, cycle_elapsed: Duration::ZERO, ..snapshot() };
    case("before the first test", ticker::render(&before));
    assert_eq!(ticker::test_stamp(&before, Duration::from_secs(1)), None, "no stamp before the first test");
    check("ticker", &text);
}

/// The ticker's clock (`CliFrontend::tick`), on the monitor's 500 ms ticks: the first tick starts
/// it, a draw is due 2 seconds after the last, and one that couldn't draw (the console was held) is
/// tried at the next tick.
#[test]
fn the_ticker_draws_2_seconds_after_its_last_draw() {
    let start = Instant::now();
    let mut last_draw = None;
    let mut tried = Vec::new();
    for ms in (500..=5000).step_by(500) {
        draw_when_due(&mut last_draw, start + Duration::from_millis(ms), || {
            tried.push(ms);
            ms != 2500
        });
    }
    assert_eq!(tried, [2500, 3000, 5000]);
    assert_eq!(last_draw, Some(start + Duration::from_millis(5000)));
}

/// Where the suite stands 41 minutes into an hour: cycle 4 of 8, its second test of 6 running, 52%
/// into its own limit, its live figures in.
fn snapshot() -> ProgressSnapshot {
    ProgressSnapshot {
        runtime: Duration::from_secs(41 * 60 + 10),
        time_limit: Some(Duration::from_secs(3600)),
        cycle: 4,
        cycle_limit: Some(8),
        cycle_elapsed: Duration::from_secs(12 * 60 + 34),
        tests_done: 1,
        tests_per_cycle: 6,
        finished_errors: 0,
        live_errors: 0,
        whea: WheaCounts::default(),
        test: Some(TestSnapshot {
            number: 2,
            name: "Mem-Refresh128".to_string(),
            elapsed: Duration::from_secs(5 * 60 + 12),
            time_limit: Some(Duration::from_secs(600)),
            finished: false,
            publishes: true,
            live: LiveFigures {
                published: true,
                bytes: 285 * GIB,
                bytes_per_sec: 19108.7 * 1024.0 * 1024.0,
                ..Default::default()
            },
        }),
    }
}

/// The bits of a check that found `mask` wrong
fn bits(mask: u64) -> FailingBits {
    let mut bits = FailingBits::default();
    bits.add_check(mask);
    bits
}

/// A step of 5 passes of 61 s on two threads, both numbered CPU 0, so no figure depends on this
/// machine's topology: unsealed, on no cycle limit, ended by its time limit, with a corrected WHEA
/// event; a test and a verify repeated.
fn step() -> StepReport {
    let whea = WheaCounts { total: 1, corrected: 1 };
    let thread = |id: usize, ms: u64, passes: u32| (id, 0, 60 * GIB, Duration::from_millis(ms), 0, 1_250_000_000, passes);
    StepReport {
        cycle: 3,
        summary: TestSummary {
            step: 5,
            test_index: 4,
            id: Some("7".to_string()),
            label: "Mem-Refresh256 (Test 7)".to_string(),
            seal_errors: 0,
            name: "Mem-Refresh256".to_string(),
            duration_ms: 61_250,
            timed_ms: 60_000.0,
            bytes_processed: 120 * GIB,
            operations: 2_500_000_000,
            cycles_completed: (4, 5),
            stopped_by: Some("time"),
            ops: Default::default(),
            throughput_mib_s: 2048.0,
            errors: 0,
            alarms: 0,
            not_reproduced: 0,
            failing_bits: Default::default(),
            seal_failing_bits: Default::default(),
            whea_total: whea.total,
            whea_corrected: whea.corrected,
            latency_samples: None,
            latency_p5_ns: None,
            latency_p10_ns: None,
            latency_p25_ns: None,
            latency_p50_ns: None,
            latency_p75_ns: None,
            latency_p90_ns: None,
            latency_p95_ns: None,
            latency_p99_ns: None,
            latency_p99_9_ns: None,
            latency_spread: None,
            threads: Vec::new(),
        },
        wall: Duration::from_millis(61_250),
        timed_secs: 60.0,
        cycle_limit: None,
        stopped_by_time: true,
        sealed: false,
        config: StepConfigLine {
            parameter: "-".to_string(),
            mode: "TMR-10".to_string(),
            extent: "FullAllocation".to_string(),
            chunk: "Absolute (4.00 MiB)".to_string(),
            test_reps: 2,
            verify_reps: 3,
            write_read_cycles: 1,
        },
        threads: crate::reporting::converters::create_thread_timing_report(&[thread(0, 60_000, 5), thread(1, 59_500, 4)], whea),
        latency: None,
        stamp: Some(ProgressSnapshot {
            cycle: 3,
            tests_done: 5,
            whea,
            test: Some(TestSnapshot { number: 5, name: "Mem-Refresh256 (Test 7)".to_string(), finished: true, ..snapshot().test.unwrap() }),
            ..snapshot()
        }),
    }
}

/// Four threads on two cores of two CPUs each, one thread with errors, and the run's WHEA event on
/// each table's aggregate row
fn cpu_performance() -> CpuPerformanceSummary {
    let whea = WheaCounts { total: 1, corrected: 1 };
    let threads = (0..4)
        .map(|t| ThreadPerformance {
            thread_id: t,
            cpu_id: t,
            physical_core_id: t / 2,
            numa_node: 0,
            total_time_ms: 60_000 + 500 * t as u128,
            total_bytes: 60 * GIB,
            throughput_mib_s: 60.0 * 1024.0 / (60.0 + 0.5 * t as f64),
            total_errors: u64::from(t == 3),
        })
        .collect::<Vec<_>>();
    let cpus = threads.iter()
        .map(|t| CpuPerformance {
            cpu_id: t.cpu_id,
            physical_core_id: t.physical_core_id,
            numa_node: t.numa_node,
            total_time_ms: t.total_time_ms,
            total_bytes: t.total_bytes,
            throughput_mib_s: t.throughput_mib_s,
            total_errors: t.total_errors,
        })
        .collect();
    let cores = (0..2)
        .map(|core| PhysicalCorePerformance {
            core_id: core,
            logical_cpus: vec![2 * core, 2 * core + 1],
            total_time_ms: 120_000 + 1000 * core as u128,
            total_bytes: 120 * GIB,
            throughput_mib_s: 120.0 * 1024.0 / (120.0 + core as f64),
            total_errors: u64::from(core == 1),
        })
        .collect();
    CpuPerformanceSummary {
        by_thread: PerformanceByThreadReport { threads, pinned: true, whea },
        by_cpu: PerformanceByCpuReport { cpus, whea },
        by_core: PerformanceByPhysicalCoreReport { cores, whea },
    }
}
