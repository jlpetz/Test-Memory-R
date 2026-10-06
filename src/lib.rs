#![allow(unsafe_op_in_unsafe_fn)]
#![feature(portable_simd)]
#![feature(clflushopt_target_feature)]
#![feature(simd_x86_clflushopt)]

// The whole program, the command line included, is in this crate, and `cli` is its only public
// item. Everything else is private, so `dead_code` checks all of it. While `main.rs` held the CLI
// and every module here was `pub`, all of them counted as public API, and an uncalled `pub fn` was
// never reported (TODO #69 E).
pub mod cli;

mod config;
mod constants;
mod memory;
mod tests;
mod progress;
mod console; // single writer path while the progress ticker is on screen
mod layout;
mod runner;
mod simd;
mod cache;
mod tsc; // TSC frequency detection (x86_64)
mod results; // New results module
mod table;
mod formatting;
mod thread_pool;
mod cpu_topology;
mod cpu_selection; // two-stage CPU selection: skip-cores filter + cpu-stride spacing
mod reporting;
mod params; // Centralized parameter registry
mod latency_tests; // Latency measurement tests
mod latency_tests_v2; // v2 latency tests: Layout A/B reads + NT write saturation PoC
mod bandwidth_tests; // Bandwidth measurement tests
mod calibration; // Adaptive cache calibration
mod pattern_gen; // v2 pattern generation (LCG, Mode 0/1/2)
mod test_harness; // v2 zero-cost test orchestration harness
mod test_scaffolding; // shared zero-cost bookkeeping for loop-owning v1 tests (TODO #19 Part A)
mod test_memory; // where a test's extent lies in a thread's blocks (TODO 76)
mod app_config; // Persistent application config (tmr-cfg.json)
mod smbios; // SMBIOS table parser (system identity, memory modules)
mod whea; // WHEA hardware-error monitoring via the System event log (TODO #63)
mod run_context; // Run identity + resolved config recorded into result files (TODO #67)
mod seal; // the seal: TM5's test 0, checked and resealed around every chunk (TODO 74)
mod error_context; // where a worker is in the run, for its error lines (TODO 74)

// Common result type for the crate
pub(crate) type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

// Crate-root shortcuts, so other modules can write `crate::X`
use config::*;
use memory::{check_large_page_privilege, check_restart_needed};
use tests::{set_calibration_data, set_active_threads_per_core};
use progress::ProgressTracker;
use layout::{EnhancedMemoryLayout, BlockInfo};
use runner::AllocationBlock;
use simd::detect_simd_capabilities;
use cache::SystemInfo;

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum ErrorMode {
    Log,   // Log errors and continue (default)
    Halt,  // Stop current test on first error
    Panic, // Panic on first error (for debugging)
}


/// One variant since the driver-client purge (TODO #4/5). A ring-0 backend would come back as a
/// second one.
///
/// Names the API only. Whether a block got large pages is decided per allocation, and the results
/// file records the outcome as `page_mix`.
#[derive(Debug, Clone, Copy)]
pub(crate) enum MemoryBackend {
    VirtualAlloc2,
}

pub(crate) struct RuntimeConfig {
    pub memory_backend: MemoryBackend,
    pub large_pages_available: bool,
    pub cpu_list: Option<Vec<usize>>,
    /// Pin each worker to its `cpu_list` entry. When false the OS schedules the workers, and
    /// `cpu_list` is only the ids they are numbered by.
    pub pin_threads: bool,
    pub enhanced_memory_strategy: crate::memory::allocation_strategy::EnhancedMemoryStrategy,
    pub memory_allocation: MemoryAllocationConfig,
}
