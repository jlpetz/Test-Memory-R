use crate::tests::TestStats;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use std::thread;

// Global progress tracking
pub struct ProgressTracker {
    pub total_tests: AtomicU64,
    pub completed_tests: AtomicU64,
    pub total_errors: AtomicU64,
    pub current_phase: Mutex<String>,
    pub current_throughput: AtomicU64,
}

impl ProgressTracker {
    pub fn new() -> Self {
        Self {
            total_tests: AtomicU64::new(0),
            completed_tests: AtomicU64::new(0),
            total_errors: AtomicU64::new(0),
            current_phase: Mutex::new("Initializing".to_string()),
            current_throughput: AtomicU64::new(0),
        }
    }

    pub fn add_errors(&self, count: u64) {
        self.total_errors.fetch_add(count, Ordering::Relaxed);
    }

    pub fn complete_test(&self, stats: &TestStats) {
        self.completed_tests.fetch_add(1, Ordering::Relaxed);
        self.add_errors(stats.error_count);

        if stats.elapsed_ms > 0 {
            let throughput = (stats.bytes_processed as u128 * 1000 * 1000) / stats.elapsed_ms;
            self.current_throughput.store(throughput as u64, Ordering::Relaxed);
        }
    }

    pub fn set_phase(&self, phase: &str) {
        if let Ok(mut current) = self.current_phase.lock() {
            *current = phase.to_string();
        }
    }

    pub fn get_status(&self) -> (u64, u64, u64, String, f64) {
        let completed = self.completed_tests.load(Ordering::Relaxed);
        let total = self.total_tests.load(Ordering::Relaxed);
        let errors = self.total_errors.load(Ordering::Relaxed);
        let phase = self
            .current_phase
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_else(|_| "Unknown".to_string());
        let throughput_raw = self.current_throughput.load(Ordering::Relaxed);
        let throughput_gib_s = (throughput_raw as f64) / (1000.0 * 1024.0 * 1024.0 * 1024.0);

        (completed, total, errors, phase, throughput_gib_s)
    }
}

pub fn progress_reporter(progress: Arc<ProgressTracker>) {
    let mut last_update = Instant::now();

    loop {
        thread::sleep(std::time::Duration::from_millis(1000));

        let (completed, total, errors, phase, throughput) = progress.get_status();

        if last_update.elapsed().as_secs() >= 5 || phase == "Completed" {
            let progress_pct = if total > 0 { (completed * 100) / total } else { 0 };

            print!("\r\x1b[K");
            print!(
                "Progress: {}/{} ({}%) | Errors: {} | Phase: {} | Speed: {:.2} GiB/s",
                completed, total, progress_pct, errors, phase, throughput
            );

            if phase == "Completed" {
                println!();
                break;
            }

            std::io::Write::flush(&mut std::io::stdout()).unwrap_or(());
            last_update = Instant::now();
        }

        if phase == "Completed" {
            break;
        }
    }
}