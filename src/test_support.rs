//! What the CLI's tests drive the engine with (TODO 29). The console's text is the CLI's (`crates/tmr-cli`), and its goldens show it keeps
//! its bytes; much of what they show is the engine's code at work, which they reach here: a cycle of `execute_test_cycle` on heap regions
//! (`FakeCycle`), the run's entry point on a fixed layout (`GoldenRun`), the detections that log at their first use, the reports that take
//! the engine's code to build, each from a fixed model, and results files saved from such models (`ResultsFile`, for `--show-results`). A
//! unit test build has it, and a build with the `test-support` feature, which only tmr-cli's tests turn on (its dev-dependency), so a plain
//! build of tmr.exe or the window has none of it. Cargo builds the engine once a command, with every feature that command's targets ask
//! for: under `cargo build --all-targets` tmr.exe links it too. Ship, benchmark and read the asm of a plain build only
//! (`cargo build --release`, `cargo rustc --release -p test-memory-r --bin tmr`).

use std::path::Path;
use std::time::Duration;

use crate::failing_bits::FailingBits;
use crate::memory::allocator::PageSizeLevel::{self, Huge, Large};
use crate::memory::stitched::{Commit, Run, ThreadSpan};
use crate::progress::TestSummary;
pub use crate::reporting::converters::create_thread_timing_report;
use crate::reporting::models::{
    AllocationReport, ConsolidatedMemoryReport, CpuTopologyReport, FinalTestSummaryReport, TestConfigurationReport,
};
use crate::results::{TempDir, TestRunResult};
pub use crate::runner::TestRunOverrides;
pub use crate::runner::test_support::{FakeCycle, Flip, GoldenRun};

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;

/// The built-in plan a plain run takes (`plan=stability`)
pub fn default_plan() -> &'static crate::plans::Plan {
    crate::plans::find(crate::plans::DEFAULT).expect("the default plan is built in")
}

/// Runs the detections, which log what they found at their first use, wherever that is: before a capture of what a test prints, so no
/// golden depends on which test ran first.
pub fn detect() {
    let _ = (
        crate::tests::get_system_info(),
        crate::smbios::get_smbios(),
        crate::pattern::transfer_beat(),
        crate::cpu_topology::get_cpu_topology(),
    );
}

/// The bits of a check that found `mask` wrong
pub fn failing_bits(mask: u64) -> FailingBits {
    let mut bits = FailingBits::default();
    bits.add_check(mask);
    bits
}

/// Compares `text` with the golden `<dir>/<name>.txt`, or under `TMR_BLESS=1` writes it there. A difference fails with the first line that
/// differs, and the whole text after it. The engine's goldens are in `testdata/console/` (`check`), the CLI's in its crate's.
pub fn check_golden(dir: &Path, name: &str, text: &str) {
    let path = dir.join(format!("{name}.txt"));
    if std::env::var_os("TMR_BLESS").is_some_and(|v| v == "1") {
        std::fs::create_dir_all(dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
        std::fs::write(&path, text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        return;
    }
    let golden = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}; `TMR_BLESS=1 cargo test` writes it", path.display()));
    if golden != text {
        let (want, got): (Vec<&str>, Vec<&str>) = (golden.split('\n').collect(), text.split('\n').collect());
        let at = want.iter().zip(&got).position(|(a, b)| a != b).unwrap_or(want.len().min(got.len()));
        panic!(
            "{name}: line {} differs from its golden ({} lines, now {}):\n  golden: {:?}\n  now:    {:?}\n\
                `TMR_BLESS=1 cargo test` rewrites the golden. The text now:\n{text}",
            at + 1,
            want.len(),
            got.len(),
            want.get(at).unwrap_or(&""),
            got.get(at).unwrap_or(&"")
        );
    }
}

/// `check_golden` against the engine's goldens, `testdata/console/<name>.txt`
#[cfg(test)]
pub(crate) fn check(name: &str, text: &str) {
    check_golden(&Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata").join("console"), name, text);
}

/// The start-up table (TODO 108) of `cores` under `request`, through the selection's own code (`cpu_selection::choose`)
fn startup_topology(cores: &[crate::cpu_selection::Core], request: &crate::cpu_selection::Request, hybrid: bool) -> CpuTopologyReport {
    let choice = crate::cpu_selection::choose(cores, request).expect("the fixture's request fits its cores");
    crate::cpu_selection::startup_topology(cores, request, &choice, true, hybrid).0
}

/// A Ryzen 5 8500G (2 Zen 4 cores, 0 and 4, and 4 Zen 4c, SMT, one L3; Windows' priorities 1 0 0 0 1 0) under core-priority=hybrid and
/// `cputype=cores skip-cores=1 cpus=60%`: Zen 4c core 5 skipped, the last in the core order; cores 0, 4 and 1 assigned, their siblings
/// left out; cores 2 and 3 free.
pub fn hybrid_cpu_topology_report() -> CpuTopologyReport {
    use crate::cpu_selection::{Request, SkipSpec, synthetic};
    use crate::cpu_topology::CoreType::{Efficiency, Performance};
    let mut cores = synthetic(&[1, 0, 0, 0, 1, 0], 64, 64, true);
    for core in &mut cores {
        core.core_type = if core.priority == 1 { Performance(0) } else { Efficiency(0) };
    }
    let request = Request { skip: SkipSpec::Count(1), cpus: "60%", stride: None, count_cores: true, one_per_core: true };
    startup_topology(&cores, &request, true)
}

/// An m8a.4xlarge (16 cores without SMT in two L3 groups of 8, one NUMA node, every priority 0) under `skip-cores=1 cpus=50%`: the
/// groups take turns, 4 threads in each, and core 15 is skipped, the last in the order.
pub fn two_l3_cpu_topology_report() -> CpuTopologyReport {
    use crate::cpu_selection::{Request, SkipSpec, synthetic};
    let request = Request { skip: SkipSpec::Count(1), cpus: "50%", stride: None, count_cores: false, one_per_core: false };
    startup_topology(&synthetic(&[0; 16], 8, 16, false), &request, false)
}

/// `memory=50%` on a 32 GiB box, six threads, each share rounded up to whole GiB.
pub fn consolidated_memory_report() -> ConsolidatedMemoryReport {
    use crate::memory::allocation_strategy::{AllocationMode, ReserveAmount, ShareRounding, SystemMemoryInfo};
    let mem_info = SystemMemoryInfo {
        total_installed_bytes: 32 * GIB,
        total_physical_bytes: 32 * GIB - 1200 * MIB,
        available_physical_bytes: 27 * GIB + 300 * MIB,
        used_physical_bytes: 3 * GIB + 548 * MIB,
        total_virtual_bytes: 36 * GIB + 900 * MIB,
        memory_load_percent: 12,
    };
    let mode = AllocationMode::ReserveFromAvailable { reserve: ReserveAmount::Percentage(50.0) };
    let allocation = mode.calculate_allocation_with_threads(&mem_info, 6, ShareRounding::default());
    crate::reporting::converters::create_consolidated_memory_report(&mem_info, &allocation)
}

/// An unsealed plan at 256 bits with a test run twice: the table and its notes (the cycle, the seal, the width). Its tests' sizes don't
/// depend on this machine's caches: absolute and full extents, absolute chunks. The CLI's plan-only golden has a sealed plan (`GoldenRun`).
pub fn test_configuration_report() -> TestConfigurationReport {
    use crate::reporting::converters::{PlanSequence, create_test_configuration_report_v2};
    let _width = crate::simd::TestWidth::set(Some(crate::simd::Width::W256));
    let cache_info = crate::cache::CacheInfo::default();
    let suite = crate::runner::create_test_definitions(&cache_info);
    let pick = |name: &str| suite.iter().find(|d| d.actual_name == name || d.original_name == Some(name)).cloned().expect(name);
    let mut tests: Vec<_> = ["Mem-StuckBitAuto", "Bench-Init-TMR-10", "Bench-Verify-TMR-10", "Mem-SimpleV2-256", "Mem-MirrorV2-256"]
        .into_iter()
        .map(pick)
        .collect();
    for (i, def) in tests.iter_mut().enumerate() {
        def.test_index = i;
    }
    // Mem-StuckBit again at the end of the cycle
    tests.push(tests[0].clone());
    let sequence = PlanSequence { order: None, seal: None, seal_off_reason: "seal=off", tm5: false, width: crate::simd::width_given() };
    let timing = crate::runner::TestSuiteTiming::cycles_only(3);
    create_test_configuration_report_v2(&tests, &timing, &cache_info, &[(4 * GIB) as usize; 4], None, &sequence)
}

/// A span from its requests in order, `(bytes, page, node, refused)`, laid out as the stitched allocator lays them: each at the cursor,
/// granted ones merging into runs.
pub(crate) fn span(thread_id: usize, home: u32, target: usize, requests: &[(usize, PageSizeLevel, u32, bool)]) -> ThreadSpan {
    let (mut runs, mut commits, mut cursor) = (Vec::<Run>::new(), Vec::new(), 0);
    for &(len, page, node, refused) in requests {
        commits.push(Commit { len, page, numa_node: Some(node), refused });
        if refused {
            continue;
        }
        match runs.last_mut() {
            Some(run) if run.page == page && run.numa_node == Some(node) => run.len += len,
            _ => runs.push(Run { offset: cursor, len, page, numa_node: Some(node) }),
        }
        cursor += len;
    }
    ThreadSpan { thread_id, numa_node: Some(home), base: thread_id * 16 * (GIB as usize), len: cursor, target, runs, commits }
}

/// `n` granted requests of `len` bytes on `page`s of `node`, for `span`
pub(crate) fn repeat(n: usize, len: usize, page: PageSizeLevel, node: u32) -> Vec<(usize, PageSizeLevel, u32, bool)> {
    vec![(len, page, node, false); n]
}

/// The allocation report of `spans` on `numa_nodes` nodes, taking 1.234 s
pub(crate) fn allocation_report(spans: Vec<ThreadSpan>, numa_nodes: u32) -> AllocationReport {
    crate::reporting::converters::create_allocation_report(spans, Duration::from_millis(1234), numa_nodes)
}

/// The 2026-10-16 run on this box, as its log tallied it: 18 × 1 GiB granted, thread 2's last 1 GiB request refused (1450), 80 × 128 MiB
/// granted.
pub fn allocation_report_one_node() -> AllocationReport {
    let (gib, mib) = (GIB as usize, MIB as usize);
    let threads = (0..4)
        .map(|t| {
            let (huge, large) = if t < 2 { (5, 16) } else { (4, 24) };
            let mut requests = repeat(huge, gib, Huge, 0);
            if t == 2 {
                requests.push((gib, Huge, 0, true));
            }
            requests.extend(repeat(large, 128 * mib, Large, 0));
            span(t, 0, 7 * gib, &requests)
        })
        .collect();
    allocation_report(threads, 1)
}

/// Two nodes; node 1 is short of 1 GiB pages, so its threads take more 2 MiB pages, and thread 3 gets 512 MiB of them from node 0, asked
/// for after a refusal on its own node.
pub fn allocation_report_two_nodes() -> AllocationReport {
    let (gib, mib) = (GIB as usize, MIB as usize);
    let mut spans = Vec::new();
    for t in 0..4 {
        let home = (t / 2) as u32;
        let mut requests = if home == 0 { repeat(7, gib, Huge, 0) } else { repeat(2, gib, Huge, 1) };
        if home == 0 {
            requests.extend(repeat(8, 128 * mib, Large, 0));
        } else if t == 2 {
            requests.extend(repeat(48, 128 * mib, Large, 1));
        } else {
            requests.extend(repeat(44, 128 * mib, Large, 1));
            requests.push((128 * mib, Large, 1, true));
            requests.extend(repeat(4, 128 * mib, Large, 0));
        }
        spans.push(span(t, home, 8 * gib, &requests));
    }
    allocation_report(spans, 2)
}

/// Step `step` of the built-in suite's simplest kind: one pass of `duration_ms` over `bytes_processed`, clean but for `seal_errors`, at
/// 1000 MiB/s when it ran at all.
pub(crate) fn summary(step: usize, duration_ms: u128, bytes_processed: u64, seal_errors: u64) -> TestSummary {
    TestSummary {
        step,
        test_index: step - 1,
        id: None,
        label: format!("step {step}"),
        seal_errors,
        name: "Mem-SimpleV2-Auto".to_string(),
        duration_ms,
        timed_ms: duration_ms as f64,
        bytes_processed,
        operations: 0,
        cycles_completed: (1, 1),
        stopped_by: Some("cycles"),
        ops: Default::default(),
        throughput_mib_s: if duration_ms > 0 { 1000.0 } else { 0.0 },
        errors: 0,
        alarms: 0,
        not_reproduced: 0,
        failing_bits: Default::default(),
        seal_failing_bits: Default::default(),
        whea_total: 0,
        whea_corrected: 0,
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
        loaded: None,
        threads: Vec::new(),
    }
}

/// A loaded latency test's latencies (TODO 117), from raw windows through `loaded::rows` as a run's step takes them: daddy-petz at
/// DDR4-2133, its figures near the probe's (`doc/todo/117-write-latency.md`), thread 0 on CPU 2 measuring, a loader on each other core's
/// first thread, their siblings sitting out, over a 1 GiB chain at 32 MiB of L3; Lat-Loaded-DRAM's schedule (idle, 3 ABBA pairs flat out,
/// idle), and with `levels`, 3 pairs at each of 3 levels after them, both kinds within 3% of the lower two, the reads 7% short of the
/// third. No histograms: its P50s and P99s are absent.
pub fn loaded_summary(levels: bool) -> crate::loaded::LoadedSummary {
    use crate::loaded::{Kind, LoadedSummary, Placement, WindowRecord};
    let mut windows: Vec<WindowRecord> = Vec::new();
    let mut window = |kind, level: Option<usize>, pair: Option<usize>, target_mib_s: Option<f64>, mean_ns: f64, mib_s: f64| {
        let index = windows.len();
        windows.push(WindowRecord { index, kind, level, pair, target_mib_s, mean_ns, mib_s, reads: 812_000, fell_behind: 0 });
    };
    let (w, r) = (Kind::Writes, Kind::Reads);
    window(Kind::Park, None, None, None, 102.5, 0.0);
    // Flat out: W R, R W, W R
    #[rustfmt::skip]
    let full = [(w, 0, 449.0, 16_097.0), (r, 0, 263.9, 27_180.0), (r, 1, 265.6, 27_151.0), (w, 1, 451.7, 16_117.0),
        (w, 2, 453.2, 16_130.0), (r, 2, 264.8, 27_205.0)];
    for (kind, pair, ns, mib_s) in full {
        window(kind, None, Some(pair), None, ns, mib_s);
    }
    if levels {
        // Each level's pairs in the same order, its writes' and reads' latency and MiB/s a pair each
        #[rustfmt::skip]
        let at = [
            (4029.0, [(111.9, 4027.0), (113.0, 4027.0), (114.2, 4030.0)], [(112.2, 4028.0), (111.6, 4026.0), (111.2, 4031.0)]),
            (8058.0, [(125.1, 8051.0), (127.0, 8049.0), (129.9, 8055.0)], [(121.2, 8055.0), (121.0, 8060.0), (121.8, 8052.0)]),
            (12_087.0, [(150.3, 12_080.0), (148.9, 12_085.0), (152.0, 12_090.0)], [(133.1, 11_210.0), (132.4, 11_190.0), (134.0, 11_230.0)]),
        ];
        for (level, (target, writes, reads)) in at.into_iter().enumerate() {
            for pair in 0..3 {
                let both = [(w, writes[pair]), (r, reads[pair])];
                let order = if pair % 2 == 0 { both } else { [both[1], both[0]] };
                for (kind, (ns, mib_s)) in order {
                    window(kind, Some(level), Some(pair), Some(target), ns, mib_s);
                }
            }
        }
    }
    window(Kind::Park, None, None, None, 103.1, 0.0);
    let levels = if levels { 3 } else { 0 };
    LoadedSummary {
        measurer: Placement { thread: 0, cpu: 2, core: 1, node: 0 },
        loaders: (1..8).map(|core| 2 * core).collect(),
        sat_out: (0..8).map(|core| 2 * core + 1).collect(),
        chain_bytes: GIB as usize,
        l3_bytes: 32 * MIB as usize,
        cell_bytes: crate::loaded::CELL,
        width: 256,
        block_bytes: crate::loaded::BLOCK,
        window_ms: 1000.0,
        settle_ms: 200.0,
        rows: crate::loaded::rows(&windows, levels, &|_| None),
        windows,
        partial: false,
    }
}

/// `summary` (`loaded_summary`'s) with only its windows at `indices`, its rows taken again from them as a step's are: a step with no
/// loaders (its idle windows, no loaders and no sit-outs), or one a stop cut short (its first windows, `partial`)
pub fn loaded_windows(summary: &crate::loaded::LoadedSummary, indices: &[usize], partial: bool) -> crate::loaded::LoadedSummary {
    let windows: Vec<_> = indices.iter().map(|&i| summary.windows[i]).collect();
    let levels = windows.iter().filter_map(|w| w.level).max().map_or(0, |level| level + 1);
    let idle = windows.iter().all(|w| w.kind == crate::loaded::Kind::Park);
    crate::loaded::LoadedSummary {
        loaders: if idle { Vec::new() } else { summary.loaders.clone() },
        sat_out: if idle { Vec::new() } else { summary.sat_out.clone() },
        rows: crate::loaded::rows(&windows, levels, &|_| None),
        windows,
        partial,
        ..summary.clone()
    }
}

/// TODO 117: Lat-Loaded-DRAM's step as its report gets it (`loaded_summary`), on its cycle limit: its rows in place of a latency table, and
/// its timing table its 7 loaders' alone, each numbered CPU 0, so no figure depends on the machine's topology. Both frontends' goldens
/// and tests show it.
pub fn loaded_step_report() -> crate::frontend::StepReport {
    let loader = |core: usize| (2 * core, 0, 13 * GIB, Duration::from_millis(9_600), 0, 13 * GIB / 8, 1);
    crate::frontend::StepReport {
        cycle: 1,
        summary: TestSummary {
            label: crate::loaded::LOADED_NAME.to_string(),
            name: crate::loaded::LOADED_NAME.to_string(),
            timed_ms: 9_600.0,
            // The loaders' bytes and words, and the measurer's reads
            operations: 91 * GIB / 8 + 6_496_000,
            throughput_mib_s: 91.0 * 1024.0 / 9.6,
            loaded: Some(Box::new(loaded_summary(false))),
            ..summary(2, 11_400, 91 * GIB, 0)
        },
        wall: Duration::from_millis(11_400),
        timed_secs: 9.6,
        cycle_limit: Some(1),
        stopped_by_time: false,
        sealed: false,
        config: crate::frontend::StepConfigLine {
            parameter: "-".to_string(),
            mode: "-".to_string(),
            extent: "1.00 GiB".to_string(),
            chunk: "Whole".to_string(),
            test_reps: 1,
            verify_reps: 1,
            write_read_cycles: 1,
        },
        threads: create_thread_timing_report(&(1..8).map(loader).collect::<Vec<_>>(), Default::default()),
        latency: None,
        stamp: None,
    }
}

/// The final summary of two sealed cycles: a test run twice a cycle, a latency step, a step's errors and bits in the second cycle, a final
/// check's error, the seal's work and a step's, and a WHEA event. Every table it has shows.
pub fn final_summary_report() -> FinalTestSummaryReport {
    crate::reporting::converters::create_overall_stats_summary_report(&run_with_errors())
}

/// `final_summary_report`'s run, its overall figures as a save after its second cycle has them
pub(crate) fn run_with_errors() -> TestRunResult {
    use crate::ops::Op;
    use crate::results::CycleSeal;
    let bits = |mask: u64, want: u64, actual: u64| {
        let mut b = FailingBits::default();
        b.add_check(mask);
        b.add_word(want, actual);
        b
    };
    let step = |n: usize, test_index: usize, name: &str| TestSummary {
        test_index,
        name: name.to_string(),
        label: name.to_string(),
        throughput_mib_s: 9500.0 + 250.0 * n as f64,
        ..summary(n, 4_000 + 500 * n as u128, 12 << 30, 0)
    };
    let latency = |n: usize| TestSummary {
        latency_samples: Some(2000),
        latency_p5_ns: Some(71.0),
        latency_p10_ns: Some(71.5),
        latency_p25_ns: Some(72.0),
        latency_p50_ns: Some(72.5),
        latency_p75_ns: Some(73.25),
        latency_p90_ns: Some(74.5),
        latency_p95_ns: Some(76.5),
        latency_p99_ns: Some(85.0),
        latency_p99_9_ns: Some(112.5),
        latency_spread: Some(76.5 / 71.0),
        bytes_processed: 48 << 10,
        throughput_mib_s: 410.0,
        ..step(n, 1, "Lat-L1-Read")
    };
    let mut ops = crate::ops::OpTally::default();
    ops.add(Op::SealCheck, Duration::from_millis(350), 12 << 30);
    ops.add(Op::SealFill, Duration::from_millis(420), 12 << 30);
    let seal = |errors, final_check_failing_bits| CycleSeal {
        seal_ms: 410,
        seal_bytes: 24 << 30,
        between_steps_ms: 90,
        final_check_ms: 380,
        final_check_bytes: 24 << 30,
        final_check_errors: errors,
        final_check_failing_bits,
    };
    let mut result = TestRunResult::new(Default::default());
    let first = vec![step(1, 0, "Mem-StuckBit256_A"), latency(2), TestSummary { ops, ..step(3, 0, "Mem-StuckBit256_A") }];
    result.add_cycle(1, 31, first, Some(seal(0, FailingBits::default())), false);
    let bad = TestSummary {
        errors: 3,
        alarms: 2,
        failing_bits: bits(1 << 17, 0, 1 << 17),
        seal_errors: 1,
        seal_failing_bits: bits(1 << 5, 1 << 5, 0),
        whea_total: 1,
        whea_corrected: 1,
        ..step(3, 0, "Mem-StuckBit256_A")
    };
    result.add_cycle(2, 32, vec![step(1, 0, "Mem-StuckBit256_A"), latency(2), bad], Some(seal(1, bits(1 << 40, 0, 1 << 40))), false);
    result.finalize(Duration::from_secs(3725));
    result.set_whea_totals(crate::whea::WheaCounts { total: 1, corrected: 1 }, true);
    result
}

/// The final summary of a run as the console shows it at the run's end, and as its results file gives it, saved and loaded (TODO 72): the
/// CLI's test renders the two and finds them the same text. The run is two sealed cycles with a run's figures, not round ones:
/// Mem-StuckBit256_A twice a cycle, at 100.6 and 101.6 MiB/s, which average 101.1 (101.5 from the two rounded to whole MiB/s); a latency
/// step between them whose P75 is 72.7496 ns (rounded to 2 dp, 72.75, which shows as 72.8); the seal's work and a step's timed to the
/// nanosecond; over 63 s, so 30.48 MiB/s in all.
pub fn final_summary_saved_and_loaded() -> (FinalTestSummaryReport, FinalTestSummaryReport) {
    use crate::reporting::converters::create_overall_stats_summary_report;
    let result = clean_run();
    let dir = TempDir::new("summary-saved");
    let path = dir.file("result.json");
    result.save_atomic(&path).unwrap_or_else(|e| panic!("{e}"));
    let loaded = TestRunResult::load_from_file(&path).unwrap_or_else(|e| panic!("{e}"));
    (create_overall_stats_summary_report(&result), create_overall_stats_summary_report(&loaded))
}

/// `final_summary_saved_and_loaded`'s run, its overall figures as its final save has them, but for how it ended
fn clean_run() -> TestRunResult {
    use crate::ops::Op;
    use crate::results::CycleSeal;
    let step = |n: usize, test_index: usize, name: &str, bytes: u64, mib_s: f64| TestSummary {
        test_index,
        name: name.to_string(),
        label: name.to_string(),
        timed_ms: bytes as f64 / MIB as f64 / mib_s * 1000.0,
        throughput_mib_s: mib_s,
        ..summary(n, 4_000 + 777 * n as u128, bytes, 0)
    };
    let latency = TestSummary {
        latency_samples: Some(2048),
        latency_p5_ns: Some(71.032_557_917_235_8),
        latency_p10_ns: Some(71.497_673_428_103_6),
        latency_p25_ns: Some(72.009_531_714_377_1),
        latency_p50_ns: Some(72.497_101_386_412_3),
        latency_p75_ns: Some(72.749_631_578_947_4),
        latency_p90_ns: Some(74.502_234_512_998_1),
        latency_p95_ns: Some(76.471_398_215_664_2),
        latency_p99_ns: Some(85.049_877_106_337_9),
        latency_p99_9_ns: Some(112.503_721_941_802_7),
        latency_spread: Some(76.471_398_215_664_2 / 71.032_557_917_235_8),
        ..step(2, 1, "Lat-L1-Read", 48 << 10, 410.373_917_628_441)
    };
    let mut ops = crate::ops::OpTally::default();
    ops.add(Op::SealCheck, Duration::from_nanos(350_123_457), 480 * MIB);
    ops.add(Op::SealFill, Duration::from_nanos(420_987_654), 480 * MIB);
    let seal = CycleSeal {
        seal_ms: 410,
        seal_bytes: 960 * MIB,
        between_steps_ms: 90,
        final_check_ms: 380,
        final_check_bytes: 960 * MIB,
        ..Default::default()
    };
    let mut result = TestRunResult::new(Default::default());
    for cycle in 1..=2 {
        let stuck_bit = |n: usize, mib_s: f64| step(n, 0, "Mem-StuckBit256_A", 480 * MIB, mib_s);
        let steps = vec![stuck_bit(1, 100.6), latency.clone(), TestSummary { ops, ..stuck_bit(3, 101.6) }];
        result.add_cycle(cycle, 31 + cycle, steps, Some(seal.clone()), false);
    }
    result.finalize(Duration::from_secs(63));
    result.set_whea_totals(crate::whea::WheaCounts::default(), true);
    result
}

/// A results file a test wrote (`results_file_finished`, `results_file_never_finished`), in a temp folder of its own, which goes when it
/// drops
pub struct ResultsFile {
    _dir: TempDir,
    pub path: String,
}

impl ResultsFile {
    /// `result` saved as a run saves it, in a temp folder named for `name`
    fn of(name: &str, result: &TestRunResult) -> Self {
        let dir = TempDir::new(name);
        let path = dir.file("TMR_2026-11-02_14-03-07.json");
        result.save_atomic(&path).unwrap_or_else(|e| panic!("{e}"));
        ResultsFile { _dir: dir, path }
    }
}

/// The results file of a run that finished (TODO 72's `--show-results`): `final_summary_saved_and_loaded`'s two clean cycles, completed
/// and passed, by this build (`stamped`)
pub fn results_file_finished() -> ResultsFile {
    let mut result = clean_run();
    stamped(&mut result);
    let (outcome, status) = (crate::progress::RunOutcome::Completed, crate::runner::RunStatus::Passed);
    (result.overall_stats.outcome, result.overall_stats.status) = (Some(outcome.key().into()), Some(status.key().into()));
    ResultsFile::of("show-finished", &result)
}

/// The results file of a run that never finished (TODO 72's `--show-results`): `final_summary_report`'s two cycles, with their errors,
/// saved after the second with the run going on, and by another build (`another_build`)
pub fn results_file_never_finished() -> ResultsFile {
    let mut result = run_with_errors();
    stamped(&mut result);
    result.metadata.build = Some(another_build());
    result.overall_stats.in_progress = true;
    ResultsFile::of("show-never-finished", &result)
}

/// `result` as a run started at 2026-11-02 14:03:07 +10:00 saves it: the start, the file's name and the run ID, which the clock and the OS
/// give a run, fixed for a golden. Its build stays the one it has (`TestRunResult::new` gives this one).
pub(crate) fn stamped(result: &mut TestRunResult) {
    let metadata = &mut result.metadata;
    (metadata.start_time, metadata.start_time_utc) = (1_793_592_187, "2026-11-02 04:03:07 UTC".to_string());
    metadata.start_time_local = "2026-11-02 14:03:07 +10:00".to_string();
    metadata.filename = "results/TMR_2026-11-02_14-03-07.json".to_string();
    metadata.run_id = Some("5f0c3a9e81d24b7c9a6e0d13f2b84c57".to_string());
}

/// A build that isn't this one, as a results file records it: a commit with changes, by a nightly of 2026-10-21
pub(crate) fn another_build() -> crate::results::BuildRecord {
    crate::results::BuildRecord {
        commit: "0d274ef9b6c1a3d5e7f8091a2b3c4d5e6f708192".to_string(),
        dirty: true,
        rustc: "rustc 1.101.0-nightly (1a2c4797e 2026-10-21)".to_string(),
        profile: "release".to_string(),
    }
}
