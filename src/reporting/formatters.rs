/// Formatting logic for transforming raw data into presentation-ready format
/// This layer handles all the "how to format" decisions
use super::models::*;
use crate::constants::bytes_to_gib_f64;
use crate::whea::WheaCounts;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::time::Duration;

/// Footer for the tables whose CPU columns are marked ⚠️ when pinning is off.
const UNPINNED_CPU_NOTE: &str = "⚠️ Pinning is off: the OS moves threads between CPUs. \
    A thread's CPU here only chooses the NUMA node its memory comes from.";

/// Trait for formatting report data
pub trait ReportFormatter: Send + Sync {
    /// Format bytes into human-readable string
    fn format_bytes(&self, bytes: u64) -> String;
    
    /// Format bytes with specific precision
    fn format_bytes_precise(&self, bytes: u64, precision: usize) -> String;

    /// Format signed bytes with +/- prefix (eg. "+1.23 GiB" or "-0.50 GiB")
    fn format_bytes_signed(&self, bytes: i64) -> String;

    /// Format duration
    fn format_duration(&self, duration: Duration) -> String;
    
    /// Prepare CPU info table
    fn prepare_cpu_info_table(&self, cpu_info: &CpuInfo) -> TableData;
    
    /// Prepare cache info table
    fn prepare_cache_info_table(&self, cache_info: &CacheInfo) -> TableData;

    /// Prepare TSC calibration info table
    fn prepare_tsc_info_table(&self, tsc_info: &TscCalibrationInfo) -> TableData;

    /// Prepare thread timing deviation table
    fn prepare_thread_timing_table(&self, report: &ThreadTimingReport) -> TableData;
    
    /// Prepare thread allocation table
    fn prepare_thread_allocation_table(&self, report: &ThreadAllocationReport) -> TableData;
    
    /// Prepare performance by thread table
    fn prepare_performance_by_thread_table(&self, report: &PerformanceByThreadReport) -> TableData;
    
    /// Prepare performance by CPU table
    fn prepare_performance_by_cpu_table(&self, report: &PerformanceByCpuReport) -> TableData;
    
    /// Prepare performance by physical core table
    fn prepare_performance_by_physical_core_table(&self, report: &PerformanceByPhysicalCoreReport) -> TableData;
    
    /// Prepare CPU topology table
    fn prepare_cpu_topology_table(&self, report: &CpuTopologyReport) -> TableData;
    
    /// Prepare consolidated memory report table
    fn prepare_consolidated_memory_table(&self, report: &ConsolidatedMemoryReport) -> TableData;
    
    /// Format the extent mode, with the calculated size for cache targets
    fn format_extent_mode_with_size(&self, mode: &crate::tests::ExtentMode, cache_info: &crate::cache::CacheInfo, thread_count: usize) -> String;
    fn format_chunk_mode(&self, mode: &crate::tests::ChunkMode) -> String;
    /// Format chunk mode, appending the resolved byte size for `Cache` targets.
    ///
    /// `Cache` specs hide a real behavioural detail: `L1`/`L2` divide by *active threads per
    /// core*, so `Cache (L2)` resolves to a different size with and without SMT. Absolute
    /// specs are self-explanatory and are left as-is.
    fn format_chunk_mode_with_size(&self, mode: &crate::tests::ChunkMode, cache_info: &crate::cache::CacheInfo, thread_count: usize) -> String;

    /// Prepare test configuration table
    fn prepare_test_configuration_table(&self, report: &TestConfigurationReport) -> TableData;
    
    /// Prepare cycle report table
    #[expect(dead_code, reason = "TODO #29: the per-cycle report is to be revived, not deleted (from TODO #69 F)")]
    fn prepare_cycle_report_table(&self, report: &CycleReport) -> TableData;
    
    /// Prepare final test summary tables
    fn prepare_final_summary_overview_table(&self, report: &FinalTestSummaryReport) -> TableData;
    fn prepare_final_summary_performance_table(&self, report: &FinalTestSummaryReport) -> TableData;
    fn prepare_errors_by_step_table(&self, report: &FinalTestSummaryReport) -> TableData;
    
    /// Prepare block allocation report tables (Option B layout - separate tables)
    fn prepare_block_size_distribution_table(&self, report: &BlockAllocationReport) -> TableData;
    fn prepare_page_type_summary_table(&self, report: &BlockAllocationReport) -> TableData;
    fn prepare_thread_block_allocation_table(&self, report: &BlockAllocationReport) -> TableData;
    fn prepare_numa_distribution_table(&self, report: &BlockAllocationReport) -> TableData;
    fn prepare_allocation_fairness_table(&self, report: &BlockAllocationReport) -> TableData;

    /// Prepare latency test summary tables (multi-threaded results)
    fn prepare_latency_per_thread_table(&self, level: &LatencyLevelSummary) -> TableData;
}

/// Default formatter implementation
pub struct DefaultFormatter {
    use_binary_units: bool,
}

impl Default for DefaultFormatter {
    fn default() -> Self {
        Self::new()
    }
}

impl DefaultFormatter {
    pub fn new() -> Self {
        Self {
            use_binary_units: true,
        }
    }

    /// The `Pass` cell: the verdict for a row, and the only place in a table that uses an emoji.
    ///
    /// Error counts elsewhere are plain numbers. Putting the verdict in one dedicated column keeps
    /// the glyph meaning single: ✅/❌ is pass/fail, whereas the 🔴/🟢 on the per-test topline marks
    /// *which limit ended the test*. (The previous convention rendered zero errors as ✅ inside the
    /// error columns and non-zero as a bare number, so failures had less visual weight than passes
    /// and ❌ never appeared at all.)
    fn pass_cell(&self, passed: bool) -> String {
        if passed { "✅" } else { "❌" }.to_string()
    }

    /// Adds the trailing `Errors | WHEA | Pass` headers shared by every table that carries a verdict.
    ///
    /// The `WHEA` column exists only when the table has something to put in it, so a healthy run's
    /// tables look exactly as they did before WHEA monitoring existed. Every table uses this, so the
    /// three columns have the same names, order and alignment everywhere.
    fn with_verdict_headers(&self, table: TableData, has_whea: bool) -> TableData {
        let table = table.add_header("Errors", ColumnAlignment::Right);
        let table = if has_whea { table.add_header("WHEA", ColumnAlignment::Right) } else { table };
        table.add_header("Pass", ColumnAlignment::Center)
    }

    /// Appends the `Errors | WHEA | Pass` cells matching [`Self::with_verdict_headers`].
    ///
    /// `whea` is `None` on per-thread/CPU/core rows: WHEA events are system-wide and nothing in them
    /// identifies the core that faulted, so those rows get a blank WHEA cell (the same way the
    /// aggregate row leaves its location columns blank) and a data-errors-only verdict. The
    /// aggregate row passes `Some`, as does any per-test row — a per-test count *is* attributable,
    /// being the delta across that test. WHEA and data errors stay in separate columns rather than
    /// being summed: a combined figure over a column of per-row zeros would not add up.
    fn push_verdict_cells(&self, row: &mut Vec<String>, errors: u64, whea: Option<WheaCounts>, has_whea: bool) {
        row.push(errors.to_string());
        if has_whea {
            row.push(whea.map(|w| w.to_string()).unwrap_or_default());
        }
        let whea_total = whea.map_or(0, |w| w.total);
        row.push(self.pass_cell(errors == 0 && whea_total == 0));
    }
}

impl ReportFormatter for DefaultFormatter {
    fn format_bytes(&self, bytes: u64) -> String {
        self.format_bytes_precise(bytes, 2)
    }
    
    fn format_bytes_precise(&self, bytes: u64, precision: usize) -> String {
        let (value, unit) = if self.use_binary_units {
            // Binary units (GiB, MiB, KiB)
            if bytes >= 1024_u64.pow(3) {
                (bytes as f64 / 1024_f64.powi(3), "GiB")
            } else if bytes >= 1024_u64.pow(2) {
                (bytes as f64 / 1024_f64.powi(2), "MiB")
            } else if bytes >= 1024 {
                (bytes as f64 / 1024_f64, "KiB")
            } else {
                (bytes as f64, "B")
            }
        } else {
            // Decimal units (GB, MB, KB)
            if bytes >= 1000_u64.pow(3) {
                (bytes as f64 / 1000_f64.powi(3), "GB")
            } else if bytes >= 1000_u64.pow(2) {
                (bytes as f64 / 1000_f64.powi(2), "MB")
            } else if bytes >= 1000 {
                (bytes as f64 / 1000_f64, "KB")
            } else {
                (bytes as f64, "B")
            }
        };
        
        format!("{:.precision$} {}", value, unit, precision = precision)
    }

    fn format_bytes_signed(&self, bytes: i64) -> String {
        let sign = if bytes >= 0 { "+" } else { "" };  // Negative sign is automatic
        let abs_bytes = bytes.unsigned_abs();
        let (value, unit) = if self.use_binary_units {
            // Binary units (GiB, MiB, KiB)
            if abs_bytes >= 1024_u64.pow(3) {
                (bytes as f64 / 1024_f64.powi(3), "GiB")
            } else if abs_bytes >= 1024_u64.pow(2) {
                (bytes as f64 / 1024_f64.powi(2), "MiB")
            } else if abs_bytes >= 1024 {
                (bytes as f64 / 1024_f64, "KiB")
            } else {
                (bytes as f64, "B")
            }
        } else {
            // Decimal units (GB, MB, KB)
            if abs_bytes >= 1000_u64.pow(3) {
                (bytes as f64 / 1000_f64.powi(3), "GB")
            } else if abs_bytes >= 1000_u64.pow(2) {
                (bytes as f64 / 1000_f64.powi(2), "MB")
            } else if abs_bytes >= 1000 {
                (bytes as f64 / 1000_f64, "KB")
            } else {
                (bytes as f64, "B")
            }
        };

        if bytes >= 0 {
            format!("{}{:.2} {}", sign, value, unit)
        } else {
            format!("{:.2} {}", value, unit)  // Negative sign already in value
        }
    }

    fn format_duration(&self, duration: Duration) -> String {
        let total_secs = duration.as_secs();
        let hours = total_secs / 3600;
        let minutes = (total_secs % 3600) / 60;
        let seconds = total_secs % 60;
        let millis = duration.subsec_millis();
        
        if hours > 0 {
            format!("{:02}:{:02}:{:02}", hours, minutes, seconds)
        } else if minutes > 0 {
            format!("{:02}:{:02}.{:03}", minutes, seconds, millis)
        } else {
            format!("{}.{:03}s", seconds, millis)
        }
    }
    
    fn prepare_cpu_info_table(&self, cpu_info: &CpuInfo) -> TableData {
        TableData::new()
            .with_title("CPU Information")
            .add_header("Property", ColumnAlignment::Left)
            .add_header("Value", ColumnAlignment::Left)
            .add_row(vec![
                "CPU".to_string(),
                format!("{} ({})", cpu_info.brand, cpu_info.vendor),
            ])
            .add_row(vec![
                "Architecture".to_string(),
                format!("Family {}, Model {}, Stepping {}", 
                    cpu_info.family, cpu_info.model, cpu_info.stepping),
            ])
            .add_row(vec![
                "Cores".to_string(),
                format!("{} physical, {} logical{}", 
                    cpu_info.physical_cores,
                    cpu_info.logical_cores,
                    if cpu_info.has_hyperthreading { " (HT enabled)" } else { "" }
                ),
            ])
            .add_row(vec![
                "SIMD Support".to_string(),
                cpu_info.simd_capabilities.join(", "),
            ])
    }
    
    fn prepare_cache_info_table(&self, cache_info: &CacheInfo) -> TableData {
        TableData::new()
            .with_title(format!("Cache Architecture ({})", cache_info.detection_method))
            .add_header("Cache Level", ColumnAlignment::Left)
            .add_header("Total Size", ColumnAlignment::Right)
            .add_header("Per Core", ColumnAlignment::Right)
            .add_row(vec![
                "L1 Data".to_string(),
                self.format_bytes(cache_info.l1_data_total),
                self.format_bytes(cache_info.per_core_l1d),
            ])
            .add_row(vec![
                "L1 Instruction".to_string(),
                self.format_bytes(cache_info.l1_instruction_total),
                self.format_bytes(cache_info.per_core_l1i),
            ])
            .add_row(vec![
                "L2".to_string(),
                self.format_bytes(cache_info.l2_total),
                self.format_bytes(cache_info.per_core_l2),
            ])
            .add_row(vec![
                "L3 (Shared)".to_string(),
                self.format_bytes(cache_info.l3_total),
                "-".to_string(),
            ])
            .add_row(vec![
                "Cache Line Size".to_string(),
                format!("{} bytes", cache_info.cache_line_size),
                "-".to_string(),
            ])
    }

    fn prepare_tsc_info_table(&self, tsc_info: &TscCalibrationInfo) -> TableData {
        // Use clear status indicators: ✅ = good, ❌ = bad/unreliable
        let converge_status = if tsc_info.converged {
            "✅ Converged"
        } else {
            "❌ Timed out (not converged)"
        };

        // Invariant TSC = constant rate (good for timing)
        // Variable TSC = rate changes with CPU state (unreliable for timing!)
        let invariant_str = if tsc_info.is_invariant {
            "✅ Invariant (constant rate)"
        } else {
            "❌ Variable (unreliable for timing!)"
        };

        // Round to nearest MHz for "likely" frequency display
        let freq_mhz = tsc_info.frequency_ghz * 1000.0;
        let rounded_mhz = freq_mhz.round();
        let rounded_ghz = rounded_mhz / 1000.0;

        TableData::new()
            .with_title("TSC Calibration")
            .add_header("Property", ColumnAlignment::Left)
            .add_header("Value", ColumnAlignment::Right)
            .add_row(vec![
                "Measured Freq".to_string(),
                format!("{:.6} GHz", tsc_info.frequency_ghz),
            ])
            .add_row(vec![
                "Likely Freq".to_string(),
                format!("{:.0} MHz ({:.3} GHz)", rounded_mhz, rounded_ghz),
            ])
            .add_row(vec![
                "Detection Method".to_string(),
                tsc_info.detection_method.clone(),
            ])
            .add_row(vec![
                "TSC Type".to_string(),
                invariant_str.to_string(),
            ])
            .add_row(vec![
                "Confidence".to_string(),
                format!("{:.0}%", tsc_info.confidence_percent),
            ])
            .add_row(vec![
                "Calibration".to_string(),
                format!("{} samples in {}ms ({})", tsc_info.samples, tsc_info.calibration_time_ms, converge_status),
            ])
            .add_row(vec![
                "Std Deviation".to_string(),
                format!("{:.6} GHz", tsc_info.std_dev_ghz),
            ])
    }

    fn prepare_thread_timing_table(&self, report: &ThreadTimingReport) -> TableData {
        let has_whea = report.whea.total > 0;
        let mut table = TableData::new()
            .add_header("Thread", ColumnAlignment::Right)
            .add_header("L CPU", ColumnAlignment::Right)
            .add_header("P Core", ColumnAlignment::Right)
            .add_header("NUMA", ColumnAlignment::Center)
            .add_header("Time", ColumnAlignment::Right)
            .add_header("Dev T", ColumnAlignment::Right)
            .add_header("Data", ColumnAlignment::Right)
            .add_header("Dev D", ColumnAlignment::Right)
            .add_header("Speed", ColumnAlignment::Right)
            .add_header("Dev S", ColumnAlignment::Right)
            .add_header("Cycles", ColumnAlignment::Right);
        table = self.with_verdict_headers(table, has_whea);

        for timing in &report.thread_timings {
            let deviation_time_str = if timing.deviation_ms >= 0 {
                format!("+{:.1}s", timing.deviation_ms as f64 / 1000.0)
            } else {
                format!("{:.1}s", timing.deviation_ms as f64 / 1000.0)
            };

            let deviation_data_str = if timing.deviation_data_bytes >= 0 {
                format!("+{}", self.format_bytes(timing.deviation_data_bytes as u64))
            } else {
                format!("-{}", self.format_bytes(timing.deviation_data_bytes.unsigned_abs()))
            };

            let deviation_speed_str = if timing.deviation_speed_mib_s >= 0.0 {
                format!("+{:.1}", timing.deviation_speed_mib_s)
            } else {
                format!("{:.1}", timing.deviation_speed_mib_s)
            };

            let mut row = vec![
                timing.thread_id.to_string(),
                format!("{}", timing.cpu_id),
                format!("{}", timing.physical_core_id),
                format!("{}", timing.numa_node),
                format!("{:.1}s", timing.runtime_ms as f64 / 1000.0),
                deviation_time_str,
                self.format_bytes(timing.data_bytes),
                deviation_data_str,
                format!("{:.1} MiB/s", timing.throughput_mib_s),
                deviation_speed_str,
                timing.cycles_completed.to_string(),
            ];
            self.push_verdict_cells(&mut row, timing.errors, None, has_whea);
            table = table.add_row(row);
        }

        // Add average row
        if !report.thread_timings.is_empty() {
            let avg_runtime = report.average_elapsed_ms;
            let total_data: u64 = report.thread_timings.iter().map(|t| t.data_bytes).sum();
            let avg_data = total_data / report.thread_timings.len() as u64;
            let avg_speed = report.thread_timings.iter()
                .map(|t| t.throughput_mib_s)
                .sum::<f64>() / report.thread_timings.len() as f64;
            let total_errors: u64 = report.thread_timings.iter().map(|t| t.errors).sum();
            let avg_cycles = report.thread_timings.iter()
                .map(|t| t.cycles_completed as f64)
                .sum::<f64>() / report.thread_timings.len() as f64;

            let mut row = vec![
                "Avg".to_string(),
                "".to_string(),
                "".to_string(),
                "".to_string(),
                format!("{:.1}s", avg_runtime as f64 / 1000.0),
                "".to_string(),
                self.format_bytes(avg_data),
                "".to_string(),
                format!("{:.1} MiB/s", avg_speed),
                "".to_string(),
                format!("{:.1}", avg_cycles), // Cycles column - show average
            ];
            self.push_verdict_cells(&mut row, total_errors, Some(report.whea), has_whea);
            table = table.add_row(row);
        }

        table
    }

    fn prepare_thread_allocation_table(&self, report: &ThreadAllocationReport) -> TableData {
        let mut table = TableData::new()
            .with_title("Per-Thread Allocation Breakdown")
            .add_header("Thread", ColumnAlignment::Right)
            .add_header("Total Size", ColumnAlignment::Right)
            .add_header("Huge #", ColumnAlignment::Right)
            .add_header("Huge GiB", ColumnAlignment::Right)
            .add_header("Large #", ColumnAlignment::Right)
            .add_header("Large GiB", ColumnAlignment::Right)
            .add_header("Regular #", ColumnAlignment::Right)
            .add_header("Regular GiB", ColumnAlignment::Right);
        
        for alloc in &report.allocations {
            table = table.add_row(vec![
                alloc.thread_id.to_string(),
                self.format_bytes(alloc.total_size_bytes),
                if alloc.huge_pages_count > 0 {
                    alloc.huge_pages_count.to_string()
                } else {
                    "-".to_string()
                },
                if alloc.huge_pages_count > 0 {
                    format!("{:.2}", alloc.huge_pages_count as f64)
                } else {
                    "-".to_string()
                },
                if alloc.large_pages_count > 0 {
                    alloc.large_pages_count.to_string()
                } else {
                    "-".to_string()
                },
                if alloc.large_pages_count > 0 {
                    format!("{:.2}", (alloc.large_pages_count * 2) as f64 / 1024.0)
                } else {
                    "-".to_string()
                },
                if alloc.regular_pages_count > 0 {
                    alloc.regular_pages_count.to_string()
                } else {
                    "-".to_string()
                },
                if alloc.regular_pages_count > 0 {
                    format!("{:.2}", (alloc.regular_pages_count * 4) as f64 / (1024.0 * 1024.0))
                } else {
                    "-".to_string()
                },
            ]);
        }
        
        table
    }
    
    fn prepare_performance_by_thread_table(&self, report: &PerformanceByThreadReport) -> TableData {
        let has_whea = report.whea.total > 0;
        let cpu_mark = if report.pinned { "" } else { " ⚠️" };
        let mut table = TableData::new()
            .with_title("Performance by Thread")
            .add_header("Thread", ColumnAlignment::Right)
            .add_header(format!("L CPU{}", cpu_mark), ColumnAlignment::Right)
            .add_header(format!("P Core{}", cpu_mark), ColumnAlignment::Right)
            .add_header("NUMA", ColumnAlignment::Center)
            .add_header("Time", ColumnAlignment::Right)
            .add_header("Dev T", ColumnAlignment::Right)
            .add_header("Data", ColumnAlignment::Right)
            .add_header("Dev D", ColumnAlignment::Right)
            .add_header("Speed", ColumnAlignment::Right)
            .add_header("Dev S", ColumnAlignment::Right);
        table = self.with_verdict_headers(table, has_whea);

        // Calculate averages for variance
        let avg_time_ms = if !report.threads.is_empty() {
            report.threads.iter().map(|t| t.total_time_ms).sum::<u128>() / report.threads.len() as u128
        } else {
            0
        };
        let avg_bytes = if !report.threads.is_empty() {
            report.threads.iter().map(|t| t.total_bytes).sum::<u64>() / report.threads.len() as u64
        } else {
            0
        };
        let avg_speed = if !report.threads.is_empty() {
            report.threads.iter().map(|t| t.throughput_mib_s).sum::<f64>() / report.threads.len() as f64
        } else {
            0.0
        };

        for thread in &report.threads {
            let dev_time_ms = thread.total_time_ms as i128 - avg_time_ms as i128;
            let dev_bytes = thread.total_bytes as i64 - avg_bytes as i64;
            let dev_speed = thread.throughput_mib_s - avg_speed;

            let mut row = vec![
                thread.thread_id.to_string(),
                format!("{}", thread.cpu_id),
                format!("{}", thread.physical_core_id),
                format!("{}", thread.numa_node),
                self.format_duration(Duration::from_millis(thread.total_time_ms as u64)),
                format!("{:+.1}s", dev_time_ms as f64 / 1000.0),
                self.format_bytes(thread.total_bytes),
                self.format_bytes_signed(dev_bytes),
                format!("{:.1} MiB/s", thread.throughput_mib_s),
                format!("{:+.1}", dev_speed),
            ];
            self.push_verdict_cells(&mut row, thread.total_errors, None, has_whea);
            table = table.add_row(row);
        }

        // Add average row
        if !report.threads.is_empty() {
            let total_errors: u64 = report.threads.iter().map(|t| t.total_errors).sum();
            let mut row = vec![
                "Avg".to_string(),
                "".to_string(),
                "".to_string(),
                "".to_string(),
                self.format_duration(Duration::from_millis(avg_time_ms as u64)),
                "".to_string(),
                self.format_bytes(avg_bytes),
                "".to_string(),
                format!("{:.1} MiB/s", avg_speed),
                "".to_string(),
            ];
            self.push_verdict_cells(&mut row, total_errors, Some(report.whea), has_whea);
            table = table.add_row(row);
        }

        if !report.pinned {
            table = table.with_footer(UNPINNED_CPU_NOTE);
        }
        table
    }

    fn prepare_performance_by_cpu_table(&self, report: &PerformanceByCpuReport) -> TableData {
        let has_whea = report.whea.total > 0;
        let mut table = TableData::new()
            .with_title("Performance by CPU")
            .add_header("L CPU", ColumnAlignment::Right)
            .add_header("P Core", ColumnAlignment::Right)
            .add_header("NUMA", ColumnAlignment::Center)
            .add_header("Time", ColumnAlignment::Right)
            .add_header("Dev T", ColumnAlignment::Right)
            .add_header("Data", ColumnAlignment::Right)
            .add_header("Dev D", ColumnAlignment::Right)
            .add_header("Speed", ColumnAlignment::Right)
            .add_header("Dev S", ColumnAlignment::Right);
        table = self.with_verdict_headers(table, has_whea);

        // Calculate averages for variance
        let avg_time_ms = if !report.cpus.is_empty() {
            report.cpus.iter().map(|c| c.total_time_ms).sum::<u128>() / report.cpus.len() as u128
        } else {
            0
        };
        let avg_bytes = if !report.cpus.is_empty() {
            report.cpus.iter().map(|c| c.total_bytes).sum::<u64>() / report.cpus.len() as u64
        } else {
            0
        };
        let avg_speed = if !report.cpus.is_empty() {
            report.cpus.iter().map(|c| c.throughput_mib_s).sum::<f64>() / report.cpus.len() as f64
        } else {
            0.0
        };

        for cpu in &report.cpus {
            let dev_time_ms = cpu.total_time_ms as i128 - avg_time_ms as i128;
            let dev_bytes = cpu.total_bytes as i64 - avg_bytes as i64;
            let dev_speed = cpu.throughput_mib_s - avg_speed;

            let mut row = vec![
                format!("{}", cpu.cpu_id),
                format!("{}", cpu.physical_core_id),
                format!("{}", cpu.numa_node),
                self.format_duration(Duration::from_millis(cpu.total_time_ms as u64)),
                format!("{:+.1}s", dev_time_ms as f64 / 1000.0),
                self.format_bytes(cpu.total_bytes),
                self.format_bytes_signed(dev_bytes),
                format!("{:.1} MiB/s", cpu.throughput_mib_s),
                format!("{:+.1}", dev_speed),
            ];
            self.push_verdict_cells(&mut row, cpu.total_errors, None, has_whea);
            table = table.add_row(row);
        }

        // Add average row
        if !report.cpus.is_empty() {
            let total_errors: u64 = report.cpus.iter().map(|c| c.total_errors).sum();
            let mut row = vec![
                "Avg".to_string(),
                "".to_string(),
                "".to_string(),
                self.format_duration(Duration::from_millis(avg_time_ms as u64)),
                "".to_string(),
                self.format_bytes(avg_bytes),
                "".to_string(),
                format!("{:.1} MiB/s", avg_speed),
                "".to_string(),
            ];
            self.push_verdict_cells(&mut row, total_errors, Some(report.whea), has_whea);
            table = table.add_row(row);
        }

        table
    }

    fn prepare_performance_by_physical_core_table(&self, report: &PerformanceByPhysicalCoreReport) -> TableData {
        let has_whea = report.whea.total > 0;
        let mut table = TableData::new()
            .with_title("Performance by Physical Core")
            .add_header("P Core", ColumnAlignment::Center)
            .add_header("L CPUs", ColumnAlignment::Center)
            .add_header("Time", ColumnAlignment::Right)
            .add_header("Dev T", ColumnAlignment::Right)
            .add_header("Data", ColumnAlignment::Right)
            .add_header("Dev D", ColumnAlignment::Right)
            .add_header("Speed", ColumnAlignment::Right)
            .add_header("Dev S", ColumnAlignment::Right);
        table = self.with_verdict_headers(table, has_whea);

        // Calculate averages for variance
        let avg_time_ms = if !report.cores.is_empty() {
            report.cores.iter().map(|c| c.total_time_ms).sum::<u128>() / report.cores.len() as u128
        } else {
            0
        };
        let avg_bytes = if !report.cores.is_empty() {
            report.cores.iter().map(|c| c.total_bytes).sum::<u64>() / report.cores.len() as u64
        } else {
            0
        };
        let avg_speed = if !report.cores.is_empty() {
            report.cores.iter().map(|c| c.throughput_mib_s).sum::<f64>() / report.cores.len() as f64
        } else {
            0.0
        };

        for core in &report.cores {
            let cpu_list = core.logical_cpus.iter()
                .map(|cpu| cpu.to_string())
                .collect::<Vec<_>>()
                .join(",");

            let dev_time_ms = core.total_time_ms as i128 - avg_time_ms as i128;
            let dev_bytes = core.total_bytes as i64 - avg_bytes as i64;
            let dev_speed = core.throughput_mib_s - avg_speed;

            let mut row = vec![
                format!("{}", core.core_id),
                cpu_list,
                self.format_duration(Duration::from_millis(core.total_time_ms as u64)),
                format!("{:+.1}s", dev_time_ms as f64 / 1000.0),
                self.format_bytes(core.total_bytes),
                self.format_bytes_signed(dev_bytes),
                format!("{:.1} MiB/s", core.throughput_mib_s),
                format!("{:+.1}", dev_speed),
            ];
            self.push_verdict_cells(&mut row, core.total_errors, None, has_whea);
            table = table.add_row(row);
        }

        // Add average row
        if !report.cores.is_empty() {
            let total_errors: u64 = report.cores.iter().map(|c| c.total_errors).sum();
            let mut row = vec![
                "Avg".to_string(),
                "".to_string(),
                self.format_duration(Duration::from_millis(avg_time_ms as u64)),
                "".to_string(),
                self.format_bytes(avg_bytes),
                "".to_string(),
                format!("{:.1} MiB/s", avg_speed),
                "".to_string(),
            ];
            self.push_verdict_cells(&mut row, total_errors, Some(report.whea), has_whea);
            table = table.add_row(row);
        }

        table
    }

    fn prepare_cpu_topology_table(&self, report: &CpuTopologyReport) -> TableData {
        let mut table = TableData::new()
            .with_title("CPU Topology and Thread Assignment")
            .add_header("Logical CPU", ColumnAlignment::Right)
            .add_header("Physical Core", ColumnAlignment::Right)
            .add_header("Type", ColumnAlignment::Center)
            .add_header("Threads", ColumnAlignment::Center)
            .add_header("NUMA Node", ColumnAlignment::Right)
            .add_header("SMT", ColumnAlignment::Center)
            .add_header("Status", ColumnAlignment::Center)
            .add_header(if report.pinned { "Thread ID" } else { "Thread ID ⚠️" }, ColumnAlignment::Right);
        
        for cpu in &report.cpus {
            table = table.add_row(vec![
                format!("{}", cpu.logical_cpu),
                format!("{}", cpu.physical_core),
                cpu.core_type.clone(),
                format!("{}", cpu.threads_on_core),
                format!("{}", cpu.numa_node),
                if cpu.is_hyperthreaded { "Yes" } else { "No" }.to_string(),
                cpu.status.clone(),
                cpu.thread_id.map(|id| id.to_string()).unwrap_or_else(|| "-".to_string()),
            ]);
        }
        
        // Add summary as footer
        let mut summary_parts = vec![
            format!("Total: {} logical CPUs, {} physical cores", 
                report.summary.total_logical, report.summary.total_physical),
        ];
        
        if report.summary.is_hybrid {
            summary_parts.push("Hybrid CPU Architecture Detected!".to_string());
            
            if report.summary.performance_cores > 0 && report.summary.efficiency_cores > 0 {
                summary_parts.push(format!(
                    "P-cores: {} ({} logical), E-cores: {} ({} logical)",
                    report.summary.performance_cores,
                    report.summary.performance_logical,
                    report.summary.efficiency_cores,
                    report.summary.efficiency_logical
                ));
            }
        }
        
        if report.summary.smt_excluded_count > 0 {
            summary_parts.push(format!(
                "Assigned: {}, Available: {}, SMT Excluded: {}, Skipped: {}",
                report.summary.assigned_count,
                report.summary.available_count,
                report.summary.smt_excluded_count,
                report.summary.skipped_count
            ));
        } else {
            summary_parts.push(format!(
                "Assigned: {}, Available: {}, Skipped: {}",
                report.summary.assigned_count,
                report.summary.available_count,
                report.summary.skipped_count
            ));
        }
        if !report.pinned {
            summary_parts.push(UNPINNED_CPU_NOTE.to_string());
        }

        table.with_footer(summary_parts.join("\n"))
    }
    
    
    fn prepare_consolidated_memory_table(&self, report: &ConsolidatedMemoryReport) -> TableData {
        let mut table = TableData::new()
            .with_title("Memory Status & Allocation Plan")
            .add_header("Memory Component", ColumnAlignment::Left)
            .add_header("Amount", ColumnAlignment::Right)
            .add_header("Relative", ColumnAlignment::Right)
            .add_header("Relative Info", ColumnAlignment::Left);
        
        // System memory overview
        table = table
            .add_row(vec![
                "Total Installed RAM".to_string(),
                self.format_bytes(report.total_installed_bytes),
                "-".to_string(),
                "Hardware capacity".to_string(),
            ])
            .add_row(vec![
                "Total Physical RAM".to_string(),
                self.format_bytes(report.total_physical_bytes),
                "-".to_string(),
                "OS available".to_string(),
            ])
            .add_row(vec![
                "Currently Available".to_string(),
                self.format_bytes(report.available_physical_bytes),
                format!("{:.1}%", (report.available_physical_bytes as f64 / report.total_physical_bytes as f64) * 100.0),
                "free".to_string(),
            ])
            .add_row(vec![
                "Currently Used".to_string(),
                self.format_bytes(report.used_physical_bytes),
                format!("{}.0%", report.memory_load_percent),
                "load".to_string(),
            ])
            .add_row(vec![
                "Virtual Memory Total".to_string(),
                self.format_bytes(report.total_virtual_bytes),
                "-".to_string(),
                "Physical + Page file".to_string(),
            ]);
        
        // Allocation planning section separator
        table = table.add_row(vec![
            "".to_string(),
            "".to_string(), 
            "".to_string(),
            "".to_string(),
        ]);
        
        // Percentages are of the spec's reference: available, or total installed
        let of_reference = |bytes: u64| format!("{:.1}%", bytes as f64 / report.reference_bytes as f64 * 100.0);
        let relative_info = format!("of {}", report.reference_name);
        let (requested_label, requested_bytes) = match report.target_bytes {
            Some(target) => ("Target Requested", target),
            None => ("Reserve Requested", report.requested_reserve_bytes),
        };
        let rounding_diff = report.reserve_bytes as i64 - report.requested_reserve_bytes as i64;
        // When the rounding asked for would pass the reference: what was asked, and that it did not fit
        let rounding_note = match &report.rounding_asked {
            None => report.rounding_direction.clone(),
            Some((step, direction)) if *direction == report.rounding_direction => format!(
                "{} ({} step exceeds {})", report.rounding_direction, self.format_bytes(*step), report.reference_name),
            Some((_, direction)) => format!(
                "{} ({} exceeds {})", report.rounding_direction, direction, report.reference_name),
        };

        table = table.add_row(vec![
            requested_label.to_string(),
            self.format_bytes(requested_bytes),
            of_reference(requested_bytes),
            relative_info.clone(),
        ]);
        // Under a `-target` spec the raw figure is the target itself, already on the row above,
        // unless the target is more than is installed.
        if report.target_bytes != Some(report.raw_allocation_bytes) {
            table = table.add_row(vec![
                "Testing Raw".to_string(),
                self.format_bytes(report.raw_allocation_bytes),
                of_reference(report.raw_allocation_bytes),
                relative_info.clone(),
            ]);
        }

        table
            .add_row(vec![
                "Threads".to_string(),
                report.thread_count.to_string(),
                "-".to_string(),
                "".to_string(),
            ])
            .add_row(vec![
                "Per Thread Raw".to_string(),
                self.format_bytes_precise(report.per_thread_raw_bytes, 3),
                "-".to_string(),
                "".to_string(),
            ])
            .add_row(vec![
                "Per Thread Round To".to_string(),
                self.format_bytes(report.rounding_step_bytes),
                "-".to_string(),
                rounding_note,
            ])
            .add_row(vec![
                "Per Thread Rounded".to_string(),
                self.format_bytes_precise(report.per_thread_bytes, 3),
                "-".to_string(),
                "".to_string(),
            ])
            .add_row(vec![
                "Testing Allocation".to_string(),
                self.format_bytes(report.allocation_bytes),
                of_reference(report.allocation_bytes),
                relative_info.clone(),
            ])
            .add_row(vec![
                "Reserve Left".to_string(),
                self.format_bytes(report.reserve_bytes),
                of_reference(report.reserve_bytes),
                relative_info,
            ])
            .add_row(vec![
                "Reserve Rounding Diff".to_string(),
                self.format_bytes_signed(rounding_diff),
                "-".to_string(),
                "Threads × per-thread rounding".to_string(),
            ])
    }
    
    fn format_extent_mode_with_size(&self, mode: &crate::tests::ExtentMode, cache_info: &crate::cache::CacheInfo, thread_count: usize) -> String {
        use crate::tests::ExtentMode;
        match mode {
            ExtentMode::FullAllocation => "FullAllocation".to_string(),
            ExtentMode::Absolute { size_bytes } => {
                self.format_bytes(*size_bytes as u64)
            }
            ExtentMode::CacheTotal { fraction } => {
                // Naive sum-of-tiers — no thread division (CacheTotal is intentionally coarse).
                // The same total the test sizes from (`calculate_extent_size`).
                let size = (cache_info.total_cache as f64 * fraction) as u64;
                format!("{} (CacheTotal {:.2}x)", self.format_bytes(size), fraction)
            }
            ExtentMode::Cache { target } => {
                // Calculate the actual extent size using the target's method
                let size = target.size_bytes(cache_info, thread_count);
                let is_vm = cache_info.is_virtual_machine;
                // DRAMFull returns usize::MAX as sentinel for "use full allocation"
                if size == usize::MAX {
                    format!("Full Alloc ({})", target.name_with_context(thread_count, is_vm))
                } else {
                    format!("{} ({})", self.format_bytes(size as u64), target.name_with_context(thread_count, is_vm))
                }
            }
        }
    }

    fn format_chunk_mode(&self, mode: &crate::tests::ChunkMode) -> String {
        use crate::tests::ChunkMode;
        match mode {
            ChunkMode::Auto => "Auto".to_string(),
            ChunkMode::Whole => "Whole".to_string(),
            ChunkMode::Absolute { size_bytes } => {
                format!("Absolute ({})", self.format_bytes(*size_bytes as u64))
            }
            ChunkMode::CacheTotal { fraction } => {
                format!("CacheTotal ({:.2}x)", fraction)
            }
            ChunkMode::Cache { target } => {
                format!("Cache ({})", target.name())
            }
            ChunkMode::Tm5Block { window, divisor, granularity } => {
                format!("TM5 block (.cfg window {}/{}, {} lock)", self.format_bytes(*window as u64), divisor, self.format_bytes(*granularity as u64))
            }
        }
    }

    fn format_chunk_mode_with_size(&self, mode: &crate::tests::ChunkMode, cache_info: &crate::cache::CacheInfo, thread_count: usize) -> String {
        use crate::tests::ChunkMode;
        match mode {
            // Show the resolved size: `Cache (L2)` alone hides the SMT divisor (L1/L2 divide by
            // active threads per core), so the same spec means 2 MiB at 1 thread/core and 1 MiB
            // under SMT. Everything else already states its size in the spec.
            ChunkMode::Cache { target } => {
                let size = target.size_bytes(cache_info, thread_count);
                if size == usize::MAX {
                    format!("Cache ({}, full extent)", target.name())
                } else {
                    format!("Cache ({}, {})", target.name(), self.format_bytes(size as u64))
                }
            }
            other => self.format_chunk_mode(other),
        }
    }


    fn prepare_test_configuration_table(&self, report: &TestConfigurationReport) -> TableData {
        let mut table = TableData::new()
            .with_title("Test Configuration")
            .add_header("#", ColumnAlignment::Right)
            .add_header("Test Name", ColumnAlignment::Left)
            .add_header("Timing", ColumnAlignment::Right)
            .add_header("Per chunk", ColumnAlignment::Left)
            .add_header("Parameter", ColumnAlignment::Left)
            .add_header("Extent", ColumnAlignment::Left)
            .add_header("Chunk", ColumnAlignment::Left);
        if report.sealed {
            table = table.add_header("Seal", ColumnAlignment::Center);
        }

        for test in &report.tests {
            let mut row = vec![
                test.number.to_string(),
                test.name.clone(),
                test.timing.clone(),
                test.per_chunk.clone(),
                test.parameter.clone(),
                test.extent_mode.clone(),
                test.chunk_mode.clone(),
            ];
            if report.sealed {
                row.push(test.seal.clone());
            }
            table = table.add_row(row);
        }

        let steps = if report.step_count != report.test_count { format!(" | Steps per cycle: {}", report.step_count) } else { String::new() };
        table.with_footer(format!("Suite Timing: {} | Total Tests: {}{}",
                                   report.suite_timing, report.test_count, steps))
    }
    
    fn prepare_cycle_report_table(&self, report: &CycleReport) -> TableData {
        // Not wired up yet (TODO #29): nothing calls `report_cycle`. Kept deliberately for a
        // between-cycle report, and kept in step with the final per-test table below so reinstating
        // it needs only the call site. Unlike that table there is nothing to average — one cycle.
        let has_whea = report.test_performances.iter().any(|t| t.whea_total > 0);

        let table = TableData::new()
            .with_title(format!("Cycle {} Report", report.cycle_number))
            .add_header("#", ColumnAlignment::Center)
            .add_header("Test Name", ColumnAlignment::Left)
            .add_header("Time", ColumnAlignment::Right)
            .add_header("Data", ColumnAlignment::Right)
            .add_header("Speed", ColumnAlignment::Right);
        let mut table = self.with_verdict_headers(table, has_whea);

        for test in &report.test_performances {
            let mut row = vec![
                test.number.to_string(),
                test.name.clone(),
                format!("{:.1}s", test.duration_secs),
                format!("{:.2} GiB", test.data_processed_gib),
                format!("{:.0} MiB/s", test.throughput_mib_s),
            ];
            let whea = WheaCounts { total: test.whea_total, corrected: test.whea_corrected };
            self.push_verdict_cells(&mut row, test.errors, Some(whea), has_whea);
            table = table.add_row(row);
        }

        table.with_footer(format!("Cycle Time: {}s", report.duration_secs))
    }
    
    fn prepare_final_summary_overview_table(&self, report: &FinalTestSummaryReport) -> TableData {
        // Both error sources fail the run: a WHEA event means the hardware reported a fault whether
        // or not a verify read could see it. First row, because it is the answer the whole table
        // exists to give — everything below it is the supporting detail.
        let passed = report.total_errors == 0 && report.whea_total == 0;

        TableData::new()
            .with_title("Final Test Summary - Overview")
            .add_header("Metric", ColumnAlignment::Left)
            .add_header("Value", ColumnAlignment::Right)
            .add_row(vec![
                "Result".to_string(),
                if passed { "PASS ✅".to_string() } else { "FAIL ❌".to_string() },
            ])
            .add_row(vec![
                "Runtime (HH:MM:SS)".to_string(),
                report.total_runtime.clone(),
            ])
            .add_row(vec![
                "Cycles Completed".to_string(),
                report.cycles_completed.to_string(),
            ])
            .add_row(vec![
                "Total Data Processed".to_string(),
                format!("{:.2} GiB", report.total_data_processed_gib),
            ])
            .add_row(vec![
                "Overall Throughput".to_string(),
                format!("{:.1} MiB/s ({:.2} GiB/s)", 
                        report.overall_throughput_mib_s, 
                        report.overall_throughput_gib_s),
            ])
            .add_row(vec![
                "Total Errors".to_string(),
                report.total_errors.to_string(),
            ])
            .add_row(vec![
                // The run-wide stages; the checks around each chunk are in the steps' times
                "Seal".to_string(),
                match &report.seal {
                    Some(seal) => format!("{} errors in the final checks, {:.1}s for {:.2} GiB sealed and checked",
                                          seal.final_check_errors, seal.secs, seal.data_gib),
                    None => "off".to_string(),
                },
            ])
            .add_row(vec![
                "Total WHEA".to_string(),
                if !report.whea_monitored {
                    // Say so explicitly: a bare "0" here would claim a clean bill of health that
                    // was never actually checked.
                    "not monitored".to_string()
                } else {
                    // The C/UC split matters: a corrected error means the fault happened but the
                    // data was still right, which is exactly the case our verify reads cannot see.
                    WheaCounts { total: report.whea_total, corrected: report.whea_corrected }.to_string()
                },
            ])
    }
    
    fn prepare_final_summary_performance_table(&self, report: &FinalTestSummaryReport) -> TableData {
        // Check if any test has latency data
        let has_latency = report.per_test_summaries.iter().any(|t| t.latency_samples.is_some());
        // Only widen the table when the OS actually reported hardware errors — on a healthy system
        // the summary looks exactly as it did before WHEA monitoring existed.
        let has_whea = report.per_test_summaries.iter().any(|t| t.whea_total > 0);

        // A test that runs more than once a cycle (TODO 74) gets a Runs column; a sealed run, Seal
        let has_repeats = report.per_test_summaries.iter().any(|t| t.runs > report.cycles_completed as u64);
        let sealed = report.seal.is_some();

        let mut table = TableData::new()
            .with_title("Final Test Summary - Per-Test Performance")
            .add_header("#", ColumnAlignment::Center)
            .add_header("Test Name", ColumnAlignment::Left);
        if has_repeats {
            table = table.add_header("Runs", ColumnAlignment::Right);
        }
        table = table
            .add_header("Time", ColumnAlignment::Right)
            .add_header("Data", ColumnAlignment::Right)
            .add_header("Speed", ColumnAlignment::Right);

        // Add latency columns only if any test has latency data
        if has_latency {
            table = table
                .add_header("Samples", ColumnAlignment::Right)
                .add_header("P5", ColumnAlignment::Right)
                .add_header("P10", ColumnAlignment::Right)
                .add_header("P25", ColumnAlignment::Right)
                .add_header("P50", ColumnAlignment::Right)
                .add_header("P75", ColumnAlignment::Right)
                .add_header("P90", ColumnAlignment::Right)
                .add_header("P95", ColumnAlignment::Right)
                .add_header("P99", ColumnAlignment::Right)
                .add_header("P99.9", ColumnAlignment::Right)
                .add_header("Spread", ColumnAlignment::Right);
        }

        if sealed {
            table = table.add_header("Seal", ColumnAlignment::Right);
        }
        table = self.with_verdict_headers(table, has_whea);

        for (idx, test) in report.per_test_summaries.iter().enumerate() {
            let mut row = vec![format!("{}", idx + 1), test.name.clone()];
            if has_repeats {
                row.push(test.runs.to_string());
            }
            row.extend([
                format!("{:.1}s", test.average_duration_secs),
                format!("{:.2} GiB", test.average_data_gib),
                format!("{:.0} MiB/s", test.average_throughput_mib_s),
            ]);

            // Add latency values if we have latency columns
            if has_latency {
                row.push(test.latency_samples.map(|s| format!("{}", s)).unwrap_or_else(|| "-".to_string()));
                row.push(test.latency_p5_ns.map(|v| format!("{:.1}", v)).unwrap_or_else(|| "-".to_string()));
                row.push(test.latency_p10_ns.map(|v| format!("{:.1}", v)).unwrap_or_else(|| "-".to_string()));
                row.push(test.latency_p25_ns.map(|v| format!("{:.1}", v)).unwrap_or_else(|| "-".to_string()));
                row.push(test.latency_p50_ns.map(|v| format!("{:.1}", v)).unwrap_or_else(|| "-".to_string()));
                row.push(test.latency_p75_ns.map(|v| format!("{:.1}", v)).unwrap_or_else(|| "-".to_string()));
                row.push(test.latency_p90_ns.map(|v| format!("{:.1}", v)).unwrap_or_else(|| "-".to_string()));
                row.push(test.latency_p95_ns.map(|v| format!("{:.1}", v)).unwrap_or_else(|| "-".to_string()));
                row.push(test.latency_p99_ns.map(|v| format!("{:.1}", v)).unwrap_or_else(|| "-".to_string()));
                row.push(test.latency_p99_9_ns.map(|v| format!("{:.1}", v)).unwrap_or_else(|| "-".to_string()));
                row.push(test.latency_spread.map(|v| format!("{:.2}x", v)).unwrap_or_else(|| "-".to_string()));
            }

            // Counts, not averages: an error is an event, and averaging over cycles would dilute a
            // single-cycle fault into "0.3 errors".
            if sealed {
                row.push(test.seal_errors.to_string());
            }
            let whea = WheaCounts { total: test.whea_total, corrected: test.whea_corrected };
            self.push_verdict_cells(&mut row, test.total_errors, Some(whea), has_whea);
            if test.seal_errors > 0 {
                // The verdict counts the seal's errors too: they are this run's
                if let Some(pass) = row.last_mut() {
                    *pass = self.pass_cell(false);
                }
            }

            table = table.add_row(row);
        }

        // Say which columns are averages and which are totals — `Data` (an average) beside `Errors`
        // (a total) is genuinely ambiguous otherwise, and nothing else on screen settles it.
        let cycles = report.cycles_completed;
        table.with_footer(format!(
            "{} cycle{}: Time/Data/Speed{} averaged per run, {}Errors{}{} summed.",
            cycles,
            if cycles == 1 { "" } else { "s" },
            if has_latency { "/latency (ns)" } else { "" },
            if has_repeats { "Runs/" } else { "" },
            if sealed { "/Seal (the seal checks before its chunks, TM5's test 0)" } else { "" },
            if has_whea { "/WHEA" } else { "" },
        ))
    }

    fn prepare_errors_by_step_table(&self, report: &FinalTestSummaryReport) -> TableData {
        let sealed = report.seal.is_some();
        let mut table = TableData::new()
            .with_title("Errors by Step")
            .add_header("Step", ColumnAlignment::Right)
            .add_header("Test", ColumnAlignment::Left)
            .add_header("Errors", ColumnAlignment::Right);
        if sealed {
            table = table.add_header("Seal", ColumnAlignment::Right);
        }
        for step in &report.errors_by_step {
            let mut row = vec![step.step.to_string(), step.label.clone(), step.errors.to_string()];
            if sealed {
                row.push(step.seal_errors.to_string());
            }
            table = table.add_row(row);
        }
        if let Some(seal) = report.seal.as_ref().filter(|s| s.final_check_errors > 0) {
            table = table.add_row(vec!["end".to_string(), "Final seal check".to_string(), "-".to_string(), seal.final_check_errors.to_string()]);
        }
        table.with_footer("Seal: found by the seal check before the step's chunks (TM5 test 0); a mirror's own errors are its seal check after it.".to_string())
    }
    
    /// Prepare block size distribution table (Table 1)
    fn prepare_block_size_distribution_table(&self, report: &BlockAllocationReport) -> TableData {
        let mut table = TableData::new()
            .with_title("Block Size Distribution")
            .add_header("Block Size", ColumnAlignment::Right)
            .add_header("Page Type", ColumnAlignment::Center)
            .add_header("Count", ColumnAlignment::Right)
            .add_header("Blocks/Thread", ColumnAlignment::Right)
            .add_header("Threads Using", ColumnAlignment::Right)
            .add_header("Total Bytes", ColumnAlignment::Right)
            .add_header("% Total", ColumnAlignment::Right);

        let total_bytes = report.total_allocated_bytes as f64;

        for block_dist in &report.block_size_distribution {
            let pct = if total_bytes > 0.0 {
                (block_dist.total_bytes as f64 / total_bytes) * 100.0
            } else {
                0.0
            };

            table = table.add_row(vec![
                format!("{} MB", block_dist.block_size_mb),
                block_dist.page_type.clone(),
                block_dist.block_count.to_string(),
                format!("{:.1}", block_dist.average_per_thread),
                block_dist.threads_with_this_size.to_string(),
                self.format_bytes(block_dist.total_bytes),
                format!("{:.1}%", pct),
            ]);
        }

        table
    }
    
    /// Prepare page type summary table (Table 2)
    fn prepare_page_type_summary_table(&self, report: &BlockAllocationReport) -> TableData {
        let mut table = TableData::new()
            .with_title("Page Type Summary")
            .add_header("Page Type", ColumnAlignment::Left)
            .add_header("Page Count", ColumnAlignment::Right)
            .add_header("Blocks", ColumnAlignment::Right)
            .add_header("Threads Using", ColumnAlignment::Right)
            .add_header("Total Bytes", ColumnAlignment::Right)
            .add_header("% Total", ColumnAlignment::Right);

        let page_summary = &report.page_type_summary;

        // Huge pages row (only if used)
        if page_summary.huge_pages.page_count > 0 {
            table = table.add_row(vec![
                "Huge (1GB)".to_string(),
                page_summary.huge_pages.page_count.to_string(),
                page_summary.huge_pages.block_count.to_string(),
                page_summary.huge_pages.threads_using.to_string(),
                self.format_bytes(page_summary.huge_pages.total_bytes),
                format!("{:.1}%", page_summary.huge_pages.percentage_of_total),
            ]);
        }

        // Large pages row (only if used)
        if page_summary.large_pages.page_count > 0 {
            table = table.add_row(vec![
                "Large (2MB)".to_string(),
                page_summary.large_pages.page_count.to_string(),
                page_summary.large_pages.block_count.to_string(),
                page_summary.large_pages.threads_using.to_string(),
                self.format_bytes(page_summary.large_pages.total_bytes),
                format!("{:.1}%", page_summary.large_pages.percentage_of_total),
            ]);
        }

        // Regular pages row (only if used)
        if page_summary.regular_pages.page_count > 0 {
            table = table.add_row(vec![
                "Regular (4KB)".to_string(),
                page_summary.regular_pages.page_count.to_string(),
                page_summary.regular_pages.block_count.to_string(),
                page_summary.regular_pages.threads_using.to_string(),
                self.format_bytes(page_summary.regular_pages.total_bytes),
                format!("{:.1}%", page_summary.regular_pages.percentage_of_total),
            ]);
        }

        table
    }
    
    /// Prepare per-thread block allocation table (Table 3)
    fn prepare_thread_block_allocation_table(&self, report: &BlockAllocationReport) -> TableData {
        let mut table = TableData::new()
            .with_title("Per-Thread Block Allocation")
            .add_header("Thread", ColumnAlignment::Right)
            .add_header("CPU", ColumnAlignment::Right)
            .add_header("NUMA", ColumnAlignment::Center)
            .add_header("Total Size", ColumnAlignment::Right)
            .add_header("Block Sizes", ColumnAlignment::Left)
            .add_header("Huge Pages", ColumnAlignment::Right)
            .add_header("Large Pages", ColumnAlignment::Right)
            .add_header("Regular Pages", ColumnAlignment::Right);

        for thread_alloc in &report.per_thread_allocation {
            // Format block sizes as "1024MB×2, 512MB×1" etc.
            let block_sizes_str = thread_alloc.block_sizes.iter()
                .map(|bs| format!("{}MB×{}", bs.size_mb, bs.count))
                .collect::<Vec<_>>()
                .join(", ");

            let breakdown = &thread_alloc.page_type_breakdown;
            
            table = table.add_row(vec![
                thread_alloc.thread_id.to_string(),
                thread_alloc.cpu_id.to_string(),
                thread_alloc.numa_node.to_string(),
                self.format_bytes(thread_alloc.total_bytes),
                if block_sizes_str.is_empty() { "-".to_string() } else { block_sizes_str },
                if breakdown.huge_pages_count > 0 {
                    format!("{} ({})", breakdown.huge_pages_count, self.format_bytes(breakdown.huge_pages_bytes))
                } else {
                    "-".to_string()
                },
                if breakdown.large_pages_count > 0 {
                    format!("{} ({})", breakdown.large_pages_count, self.format_bytes(breakdown.large_pages_bytes))
                } else {
                    "-".to_string()
                },
                if breakdown.regular_pages_count > 0 {
                    format!("{} ({})", breakdown.regular_pages_count, self.format_bytes(breakdown.regular_pages_bytes))
                } else {
                    "-".to_string()
                },
            ]);
        }

        table
    }
    
    /// Prepare NUMA distribution table (Table 4)
    fn prepare_numa_distribution_table(&self, report: &BlockAllocationReport) -> TableData {
        let mut table = TableData::new()
            .with_title("NUMA Node Distribution")
            .add_header("NUMA Node", ColumnAlignment::Center)
            .add_header("Thread Count", ColumnAlignment::Right)
            .add_header("Total Bytes", ColumnAlignment::Right)
            .add_header("Block Sizes", ColumnAlignment::Left)
            .add_header("Huge Pages", ColumnAlignment::Right)
            .add_header("Large Pages", ColumnAlignment::Right)
            .add_header("Regular Pages", ColumnAlignment::Right);

        for numa_dist in &report.numa_distribution {
            // Format block sizes as "1024MB, 512MB" etc.
            let block_sizes_str = numa_dist.block_sizes.iter()
                .map(|&size| format!("{}MB", size))
                .collect::<Vec<_>>()
                .join(", ");

            let page_types = &numa_dist.page_types;
            
            table = table.add_row(vec![
                numa_dist.node_id.to_string(),
                numa_dist.thread_count.to_string(),
                self.format_bytes(numa_dist.total_bytes),
                if block_sizes_str.is_empty() { "-".to_string() } else { block_sizes_str },
                if page_types.huge_pages_count > 0 {
                    format!("{} ({})", page_types.huge_pages_count, self.format_bytes(page_types.huge_pages_bytes))
                } else {
                    "-".to_string()
                },
                if page_types.large_pages_count > 0 {
                    format!("{} ({})", page_types.large_pages_count, self.format_bytes(page_types.large_pages_bytes))
                } else {
                    "-".to_string()
                },
                if page_types.regular_pages_count > 0 {
                    format!("{} ({})", page_types.regular_pages_count, self.format_bytes(page_types.regular_pages_bytes))
                } else {
                    "-".to_string()
                },
            ]);
        }

        table
    }
    
    /// Prepare allocation fairness analysis table (Table 5)
    fn prepare_allocation_fairness_table(&self, report: &BlockAllocationReport) -> TableData {
        let mut table = TableData::new()
            .with_title("Allocation Fairness Analysis")
            .add_header("Thread ID", ColumnAlignment::Center)
            .add_header("Allocated", ColumnAlignment::Right)
            .add_header("vs Target", ColumnAlignment::Right)
            .add_header("Status", ColumnAlignment::Left);

        let fairness = &report.allocation_fairness;

        // Each node's 1 GiB pages are one pool, so a thread is measured against the best-served
        // thread on its node. One page apart is the most an even split can promise.
        let mut node_max_huge: HashMap<u32, u64> = HashMap::new();
        for thread_alloc in &report.per_thread_allocation {
            let max = node_max_huge.entry(thread_alloc.numa_node).or_default();
            *max = (*max).max(thread_alloc.page_type_breakdown.huge_pages_count);
        }

        // Show all threads, not just unfair ones
        for thread_alloc in &report.per_thread_allocation {
            let vs_target = thread_alloc.total_bytes as i64 - thread_alloc.target_bytes as i64;
            let fewer_huge = node_max_huge[&thread_alloc.numa_node] - thread_alloc.page_type_breakdown.huge_pages_count;
            let huge_note = (fewer_huge > 1).then(|| format!("{} fewer 1 GiB pages", fewer_huge));

            let side = match vs_target.cmp(&0) {
                Ordering::Less => Some("Short"),
                Ordering::Greater => Some("Over"),
                Ordering::Equal => None,
            };
            let status = match (side, huge_note) {
                (None, None) => "✅ Fair".to_string(),
                (None, Some(note)) => format!("🟡 {}", note),
                (Some(side), None) => format!("⚠️ {}", side),
                (Some(side), Some(note)) => format!("⚠️ {}, {}", side, note),
            };

            table = table.add_row(vec![
                thread_alloc.thread_id.to_string(),
                self.format_bytes(thread_alloc.total_bytes),
                if vs_target == 0 { "-".to_string() } else { self.format_bytes_signed(vs_target) },
                status,
            ]);
        }

        table.with_footer(format!(
            "Fairness Stats: Min={}, Max={}, Mean={:.1} GiB, CV={:.3} ({}), 1 GiB pages per thread: {}-{}",
            self.format_bytes(fairness.min_allocation_bytes),
            self.format_bytes(fairness.max_allocation_bytes),
            bytes_to_gib_f64(fairness.mean_allocation_bytes as u64),
            fairness.coefficient_of_variation,
            if fairness.coefficient_of_variation < 0.1 { "Fair Distribution" } else { "Unfair Distribution" },
            fairness.min_huge_pages,
            fairness.max_huge_pages,
        ))
    }

    fn prepare_latency_per_thread_table(&self, level: &LatencyLevelSummary) -> TableData {
        // No title - test name and extent size are already shown in the main test header and config line
        let mut table = TableData::new()
            .add_header("Thread", ColumnAlignment::Right)
            .add_header("CPU", ColumnAlignment::Right)
            .add_header("Samples", ColumnAlignment::Right)
            .add_header("Min", ColumnAlignment::Right)
            .add_header("P5", ColumnAlignment::Right)
            .add_header("P10", ColumnAlignment::Right)
            .add_header("P25", ColumnAlignment::Right)
            .add_header("P50", ColumnAlignment::Right)
            .add_header("P75", ColumnAlignment::Right)
            .add_header("P90", ColumnAlignment::Right)
            .add_header("P95", ColumnAlignment::Right)
            .add_header("P99", ColumnAlignment::Right)
            .add_header("P99.9", ColumnAlignment::Right)
            .add_header("Spread", ColumnAlignment::Right);

        for result in &level.per_thread_results {
            let p = &result.percentiles;
            table = table.add_row(vec![
                result.thread_id.to_string(),
                result.cpu_id.to_string(),
                result.sample_count.to_string(),
                format!("{:.1}", p.min_ns),
                format!("{:.1}", p.p5_ns),
                format!("{:.1}", p.p10_ns),
                format!("{:.1}", p.p25_ns),
                format!("{:.1}", p.p50_ns),
                format!("{:.1}", p.p75_ns),
                format!("{:.1}", p.p90_ns),
                format!("{:.1}", p.p95_ns),
                format!("{:.1}", p.p99_ns),
                format!("{:.1}", p.p99_9_ns),
                format!("{:.2}x", p.spread_ratio),
            ]);
        }

        // Add consolidated row
        let p = &level.consolidated;
        table = table.add_row(vec![
            "ALL".to_string(),
            "-".to_string(),
            level.total_samples.to_string(),
            format!("{:.1}", p.min_ns),
            format!("{:.1}", p.p5_ns),
            format!("{:.1}", p.p10_ns),
            format!("{:.1}", p.p25_ns),
            format!("{:.1}", p.p50_ns),
            format!("{:.1}", p.p75_ns),
            format!("{:.1}", p.p90_ns),
            format!("{:.1}", p.p95_ns),
            format!("{:.1}", p.p99_ns),
            format!("{:.1}", p.p99_9_ns),
            format!("{:.2}x", p.spread_ratio),
        ]);

        table
    }
}