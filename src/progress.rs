use crate::tests::TestStats;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use std::thread;

// Enhanced progress tracking with per-cycle progress and better error tracking
pub struct ProgressTracker {
    // Test tracking - per cycle
    pub tests_per_cycle: AtomicU64,          // Number of unique tests in one cycle
    pub completed_tests_this_cycle: AtomicU64, // Tests completed in current cycle
    pub current_cycle: AtomicU64,            // Current cycle number (1-based)
    pub total_cycles: AtomicU64,             // Total planned cycles (0 = unlimited)
    
    // Performance tracking
    pub total_bytes_processed: AtomicU64,
    pub total_test_time_ms: AtomicU64,
    pub cycle_stats: Mutex<Vec<CycleStats>>,
    
    // Enhanced error tracking
    pub total_errors: AtomicU64,
    pub per_test_errors: Mutex<std::collections::HashMap<String, u64>>, // Track errors per test type
    
    // Phase and throughput tracking
    pub current_phase: Mutex<String>,
    pub current_throughput: AtomicU64,
    pub start_time: Instant,
    
    // Cycle timing
    pub cycle_start_time: Mutex<Option<Instant>>,
}

#[derive(Debug, Clone)]
pub struct CycleStats {
    pub cycle_number: u32,
    pub duration_secs: u64,
    pub bytes_processed: u64,
    pub test_stats: Vec<TestSummary>,
}

#[derive(Debug, Clone)]
pub struct TestSummary {
    pub name: String,
    pub duration_ms: u128,
    pub bytes_processed: u64,
    pub throughput_mib_s: f64,
    pub errors: u64,
}

impl ProgressTracker {
    pub fn new() -> Self {
        Self {
            tests_per_cycle: AtomicU64::new(0),
            completed_tests_this_cycle: AtomicU64::new(0),
            current_cycle: AtomicU64::new(0),
            total_cycles: AtomicU64::new(0),
            total_bytes_processed: AtomicU64::new(0),
            total_test_time_ms: AtomicU64::new(0),
            cycle_stats: Mutex::new(Vec::new()),
            total_errors: AtomicU64::new(0),
            per_test_errors: Mutex::new(std::collections::HashMap::new()),
            current_phase: Mutex::new("Initializing".to_string()),
            current_throughput: AtomicU64::new(0),
            start_time: Instant::now(),
            cycle_start_time: Mutex::new(None),
        }
    }

    pub fn set_cycle_info(&self, current_cycle: u32, total_cycles: Option<u32>, tests_per_cycle: u64) {
        self.current_cycle.store(current_cycle as u64, Ordering::Relaxed);
        self.tests_per_cycle.store(tests_per_cycle, Ordering::Relaxed);
        
        if let Some(total) = total_cycles {
            self.total_cycles.store(total as u64, Ordering::Relaxed);
        } else {
            self.total_cycles.store(0, Ordering::Relaxed);
        }
    }

    pub fn start_new_cycle(&self, cycle_number: u32) {
        // Reset per-cycle counters
        self.completed_tests_this_cycle.store(0, Ordering::Relaxed);
        self.current_cycle.store(cycle_number as u64, Ordering::Relaxed);
        
        // Update cycle timing
        if let Ok(mut cycle_start) = self.cycle_start_time.lock() {
            *cycle_start = Some(Instant::now());
        }
    }
    
    pub fn complete_cycle(&self, cycle_number: u32, test_summaries: Vec<TestSummary>) {
        if let Ok(cycle_start) = self.cycle_start_time.lock() {
            if let Some(start_time) = *cycle_start {
                let duration = start_time.elapsed().as_secs();
                let bytes = test_summaries.iter().map(|t| t.bytes_processed).sum();
                
                let cycle_stat = CycleStats {
                    cycle_number,
                    duration_secs: duration,
                    bytes_processed: bytes,
                    test_stats: test_summaries,
                };
                
                if let Ok(mut stats) = self.cycle_stats.lock() {
                    stats.push(cycle_stat);
                }
            }
        }
    }

    pub fn add_errors(&self, count: u64) {
        self.total_errors.fetch_add(count, Ordering::Relaxed);
    }

    pub fn add_test_errors(&self, test_name: &str, count: u64) {
        self.add_errors(count);
        
        if count > 0 {
            if let Ok(mut per_test_errors) = self.per_test_errors.lock() {
                *per_test_errors.entry(test_name.to_string()).or_insert(0) += count;
            }
        }
    }

    pub fn complete_test(&self, stats: &TestStats) {
        // Increment completed tests for this cycle
        self.completed_tests_this_cycle.fetch_add(1, Ordering::Relaxed);
        
        // Track overall stats
        self.total_bytes_processed.fetch_add(stats.bytes_processed as u64, Ordering::Relaxed);
        self.total_test_time_ms.fetch_add(stats.elapsed_ms as u64, Ordering::Relaxed);
        
        // Track errors per test
        self.add_test_errors(stats.name, stats.error_count);

        if stats.elapsed_ms > 0 {
            let throughput = (stats.bytes_processed as u128 * 1000) / stats.elapsed_ms;
            self.current_throughput.store(throughput as u64, Ordering::Relaxed);
        }
    }

    pub fn set_phase(&self, phase: &str) {
        if let Ok(mut current) = self.current_phase.lock() {
            *current = phase.to_string();
        }
    }

    pub fn get_status(&self) -> ProgressStatus {
        let completed = self.completed_tests_this_cycle.load(Ordering::Relaxed);
        let tests_per_cycle = self.tests_per_cycle.load(Ordering::Relaxed);
        let current_cycle = self.current_cycle.load(Ordering::Relaxed);
        let total_cycles = self.total_cycles.load(Ordering::Relaxed);
        let errors = self.total_errors.load(Ordering::Relaxed);
        
        let phase = self
            .current_phase
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_else(|_| "Unknown".to_string());
            
        let throughput_raw = self.current_throughput.load(Ordering::Relaxed);
        let throughput_gib_s = (throughput_raw as f64) / (1024.0 * 1024.0 * 1024.0);
        let throughput_mib_s = (throughput_raw as f64) / (1024.0 * 1024.0);
        let total_runtime = self.start_time.elapsed();

        // Calculate progress percentage for current cycle only
        let progress_pct = if tests_per_cycle > 0 {
            ((completed * 100) / tests_per_cycle).min(100)
        } else {
            0
        };

        ProgressStatus {
            completed_tests: completed,
            tests_per_cycle,
            current_cycle,
            total_cycles,
            errors,
            phase,
            throughput_gib_s,
            throughput_mib_s,
            total_runtime,
            progress_percent: progress_pct,
        }
    }
    
    pub fn get_cycle_stats(&self) -> Vec<CycleStats> {
        self.cycle_stats.lock().map(|guard| guard.clone()).unwrap_or_default()
    }

    pub fn get_per_test_error_summary(&self) -> std::collections::HashMap<String, u64> {
        self.per_test_errors.lock().map(|guard| guard.clone()).unwrap_or_default()
    }
}

pub struct ProgressStatus {
    pub completed_tests: u64,
    pub tests_per_cycle: u64,
    pub current_cycle: u64,
    pub total_cycles: u64, // 0 = unlimited
    pub errors: u64,
    pub phase: String,
    pub throughput_gib_s: f64,
    pub throughput_mib_s: f64,
    pub total_runtime: std::time::Duration,
    pub progress_percent: u64,
}

pub fn progress_reporter(progress: Arc<ProgressTracker>) {
    let mut last_update = Instant::now();

    loop {
        thread::sleep(std::time::Duration::from_millis(500)); // Check more frequently

        let status = progress.get_status();

        // Update progress display every 2 seconds or when completed
        if last_update.elapsed().as_secs() >= 2 || status.phase == "Completed" {
            // Clear the line and redraw progress (in case logs interrupted us)
            print!("\r\x1b[K");
            
            // Format runtime as HH:MM:SS
            let runtime_secs = status.total_runtime.as_secs();
            let hours = runtime_secs / 3600;
            let minutes = (runtime_secs % 3600) / 60;
            let seconds = runtime_secs % 60;
            let runtime_str = format!("{:02}:{:02}:{:02}", hours, minutes, seconds);

            // Format progress display with enhanced error information
            if status.total_cycles == 0 {
                // Unlimited cycles mode
                print!(
                    "Runtime: {} | Cycle: {} | Progress: {}/{} ({}%) | {} | Speed: {:.2} GiB/s ({:.1} MiB/s){}",
                    runtime_str, 
                    status.current_cycle,
                    status.completed_tests, 
                    status.tests_per_cycle, 
                    status.progress_percent, 
                    status.phase, 
                    status.throughput_gib_s, 
                    status.throughput_mib_s,
                    if status.errors > 0 { 
                        format!(" | ⚠️ Errors: {}", status.errors) 
                    } else { 
                        String::new() 
                    }
                );
            } else {
                // Limited cycles mode
                let cycle_info = format!("Cycle: {}/{}", status.current_cycle, status.total_cycles);
                print!(
                    "Runtime: {} | {} | Progress: {}/{} ({}%) | {} | Speed: {:.2} GiB/s ({:.1} MiB/s){}",
                    runtime_str,
                    cycle_info,
                    status.completed_tests, 
                    status.tests_per_cycle, 
                    status.progress_percent, 
                    status.phase, 
                    status.throughput_gib_s, 
                    status.throughput_mib_s,
                    if status.errors > 0 { 
                        format!(" | ⚠️ Errors: {}", status.errors) 
                    } else { 
                        String::new() 
                    }
                );
            }

            if status.phase == "Completed" {
                println!(); // Final newline when completed
                
                // Print per-test error summary if there were any errors
                let error_summary = progress.get_per_test_error_summary();
                if !error_summary.is_empty() {
                    println!("\nError Summary by Test:");
                    for (test_name, error_count) in error_summary {
                        println!("  {}: {} errors", test_name, error_count);
                    }
                }
                break;
            }

            // Flush stdout to ensure progress appears immediately
            std::io::Write::flush(&mut std::io::stdout()).unwrap_or(());
            last_update = Instant::now();
        }

        if status.phase == "Completed" {
            break;
        }
    }
}