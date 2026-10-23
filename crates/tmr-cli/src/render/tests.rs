//! Goldens of the CLI's frontend (TODO 29), from fixed payloads: each event the console goldens of
//! a run don't show (`testdata/console/{cycle,plan_only,not_run}.txt`, `run_tests.rs`), through
//! `CliFrontend::event` as a run sends it, and the ticker; and the ticker's clock. `Run::Allocated` and
//! `Run::FinalSummary` have none of their own: each shows one report, whose golden is the
//! Reporter's (`report_allocation_*`, `report_final_summary`). And a session's: every setup event,
//! as a session sends it, and each way a session ends. A session itself can't run in a test: its
//! setup checks and grants the large-page privilege, and runs secedit.

use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::TimeZone;
use tmr::frontend::{
    CacheCalibrationState, CpuPerformance, CpuPerformanceSummary, ErrorMode, Frontend, LiveFigures, MemoryModule, MemoryStatus, NotRun,
    PageSizeLevel, ParamNote, PerformanceByCpuReport, PerformanceByPhysicalCoreReport, PerformanceByThreadReport, PhysicalCorePerformance,
    ProgressSnapshot, ProgressTracker, Run, RunOutcome, RunStatus, SealCalibrationState, SealOp, Selection, Setup, Stage, StepConfigLine,
    StepReport, TestSnapshot, TestSummary, ThreadPerformance, WheaCounts,
};
use tmr::session::{SessionEnd, Stopped};
use tmr::test_support::failing_bits;

use super::{CliFrontend, draw_when_due, setup, ticker};
use crate::golden::{capture, check};

const GIB: u64 = 1 << 30;

/// The run's events the console goldens of a run don't reach, each after a `## case` line.
#[test]
fn events_print_as_their_golden() {
    let fe = CliFrontend::new();
    let left_out =
        |names: &[&str]| Run::LeftOut { tests: names.iter().map(|name| (name.to_string(), "no AVX-512F".to_string())).collect() };
    let verdict = |outcome, status, memory_errors, whea| Run::SuiteEnded { outcome, status, memory_errors, whea };
    let suite = |seed, transfer_bits, transfer_source| Run::SuiteStarted {
        progress: Arc::new(ProgressTracker::new()),
        seed,
        transfer_bits,
        transfer_source,
    };
    let no_whea = WheaCounts::default();
    let cases = vec![
        ("a plan the suite can't give", Run::NotRun(NotRun::Plan("plan stability: bug: no test Mem-Nope in the suite".to_string()))),
        ("every test left out", Run::NotRun(NotRun::NothingLeftToRun)),
        ("allocation failed", Run::NotRun(NotRun::Allocation("thread 2: no memory left for its span".to_string()))),
        ("two tests left out", left_out(&["Mem-SimpleV2-512", "Mem-MirrorV2-512"])),
        (
            "seven tests left out",
            left_out(&[
                "Lat-L1-Read-512", "Lat-L1-Write-512", "Lat-L1-Copy-512", "Lat-L2-Read-512", "Lat-L2-Write-512", "Lat-L2-Copy-512",
                "Spd-Read-512",
            ]),
        ),
        ("WHEA monitored", Run::WheaMonitoring(Ok(()))),
        ("WHEA not monitored", Run::WheaMonitoring(Err("the System event log can't be read".to_string()))),
        ("allocation between the default page sizes", Run::AllocationStarting { min: PageSizeLevel::Large, max: PageSizeLevel::Huge }),
        ("allocation of one page size", Run::AllocationStarting { min: PageSizeLevel::Regular, max: PageSizeLevel::Regular }),
        ("allocation between other page sizes", Run::AllocationStarting { min: PageSizeLevel::Regular, max: PageSizeLevel::Huge }),
        ("the suite starts", suite(0x0123_4567_89ab_cdef, Some(64), "SMBIOS")),
        ("the suite starts, its transfer width unknown", suite(7, None, "none")),
        ("a cycle of a limit", Run::CycleStarted { cycle: 2, limit: Some(3) }),
        ("a cycle with no limit", Run::CycleStarted { cycle: 41, limit: None }),
        ("memory sealed", Run::SealStage {
            op: SealOp::SealAll,
            kernel: "tm5-512".to_string(),
            bytes: 24 * GIB,
            errors: 0,
            bits: Default::default(),
            duration: Duration::from_millis(1250),
        }),
        ("a final check that found errors", Run::SealStage {
            op: SealOp::FinalCheck,
            kernel: "tmr-256".to_string(),
            bytes: 24 * GIB,
            errors: 2,
            bits: failing_bits(1 << 5),
            duration: Duration::from_millis(1180),
        }),
        ("a step unsealed, ended by its time limit, with a WHEA event and repetitions", Run::StepFinished(Box::new(step()))),
        (
            "WHEA after the monitor's last tick",
            Run::LateWhea(vec![
                "⚠️  WHEA: corrected Memory error on DIMM_A1 (Event 47)".to_string(),
                "⚠️  WHEA: corrected Memory error on DIMM_A1 (Event 47)".to_string(),
            ]),
        ),
        ("the CPU performance summary", Run::CpuPerformance(Box::new(cpu_performance()))),
        (
            "the CPU performance summary, no rows",
            Run::CpuPerformance(Box::new(CpuPerformanceSummary {
                by_thread: PerformanceByThreadReport { threads: Vec::new(), pinned: true, whea: no_whea },
                by_cpu: PerformanceByCpuReport { cpus: Vec::new(), whea: no_whea },
                by_core: PerformanceByPhysicalCoreReport { cores: Vec::new(), whea: no_whea },
            })),
        ),
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

/// A cycle's end has no line on the console (TODO 29): it is for the GUI
#[test]
fn a_cycle_ending_prints_nothing() {
    let fe = CliFrontend::new();
    for partial in [false, true] {
        let ((), printed) = capture(|| fe.event(Run::CycleFinished { cycle: 2, partial }.into()));
        assert_eq!(printed, "", "partial: {partial}");
    }
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
    test.live = LiveFigures { stage: Some(Stage::CheckingSeal), ..Default::default() };
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
            live: LiveFigures { published: true, bytes: 285 * GIB, bytes_per_sec: 19108.7 * 1024.0 * 1024.0, ..Default::default() },
        }),
    }
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
        threads: tmr::test_support::create_thread_timing_report(&[thread(0, 60_000, 5), thread(1, 59_500, 4)], whea),
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
    let cpus = threads
        .iter()
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

/// A session's setup events, each after a `## case` line, through `CliFrontend::event` as a session
/// sends them, all their forms. `Setup::Starting` is shown apart, with a Ctrl+C of the test's own:
/// the CLI's (`crate::take_ctrl_c`) would take it from the test process.
#[test]
fn setup_events_print_as_their_golden() {
    let fe = CliFrontend::new();
    let at = |day| chrono::Utc.with_ymd_and_hms(2026, 10, day, 9, 5, 0).unwrap();
    let args = |args: &[&str]| args.iter().map(|a| a.to_string()).collect::<Vec<_>>();
    let selection = |error_mode, pinned, cpus_to_skip, avoid_smt_doubling, cycles, duration_secs| {
        Setup::Selection(Box::new(Selection {
            error_mode,
            threads: 3,
            available: 6,
            cputype: "cores".to_string(),
            cpu_list: vec![2, 4, 6],
            pinned,
            cpus_to_skip,
            avoid_smt_doubling,
            cycles,
            duration_secs,
            simd: "SSE, SSE2, SSE3, SSE4.1, SSE4.2, AVX, AVX2".to_string(),
        }))
    };
    let large_pages =
        |usable, min_page: &str, min_needs_large| Setup::LargePageStatus { usable, min_page: min_page.to_string(), min_needs_large };
    let topology = |smt_p_cores, e_cores_skipped_first| Setup::CpuTopology {
        report: Box::new(crate::renderer::tests::cpu_topology_report()),
        smt_p_cores,
        e_cores_skipped_first,
    };
    let cases = vec![
        ("a test filter", Setup::TestFilter { pattern: "Mem-Mirror*,Lat-L3-*".to_string() }),
        ("the quick test", Setup::QuickTest),
        (
            "every parameter note",
            Setup::ParamNotes(vec![
                ParamNote::MinPage("regular".to_string()),
                ParamNote::MaxPage("large".to_string()),
                ParamNote::Block("hugechunk", "2GiB".to_string()),
                ParamNote::Block("blkround", "down".to_string()),
                ParamNote::Topology("windowsv2"),
                ParamNote::SkipCores(1),
                ParamNote::PinningOff,
            ]),
        ),
        ("the header of a release build", Setup::StartupHeader {
            command_line: args(&["tmr.exe", "memory=50%", "cputype=cores", "cpus=100%", "skip-cores=0", "cycles=1"]),
            release: true,
        }),
        ("the header of a debug build", Setup::StartupHeader { command_line: args(&["tmr.exe"]), release: false }),
        ("the parameters, given and defaults", Setup::StartupParams {
            rust_log: "info".to_string(),
            rust_backtrace: None,
            explicit: args(&["memory=50%", "--startup-debug"]),
            defaults: args(&["cputype=threads (default)", "errors=log (default)"]),
        }),
        ("the parameters, a backtrace set and none given", Setup::StartupParams {
            rust_log: "debug".to_string(),
            rust_backtrace: Some("1".to_string()),
            explicit: Vec::new(),
            defaults: Vec::new(),
        }),
        ("cores", Setup::CpuType { cputype: "cores".to_string(), avoid_smt: true }),
        ("threads", Setup::CpuType { cputype: "threads".to_string(), avoid_smt: false }),
        ("a core range left out", Setup::CpuFilter { lo: 0, hi: 7 }),
        ("a share of the CPUs", Setup::CpuCount { percent: 50, available: 6, threads: 3 }),
        ("pinned, a CPU skipped, SMT doubling avoided, both limits", selection(ErrorMode::Log, true, 1, true, Some(3), Some(600))),
        ("unpinned, halting, cycles only", selection(ErrorMode::Halt, false, 0, false, Some(1), None)),
        ("panicking, time only", selection(ErrorMode::Panic, true, 0, false, None, Some(60))),
        ("no limits", selection(ErrorMode::Log, true, 0, false, None, None)),
        ("large pages are checked", Setup::LargePageCheck),
        ("what the check did", Setup::LargePageDiagnostic {
            report: "🔍 Large Page Diagnostic\n  Privilege: assigned and enabled\n  Test allocation: 2 MiB, succeeded".to_string(),
        }),
        ("large pages usable", large_pages(true, "large", true)),
        ("large pages not usable, and minpage needs them", large_pages(false, "large", true)),
        ("large pages not usable, and minpage doesn't need them", large_pages(false, "regular", false)),
        ("the extended cache calibration", Setup::CacheCalibration(CacheCalibrationState::Loaded { extended: true, at: at(20) })),
        ("the standard cache calibration", Setup::CacheCalibration(CacheCalibrationState::Loaded { extended: false, at: at(2) })),
        ("no cache calibration", Setup::CacheCalibration(CacheCalibrationState::None)),
        ("another machine's cache calibration", Setup::CacheCalibration(CacheCalibrationState::Stale)),
        ("the cache calibration off", Setup::CacheCalibration(CacheCalibrationState::Disabled)),
        ("the seal calibration", Setup::SealCalibration(SealCalibrationState::Loaded { bits: 256, at: at(21) })),
        ("another machine's seal calibration", Setup::SealCalibration(SealCalibrationState::Stale)),
        ("no seal calibration", Setup::SealCalibration(SealCalibrationState::None)),
        ("a hybrid's topology, E-cores skipped first", topology(Some(2), true)),
        ("a hybrid's topology, no P-core with SMT", topology(Some(0), false)),
        ("a topology with no hybrid notes", topology(None, false)),
        (
            "the memory status",
            Setup::MemoryStatus(Some(MemoryStatus {
                total_phys: 64 * GIB - 600 * (1 << 20),
                avail_phys: 51 * GIB,
                total_page_file: 72 * GIB,
                avail_page_file: 58 * GIB,
            })),
        ),
        ("no memory status, on stderr", Setup::MemoryStatus(None)),
        ("the system", Setup::SystemInfo {
            report: Box::new(crate::renderer::tests::system_info_report()),
            memory_modules: vec![MemoryModule { locator: "DIMM_A2".to_string(), ..Default::default() }],
        }),
        ("realtime on every CPU", Setup::RealtimeOnEveryCpu),
        ("the default plan", Setup::Plan { name: "stability", about: "the stability tests at the widest width", is_default: true }),
        ("a plan named", Setup::Plan { name: "quick", about: "the stability tests once", is_default: false }),
    ];
    let mut text = String::new();
    for (case, setup) in cases {
        let ((), printed) = capture(|| fe.event(setup.into()));
        text += &format!("## {case}\n{printed}");
    }
    let mut ctrl_c_taken = 0;
    let ((), printed) = capture(|| setup::show(Setup::Starting, || ctrl_c_taken += 1));
    assert_eq!(ctrl_c_taken, 1, "the tests' start takes Ctrl+C, once");
    text += &format!("## the tests start\n{printed}");
    check("setup", &text);
}

/// TODO 99: the line each seal calibration shows at startup, as the CLI's test of it said before
/// the split: Loaded, with its width and when; Stale; None. Which one a stored calibration is, is
/// the session's to say (`tmr::session::load_seal_calibration`'s test).
#[test]
fn the_seal_calibration_says_which_width_auto_takes() {
    let at = chrono::Utc.with_ymd_and_hms(2026, 10, 21, 9, 5, 0).unwrap();
    let line = |state| setup::text(&Setup::SealCalibration(state));
    assert_eq!(line(SealCalibrationState::Loaded { bits: 128, at }), "  Seal calibration: Loaded (128 bits, 2026-10-21 09:05)\n");
    assert_eq!(line(SealCalibrationState::Stale), "  Seal calibration: Stale (hardware changed, re-run --calibrate-seal)\n");
    assert_eq!(line(SealCalibrationState::None), "  Seal calibration: None (run --calibrate-seal to calibrate)\n");
}

/// How a session ends on the console, each after a `## case` line: a run's verdict,
/// `--startup-debug`'s, and why setup stopped one (an unknown flag shows the help, with tmr.exe's
/// name). The restart chosen and a command show nothing here: the prompt said its piece, and a
/// command writes its own (`tmr::commands::run`).
#[test]
fn session_ends_print_as_their_golden() {
    let ran = |status, secs, plan_only| SessionEnd::Ran { status, elapsed: Duration::from_secs(secs), plan_only };
    let cases = vec![
        ("the plan shown", ran(RunStatus::Passed, 0, true)),
        ("the plan not shown", ran(RunStatus::NotRun, 0, true)),
        ("passed", ran(RunStatus::Passed, 3723, false)),
        ("failed", ran(RunStatus::Failed, 59, false)),
        ("stopped before testing", ran(RunStatus::NotRun, 1, false)),
        ("interrupted", ran(RunStatus::Interrupted, 90061, false)),
        ("the restart chosen", SessionEnd::RestartChosen),
        ("a command", SessionEnd::Command(tmr::commands::Command::Version)),
        ("unknown flags", SessionEnd::Stopped(Stopped::UnknownFlags(vec!["--latency-test".to_string(), "-x".to_string()]))),
        (
            "an invalid parameter, on stderr",
            SessionEnd::Stopped(Stopped::Invalid(
                "❌ Unknown parameter: colour\n💡 Run with --help to see all valid parameters".to_string(),
            )),
        ),
        (
            "an invalid memory override",
            SessionEnd::Stopped(Stopped::MemoryOverride { spec: "lots".to_string(), error: "Invalid memory format: lots".to_string() }),
        ),
        (
            "CPUs that can't be selected, on stderr",
            SessionEnd::Stopped(Stopped::CpuSelection("cpus=8 asks for 8 threads, but only 6 CPUs are available".to_string())),
        ),
    ];
    let mut text = String::new();
    for (case, end) in cases {
        let ((), printed) = capture(|| super::show_end(&end, "tmr.exe"));
        text += &format!("## {case}\n{printed}");
    }
    check("session_end", &text);
}
