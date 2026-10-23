//! Goldens of the Reporter's methods, each from a fixed model (TODO 29): the bytes the console gets, the console renderer's text, so the
//! split's move of the renderer can be shown to keep them. The models that take the engine's code to build are the engine's fixtures
//! (`tmr::test_support`): the memory plan, the test plan, the allocation report and the final summary. `report_cycle` has none: nothing
//! calls it (TODO 29 revives it).

use tmr::frontend::{
    CacheInfo, CpuInfo, CpuPerformance, CpuTopologyEntry, CpuTopologyReport, LatencyLevelSummary, LatencyPercentiles,
    LatencyTestSummaryReport, LatencyThreadResult, PerformanceByCpuReport, PerformanceByPhysicalCoreReport, PerformanceByThreadReport,
    PhysicalCorePerformance, SystemInfoReport, ThreadPerformance, ThreadTiming, ThreadTimingReport, TopologySummary, TscCalibrationInfo,
    WheaCounts,
};
use tmr::test_support;

use crate::golden::check;
use crate::render::rendered;

const KIB: u64 = 1 << 10;
const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;

/// A 6-core box with SMT, as `build_system_info_report` makes it.
#[test]
fn system_info() {
    let (result, text) = rendered(|reporter| reporter.report_system_info(&system_info_report()));
    result.unwrap();
    check("report_system_info", &text);
}

/// `system_info`'s: a 6-core box with SMT. Setup's event has it too (`render`'s goldens).
pub(crate) fn system_info_report() -> SystemInfoReport {
    SystemInfoReport {
        cpu_info: CpuInfo {
            brand: "AMD Ryzen 5 8600G w/ Radeon 760M Graphics".to_string(),
            vendor: "AuthenticAMD".to_string(),
            family: 25,
            model: 117,
            stepping: 2,
            physical_cores: 6,
            logical_cores: 12,
            has_hyperthreading: true,
            simd_capabilities: ["SSE2", "AVX", "AVX2", "AVX-512F", "AVX-512BW"].map(String::from).to_vec(),
        },
        cache_info: CacheInfo {
            l1_data_total: 192 * KIB,
            l1_instruction_total: 192 * KIB,
            l2_total: 6 * MIB,
            l3_total: 16 * MIB,
            per_core_l1d: 32 * KIB,
            per_core_l1i: 32 * KIB,
            per_core_l2: MIB,
            cache_line_size: 64,
            detection_method: "CPUID leaf 0x8000001D".to_string(),
        },
        tsc_info: TscCalibrationInfo {
            frequency_ghz: 4.299_874,
            detection_method: "CPUID 0x15".to_string(),
            is_invariant: true,
            confidence_percent: 99.0,
            samples: 12,
            calibration_time_ms: 240,
            std_dev_ghz: 0.000_012,
            converged: true,
        },
    }
}

/// A step's threads, one slower and one with errors, and a WHEA event during the step: the WHEA column shows only then.
#[test]
fn thread_timing() {
    let timing = |thread_id, cpu_id, runtime_ms, deviation_ms, errors| ThreadTiming {
        thread_id,
        cpu_id,
        physical_core_id: cpu_id / 2,
        numa_node: 0,
        runtime_ms,
        deviation_ms,
        data_bytes: 4 * GIB,
        deviation_data_bytes: 0,
        throughput_mib_s: 4096.0 * 1000.0 / runtime_ms as f64,
        deviation_speed_mib_s: 0.0,
        errors,
        cycles_completed: 1,
    };
    let report = ThreadTimingReport {
        average_elapsed_ms: 1250,
        thread_timings: vec![timing(1, 2, 1400, 150, 0), timing(0, 0, 1100, -150, 3)],
        whea: WheaCounts { total: 1, corrected: 1 },
    };
    let (result, text) = rendered(|reporter| reporter.report_thread_timing(&report));
    result.unwrap();
    check("report_thread_timing", &text);
}

/// Unpinned, so the CPU columns are flagged, with no WHEA.
#[test]
fn performance_by_thread() {
    let thread = |thread_id, cpu_id, total_time_ms, total_errors| ThreadPerformance {
        thread_id,
        cpu_id,
        physical_core_id: cpu_id / 2,
        numa_node: 0,
        total_time_ms,
        total_bytes: 24 * GIB,
        throughput_mib_s: 24.0 * 1024.0 * 1000.0 / total_time_ms as f64,
        total_errors,
    };
    let report = PerformanceByThreadReport {
        threads: vec![thread(0, 0, 61_500, 0), thread(1, 2, 63_250, 2)],
        pinned: false,
        whea: WheaCounts::default(),
    };
    let (result, text) = rendered(|reporter| reporter.report_performance_by_thread(&report));
    result.unwrap();
    check("report_performance_by_thread", &text);
}

#[test]
fn performance_by_cpu() {
    let cpu = |cpu_id, total_time_ms| CpuPerformance {
        cpu_id,
        physical_core_id: cpu_id / 2,
        numa_node: 0,
        total_time_ms,
        total_bytes: 24 * GIB + 512 * MIB,
        throughput_mib_s: 24.5 * 1024.0 * 1000.0 / total_time_ms as f64,
        total_errors: 0,
    };
    let report = PerformanceByCpuReport { cpus: vec![cpu(0, 61_500), cpu(2, 3_725_000)], whea: WheaCounts { total: 2, corrected: 1 } };
    let (result, text) = rendered(|reporter| reporter.report_performance_by_cpu(&report));
    result.unwrap();
    check("report_performance_by_cpu", &text);
}

#[test]
fn performance_by_physical_core() {
    let core = |core_id, logical_cpus: &[usize], total_time_ms, total_errors| PhysicalCorePerformance {
        core_id,
        logical_cpus: logical_cpus.to_vec(),
        total_time_ms,
        total_bytes: 48 * GIB,
        throughput_mib_s: 48.0 * 1024.0 * 1000.0 / total_time_ms as f64,
        total_errors,
    };
    let report = PerformanceByPhysicalCoreReport {
        cores: vec![core(0, &[0, 1], 123_000, 0), core(1, &[2, 3], 126_500, 1)],
        whea: WheaCounts::default(),
    };
    let (result, text) = rendered(|reporter| reporter.report_performance_by_physical_core(&report));
    result.unwrap();
    check("report_performance_by_physical_core", &text);
}

/// A hybrid box under `cputype=cores` and `skip-cores=1`: a skipped core, SMT siblings left out, one E-core free.
#[test]
fn cpu_topology() {
    let (result, text) = rendered(|reporter| reporter.report_cpu_topology(&cpu_topology_report()));
    result.unwrap();
    check("report_cpu_topology", &text);
}

/// `cpu_topology`'s: a hybrid box under `cputype=cores` and `skip-cores=1`. Setup's event has it too (`render`'s goldens).
pub(crate) fn cpu_topology_report() -> CpuTopologyReport {
    let cpu = |logical_cpu: usize, core_type: &str, status: &str, thread_id| CpuTopologyEntry {
        logical_cpu,
        physical_core: logical_cpu / 2,
        core_type: core_type.to_string(),
        threads_on_core: 2,
        numa_node: 0,
        is_hyperthreaded: true,
        status: status.to_string(),
        thread_id,
    };
    CpuTopologyReport {
        cpus: vec![
            cpu(0, "P-core", "Skipped", None),
            cpu(1, "P-core", "Skipped", None),
            cpu(2, "P-core", "Assigned", Some(0)),
            cpu(3, "P-core", "SMT Excluded", None),
            cpu(4, "E-core", "Assigned", Some(1)),
            cpu(5, "E-core", "SMT Excluded", None),
            cpu(6, "E-core", "Available", None),
            cpu(7, "E-core", "SMT Excluded", None),
        ],
        summary: TopologySummary {
            is_hybrid: true,
            total_logical: 8,
            total_physical: 4,
            performance_cores: 2,
            efficiency_cores: 2,
            performance_logical: 4,
            efficiency_logical: 4,
            assigned_count: 2,
            available_count: 1,
            skipped_count: 2,
            smt_excluded_count: 3,
        },
        pinned: true,
    }
}

/// `memory=50%` on a 32 GiB box, six threads, each share rounded up to whole GiB
/// (`test_support::consolidated_memory_report`).
#[test]
fn consolidated_memory() {
    let (result, text) = rendered(|reporter| reporter.report_consolidated_memory(&test_support::consolidated_memory_report()));
    result.unwrap();
    check("report_consolidated_memory", &text);
}

/// An unsealed plan at 256 bits with a test run twice: the table and its notes (the cycle, the seal, the width;
/// `test_support::test_configuration_report`). The plan-only golden has a sealed plan (`render/run_tests.rs`).
#[test]
fn test_configuration() {
    let (result, text) = rendered(|reporter| reporter.report_test_configuration(&test_support::test_configuration_report()));
    result.unwrap();
    check("report_test_configuration", &text);
}

/// The 2026-10-16 run on the dev box, one node: 18 × 1 GiB granted, one refused, 80 × 128 MiB (`test_support::allocation_report_one_node`).
#[test]
fn allocation_on_one_node() {
    let (result, text) = rendered(|reporter| reporter.report_allocation(&test_support::allocation_report_one_node()));
    result.unwrap();
    check("report_allocation_one_node", &text);
}

/// Two nodes, node 1 short of 1 GiB pages and one of its threads served from node 0 (`test_support::allocation_report_two_nodes`).
#[test]
fn allocation_on_two_nodes() {
    let (result, text) = rendered(|reporter| reporter.report_allocation(&test_support::allocation_report_two_nodes()));
    result.unwrap();
    check("report_allocation_two_nodes", &text);
}

/// The final summary of two sealed cycles, with every table it has (`test_support::final_summary_report`).
#[test]
fn final_summary() {
    let (result, text) = rendered(|reporter| reporter.report_final_summary(&test_support::final_summary_report()));
    result.unwrap();
    check("report_final_summary", &text);
}

/// Two threads' latencies and the consolidated row.
#[test]
fn latency_summary() {
    let percentiles = |base: f64| LatencyPercentiles {
        min_ns: base - 3.0,
        p5_ns: base - 1.5,
        p10_ns: base - 1.0,
        p25_ns: base - 0.5,
        p50_ns: base,
        p75_ns: base + 0.75,
        p90_ns: base + 2.0,
        p95_ns: base + 4.0,
        p99_ns: base + 12.5,
        p99_9_ns: base + 40.0,
        spread_ratio: (base + 4.0) / (base - 1.5),
    };
    let thread = |thread_id, cpu_id, base| LatencyThreadResult { thread_id, cpu_id, sample_count: 1000, percentiles: percentiles(base) };
    let report = LatencyTestSummaryReport {
        levels_tested: vec![LatencyLevelSummary {
            total_samples: 2000,
            per_thread_results: vec![thread(0, 0, 72.5), thread(1, 2, 74.0)],
            consolidated: percentiles(73.25),
        }],
    };
    let (result, text) = rendered(|reporter| reporter.report_latency_summary(&report));
    result.unwrap();
    check("report_latency_summary", &text);
}
