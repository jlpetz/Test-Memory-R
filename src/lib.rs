#![allow(unsafe_op_in_unsafe_fn)]
#![feature(portable_simd)]
#![feature(clflushopt_target_feature)]
#![feature(simd_x86_clflushopt)]

pub mod config;
pub mod constants;
pub mod memory;
pub mod tests;
pub mod progress;
pub mod console; // single writer path while the progress ticker is on screen
pub mod layout;
pub mod runner;
pub mod simd;
pub mod cache;
pub mod tsc; // TSC frequency detection (x86_64)
pub mod results; // New results module
pub mod table;
pub mod formatting;
pub mod thread_pool;
pub mod cpu_topology;
pub mod cpu_selection; // two-stage CPU selection: skip-cores filter + cpu-stride spacing
pub mod reporting;
pub mod params; // Centralized parameter registry
pub mod latency_tests; // Latency measurement tests
pub mod latency_tests_v2; // v2 latency tests: Layout A/B reads + NT write saturation PoC
pub mod bandwidth_tests; // Bandwidth measurement tests
pub mod calibration; // Adaptive cache calibration
pub mod pattern_gen; // v2 pattern generation (LCG, Mode 0/1/2)
pub mod test_harness; // v2 zero-cost test orchestration harness
pub mod test_scaffolding; // shared zero-cost bookkeeping for loop-owning v1 tests (TODO #19 Part A)
pub mod app_config; // Persistent application config (tmr-cfg.json)
pub mod smbios; // SMBIOS table parser (system identity, memory modules)
pub mod whea; // WHEA hardware-error monitoring via the System event log (TODO #63)
pub mod run_context; // Run identity + resolved config recorded into result files (TODO #67)

// Common result type for the crate
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

// Re-export main types and functions
pub use config::*;
// New unified memory system exports:
pub use memory::{MemoryAllocator, MemoryBuffer, AllocationConfig, BackendType, MemoryType, PageType, setup_large_pages_automatically, check_large_page_privilege};
// Legacy compatibility 
pub use memory::{diagnose_and_setup_large_pages, auto_grant_large_page_privilege, check_restart_needed};
pub use tests::TestMemoryConfig;
pub use tests::{set_calibration_data, set_active_threads_per_core};
pub use progress::ProgressTracker;
pub use layout::{EnhancedMemoryLayout, BlockInfo};
pub use tests::{WindowMode, ChunkMode};
pub use runner::{run_tests_with_layout, run_tests_with_layout_and_timing, TestSuiteTiming, AllocationBlock, print_current_memory_status, detect_runtime_capabilities};
pub use simd::detect_simd_capabilities;
pub use cache::{CacheInfo, SystemInfo};
pub use results::{TestRunResult, TestComparison, compare_test_results}; // New exports

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ErrorMode {
    Log,   // Log errors and continue (default)
    Halt,  // Stop current test on first error
    Panic, // Panic on first error (for debugging)
}


#[derive(Debug, Clone, Copy)]
pub enum MemoryBackend {
    NativeLargePages, // Use Windows large pages
    NativeRegular,    // Use regular Windows allocation
}

pub struct RuntimeConfig {
    pub memory_backend: MemoryBackend,
    pub large_pages_available: bool,
    pub cpu_list: Option<Vec<usize>>,
    pub enhanced_memory_strategy: crate::memory::allocation_strategy::EnhancedMemoryStrategy,
    pub memory_allocation: MemoryAllocationConfig,
}
