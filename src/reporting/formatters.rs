/// Formatting logic for transforming raw data into presentation-ready format
/// This layer handles all the "how to format" decisions
use super::models::*;
use crate::constants::BYTES_PER_MIB;
use crate::whea::WheaCounts;
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
    fn prepare_intermediate_table(&self, report: &FinalTestSummaryReport) -> TableData;
    fn prepare_failing_bits_table(&self, report: &FinalTestSummaryReport) -> TableData;
    
    /// The allocation report's tables (TODO 81)
    fn prepare_allocation_nodes_table(&self, report: &AllocationReport) -> TableData;
    fn prepare_allocation_commits_table(&self, report: &AllocationReport) -> TableData;
    fn prepare_allocation_threads_table(&self, report: &AllocationReport) -> TableData;
    fn prepare_thread_commits_table(&self, report: &AllocationReport) -> TableData;
    fn prepare_fairness_by_thread_table(&self, report: &AllocationReport) -> TableData;
    fn prepare_fairness_summary_table(&self, report: &AllocationReport) -> TableData;

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

    fn bytes_or_dash(&self, bytes: u64) -> String {
        if bytes == 0 { "-".to_string() } else { self.format_bytes(bytes) }
    }

    /// Remote bytes and their share of the allocation, `0.50 GiB (3.3%)`, or `-`.
    fn remote_cell(&self, remote: u64, allocated: u64) -> String {
        if remote == 0 {
            return "-".to_string();
        }
        format!("{} ({:.1}%)", self.format_bytes(remote), 100.0 * remote as f64 / allocated.max(1) as f64)
    }

    /// A deviation from the other threads' median, in the dimension's unit; `=` when none.
    fn deviation_text(&self, dimension: FairnessDimension, deviation: f64) -> String {
        if deviation == 0.0 {
            "=".to_string()
        } else if dimension == FairnessDimension::Requests {
            if deviation.fract() == 0.0 { format!("{:+}", deviation as i64) } else { format!("{deviation:+.1}") }
        } else {
            self.format_bytes_signed(deviation.round() as i64)
        }
    }

    /// A dimension's value for the summary: bytes, or for Requests a count.
    fn dimension_value(&self, dimension: FairnessDimension, value: f64) -> String {
        match dimension {
            FairnessDimension::Requests if value.fract() == 0.0 => format!("{}", value as u64),
            FairnessDimension::Requests => format!("{value:.1}"),
            _ => self.bytes_or_dash(value.round() as u64),
        }
    }

    /// The thread table's Status: ✅, or the worst flag and each dimension that has one.
    fn thread_status(&self, report: &AllocationReport, i: usize) -> String {
        let t = &report.threads[i];
        let mut worst = FairnessFlag::Fine;
        let mut notes = Vec::new();
        for d in &report.fairness {
            let cell = &d.cells[i];
            if cell.flag == FairnessFlag::Fine {
                continue;
            }
            worst = worst.max(cell.flag);
            let deviation = self.deviation_text(d.dimension, cell.deviation);
            notes.push(match d.dimension {
                FairnessDimension::Allocated if t.allocated_bytes < t.target_bytes => {
                    format!("short {}", self.format_bytes(t.target_bytes - t.allocated_bytes))
                }
                FairnessDimension::Allocated => format!("{deviation} vs the others"),
                FairnessDimension::Huge | FairnessDimension::Large => {
                    format!("{} {deviation}", dimension_name(d.dimension))
                }
                FairnessDimension::Regular => format!("4 KiB {}", self.format_bytes(t.regular_bytes)),
                FairnessDimension::Remote => format!("remote {}", self.remote_cell(t.remote_bytes, t.allocated_bytes)),
                FairnessDimension::Requests => format!("{} requests", t.commits.len()),
            });
        }
        match worst {
            FairnessFlag::Fine => "✅".to_string(),
            flag => format!("{} {}", flag_glyph(flag), notes.join(", ")),
        }
    }
}

fn count_or_dash(count: usize) -> String {
    if count == 0 { "-".to_string() } else { count.to_string() }
}

fn node_cell(node: Option<u32>) -> String {
    node.map_or_else(|| "-".to_string(), |n| n.to_string())
}

/// A request size in MB: whole, or to two places when it isn't (4 KiB requests can be any size).
fn mb_label(bytes: u64) -> String {
    if bytes.is_multiple_of(BYTES_PER_MIB) {
        (bytes / BYTES_PER_MIB).to_string()
    } else {
        format!("{:.2}", bytes as f64 / BYTES_PER_MIB as f64)
    }
}

fn page_name(page: crate::memory::allocator::PageSizeLevel) -> &'static str {
    use crate::memory::allocator::PageSizeLevel;
    match page {
        PageSizeLevel::Huge => "1 GiB",
        PageSizeLevel::Large => "2 MiB",
        PageSizeLevel::Regular => "4 KiB",
    }
}

/// Requests in order as their sizes in MB, a run of equal ones written once with `×count`, and
/// a 1 GiB or 2 MiB one from a node other than `home` tagged `@node`: `1024×2+128×4@0`.
fn commit_signature<'a>(
    commits: impl Iterator<Item = &'a crate::memory::stitched::Commit>,
    home: Option<u32>,
) -> String {
    use crate::memory::allocator::PageSizeLevel;
    let mut runs: Vec<(String, usize)> = Vec::new();
    for c in commits {
        let mut label = mb_label(c.len as u64);
        if c.page != PageSizeLevel::Regular
            && let Some(node) = c.numa_node.filter(|&n| Some(n) != home)
        {
            label.push_str(&format!("@{node}"));
        }
        match runs.last_mut() {
            Some((last, count)) if *last == label => *count += 1,
            _ => runs.push((label, 1)),
        }
    }
    runs.iter()
        .map(|(label, count)| if *count == 1 { label.clone() } else { format!("{label}×{count}") })
        .collect::<Vec<_>>()
        .join("+")
}

/// The fairness dimensions a report shows: Remote only where memory can be remote.
fn shown_dimensions(report: &AllocationReport) -> Vec<&DimensionFairness> {
    report
        .fairness
        .iter()
        .filter(|d| report.numa_nodes > 1 || d.dimension != FairnessDimension::Remote)
        .collect()
}

/// A dimension's name, the same as its column in the node and thread tables.
fn dimension_name(dimension: FairnessDimension) -> &'static str {
    match dimension {
        FairnessDimension::Allocated => "Allocated",
        FairnessDimension::Huge => "1 GiB",
        FairnessDimension::Large => "2 MiB",
        FairnessDimension::Regular => "4 KiB",
        FairnessDimension::Remote => "Remote",
        FairnessDimension::Requests => "Requests",
    }
}

fn flag_glyph(flag: FairnessFlag) -> &'static str {
    match flag {
        FairnessFlag::Fine => "",
        FairnessFlag::Notice => "🟡",
        FairnessFlag::Warn => "⚠️",
    }
}

/// Thread ids as ranges where they run on: `4-6, 9`.
fn thread_ids_label(ids: &[usize]) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut k = 0;
    while k < ids.len() {
        let start = ids[k];
        while k + 1 < ids.len() && ids[k + 1] == ids[k] + 1 {
            k += 1;
        }
        parts.push(if ids[k] == start { start.to_string() } else { format!("{start}-{}", ids[k]) });
        k += 1;
    }
    parts.join(", ")
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
                // Its time and data are the Intermediate Operations table's
                "Seal".to_string(),
                match &report.seal {
                    Some(seal) => format!("{} errors in the final checks", seal.final_check_errors),
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

    /// The run's work around its tests, by operation (TODO 90): `n/a` where it did none
    fn prepare_intermediate_table(&self, report: &FinalTestSummaryReport) -> TableData {
        let mut table = TableData::new()
            .with_title("Final Test Summary - Intermediate Operations")
            .add_header("Operation", ColumnAlignment::Left)
            .add_header("Time", ColumnAlignment::Right)
            .add_header("Data", ColumnAlignment::Right)
            .add_header("Speed", ColumnAlignment::Right);
        for entry in &report.intermediate {
            let row = match entry.secs {
                None => vec![entry.name.to_string(), "n/a".to_string(), "n/a".to_string(), "n/a".to_string()],
                Some(secs) => {
                    let data = entry.bytes.map_or("-".to_string(), |b| format!("{:.2} GiB", b as f64 / (1u64 << 30) as f64));
                    let speed = match entry.bytes {
                        Some(b) if secs > 0.0 => format!("{:.0} MiB/s", b as f64 / (1u64 << 20) as f64 / secs),
                        _ => "-".to_string(),
                    };
                    vec![entry.name.to_string(), format!("{secs:.1}s"), data, speed]
                }
            };
            table = table.add_row(row);
        }
        table.with_footer("Over the run: each cycle's seal at its start and check at its end, and every step's work around its test. A step's time is its slowest thread's.")
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

    /// Which bits failed, by test, the seal's checks, and over all of them when there's more than
    /// one row (TODO 97)
    fn prepare_failing_bits_table(&self, report: &FinalTestSummaryReport) -> TableData {
        let mut rows: Vec<(String, &crate::failing_bits::FailingBits)> = report.per_test_summaries.iter()
            .filter(|t| !t.failing_bits.is_empty())
            .map(|t| (t.name.clone(), &t.failing_bits))
            .collect();
        if !report.seal_failing_bits.is_empty() {
            rows.push(("Seal".to_string(), &report.seal_failing_bits));
        }
        let mut all = report.failing_bits.clone();
        all.add(&report.seal_failing_bits);
        if rows.len() > 1 {
            rows.push(("All".to_string(), &all));
        }
        let mut table = TableData::new()
            .with_title("Failing Bits")
            .add_header("Test", ColumnAlignment::Left)
            .add_header("Bit: failed checks / bad words found (0→1 + 1→0)", ColumnAlignment::Left);
        for (name, bits) in rows {
            table = table.add_row(vec![name, bits.detail()]);
        }
        table.with_footer("A failed check counts each bit its differences had; a bad word, each bit it had wrong. Every bit is in the results JSON.".to_string())
    }
    
    fn prepare_allocation_nodes_table(&self, report: &AllocationReport) -> TableData {
        let multi = report.numa_nodes > 1;
        let mut title = format!("Memory Allocation ({}, {:.2} s)", report.allocator, report.elapsed.as_secs_f64());
        if !multi {
            title.push_str(": 1 NUMA node, all memory local");
        }
        let mut table = TableData::new()
            .with_title(title)
            .add_header("Node", ColumnAlignment::Right)
            .add_header("Threads", ColumnAlignment::Right)
            .add_header("Target", ColumnAlignment::Right)
            .add_header("Allocated", ColumnAlignment::Right)
            .add_header("1 GiB", ColumnAlignment::Right)
            .add_header("2 MiB", ColumnAlignment::Right)
            .add_header("4 KiB", ColumnAlignment::Right);
        if multi {
            table = table
                .add_header("Local", ColumnAlignment::Right)
                .add_header("Remote", ColumnAlignment::Right);
        }
        table = table
            .add_header("Requests", ColumnAlignment::Right)
            .add_header("Refused", ColumnAlignment::Right);

        let mut total = AllocationNode {
            numa_node: None,
            threads: 0,
            target_bytes: 0,
            allocated_bytes: 0,
            huge_bytes: 0,
            large_bytes: 0,
            regular_bytes: 0,
            remote_bytes: 0,
            requests: 0,
            refused: 0,
        };
        let row = |label: String, n: &AllocationNode| {
            let mut cells = vec![
                label,
                n.threads.to_string(),
                self.format_bytes(n.target_bytes),
                self.format_bytes(n.allocated_bytes),
                self.bytes_or_dash(n.huge_bytes),
                self.bytes_or_dash(n.large_bytes),
                self.bytes_or_dash(n.regular_bytes),
            ];
            if multi {
                cells.push(self.format_bytes(n.allocated_bytes - n.remote_bytes));
                cells.push(self.remote_cell(n.remote_bytes, n.allocated_bytes));
            }
            cells.push(n.requests.to_string());
            cells.push(count_or_dash(n.refused));
            cells
        };
        for n in &report.nodes {
            table = table.add_row(row(node_cell(n.numa_node), n));
            total.threads += n.threads;
            total.target_bytes += n.target_bytes;
            total.allocated_bytes += n.allocated_bytes;
            total.huge_bytes += n.huge_bytes;
            total.large_bytes += n.large_bytes;
            total.regular_bytes += n.regular_bytes;
            total.remote_bytes += n.remote_bytes;
            total.requests += n.requests;
            total.refused += n.refused;
        }
        if report.nodes.len() > 1 {
            table = table.add_row(row("Total".to_string(), &total));
        }
        table
    }

    fn prepare_allocation_commits_table(&self, report: &AllocationReport) -> TableData {
        let multi = report.numa_nodes > 1;
        let mut table = TableData::new()
            .with_title("Commits (the requests to Windows, by size, page size and node)")
            .add_header("Size", ColumnAlignment::Right)
            .add_header("Pages", ColumnAlignment::Right);
        if multi {
            table = table.add_header("Node", ColumnAlignment::Right);
        }
        table = table
            .add_header("Granted", ColumnAlignment::Right)
            .add_header("Refused", ColumnAlignment::Right)
            .add_header("Threads", ColumnAlignment::Right)
            .add_header("Total", ColumnAlignment::Right);
        for g in &report.commit_groups {
            let mut cells = vec![format!("{} MB", mb_label(g.bytes)), page_name(g.page).to_string()];
            if multi {
                cells.push(if g.numa_node == g.home_node {
                    node_cell(g.numa_node)
                } else {
                    format!("{} for {}", node_cell(g.numa_node), node_cell(g.home_node))
                });
            }
            cells.extend([
                count_or_dash(g.granted),
                count_or_dash(g.refused),
                g.threads.to_string(),
                self.bytes_or_dash(g.granted_bytes),
            ]);
            table = table.add_row(cells);
        }
        table
    }

    fn prepare_allocation_threads_table(&self, report: &AllocationReport) -> TableData {
        let multi = report.numa_nodes > 1;
        let mut table = TableData::new()
            .with_title("Threads")
            .add_header("Thread", ColumnAlignment::Right);
        if multi {
            table = table.add_header("Node", ColumnAlignment::Right);
        }
        table = table
            .add_header("Allocated", ColumnAlignment::Right)
            .add_header("1 GiB", ColumnAlignment::Right)
            .add_header("2 MiB", ColumnAlignment::Right)
            .add_header("4 KiB", ColumnAlignment::Right);
        if multi {
            table = table
                .add_header("Local", ColumnAlignment::Right)
                .add_header("Remote", ColumnAlignment::Right);
        }
        table = table.add_header("Status", ColumnAlignment::Left);

        for (i, t) in report.threads.iter().enumerate() {
            let mut cells = vec![t.thread_id.to_string()];
            if multi {
                cells.push(node_cell(t.numa_node));
            }
            cells.extend([
                self.format_bytes(t.allocated_bytes),
                self.bytes_or_dash(t.huge_bytes),
                self.bytes_or_dash(t.large_bytes),
                self.bytes_or_dash(t.regular_bytes),
            ]);
            if multi {
                cells.push(self.format_bytes(t.allocated_bytes - t.remote_bytes));
                cells.push(self.remote_cell(t.remote_bytes, t.allocated_bytes));
            }
            cells.push(self.thread_status(report, i));
            table = table.add_row(cells);
        }
        if multi && report.threads.iter().any(|t| t.regular_bytes > 0) {
            table = table.with_footer("4 KiB pages only prefer the thread's node, and count as local.");
        }
        table
    }

    fn prepare_thread_commits_table(&self, report: &AllocationReport) -> TableData {
        use crate::memory::allocator::PageSizeLevel;
        const PAGES: [PageSizeLevel; 3] = [PageSizeLevel::Huge, PageSizeLevel::Large, PageSizeLevel::Regular];
        let mut table = TableData::new()
            .with_title("Commits by thread (MB each in the order asked, ×repeats, @node from another node)")
            .add_header("Thread", ColumnAlignment::Right)
            .add_header("1 GiB", ColumnAlignment::Left)
            .add_header("2 MiB", ColumnAlignment::Left)
            .add_header("4 KiB", ColumnAlignment::Left)
            .add_header("Granted", ColumnAlignment::Right)
            .add_header("Refused", ColumnAlignment::Left);
        for t in &report.threads {
            let signature = |page, refused: bool| {
                commit_signature(
                    t.commits.iter().filter(|c| c.page == page && c.refused == refused),
                    t.numa_node,
                )
            };
            let mut cells = vec![t.thread_id.to_string()];
            cells.extend(PAGES.iter().map(|&page| {
                let s = signature(page, false);
                if s.is_empty() { "-".to_string() } else { s }
            }));
            let refused = t.commits.iter().filter(|c| c.refused).count();
            cells.push(count_or_dash(t.commits.len() - refused));
            cells.push(if refused == 0 {
                "-".to_string()
            } else {
                let by_page: Vec<String> = PAGES
                    .iter()
                    .filter_map(|&page| {
                        let s = signature(page, true);
                        (!s.is_empty()).then(|| format!("{}: {s}", page_name(page)))
                    })
                    .collect();
                format!("{refused} ({})", by_page.join("; "))
            });
            table = table.add_row(cells);
        }
        table
    }

    fn prepare_fairness_by_thread_table(&self, report: &AllocationReport) -> TableData {
        let dimensions = shown_dimensions(report);
        let mut table = TableData::new()
            .with_title("Fairness by thread (each against the median of the other threads)")
            .add_header("Thread", ColumnAlignment::Right);
        for d in &dimensions {
            table = table.add_header(dimension_name(d.dimension), ColumnAlignment::Left);
        }
        for (i, t) in report.threads.iter().enumerate() {
            let mut cells = vec![t.thread_id.to_string()];
            for d in &dimensions {
                let cell = &d.cells[i];
                let text = self.deviation_text(d.dimension, cell.deviation);
                cells.push(match cell.flag {
                    FairnessFlag::Fine => text,
                    flag => format!("{text} {}", flag_glyph(flag)),
                });
            }
            table = table.add_row(cells);
        }
        table
    }

    fn prepare_fairness_summary_table(&self, report: &AllocationReport) -> TableData {
        let mut table = TableData::new()
            .with_title("Fairness summary")
            .add_header("Dimension", ColumnAlignment::Left)
            .add_header("Min", ColumnAlignment::Right)
            .add_header("Median", ColumnAlignment::Right)
            .add_header("Max", ColumnAlignment::Right)
            .add_header("Worst", ColumnAlignment::Left)
            .add_header("Verdict", ColumnAlignment::Center);
        for d in shown_dimensions(report) {
            let worst = d.cells.iter().map(|c| c.badness).fold(0.0, f64::max);
            let worst_cell = if worst > 0.0 {
                let at: Vec<usize> = (0..d.cells.len())
                    .filter(|&i| d.cells[i].badness >= worst * (1.0 - 1e-9))
                    .collect();
                let ids: Vec<usize> = at.iter().map(|&i| report.threads[i].thread_id).collect();
                let first = d.cells[at[0]].deviation;
                let mut deviation = self.deviation_text(d.dimension, first);
                // Tied on either side of the others: two nodes' threads, one short, one over.
                if at.iter().any(|&i| d.cells[i].deviation.signum() != first.signum()) {
                    deviation = format!("±{}", deviation.trim_start_matches(['+', '-']));
                }
                format!("{} ({deviation})", thread_ids_label(&ids))
            } else {
                "-".to_string()
            };
            let flag = d.cells.iter().map(|c| c.flag).max().unwrap_or(FairnessFlag::Fine);
            table = table.add_row(vec![
                dimension_name(d.dimension).to_string(),
                self.dimension_value(d.dimension, d.min),
                self.dimension_value(d.dimension, d.median),
                self.dimension_value(d.dimension, d.max),
                worst_cell,
                match flag {
                    FairnessFlag::Fine => "✅".to_string(),
                    flag => flag_glyph(flag).to_string(),
                },
            ]);
        }
        table
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