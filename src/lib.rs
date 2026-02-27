#![allow(unsafe_op_in_unsafe_fn)]
#![feature(portable_simd)]

pub mod config;
pub mod constants;
pub mod memory;
pub mod tests;
pub mod progress;
pub mod layout;
pub mod runner;
pub mod simd;
pub mod cache;
pub mod tsc; // TSC frequency detection (x86_64)
pub mod results; // New results module
pub mod table;
pub mod formatting;
pub mod thread_pool;
pub mod test_framework;
pub mod driver;
pub mod cpu_topology;
pub mod reporting;
pub mod params; // Centralized parameter registry
pub mod latency_tests; // Latency measurement tests
pub mod bandwidth_tests; // Bandwidth measurement tests
pub mod calibration; // Adaptive cache calibration

// Common result type for the crate
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

// Re-export main types and functions
pub use config::*;
// New unified memory system exports:
pub use memory::{MemoryAllocator, MemoryBuffer, AllocationConfig, BackendType, MemoryType, PageType, setup_large_pages_automatically, check_large_page_privilege};
// Legacy compatibility 
pub use memory::{diagnose_and_setup_large_pages, auto_grant_large_page_privilege, check_restart_needed};
pub use driver::{reset_driver, DriverStatus, display_driver_info, check_and_display_driver_status, refresh_driver_status, display_driver_stats, is_driver_connected, compare_app_vs_driver_stats, reset_app_driver_stats, display_app_driver_stats_table, set_use_remap_all};
pub use tests::TestMemoryConfig;
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
    KernelDriver,    // Use TMR kernel driver
    NativeLargePages, // Use Windows large pages
    NativeRegular,    // Use regular Windows allocation
}

pub struct RuntimeConfig {
    pub memory_backend: MemoryBackend,
    pub driver_available: bool,
    pub large_pages_available: bool,
    pub use_driver_chunking: bool,
    pub cpu_list: Option<Vec<usize>>,
    pub enhanced_memory_strategy: crate::memory::allocation_strategy::EnhancedMemoryStrategy,
    pub memory_allocation: MemoryAllocationConfig,
}
