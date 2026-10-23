//! What the CLI's tests drive the engine with (TODO 29). The console's text is the CLI's
//! (`crates/tmr-cli`), and its goldens show it keeps its bytes; much of what they show is the
//! engine's code at work, which they reach here: a cycle of `execute_test_cycle` on heap regions
//! (`FakeCycle`), the run's entry point on a fixed layout (`GoldenRun`), the detections that log at
//! their first use, and the reports that take the engine's code to build, each from a fixed model.
//! A unit test build has it, and a build with the `test-support` feature, which only tmr-cli's tests
//! turn on (its dev-dependency), so a plain build of tmr.exe or the window has none of it. Cargo
//! builds the engine once a command, with every feature that command's targets ask for: under
//! `cargo build --all-targets` tmr.exe links it too. Ship, benchmark and read the asm of a plain
//! build only (`cargo build --release`, `cargo rustc --release -p tmr-app --bin tmr`).

use std::path::Path;
use std::time::Duration;

use crate::failing_bits::FailingBits;
use crate::memory::allocator::PageSizeLevel::{self, Huge, Large};
use crate::memory::stitched::{Commit, Run, ThreadSpan};
use crate::progress::TestSummary;
pub use crate::reporting::converters::create_thread_timing_report;
use crate::reporting::models::{AllocationReport, ConsolidatedMemoryReport, FinalTestSummaryReport, TestConfigurationReport};
pub use crate::runner::TestRunOverrides;
pub use crate::runner::test_support::{FakeCycle, Flip, GoldenRun};

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;

/// The built-in plan a plain run takes (`plan=stability`)
pub fn default_plan() -> &'static crate::plans::Plan {
    crate::plans::find(crate::plans::DEFAULT).unwrap()
}

/// Runs the detections, which log what they found at their first use, wherever that is: before a
/// capture of what a test prints, so no golden depends on which test ran first.
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

/// Compares `text` with the golden `<dir>/<name>.txt`, or under `TMR_BLESS=1` writes it there. A
/// difference fails with the first line that differs, and the whole text after it. The engine's
/// goldens are in `testdata/console/` (`check`), the CLI's in its crate's.
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

/// An unsealed plan at 256 bits with a test run twice: the table and its notes (the cycle, the
/// seal, the width). Its tests' sizes don't depend on this machine's caches: absolute and full
/// extents, absolute chunks. The CLI's plan-only golden has a sealed plan (`GoldenRun`).
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

/// A span from its requests in order, `(bytes, page, node, refused)`, laid out as the
/// stitched allocator lays them: each at the cursor, granted ones merging into runs.
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

/// The 2026-10-16 run on this box, as its log tallied it: 18 × 1 GiB granted, thread 2's
/// last 1 GiB request refused (1450), 80 × 128 MiB granted.
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

/// Two nodes; node 1 is short of 1 GiB pages, so its threads take more 2 MiB pages, and
/// thread 3 gets 512 MiB of them from node 0, asked for after a refusal on its own node.
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

/// Step `step` of the built-in suite's simplest kind: one pass of `duration_ms` over
/// `bytes_processed`, clean but for `seal_errors`, at 1000 MiB/s when it ran at all.
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
        threads: Vec::new(),
    }
}

/// The final summary of two sealed cycles: a test run twice a cycle, a latency step, a step's
/// errors and bits in the second cycle, a final check's error, the seal's work and a step's, and a
/// WHEA event. Every table it has shows.
pub fn final_summary_report() -> FinalTestSummaryReport {
    use crate::ops::Op;
    use crate::results::{CycleSeal, TestRunResult};
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
    crate::reporting::converters::create_overall_stats_summary_report(&result)
}
