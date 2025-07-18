pub mod config;
pub mod memory;
pub mod dma_memory;
pub mod tests;
pub mod progress;
pub mod layout;
pub mod runner;
pub mod simd;
pub mod cache;
pub mod results; // New results module

// Re-export main types and functions
pub use config::*;
pub use memory::TestBuffer;
pub use dma_memory::{reset_driver, DriverStatus, display_driver_info, check_and_display_driver_status, refresh_driver_status, display_driver_stats, is_driver_connected};
pub use tests::TestMemoryConfig;
pub use progress::ProgressTracker;
pub use layout::{MemoryLayout, MemoryStrategy, AllocationMode, WindowMode, BlockMode, BlockInfo};
pub use runner::{run_tests_with_layout, run_tests_with_layout_and_timing, TestSuiteTiming, AllocatedBlock};
pub use simd::detect_simd_capabilities;
pub use cache::{CacheInfo, SystemInfo};
pub use results::{TestRunResult, TestComparison, compare_test_results}; // New exports

#[derive(Debug, Clone, Copy)]
pub enum ErrorMode {
    Log,   // Log errors and continue (default)
    Halt,  // Stop current test on first error
    Panic, // Panic on first error (for debugging)
}

pub fn check_large_page_privilege() -> Result<(), &'static str> {
    memory::check_large_page_privilege()
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
	pub use_driver_chunking: bool,  // Add this flag
}