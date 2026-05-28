/// Formatting logic for transforming raw data into presentation-ready format
/// This layer handles all the "how to format" decisions
use super::models::*;
use crate::constants::bytes_to_gib_f64;
use std::time::Duration;

/// Trait for formatting report data
pub trait ReportFormatter: Send + Sync {
    /// Format bytes into human-readable string
    fn format_bytes(&self, bytes: u64) -> String;
    
    /// Format bytes with specific precision
    fn format_bytes_precise(&self, bytes: u64, precision: usize) -> String;

    /// Format signed bytes with +/- prefix (eg. "+1.23 GiB" or "-0.50 GiB")
    fn format_bytes_signed(&self, bytes: i64) -> String;

    /// Format percentage
    fn format_percentage(&self, value: f64) -> String;
    
    /// Format duration
    fn format_duration(&self, duration: Duration) -> String;
    
    /// Format memory address
    fn format_address(&self, address: u64) -> String;
    
    /// Format bandwidth
    fn format_bandwidth(&self, bytes_per_second: f64) -> String;
    
    
    /// Prepare CPU info table
    fn prepare_cpu_info_table(&self, cpu_info: &CpuInfo) -> TableData;
    
    /// Prepare cache info table
    fn prepare_cache_info_table(&self, cache_info: &CacheInfo) -> TableData;

    /// Prepare TSC calibration info table
    fn prepare_tsc_info_table(&self, tsc_info: &TscCalibrationInfo) -> TableData;

    /// Format progress line
    fn format_progress_line(&self, progress: &TestProgressReport) -> String;
    
    /// Format success message
    fn format_success_message(&self, results: &FinalResultsReport) -> String;
    
    /// Format failure message
    fn format_failure_message(&self, results: &FinalResultsReport) -> String;
    
    /// Prepare statistics table
    fn prepare_statistics_table(&self, stats: &TestStatistics) -> TableData;
    
    /// Format driver status
    fn format_driver_status(&self, status: &DriverStatusReport) -> String;
    
    /// Prepare driver statistics table
    fn prepare_driver_stats_table(&self, stats: &DriverStatistics) -> TableData;
    
    /// Prepare thread timing deviation table
    fn prepare_thread_timing_table(&self, report: &ThreadTimingReport) -> TableData;
    
    /// Prepare current memory status table
    fn prepare_memory_status_table(&self, status: &CurrentMemoryStatus) -> TableData;
    
    /// Prepare thread allocation table
    fn prepare_thread_allocation_table(&self, report: &ThreadAllocationReport) -> TableData;
    
    /// Prepare page allocation table
    fn prepare_page_allocation_table(&self, report: &PageAllocationReport) -> TableData;
    
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
    
    /// Format memory strategy components
    fn format_allocation_mode(&self, mode: &crate::memory::allocation_strategy::AllocationMode) -> String;
    fn format_window_mode(&self, mode: &crate::tests::WindowMode) -> String;
    /// Format window mode with calculated size for CacheLevel targets
    fn format_window_mode_with_size(&self, mode: &crate::tests::WindowMode, cache_info: &crate::cache::CacheInfo, thread_count: usize) -> String;
    fn format_chunk_mode(&self, mode: &crate::tests::ChunkMode) -> String;
    
    /// Prepare test configuration table
    fn prepare_test_configuration_table(&self, report: &TestConfigurationReport) -> TableData;
    
    /// Prepare cycle report table
    fn prepare_cycle_report_table(&self, report: &CycleReport) -> TableData;
    
    /// Prepare CPU variance table
    fn prepare_cpu_variance_table(&self, report: &CpuVarianceReport) -> TableData;
    
    /// Prepare final test summary tables
    fn prepare_final_summary_overview_table(&self, report: &FinalTestSummaryReport) -> TableData;
    fn prepare_final_summary_performance_table(&self, report: &FinalTestSummaryReport) -> TableData;
    
    /// Prepare block allocation report tables (Option B layout - separate tables)
    fn prepare_block_size_distribution_table(&self, report: &BlockAllocationReport) -> TableData;
    fn prepare_page_type_summary_table(&self, report: &BlockAllocationReport) -> TableData;
    fn prepare_thread_block_allocation_table(&self, report: &BlockAllocationReport) -> TableData;
    fn prepare_numa_distribution_table(&self, report: &BlockAllocationReport) -> TableData;
    fn prepare_allocation_fairness_table(&self, report: &BlockAllocationReport) -> TableData;

    /// Prepare latency test summary tables (multi-threaded results)
    fn prepare_latency_summary_consolidated_table(&self, report: &LatencyTestSummaryReport) -> TableData;
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
        let abs_bytes = bytes.abs() as u64;
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

    fn format_percentage(&self, value: f64) -> String {
        format!("{:.1}%", value)
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
    
    fn format_address(&self, address: u64) -> String {
        format!("0x{:016X}", address)
    }
    
    fn format_bandwidth(&self, bytes_per_second: f64) -> String {
        let gb_per_sec = bytes_per_second / 1024_f64.powi(3);
        format!("{:.2} GB/s", gb_per_sec)
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

    fn format_progress_line(&self, progress: &TestProgressReport) -> String {
        let cycle_info = match progress.total_cycles {
            Some(total) => format!("Cycle {}/{}", progress.current_cycle, total),
            None => format!("Cycle {}", progress.current_cycle),
        };
        
        let time_info = match progress.estimated_time_remaining {
            Some(remaining) => format!(" | ETA: {}", self.format_duration(remaining)),
            None => String::new(),
        };
        
        format!(
            "[{}] {} - {} | {:.1}% | {} | {} errors | {:.2} GB tested | {}{}",
            self.format_duration(progress.elapsed_time),
            cycle_info,
            progress.current_test,
            progress.progress_percent,
            progress.current_phase,
            progress.errors_found,
            progress.memory_tested_bytes as f64 / 1024_f64.powi(3),
            self.format_bandwidth(progress.operations_per_second),
            time_info
        )
    }
    
    fn format_success_message(&self, results: &FinalResultsReport) -> String {
        format!(
            "✅ All memory tests completed successfully in {} ({} cycles, {:.1}% coverage)",
            self.format_duration(results.total_duration),
            results.cycles_completed,
            results.coverage_percent
        )
    }
    
    fn format_failure_message(&self, results: &FinalResultsReport) -> String {
        format!(
            "❌ Tests failed with {} errors in {} ({} cycles completed)",
            results.total_errors,
            self.format_duration(results.total_duration),
            results.cycles_completed
        )
    }
    
    fn prepare_statistics_table(&self, stats: &TestStatistics) -> TableData {
        let mut table = TableData::new()
            .with_title("Test Statistics")
            .add_header("Metric", ColumnAlignment::Left)
            .add_header("Value", ColumnAlignment::Right);
        
        table = table
            .add_row(vec![
                "Total Operations".to_string(),
                format!("{}", stats.total_operations),
            ])
            .add_row(vec![
                "Memory Tested".to_string(),
                self.format_bytes(stats.bytes_tested),
            ])
            .add_row(vec![
                "Average Bandwidth".to_string(),
                self.format_bandwidth(stats.average_bandwidth_gb_s * 1024_f64.powi(3)),
            ])
            .add_row(vec![
                "Peak Bandwidth".to_string(),
                self.format_bandwidth(stats.peak_bandwidth_gb_s * 1024_f64.powi(3)),
            ]);
        
        table
    }
    
    fn format_driver_status(&self, status: &DriverStatusReport) -> String {
        if status.available {
            if let Some(version) = &status.version {
                format!(
                    "✅ DMA Driver: Available (v{}.{}.{}.{})",
                    version.major, version.minor, version.build, version.revision
                )
            } else {
                "✅ DMA Driver: Available".to_string()
            }
        } else {
            format!(
                "❌ DMA Driver: {}",
                status.error_message.as_ref().unwrap_or(&"Not available".to_string())
            )
        }
    }
    
    fn prepare_driver_stats_table(&self, stats: &DriverStatistics) -> TableData {
        TableData::new()
            .with_title("Driver Statistics")
            .add_header("Metric", ColumnAlignment::Left)
            .add_header("Value", ColumnAlignment::Right)
            .add_row(vec![
                "Active Allocations".to_string(),
                format!("{}", stats.allocations_active),
            ])
            .add_row(vec![
                "Memory Allocated".to_string(),
                self.format_bytes(stats.bytes_allocated),
            ])
            .add_row(vec![
                "Huge Pages (1GB)".to_string(),
                format!("{}", stats.huge_pages_used),
            ])
            .add_row(vec![
                "Large Pages (2MB)".to_string(),
                format!("{}", stats.large_pages_used),
            ])
            .add_row(vec![
                "Standard Pages (4KB)".to_string(),
                format!("{}", stats.standard_pages_used),
            ])
            .add_row(vec![
                "NUMA Nodes".to_string(),
                format!("{:?}", stats.numa_nodes_used),
            ])
    }
    
    fn prepare_thread_timing_table(&self, report: &ThreadTimingReport) -> TableData {
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
            .add_header("Cycles", ColumnAlignment::Right)
            .add_header("Errors", ColumnAlignment::Right);

        for timing in &report.thread_timings {
            let deviation_time_str = if timing.deviation_ms >= 0 {
                format!("+{:.1}s", timing.deviation_ms as f64 / 1000.0)
            } else {
                format!("{:.1}s", timing.deviation_ms as f64 / 1000.0)
            };

            let deviation_data_str = if timing.deviation_data_bytes >= 0 {
                format!("+{}", self.format_bytes(timing.deviation_data_bytes as u64))
            } else {
                format!("-{}", self.format_bytes(timing.deviation_data_bytes.abs() as u64))
            };

            let deviation_speed_str = if timing.deviation_speed_mib_s >= 0.0 {
                format!("+{:.1}", timing.deviation_speed_mib_s)
            } else {
                format!("{:.1}", timing.deviation_speed_mib_s)
            };

            table = table.add_row(vec![
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
                if timing.errors > 0 { format!("{}", timing.errors) } else { "✅".to_string() },
            ]);
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

            table = table.add_row(vec![
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
                if total_errors > 0 { format!("{}", total_errors) } else { "✅".to_string() },
            ]);
        }

        table
    }
    
    fn prepare_memory_status_table(&self, status: &CurrentMemoryStatus) -> TableData {
        TableData::new()
            .with_title("Current Memory Status")
            .add_header("Memory Type", ColumnAlignment::Left)
            .add_header("Total", ColumnAlignment::Right)
            .add_header("Available", ColumnAlignment::Right)
            .add_header("Used", ColumnAlignment::Right)
            .add_header("Status", ColumnAlignment::Center)
            .add_row(vec![
                "Physical RAM".to_string(),
                format!("{:.2} GiB", status.physical_total_gib),
                format!("{:.2} GiB", status.physical_available_gib),
                format!("{:.2} GiB", status.physical_used_gib),
                format!("{:.1}% free", status.physical_free_percent),
            ])
            .add_row(vec![
                "Page File".to_string(),
                format!("{:.2} GiB", status.page_file_total_gib),
                format!("{:.2} GiB", status.page_file_available_gib),
                format!("{:.2} GiB", status.page_file_used_gib),
                format!("{:.1}% load", status.memory_load_percent),
            ])
            .with_footer("  Memory Strategy: Modern Optimal Allocation")
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
    
    fn prepare_page_allocation_table(&self, report: &PageAllocationReport) -> TableData {
        let mut table = TableData::new()
            .with_title("Enhanced Allocation Table")
            .add_header("Page Type", ColumnAlignment::Left)
            .add_header("User Constraints", ColumnAlignment::Center)
            .add_header("Requested", ColumnAlignment::Right)
            .add_header("Allocated", ColumnAlignment::Right)
            .add_header("Result", ColumnAlignment::Left);
        
        for page_type in &report.page_types {
            // Determine constraint based on actual allocation behavior rather than theoretical constraints
            let constraint = match page_type.page_type.as_str() {
                "Huge (1GB)" => {
                    // If system has no huge page support or driver not available, it's effectively blocked
                    if page_type.allocated > 0 || page_type.requested > 0 {
                        "✅ Allowed"
                    } else {
                        "❌ Blocked"
                    }
                },
                "Large (2MB)" => {
                    // If we allocated large pages, they were clearly allowed
                    if page_type.allocated > 0 {
                        "✅ Allowed"
                    } else if page_type.requested > 0 {
                        "❌ Failed"
                    } else {
                        "✅ Allowed"
                    }
                },
                _ => "✅ Allowed",
            };
            
            let result = if page_type.success {
                format!("✅ {}", self.format_bytes(page_type.allocated))
            } else {
                "❌ None".to_string()
            };
            
            table = table.add_row(vec![
                page_type.page_type.clone(),
                constraint.to_string(),
                if page_type.requested > 0 {
                    self.format_bytes(page_type.requested)
                } else {
                    "-".to_string()
                },
                if page_type.allocated > 0 {
                    self.format_bytes(page_type.allocated)
                } else {
                    "-".to_string()
                },
                result,
            ]);
        }
        
        table
    }
    
    fn prepare_performance_by_thread_table(&self, report: &PerformanceByThreadReport) -> TableData {
        let mut table = TableData::new()
            .with_title("Performance by Thread")
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
            .add_header("Errors", ColumnAlignment::Right);

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

            table = table.add_row(vec![
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
                if thread.total_errors > 0 { format!("{}", thread.total_errors) } else { "✅".to_string() },
            ]);
        }

        // Add average row
        if !report.threads.is_empty() {
            let total_errors: u64 = report.threads.iter().map(|t| t.total_errors).sum();
            table = table.add_row(vec![
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
                if total_errors > 0 { format!("{}", total_errors) } else { "✅".to_string() },
            ]);
        }

        table
    }
    
    fn prepare_performance_by_cpu_table(&self, report: &PerformanceByCpuReport) -> TableData {
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
            .add_header("Dev S", ColumnAlignment::Right)
            .add_header("Errors", ColumnAlignment::Right);

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

            table = table.add_row(vec![
                format!("{}", cpu.cpu_id),
                format!("{}", cpu.physical_core_id),
                format!("{}", cpu.numa_node),
                self.format_duration(Duration::from_millis(cpu.total_time_ms as u64)),
                format!("{:+.1}s", dev_time_ms as f64 / 1000.0),
                self.format_bytes(cpu.total_bytes),
                self.format_bytes_signed(dev_bytes),
                format!("{:.1} MiB/s", cpu.throughput_mib_s),
                format!("{:+.1}", dev_speed),
                if cpu.total_errors > 0 { format!("{}", cpu.total_errors) } else { "✅".to_string() },
            ]);
        }

        // Add average row
        if !report.cpus.is_empty() {
            let total_errors: u64 = report.cpus.iter().map(|c| c.total_errors).sum();
            table = table.add_row(vec![
                "Avg".to_string(),
                "".to_string(),
                "".to_string(),
                self.format_duration(Duration::from_millis(avg_time_ms as u64)),
                "".to_string(),
                self.format_bytes(avg_bytes),
                "".to_string(),
                format!("{:.1} MiB/s", avg_speed),
                "".to_string(),
                if total_errors > 0 { format!("{}", total_errors) } else { "✅".to_string() },
            ]);
        }

        table
    }
    
    fn prepare_performance_by_physical_core_table(&self, report: &PerformanceByPhysicalCoreReport) -> TableData {
        let mut table = TableData::new()
            .with_title("Performance by Physical Core")
            .add_header("P Core", ColumnAlignment::Center)
            .add_header("L CPUs", ColumnAlignment::Center)
            .add_header("Time", ColumnAlignment::Right)
            .add_header("Dev T", ColumnAlignment::Right)
            .add_header("Data", ColumnAlignment::Right)
            .add_header("Dev D", ColumnAlignment::Right)
            .add_header("Speed", ColumnAlignment::Right)
            .add_header("Dev S", ColumnAlignment::Right)
            .add_header("Errors", ColumnAlignment::Right);

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

            table = table.add_row(vec![
                format!("{}", core.core_id),
                cpu_list,
                self.format_duration(Duration::from_millis(core.total_time_ms as u64)),
                format!("{:+.1}s", dev_time_ms as f64 / 1000.0),
                self.format_bytes(core.total_bytes),
                self.format_bytes_signed(dev_bytes),
                format!("{:.1} MiB/s", core.throughput_mib_s),
                format!("{:+.1}", dev_speed),
                if core.total_errors > 0 { format!("{}", core.total_errors) } else { "✅".to_string() },
            ]);
        }

        // Add average row
        if !report.cores.is_empty() {
            let total_errors: u64 = report.cores.iter().map(|c| c.total_errors).sum();
            table = table.add_row(vec![
                "Avg".to_string(),
                "".to_string(),
                self.format_duration(Duration::from_millis(avg_time_ms as u64)),
                "".to_string(),
                self.format_bytes(avg_bytes),
                "".to_string(),
                format!("{:.1} MiB/s", avg_speed),
                "".to_string(),
                if total_errors > 0 { format!("{}", total_errors) } else { "✅".to_string() },
            ]);
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
            .add_header("Thread ID", ColumnAlignment::Right);
        
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
        
        // Extract percentages from allocation type if present
        let (base_strategy, percentages) = if let Some(split) = &report.split_reserve {
            let base = report.allocation_type.replace("Split Reserve from Available", "Reserve pre/post split")
                                           .replace("Split Reserve from Total", "Reserve pre/post split from Total")
                                           .replace("Split Target Allocation", "Target allocation pre/post split")
                                           .replace("Split Legacy TM5", "Legacy TM5 pre/post split");
            // Remove any existing percentage info
            let clean_base = if let Some(pos) = base.find(" (") {
                base[..pos].to_string()
            } else {
                base
            };
            (clean_base, format!("{}%:{}%", split.pre_percent as u32, split.post_percent as u32))
        } else {
            (report.allocation_type.clone(), "-".to_string())
        };
        
        table = table
            .add_row(vec![
                "Strategy Type".to_string(),
                "-".to_string(),
                percentages,
                base_strategy.clone(),
            ])
            .add_row(vec![
                "Testing Allocation".to_string(),
                self.format_bytes(report.allocation_bytes),
                format!("{:.1}%", (report.allocation_bytes as f64 / report.available_physical_bytes as f64) * 100.0),
                "of available".to_string(),
            ])
            .add_row(vec![
                "Reserved Amount".to_string(),
                self.format_bytes(report.reserve_bytes),
                format!("{:.1}%", (report.reserve_bytes as f64 / report.available_physical_bytes as f64) * 100.0),
                "of available".to_string(),
            ])
            .add_row(vec![
                "Min Start Address".to_string(),
                format!("{:.2} GiB", report.min_start_address as f64 / 1024_f64.powi(3)),
                if report.min_start_address > 0 {
                    format!("{:.1}%", (report.min_start_address as f64 / report.available_physical_bytes as f64) * 100.0)
                } else {
                    "0.0%".to_string()
                },
                "offset, of available".to_string(),
            ]);
        
        // Add split reserve details if available
        if let Some(split) = &report.split_reserve {
            table = table
                .add_row(vec![
                    "Pre-Buffer (Frag Prevention)".to_string(),
                    self.format_bytes(split.pre_buffer_bytes),
                    format!("{:.1}%", split.pre_percent),
                    "of reserve".to_string(),
                ])
                .add_row(vec![
                    "Post-Reserve (Ceil/frag Protection)".to_string(),
                    self.format_bytes(split.post_reserve_bytes),
                    format!("{:.1}%", split.post_percent),
                    "of reserve".to_string(),
                ]);
        }
        
        // Add footer with allocation strategy summary
        let footer = if report.split_reserve.is_some() {
            format!("Strategy: {} with split reserve optimization", base_strategy)
        } else {
            format!("Strategy: {}", base_strategy)
        };
        
        table.with_footer(&footer)
    }
    
    fn format_allocation_mode(&self, mode: &crate::memory::allocation_strategy::AllocationMode) -> String {
        use crate::memory::allocation_strategy::{AllocationMode, ReserveAmount};
        match mode {
            AllocationMode::ReserveFromAvailable { reserve } => {
                match reserve {
                    ReserveAmount::Bytes(bytes) => format!("ReserveFromAvailable ({})", self.format_bytes(*bytes)),
                    ReserveAmount::Percentage(pct) => format!("ReserveFromAvailable ({:.1}%)", pct),
                }
            }
            AllocationMode::ReserveFromTotal { reserve } => {
                match reserve {
                    ReserveAmount::Bytes(bytes) => format!("ReserveFromTotal ({})", self.format_bytes(*bytes)),
                    ReserveAmount::Percentage(pct) => format!("ReserveFromTotal ({:.1}%)", pct),
                }
            }
            AllocationMode::AllocateTarget { target } => {
                match target {
                    ReserveAmount::Bytes(bytes) => format!("AllocateTarget ({})", self.format_bytes(*bytes)),
                    ReserveAmount::Percentage(pct) => format!("AllocateTarget ({:.1}%)", pct),
                }
            }
            AllocationMode::LegacyTM5 { reserve_mb } => {
                format!("LegacyTM5 (reserve: {} MB)", reserve_mb)
            }
        }
    }
    
    fn format_window_mode(&self, mode: &crate::tests::WindowMode) -> String {
        use crate::tests::WindowMode;
        match mode {
            WindowMode::FullAllocation => "FullAllocation".to_string(),
            WindowMode::Absolute { size_bytes } => {
                format!("Absolute ({})", self.format_bytes(*size_bytes as u64))
            }
            WindowMode::CacheTotal { fraction } => {
                format!("CacheTotal ({:.2}x)", fraction)
            }
            WindowMode::Cache { target } => {
                format!("Cache ({})", target.name())
            }
        }
    }

    fn format_window_mode_with_size(&self, mode: &crate::tests::WindowMode, cache_info: &crate::cache::CacheInfo, thread_count: usize) -> String {
        use crate::tests::WindowMode;
        match mode {
            WindowMode::FullAllocation => "FullAllocation".to_string(),
            WindowMode::Absolute { size_bytes } => {
                self.format_bytes(*size_bytes as u64)
            }
            WindowMode::CacheTotal { fraction } => {
                // Naive sum-of-tiers — no thread division (CacheTotal is intentionally coarse)
                let total_cache = cache_info.per_core_l1d + cache_info.per_core_l2 + cache_info.l3_cache;
                let size = (total_cache as f64 * fraction) as u64;
                format!("{} (CacheTotal {:.2}x)", self.format_bytes(size), fraction)
            }
            WindowMode::Cache { target } => {
                // Calculate the actual window size using the target's method
                let size = target.calculate_window_size(cache_info, thread_count);
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
            ChunkMode::Absolute { size_bytes } => {
                format!("Absolute ({})", self.format_bytes(*size_bytes as u64))
            }
            ChunkMode::Fraction { fraction } => {
                format!("Fraction ({:.1}%)", fraction * 100.0)
            }
            ChunkMode::CacheTotal { fraction } => {
                format!("CacheTotal ({:.2}x)", fraction)
            }
            ChunkMode::Cache { target } => {
                format!("Cache ({})", target.name())
            }
        }
    }
    
    fn prepare_test_configuration_table(&self, report: &TestConfigurationReport) -> TableData {
        let mut table = TableData::new()
            .with_title("Test Configuration")
            .add_header("#", ColumnAlignment::Right)
            .add_header("Test Name", ColumnAlignment::Left)
            .add_header("Timing", ColumnAlignment::Right)
            .add_header("Parameter", ColumnAlignment::Left)
            .add_header("Window Mode", ColumnAlignment::Left)
            .add_header("Block Mode", ColumnAlignment::Left)
            .add_header("Flags", ColumnAlignment::Left);

        for test in &report.tests {
            table = table.add_row(vec![
                test.number.to_string(),
                test.name.clone(),
                test.timing.clone(),
                test.parameter.clone(),
                test.window_mode.clone(),
                test.chunk_mode.clone(),
                test.flags.join(", "),
            ]);
        }
        
        table.with_footer(format!("Suite Timing: {} | Total Tests: {}", 
                                   report.suite_timing, report.test_count))
    }
    
    fn prepare_cycle_report_table(&self, report: &CycleReport) -> TableData {
        let mut table = TableData::new()
            .with_title(format!("Cycle {} Report", report.cycle_number))
            .add_header("#", ColumnAlignment::Right)
            .add_header("Test Name", ColumnAlignment::Left)
            .add_header("Duration", ColumnAlignment::Right)
            .add_header("Data", ColumnAlignment::Right)
            .add_header("Throughput", ColumnAlignment::Right)
            .add_header("Errors", ColumnAlignment::Center);
        
        for test in &report.test_performances {
            let error_display = if test.errors > 0 {
                format!("{}", test.errors)
            } else {
                "✅".to_string()
            };
            
            table = table.add_row(vec![
                test.number.to_string(),
                test.name.clone(),
                format!("{:.1}s", test.duration_secs),
                format!("{:.2} GiB", test.data_processed_gib),
                format!("{:.1} MiB/s ({:.2} GiB/s)", 
                        test.throughput_mib_s, test.throughput_gib_s),
                error_display,
            ]);
        }
        
        table.with_footer(format!("Cycle Duration: {}s", report.duration_secs))
    }
    
    fn prepare_cpu_variance_table(&self, report: &CpuVarianceReport) -> TableData {
        let mut table = TableData::new()
            .with_title("CPU Performance Variance")
            .add_header("L CPU", ColumnAlignment::Right)
            .add_header("Speed", ColumnAlignment::Right)
            .add_header("Variance", ColumnAlignment::Right)
            .add_header("Test Count", ColumnAlignment::Right)
            .add_header("Status", ColumnAlignment::Center);
        
        let mut entries = report.cpu_performances.clone();
        entries.sort_by(|a, b| a.cpu_id.cmp(&b.cpu_id));
        
        for cpu in &entries {
            let variance_str = format!("{:+.1}%", cpu.variance_percent);
            let status = match cpu.variance_percent.abs() {
                v if v < 5.0 => "✅ Normal",
                v if v < 10.0 => "⚠️ Moderate",
                _ => "❌ High",
            };
            
            table = table.add_row(vec![
                cpu.cpu_id.to_string(),
                format!("{:.1} MiB/s", cpu.throughput_mib_s),
                variance_str,
                cpu.test_count.to_string(),
                status.to_string(),
            ]);
        }
        
        table.with_footer(format!("Average Throughput: {:.1} MiB/s", 
                                   report.average_throughput_mib_s))
    }
    
    fn prepare_final_summary_overview_table(&self, report: &FinalTestSummaryReport) -> TableData {
        TableData::new()
            .with_title("Final Test Summary - Overview")
            .add_header("Metric", ColumnAlignment::Left)
            .add_header("Value", ColumnAlignment::Right)
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
                if report.total_errors > 0 {
                    format!("{}", report.total_errors)
                } else {
                    "✅".to_string()
                },
            ])
    }
    
    fn prepare_final_summary_performance_table(&self, report: &FinalTestSummaryReport) -> TableData {
        // Check if any test has latency data
        let has_latency = report.per_test_summaries.iter().any(|t| t.latency_samples.is_some());

        let mut table = TableData::new()
            .with_title("Final Test Summary - Per-Test Performance")
            .add_header("#", ColumnAlignment::Center)
            .add_header("Test Name", ColumnAlignment::Left)
            .add_header("Duration", ColumnAlignment::Right)
            .add_header("Data", ColumnAlignment::Right)
            .add_header("Throughput", ColumnAlignment::Right);

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

        table = table.add_header("Err", ColumnAlignment::Center);

        for (idx, test) in report.per_test_summaries.iter().enumerate() {
            let error_display = if test.total_errors > 0 {
                format!("{}", test.total_errors)
            } else {
                "✅".to_string()
            };

            let mut row = vec![
                format!("{}", idx + 1),
                test.name.clone(),
                format!("{:.1}s", test.average_duration_secs),
                format!("{:.2} GiB", test.total_data_gib),
                format!("{:.0} MiB/s", test.average_throughput_mib_s),
            ];

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

            row.push(error_display);
            table = table.add_row(row);
        }

        let footer = if has_latency {
            format!("Averaged across {} cycles. Latency values in nanoseconds.", report.cycles_completed)
        } else {
            format!("Averaged across {} cycles", report.cycles_completed)
        };
        table.with_footer(footer)
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
            .add_header("Deviation", ColumnAlignment::Right)
            .add_header("Status", ColumnAlignment::Left);

        let fairness = &report.allocation_fairness;

        // Show all threads, not just unfair ones
        for thread_alloc in &report.per_thread_allocation {
            let deviation_percent = ((thread_alloc.total_bytes as f64 - fairness.mean_allocation_bytes) / fairness.mean_allocation_bytes) * 100.0;
            
            let deviation_str = if deviation_percent >= 0.0 {
                format!("+{:.1}%", deviation_percent)
            } else {
                format!("{:.1}%", deviation_percent)
            };
            
            let status = if deviation_percent.abs() > 10.0 {
                "⚠️ Unfair"
            } else if deviation_percent.abs() > 5.0 {
                "🟡 Slightly unfair"
            } else {
                "✅ Fair"
            };
            
            table = table.add_row(vec![
                thread_alloc.thread_id.to_string(),
                self.format_bytes(thread_alloc.total_bytes),
                deviation_str,
                status.to_string(),
            ]);
        }

        table.with_footer(format!(
            "Fairness Stats: Min={}, Max={}, Mean={:.1} GiB, CV={:.3} ({})",
            self.format_bytes(fairness.min_allocation_bytes),
            self.format_bytes(fairness.max_allocation_bytes),
            bytes_to_gib_f64(fairness.mean_allocation_bytes as u64),
            fairness.coefficient_of_variation,
            if fairness.coefficient_of_variation < 0.1 { "Fair Distribution" } else { "Unfair Distribution" }
        ))
    }

    fn prepare_latency_summary_consolidated_table(&self, report: &LatencyTestSummaryReport) -> TableData {
        let mut table = TableData::new()
            .with_title(format!("Latency Test Summary ({} threads)", report.thread_count))
            .add_header("Level", ColumnAlignment::Left)
            .add_header("Window", ColumnAlignment::Right)
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
            .add_header("Max", ColumnAlignment::Right)
            .add_header("Spread", ColumnAlignment::Right);

        for level in &report.levels_tested {
            let p = &level.consolidated;
            let window_str = if level.window_size_bytes >= 1024 * 1024 {
                format!("{} MB", level.window_size_bytes / (1024 * 1024))
            } else {
                format!("{} KB", level.window_size_bytes / 1024)
            };

            table = table.add_row(vec![
                level.target_name.clone(),
                window_str,
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
                format!("{:.1}", p.max_ns),
                format!("{:.2}x", p.spread_ratio),
            ]);
        }

        table.with_footer("All values in nanoseconds. Spread = P95/P5 ratio.")
    }

    fn prepare_latency_per_thread_table(&self, level: &LatencyLevelSummary) -> TableData {
        // No title - test name and window size are already shown in the main test header and config line
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