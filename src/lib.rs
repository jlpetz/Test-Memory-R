pub mod config;
pub mod memory;
pub mod tests;
pub mod progress;
pub mod layout;
pub mod runner;
pub mod simd;
pub mod cache;

// Re-export main types and functions for backward compatibility
pub use config::*;
pub use memory::TestBuffer;
pub use tests::TestMemoryConfig;
pub use progress::ProgressTracker;
pub use layout::{MemoryLayout, MemoryStrategy, BlockInfo};
pub use runner::{run_tests_with_layout, AllocatedBlock};
pub use simd::detect_simd_capabilities;
pub use cache::{CacheInfo, SystemInfo};

pub const DEFAULT_RESERVE_PERCENT: f64 = 10.0;

#[derive(Debug, Clone, Copy)]
pub enum ErrorMode {
    Log,   // Log errors and continue (default)
    Halt,  // Stop current test on first error
    Panic, // Panic on first error (for debugging)
}

pub fn check_large_page_privilege() -> Result<(), &'static str> {
    memory::check_large_page_privilege()
}