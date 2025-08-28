use crate::progress::TestSummary;
use crate::constants::{BYTES_PER_GIB_F64, MB_F64};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestRunResult {
    pub metadata: TestRunMetadata,
    pub system_info: SystemInfoSnapshot,
    pub cycles: Vec<CycleResult>,
    pub overall_stats: OverallStats,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestRunMetadata {
    pub tmr_version: String,
    pub start_time: u64,          // Unix timestamp
    pub start_time_iso: String,   // Human-readable ISO format
    pub filename: String,         // Generated filename
    pub config_name: Option<String>, // If loaded from config
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemInfoSnapshot {
    pub cpu_brand: String,
    pub cpu_cores: usize,
    pub total_memory_gib: f64,
    pub allocated_memory_gib: f64,
    pub thread_count: usize,
    pub large_pages_enabled: bool,
    pub simd_capabilities: String,
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
    pub throughput_mib_s: f64,
    pub throughput_gib_s: f64,
    pub errors: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CycleStats {
    pub total_bytes: u64,
    pub total_errors: u64,
    pub avg_throughput_mib_s: f64,
    pub avg_throughput_gib_s: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverallStats {
    pub total_runtime_secs: u64,
    pub cycles_completed: u32,
    pub total_data_processed_gib: f64,
    pub overall_throughput_mib_s: f64,
    pub overall_throughput_gib_s: f64,
    pub total_errors: u64,
    pub per_test_averages: Vec<TestAverage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestAverage {
    pub test_number: usize,
    pub name: String,
    pub avg_duration_ms: u128,
    pub avg_bytes_processed: u64,
    pub avg_throughput_mib_s: f64,
    pub avg_throughput_gib_s: f64,
    pub total_errors: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestComparison {
    pub baseline: TestRunMetadata,
    pub current: TestRunMetadata,
    pub comparison_time: String,
    pub overall_comparison: OverallComparison,
    pub per_test_comparisons: Vec<TestComparisonResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverallComparison {
    pub runtime_diff_secs: i64,
    pub runtime_diff_percent: f64,
    pub throughput_diff_mib_s: f64,
    pub throughput_diff_percent: f64,
    pub data_processed_diff_gib: f64,
    pub errors_diff: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestComparisonResult {
    pub test_number: usize,
    pub name: String,
    pub duration_diff_ms: i128,
    pub duration_diff_percent: f64,
    pub throughput_diff_mib_s: f64,
    pub throughput_diff_percent: f64,
    pub bytes_diff: i64,
    pub errors_diff: i64,
}

impl Default for TestRunResult {
    fn default() -> Self {
        Self::new()
    }
}

impl TestRunResult {
    pub fn new() -> Self {
        // Create results directory if it doesn't exist
        if let Err(e) = std::fs::create_dir_all("results") {
            log::warn!("Failed to create results directory: {}", e);
        }

        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        let start_time_iso = chrono::DateTime::from_timestamp(now as i64, 0)
            .unwrap_or_default()
            .format("%Y-%m-%d %H:%M:%S UTC")
            .to_string();
        
        let filename = format!("results/TMR_{}.json", 
            chrono::DateTime::from_timestamp(now as i64, 0)
                .unwrap_or_default()
                .format("%Y-%m-%d_%H-%M-%S"));

        Self {
            metadata: TestRunMetadata {
                tmr_version: "1.0.0".to_string(),
                start_time: now,
                start_time_iso,
                filename: filename.clone(),
                config_name: None,
            },
            system_info: SystemInfoSnapshot {
                cpu_brand: "Unknown".to_string(),
                cpu_cores: num_cpus::get_physical(),
                total_memory_gib: 0.0,
                allocated_memory_gib: 0.0,
                thread_count: 0,
                large_pages_enabled: false,
                simd_capabilities: crate::detect_simd_capabilities(),
            },
            cycles: Vec::new(),
            overall_stats: OverallStats {
                total_runtime_secs: 0,
                cycles_completed: 0,
                total_data_processed_gib: 0.0,
                overall_throughput_mib_s: 0.0,
                overall_throughput_gib_s: 0.0,
                total_errors: 0,
                per_test_averages: Vec::new(),
            },
        }
    }

    pub fn set_system_info(&mut self, cpu_brand: &str, total_memory_gib: f64, allocated_memory_gib: f64, thread_count: usize, large_pages_enabled: bool) {
        self.system_info.cpu_brand = cpu_brand.to_string();
        self.system_info.total_memory_gib = total_memory_gib;
        self.system_info.allocated_memory_gib = allocated_memory_gib;
        self.system_info.thread_count = thread_count;
        self.system_info.large_pages_enabled = large_pages_enabled;
    }

    pub fn set_config_name(&mut self, config_name: Option<String>) {
        self.metadata.config_name = config_name;
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
                throughput_gib_s: summary.throughput_mib_s / 1024.0,
                errors: summary.errors,
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
                avg_throughput_gib_s: avg_throughput_mib / 1024.0,
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
        self.overall_stats.overall_throughput_gib_s = self.overall_stats.overall_throughput_mib_s / 1024.0;
        
        self.overall_stats.total_errors = self.cycles.iter().map(|c| c.cycle_stats.total_errors).sum();

        // Calculate per-test averages
        self.calculate_per_test_averages();
    }

    fn calculate_per_test_averages(&mut self) {
        let mut test_aggregates: HashMap<String, (usize, u128, u64, u64)> = HashMap::new(); // test_number, total_duration_ms, total_bytes, total_errors

        for cycle in &self.cycles {
            for test in &cycle.tests {
                let entry = test_aggregates.entry(test.name.clone()).or_insert((test.test_number, 0, 0, 0));
                entry.1 += test.duration_ms;
                entry.2 += test.bytes_processed;
                entry.3 += test.errors;
            }
        }

        let cycle_count = self.cycles.len() as u128;
        self.overall_stats.per_test_averages = test_aggregates.into_iter().map(|(name, (test_number, total_duration, total_bytes, total_errors))| {
            let avg_duration_ms = total_duration / cycle_count;
            let avg_bytes = total_bytes / cycle_count as u64;
            let avg_throughput_mib = if avg_duration_ms > 0 {
                (avg_bytes as f64 / MB_F64) / (avg_duration_ms as f64 / 1000.0)
            } else {
                0.0
            };

            TestAverage {
                test_number,
                name,
                avg_duration_ms,
                avg_bytes_processed: avg_bytes,
                avg_throughput_mib_s: avg_throughput_mib,
                avg_throughput_gib_s: avg_throughput_mib / 1024.0,
                total_errors,
            }
        }).collect();

        // Sort by test number
        self.overall_stats.per_test_averages.sort_by_key(|t| t.test_number);
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
        report.push_str(&format!("Run Time: {}\n", self.metadata.start_time_iso));
        report.push_str(&format!("TMR Version: {}\n", self.metadata.tmr_version));
        if let Some(config) = &self.metadata.config_name {
            report.push_str(&format!("Config: {}\n", config));
        }
        report.push('\n');

        report.push_str("System Information:\n");
        report.push_str(&format!("  CPU: {}\n", self.system_info.cpu_brand));
        report.push_str(&format!("  Cores: {} physical\n", self.system_info.cpu_cores));
        report.push_str(&format!("  Memory: {:.2} GiB total, {:.2} GiB allocated\n", 
            self.system_info.total_memory_gib, self.system_info.allocated_memory_gib));
        report.push_str(&format!("  Threads: {}\n", self.system_info.thread_count));
        report.push_str(&format!("  Large Pages: {}\n", if self.system_info.large_pages_enabled { "Enabled" } else { "Disabled" }));
        report.push_str(&format!("  SIMD: {}\n", self.system_info.simd_capabilities));
        report.push('\n');

        report.push_str("Overall Results:\n");
        report.push_str(&format!("  Runtime: {}s\n", self.overall_stats.total_runtime_secs));
        report.push_str(&format!("  Cycles: {}\n", self.overall_stats.cycles_completed));
        report.push_str(&format!("  Data Processed: {:.2} GiB\n", self.overall_stats.total_data_processed_gib));
        report.push_str(&format!("  Throughput: {:.1} MiB/s ({:.2} GiB/s)\n", 
            self.overall_stats.overall_throughput_mib_s, self.overall_stats.overall_throughput_gib_s));
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
                    test.avg_throughput_gib_s,
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

    Ok(TestComparison {
        baseline: baseline.metadata,
        current: current.metadata,
        comparison_time,
        overall_comparison,
        per_test_comparisons,
    })
}

impl TestComparison {
    pub fn to_text_report(&self) -> String {
        let mut report = String::new();
        
        report.push_str("=== TMR Test Result Comparison ===\n");
        report.push_str(&format!("Comparison Time: {}\n", self.comparison_time));
        report.push_str(&format!("Baseline: {} ({})\n", self.baseline.start_time_iso, self.baseline.filename));
        report.push_str(&format!("Current:  {} ({})\n", self.current.start_time_iso, self.current.filename));
        report.push('\n');

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