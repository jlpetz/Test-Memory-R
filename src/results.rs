use crate::progress::TestSummary;
use crate::constants::{BYTES_PER_GIB_F64, MB_F64};
use crate::formatting::{
    serialize_round_2dp, serialize_round_int, serialize_round_opt_2dp,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestRunResult {
    pub metadata: TestRunMetadata,
    /// Which machine produced this result (TODO #67). Filled in by `new()` rather than a setter —
    /// the field this replaced was an all-placeholder `SystemInfoSnapshot` whose setter nothing
    /// ever called, so every result claimed `CPU: Unknown, 0.00 GiB`.
    pub identity: crate::run_context::RunIdentity,
    /// What was actually allocated and executed (TODO #67). Required, and taken by `new()`, so a
    /// saved result can never be missing the context needed to interpret its own numbers.
    pub run_config: crate::run_context::RunConfigSnapshot,
    pub cycles: Vec<CycleResult>,
    pub overall_stats: OverallStats,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestRunMetadata {
    pub tmr_version: String,
    /// Unix timestamp — zone-free, the machine-readable anchor.
    pub start_time: u64,
    /// Start time in UTC. Kept because result files get collected from several machines and
    /// consolidated centrally, where local stamps from different boxes cannot be ordered.
    pub start_time_utc: String,
    /// The same instant in the running machine's local zone, **with its offset** (e.g.
    /// `2026-09-08 14:18:23 +10:00`). This is the one a human matches against `logs/` and against
    /// their own memory of when they ran it; the offset is recorded so it stays unambiguous once
    /// the file leaves this machine, and across DST.
    pub start_time_local: String,
    /// Generated filename. Shares its stem with this run's log file — see
    /// [`crate::run_context::run_file_stem`].
    pub filename: String,
    pub config_name: Option<String>, // If loaded from config
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CycleResult {
    pub cycle_number: u32,
    pub duration_secs: u32,
    pub tests: Vec<TestResult>,
    pub cycle_stats: CycleStats,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestResult {
    pub test_number: usize,
    pub name: String,
    pub duration_ms: u128,
    pub bytes_processed: u64,
    /// Recorded in MiB/s only. A GiB/s field alongside it would be the same measurement stored
    /// twice — every producer computed it as `mib_s / 1024.0` — and rounding the pair to
    /// different scales made the two disagree. Readers that want GiB/s divide by 1024.
    #[serde(serialize_with = "serialize_round_int")]
    pub throughput_mib_s: f64,
    pub errors: u64,
    // OS-reported hardware errors during this test, and the corrected subset (see whea.rs).
    pub whea_total: u64,
    pub whea_corrected: u64,
    // Latency metrics (Some for latency tests, None for other tests)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_samples: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "serialize_round_opt_2dp")]
    pub latency_p5_ns: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "serialize_round_opt_2dp")]
    pub latency_p10_ns: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "serialize_round_opt_2dp")]
    pub latency_p25_ns: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "serialize_round_opt_2dp")]
    pub latency_p50_ns: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "serialize_round_opt_2dp")]
    pub latency_p75_ns: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "serialize_round_opt_2dp")]
    pub latency_p90_ns: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "serialize_round_opt_2dp")]
    pub latency_p95_ns: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "serialize_round_opt_2dp")]
    pub latency_p99_ns: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "serialize_round_opt_2dp")]
    pub latency_p99_9_ns: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "serialize_round_opt_2dp")]
    pub latency_spread: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CycleStats {
    pub total_bytes: u64,
    pub total_errors: u64,
    #[serde(serialize_with = "serialize_round_int")]
    pub avg_throughput_mib_s: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverallStats {
    pub total_runtime_secs: u64,
    pub cycles_completed: u32,
    #[serde(serialize_with = "serialize_round_2dp")]
    pub total_data_processed_gib: f64,
    #[serde(serialize_with = "serialize_round_int")]
    pub overall_throughput_mib_s: f64,
    pub total_errors: u64,
    // Run-level WHEA tally, taken from the monitor's cumulative counters rather than summed from
    // the per-test figures (see `set_whea_totals`).
    pub whea_total: u64,
    pub whea_corrected: u64,
    /// Whether WHEA monitoring was actually running. Without this, `whea_total: 0` is ambiguous
    /// between "no hardware errors" and "we never looked" — which would read as a clean bill of
    /// health it did not earn.
    pub whea_monitored: bool,
    pub per_test_averages: Vec<TestAverage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestAverage {
    pub test_number: usize,
    pub name: String,
    pub avg_duration_ms: u128,
    pub avg_bytes_processed: u64,
    #[serde(serialize_with = "serialize_round_int")]
    pub avg_throughput_mib_s: f64,
    pub total_errors: u64,
    // WHEA counts summed across every cycle this test ran in (totals, not averages — a hardware
    // error is an event count, and averaging it would hide a single-cycle fault).
    pub whea_total: u64,
    pub whea_corrected: u64,
    // Latency metrics (averaged across cycles, Some for latency tests, None for other tests)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_samples: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "serialize_round_opt_2dp")]
    pub latency_p5_ns: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "serialize_round_opt_2dp")]
    pub latency_p10_ns: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "serialize_round_opt_2dp")]
    pub latency_p25_ns: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "serialize_round_opt_2dp")]
    pub latency_p50_ns: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "serialize_round_opt_2dp")]
    pub latency_p75_ns: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "serialize_round_opt_2dp")]
    pub latency_p90_ns: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "serialize_round_opt_2dp")]
    pub latency_p95_ns: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "serialize_round_opt_2dp")]
    pub latency_p99_ns: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "serialize_round_opt_2dp")]
    pub latency_p99_9_ns: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "serialize_round_opt_2dp")]
    pub latency_spread: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestComparison {
    pub baseline: TestRunMetadata,
    pub current: TestRunMetadata,
    pub comparison_time: String,
    pub overall_comparison: OverallComparison,
    pub per_test_comparisons: Vec<TestComparisonResult>,
    /// Hardware and configuration differences between the two runs (TODO #67). Non-empty means the
    /// percentages above are at least partly explained by something other than a code change; any
    /// entry with `invalidates` set means they cannot be attributed at all.
    pub run_differences: Vec<crate::run_context::RunDifference>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverallComparison {
    pub runtime_diff_secs: i64,
    #[serde(serialize_with = "serialize_round_2dp")]
    pub runtime_diff_percent: f64,
    #[serde(serialize_with = "serialize_round_int")]
    pub throughput_diff_mib_s: f64,
    #[serde(serialize_with = "serialize_round_2dp")]
    pub throughput_diff_percent: f64,
    #[serde(serialize_with = "serialize_round_2dp")]
    pub data_processed_diff_gib: f64,
    pub errors_diff: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestComparisonResult {
    pub test_number: usize,
    pub name: String,
    pub duration_diff_ms: i128,
    #[serde(serialize_with = "serialize_round_2dp")]
    pub duration_diff_percent: f64,
    #[serde(serialize_with = "serialize_round_int")]
    pub throughput_diff_mib_s: f64,
    #[serde(serialize_with = "serialize_round_2dp")]
    pub throughput_diff_percent: f64,
    pub bytes_diff: i64,
    pub errors_diff: i64,
}

impl TestRunResult {
    /// Start a result file for this run.
    ///
    /// `run_config` is a constructor argument rather than a setter because its values are only
    /// *observed* facts once allocation has returned and the thread pool exists — and because the
    /// placeholder-snapshot bug this replaced was caused precisely by an optional setter nothing
    /// called. Callers must therefore build it after `ThreadPool::new`.
    pub fn new(run_config: crate::run_context::RunConfigSnapshot) -> Self {
        // Create results directory if it doesn't exist
        if let Err(e) = std::fs::create_dir_all("results") {
            log::warn!("Failed to create results directory: {}", e);
        }

        // Read from `run_start()` rather than sampling the clock here: this runs after allocation
        // and `ThreadPool::new`, so a fresh `now()` would be seconds later than the one that named
        // the log file, leaving the pair unmatchable. See `run_context::run_start`.
        let started = crate::run_context::run_start();
        let filename = format!("results/{}.json", crate::run_context::run_file_stem());

        Self {
            metadata: TestRunMetadata {
                tmr_version: "1.0.0".to_string(),
                start_time: started.timestamp() as u64,
                start_time_utc: started
                    .with_timezone(&chrono::Utc)
                    .format("%Y-%m-%d %H:%M:%S UTC")
                    .to_string(),
                start_time_local: started.format("%Y-%m-%d %H:%M:%S %:z").to_string(),
                filename: filename.clone(),
                config_name: run_config.config_name.clone(),
            },
            // Detected here rather than via a setter, for the same reason as `run_config`.
            identity: crate::run_context::RunIdentity::detect(),
            run_config,
            cycles: Vec::new(),
            overall_stats: OverallStats {
                total_runtime_secs: 0,
                cycles_completed: 0,
                total_data_processed_gib: 0.0,
                overall_throughput_mib_s: 0.0,
                total_errors: 0,
                whea_total: 0,
                whea_corrected: 0,
                whea_monitored: false,
                per_test_averages: Vec::new(),
            },
        }
    }

    pub fn add_cycle(&mut self, cycle_number: u32, duration_secs: u32, test_summaries: Vec<TestSummary>) {
        let total_bytes: u64 = test_summaries.iter().map(|t| t.bytes_processed).sum();
        let total_errors: u64 = test_summaries.iter().map(|t| t.errors).sum();
        let avg_throughput_mib = if duration_secs > 0 {
            (total_bytes as f64 / MB_F64) / duration_secs as f64
        } else {
            0.0
        };

        let tests: Vec<TestResult> = test_summaries.iter().enumerate().map(|(i, summary)| {
            TestResult {
                test_number: i + 1,
                name: summary.name.clone(),
                duration_ms: summary.duration_ms,
                bytes_processed: summary.bytes_processed,
                throughput_mib_s: summary.throughput_mib_s,
                errors: summary.errors,
                whea_total: summary.whea_total,
                whea_corrected: summary.whea_corrected,
                // Copy latency data from TestSummary
                latency_samples: summary.latency_samples,
                latency_p5_ns: summary.latency_p5_ns,
                latency_p10_ns: summary.latency_p10_ns,
                latency_p25_ns: summary.latency_p25_ns,
                latency_p50_ns: summary.latency_p50_ns,
                latency_p75_ns: summary.latency_p75_ns,
                latency_p90_ns: summary.latency_p90_ns,
                latency_p95_ns: summary.latency_p95_ns,
                latency_p99_ns: summary.latency_p99_ns,
                latency_p99_9_ns: summary.latency_p99_9_ns,
                latency_spread: summary.latency_spread,
            }
        }).collect();

        let cycle_result = CycleResult {
            cycle_number,
            duration_secs,
            tests,
            cycle_stats: CycleStats {
                total_bytes,
                total_errors,
                avg_throughput_mib_s: avg_throughput_mib,
            },
        };

        self.cycles.push(cycle_result);
    }

    pub fn finalize(&mut self, total_runtime: std::time::Duration) {
        self.overall_stats.total_runtime_secs = total_runtime.as_secs();
        self.overall_stats.cycles_completed = self.cycles.len() as u32;
        
        let total_bytes: u64 = self.cycles.iter().map(|c| c.cycle_stats.total_bytes).sum();
        self.overall_stats.total_data_processed_gib = total_bytes as f64 / BYTES_PER_GIB_F64;
        
        self.overall_stats.overall_throughput_mib_s = if total_runtime.as_secs() > 0 {
            (total_bytes as f64 / MB_F64) / total_runtime.as_secs() as f64
        } else {
            0.0
        };
        
        self.overall_stats.total_errors = self.cycles.iter().map(|c| c.cycle_stats.total_errors).sum();

        // Calculate per-test averages
        self.calculate_per_test_averages();
    }

    fn calculate_per_test_averages(&mut self) {
        // Aggregation struct to track per-test statistics including latency
        #[derive(Default)]
        struct TestAggregate {
            test_number: usize,
            total_duration_ms: u128,
            total_bytes: u64,
            total_errors: u64,
            total_whea: u64,
            total_whea_corrected: u64,
            // Latency aggregation
            latency_count: u64,  // Number of cycles with latency data
            latency_samples_sum: u64,
            latency_p5_sum: f64,
            latency_p10_sum: f64,
            latency_p25_sum: f64,
            latency_p50_sum: f64,
            latency_p75_sum: f64,
            latency_p90_sum: f64,
            latency_p95_sum: f64,
            latency_p99_sum: f64,
            latency_p99_9_sum: f64,
            latency_spread_sum: f64,
        }

        let mut test_aggregates: HashMap<String, TestAggregate> = HashMap::new();

        for cycle in &self.cycles {
            for test in &cycle.tests {
                let entry = test_aggregates.entry(test.name.clone()).or_default();
                if entry.test_number == 0 {
                    entry.test_number = test.test_number;
                }
                entry.total_duration_ms += test.duration_ms;
                entry.total_bytes += test.bytes_processed;
                entry.total_errors += test.errors;
                entry.total_whea += test.whea_total;
                entry.total_whea_corrected += test.whea_corrected;

                // Aggregate latency data if present
                if let Some(samples) = test.latency_samples {
                    entry.latency_count += 1;
                    entry.latency_samples_sum += samples;
                    entry.latency_p5_sum += test.latency_p5_ns.unwrap_or(0.0);
                    entry.latency_p10_sum += test.latency_p10_ns.unwrap_or(0.0);
                    entry.latency_p25_sum += test.latency_p25_ns.unwrap_or(0.0);
                    entry.latency_p50_sum += test.latency_p50_ns.unwrap_or(0.0);
                    entry.latency_p75_sum += test.latency_p75_ns.unwrap_or(0.0);
                    entry.latency_p90_sum += test.latency_p90_ns.unwrap_or(0.0);
                    entry.latency_p95_sum += test.latency_p95_ns.unwrap_or(0.0);
                    entry.latency_p99_sum += test.latency_p99_ns.unwrap_or(0.0);
                    entry.latency_p99_9_sum += test.latency_p99_9_ns.unwrap_or(0.0);
                    entry.latency_spread_sum += test.latency_spread.unwrap_or(0.0);
                }
            }
        }

        let cycle_count = self.cycles.len() as u128;
        self.overall_stats.per_test_averages = test_aggregates.into_iter().map(|(name, agg)| {
            let avg_duration_ms = agg.total_duration_ms / cycle_count;
            let avg_bytes = agg.total_bytes / cycle_count as u64;
            let avg_throughput_mib = if avg_duration_ms > 0 {
                (avg_bytes as f64 / MB_F64) / (avg_duration_ms as f64 / 1000.0)
            } else {
                0.0
            };

            // Calculate averaged latency metrics if present
            let (latency_samples, latency_p5, latency_p10, latency_p25, latency_p50,
                 latency_p75, latency_p90, latency_p95, latency_p99, latency_p99_9, latency_spread) =
                if agg.latency_count > 0 {
                    let n = agg.latency_count as f64;
                    (Some(agg.latency_samples_sum / agg.latency_count),
                     Some(agg.latency_p5_sum / n),
                     Some(agg.latency_p10_sum / n),
                     Some(agg.latency_p25_sum / n),
                     Some(agg.latency_p50_sum / n),
                     Some(agg.latency_p75_sum / n),
                     Some(agg.latency_p90_sum / n),
                     Some(agg.latency_p95_sum / n),
                     Some(agg.latency_p99_sum / n),
                     Some(agg.latency_p99_9_sum / n),
                     Some(agg.latency_spread_sum / n))
                } else {
                    (None, None, None, None, None, None, None, None, None, None, None)
                };

            TestAverage {
                test_number: agg.test_number,
                name,
                avg_duration_ms,
                avg_bytes_processed: avg_bytes,
                avg_throughput_mib_s: avg_throughput_mib,
                total_errors: agg.total_errors,
                whea_total: agg.total_whea,
                whea_corrected: agg.total_whea_corrected,
                latency_samples,
                latency_p5_ns: latency_p5,
                latency_p10_ns: latency_p10,
                latency_p25_ns: latency_p25,
                latency_p50_ns: latency_p50,
                latency_p75_ns: latency_p75,
                latency_p90_ns: latency_p90,
                latency_p95_ns: latency_p95,
                latency_p99_ns: latency_p99,
                latency_p99_9_ns: latency_p99_9,
                latency_spread,
            }
        }).collect();

        // Sort by test number
        self.overall_stats.per_test_averages.sort_by_key(|t| t.test_number);
    }

    /// Records the run-level WHEA tally.
    ///
    /// Deliberately not summed from the per-test figures: those are deltas around test execution,
    /// so an event logged between tests, during allocation, or after an early exit belongs to the
    /// run but to no single test. The monitor's cumulative counter has all of them. Call after
    /// [`finalize`](Self::finalize) — it does not touch these fields.
    pub fn set_whea_totals(&mut self, whea: crate::whea::WheaCounts, monitored: bool) {
        self.overall_stats.whea_total = whea.total;
        self.overall_stats.whea_corrected = whea.corrected;
        self.overall_stats.whea_monitored = monitored;
    }

    pub fn get_filename(&self) -> &str {
        &self.metadata.filename
    }

    pub fn save_to_file(&self, path: &str) -> Result<(), String> {
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| format!("Failed to serialize test result: {}", e))?;
        
        fs::write(path, json)
            .map_err(|e| format!("Failed to write test result file: {}", e))
    }

    pub fn load_from_file(path: &str) -> Result<Self, String> {
        if !Path::new(path).exists() {
            return Err(format!("Test result file does not exist: {}", path));
        }

        let content = fs::read_to_string(path)
            .map_err(|e| format!("Failed to read test result file: {}", e))?;
        
        serde_json::from_str(&content)
            .map_err(|e| format!("Failed to parse test result JSON: {}", e))
    }

    // Generate text report for CLI output
    pub fn to_text_report(&self) -> String {
        let mut report = String::new();
        
        report.push_str("=== TMR Test Result Report ===\n");
        // Local for the text report: it is read on the machine that produced it. The UTC stamp is
        // in the JSON for whoever consolidates results later.
        report.push_str(&format!("Run Time: {}\n", self.metadata.start_time_local));
        report.push_str(&format!("TMR Version: {}\n", self.metadata.tmr_version));
        if let Some(config) = &self.metadata.config_name {
            report.push_str(&format!("Config: {}\n", config));
        }
        report.push('\n');

        report.push_str("System Information:\n");
        let id = &self.identity;
        report.push_str(&format!("  CPU: {}\n", id.machine.cpu_brand));
        report.push_str(&format!("  Cores: {} physical, {} logical\n",
            id.physical_cores, id.logical_cores));
        report.push_str(&format!("  Cache: L1d {}/core, L2 {}/core, L3 {} ({})\n",
            id.l1d_cache_per_core,
            id.l2_cache_per_core,
            id.l3_cache,
            id.cache_detection_method));
        report.push_str(&format!("  Memory: {:.2} GiB installed, {:.2} GiB OS-visible\n",
            id.installed_memory_gib, id.os_visible_memory_gib));
        if id.memory_modules_reported > 0 {
            // Labelled, because on a VM the module list is a firmware abstraction rather than
            // physical DIMMs — reading it as DIMM identity would be wrong.
            report.push_str(&format!("  Modules: {} × reported, {:.1} GiB total{}\n",
                id.memory_modules_reported,
                id.memory_reported_gib,
                if id.memory_identity_is_physical { "" } else { " (firmware view — virtualized)" }));
        }
        report.push_str(&format!("  SIMD: {}\n", id.simd_capabilities));
        report.push_str(&format!("  TSC: {:.4} GHz\n", id.tsc_frequency_ghz));
        if id.virtualized {
            report.push_str(&format!("  Virtualized: yes ({})\n",
                id.hypervisor.as_deref().unwrap_or("unknown hypervisor")));
        }
        report.push_str(&format!("  Machine ID: {}  Memory ID: {}\n", id.machine_id, id.memory_id));
        report.push('\n');

        // The resolved allocation and thread setup, without which the throughput figures below
        // cannot be compared against anything.
        {
            let cfg = &self.run_config;
            report.push_str("Run Configuration:\n");
            report.push_str(&format!("  Memory spec: {}{}\n",
                cfg.memory.spec,
                if cfg.memory.spec_is_deterministic { "" } else { " (size depends on free memory — not reproducible)" }));
            report.push_str(&format!("  Allocated: {:.3} GiB total, {:.3} GiB/thread across {} block(s)\n",
                cfg.memory.allocated_gib, cfg.memory.per_thread_gib, cfg.memory.blocks_allocated));
            report.push_str(&format!("  Pages: {}\n", cfg.memory.page_mix.describe()));
            report.push_str(&format!("  Threads: {} ({}), {} SMT thread(s)/core\n",
                cfg.threads.thread_count,
                if cfg.threads.pinned { "pinned" } else { "unpinned" },
                cfg.threads.active_threads_per_core));
            if !cfg.threads.numa_distribution.is_empty() {
                let nodes: Vec<String> = cfg.threads.numa_distribution.iter()
                    .map(|n| format!("node{}×{}", n.numa_node, n.threads))
                    .collect();
                report.push_str(&format!("  NUMA: {}\n", nodes.join(" ")));
            }
            report.push_str(&format!("  Backend: {}, channels: {}, error mode: {}\n",
                cfg.memory.backend, cfg.execution.channels, cfg.execution.error_mode));
            if let Some(filter) = &cfg.execution.test_filter {
                report.push_str(&format!("  Test filter: {}\n", filter));
            }
            if let Some(cal) = &cfg.calibration {
                report.push_str(&format!("  Calibration: {} ({} pages, {} tier(s))\n",
                    cal.timestamp, cal.page_size, cal.tiers.len()));
            } else {
                report.push_str("  Calibration: none (cache-relative windows used CPUID sizes)\n");
            }
            report.push_str(&format!("  Command: {}\n", cfg.command_line.join(" ")));
            report.push('\n');
        }

        report.push_str("Overall Results:\n");
        report.push_str(&format!("  Runtime: {}s\n", self.overall_stats.total_runtime_secs));
        report.push_str(&format!("  Cycles: {}\n", self.overall_stats.cycles_completed));
        report.push_str(&format!("  Data Processed: {:.2} GiB\n", self.overall_stats.total_data_processed_gib));
        report.push_str(&format!("  Throughput: {:.1} MiB/s ({:.2} GiB/s)\n", 
            self.overall_stats.overall_throughput_mib_s,
            self.overall_stats.overall_throughput_mib_s / 1024.0));
        report.push_str(&format!("  Total Errors: {}\n", self.overall_stats.total_errors));
        report.push('\n');

        if !self.overall_stats.per_test_averages.is_empty() {
            report.push_str("Per-Test Averages:\n");
            for test in &self.overall_stats.per_test_averages {
                report.push_str(&format!("  {}. {} - {:.1}s, {:.2} GiB @ {:.1} MiB/s ({:.2} GiB/s){}\n",
                    test.test_number,
                    test.name,
                    test.avg_duration_ms as f64 / 1000.0,
                    test.avg_bytes_processed as f64 / BYTES_PER_GIB_F64,
                    test.avg_throughput_mib_s,
                    test.avg_throughput_mib_s / 1024.0,
                    if test.total_errors > 0 { 
                        format!(" [ERRORS: {}]", test.total_errors) 
                    } else { 
                        String::new() 
                    }
                ));
            }
        }

        report
    }
}

pub fn save_test_result(result: &TestRunResult) -> Result<(), String> {
    // Ensure results directory exists
    if let Err(e) = std::fs::create_dir_all("results") {
        log::warn!("Failed to create results directory: {}", e);
    }
    
    result.save_to_file(&result.metadata.filename)
}

pub fn compare_test_results(baseline_path: &str, current_path: &str) -> Result<TestComparison, String> {
    let baseline = TestRunResult::load_from_file(baseline_path)?;
    let current = TestRunResult::load_from_file(current_path)?;

    let comparison_time = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC").to_string();

    // Overall comparison
    let runtime_diff_secs = current.overall_stats.total_runtime_secs as i64 - baseline.overall_stats.total_runtime_secs as i64;
    let runtime_diff_percent = if baseline.overall_stats.total_runtime_secs > 0 {
        (runtime_diff_secs as f64 / baseline.overall_stats.total_runtime_secs as f64) * 100.0
    } else {
        0.0
    };

    let throughput_diff_mib_s = current.overall_stats.overall_throughput_mib_s - baseline.overall_stats.overall_throughput_mib_s;
    let throughput_diff_percent = if baseline.overall_stats.overall_throughput_mib_s > 0.0 {
        (throughput_diff_mib_s / baseline.overall_stats.overall_throughput_mib_s) * 100.0
    } else {
        0.0
    };

    let overall_comparison = OverallComparison {
        runtime_diff_secs,
        runtime_diff_percent,
        throughput_diff_mib_s,
        throughput_diff_percent,
        data_processed_diff_gib: current.overall_stats.total_data_processed_gib - baseline.overall_stats.total_data_processed_gib,
        errors_diff: current.overall_stats.total_errors as i64 - baseline.overall_stats.total_errors as i64,
    };

    // Per-test comparisons
    let mut per_test_comparisons = Vec::new();
    let baseline_tests: HashMap<String, &TestAverage> = baseline.overall_stats.per_test_averages.iter()
        .map(|t| (t.name.clone(), t)).collect();

    for current_test in &current.overall_stats.per_test_averages {
        if let Some(baseline_test) = baseline_tests.get(&current_test.name) {
            let duration_diff_ms = current_test.avg_duration_ms as i128 - baseline_test.avg_duration_ms as i128;
            let duration_diff_percent = if baseline_test.avg_duration_ms > 0 {
                (duration_diff_ms as f64 / baseline_test.avg_duration_ms as f64) * 100.0
            } else {
                0.0
            };

            let throughput_diff_mib_s = current_test.avg_throughput_mib_s - baseline_test.avg_throughput_mib_s;
            let throughput_diff_percent = if baseline_test.avg_throughput_mib_s > 0.0 {
                (throughput_diff_mib_s / baseline_test.avg_throughput_mib_s) * 100.0
            } else {
                0.0
            };

            per_test_comparisons.push(TestComparisonResult {
                test_number: current_test.test_number,
                name: current_test.name.clone(),
                duration_diff_ms,
                duration_diff_percent,
                throughput_diff_mib_s,
                throughput_diff_percent,
                bytes_diff: current_test.avg_bytes_processed as i64 - baseline_test.avg_bytes_processed as i64,
                errors_diff: current_test.total_errors as i64 - baseline_test.total_errors as i64,
            });
        }
    }

    // Identity first, then configuration: a hardware change is the more fundamental explanation,
    // and on a new machine the config differences are usually downstream of it.
    let mut run_differences = crate::run_context::compare_identity(&baseline.identity, &current.identity);
    run_differences.extend(crate::run_context::compare_config(
        &baseline.run_config,
        &current.run_config,
    ));

    Ok(TestComparison {
        baseline: baseline.metadata,
        current: current.metadata,
        comparison_time,
        overall_comparison,
        per_test_comparisons,
        run_differences,
    })
}

impl TestComparison {
    pub fn to_text_report(&self) -> String {
        let mut report = String::new();
        
        report.push_str("=== TMR Test Result Comparison ===\n");
        report.push_str(&format!("Comparison Time: {}\n", self.comparison_time));
        // UTC here, unlike the single-run report above, because the two files being compared may
        // come from different machines in different zones — UTC is the only stamp that makes
        // "which ran first" answerable, and it matches `comparison_time`.
        report.push_str(&format!("Baseline: {} ({})\n", self.baseline.start_time_utc, self.baseline.filename));
        report.push_str(&format!("Current:  {} ({})\n", self.current.start_time_utc, self.current.filename));
        report.push('\n');

        // Deliberately placed *before* the numbers: the whole point of TODO #67 was that a reader
        // who sees "+20% faster" first has already drawn a conclusion by the time they reach a
        // caveat at the bottom.
        report.push_str(&crate::run_context::render_differences(&self.run_differences));

        report.push_str("Overall Performance Changes:\n");
        report.push_str(&format!("  Runtime: {}{} seconds ({:+.1}%)\n", 
            if self.overall_comparison.runtime_diff_secs >= 0 { "+" } else { "" },
            self.overall_comparison.runtime_diff_secs,
            self.overall_comparison.runtime_diff_percent));
        
        report.push_str(&format!("  Throughput: {:+.1} MiB/s ({:+.1}%)\n", 
            self.overall_comparison.throughput_diff_mib_s,
            self.overall_comparison.throughput_diff_percent));
        
        report.push_str(&format!("  Data Processed: {:+.2} GiB\n", 
            self.overall_comparison.data_processed_diff_gib));
        
        if self.overall_comparison.errors_diff != 0 {
            report.push_str(&format!("  Errors: {:+} ({})\n", 
                self.overall_comparison.errors_diff,
                if self.overall_comparison.errors_diff > 0 { "WORSE" } else { "BETTER" }));
        } else {
            report.push_str("  Errors: No change\n");
        }
        report.push('\n');

        if !self.per_test_comparisons.is_empty() {
            report.push_str("Per-Test Performance Changes:\n");
            for test in &self.per_test_comparisons {
                let duration_symbol = if test.duration_diff_ms < 0 { "⬇" } else if test.duration_diff_ms > 0 { "⬆" } else { "=" };
                let throughput_symbol = if test.throughput_diff_mib_s > 0.0 { "⬆" } else if test.throughput_diff_mib_s < 0.0 { "⬇" } else { "=" };
                
                report.push_str(&format!("  {}. {} - Duration: {}{:.1}ms ({:+.1}%) {}, Throughput: {:+.1} MiB/s ({:+.1}%) {}{}\n",
                    test.test_number,
                    test.name,
                    if test.duration_diff_ms >= 0 { "+" } else { "" },
                    test.duration_diff_ms as f64,
                    test.duration_diff_percent,
                    duration_symbol,
                    test.throughput_diff_mib_s,
                    test.throughput_diff_percent,
                    throughput_symbol,
                    if test.errors_diff != 0 { 
                        format!(", Errors: {:+}", test.errors_diff) 
                    } else { 
                        String::new() 
                    }
                ));
            }
        }

        report
    }

    pub fn save_to_file(&self, path: &str) -> Result<(), String> {
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| format!("Failed to serialize comparison: {}", e))?;
        
        fs::write(path, json)
            .map_err(|e| format!("Failed to write comparison file: {}", e))
    }
}

// CLI command for comparing results
pub fn compare_results_command(baseline_path: &str, current_path: &str, output_path: Option<&str>) -> Result<(), String> {
    let comparison = compare_test_results(baseline_path, current_path)?;
    
    println!("{}", comparison.to_text_report());
    
    if let Some(output) = output_path {
        comparison.save_to_file(output)?;
        println!("\nComparison saved to: {}", output);
    }
    
    Ok(())
}
