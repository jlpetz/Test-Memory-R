/// Global constants used throughout the TMR application
///
/// This module centralizes magic numbers and commonly used values
/// to improve maintainability and code clarity.
/// Memory page size constants as usize (for chunk_size comparisons)
pub const HUGE_PAGE_SIZE_USIZE: usize = 1024 * 1024 * 1024; // 1GB
pub const LARGE_PAGE_SIZE_USIZE: usize = 2 * 1024 * 1024; // 2MB  

/// Memory size conversion constants
pub const BYTES_PER_MIB: u64 = 1024 * 1024;
pub const BYTES_PER_GIB: u64 = 1024 * 1024 * 1024;

/// Memory size conversion constants as usize
pub const BYTES_PER_MIB_USIZE: usize = 1024 * 1024;

/// Floating point memory conversion constants (for divisions)
pub const BYTES_PER_GIB_F64: f64 = 1024.0 * 1024.0 * 1024.0;

/// Base units for cache sizes and other calculations
pub const KB: usize = 1024;
pub const MB: usize = 1024 * 1024;

/// Floating point base units for conversions
pub const KB_F64: f64 = 1024.0;
pub const MB_F64: f64 = 1024.0 * 1024.0;

/// Common power-of-2 constants
pub const PAGE_SIZE_4KB: usize = 4 * 1024;

/// Common memory sizes in MB
pub const MB_16: usize = 16 * MB;
pub const MB_32: usize = 32 * MB;
pub const MB_64: usize = 64 * MB;

/// Utility functions for memory conversions
pub fn bytes_to_gib_f64(bytes: u64) -> f64 {
    bytes as f64 / BYTES_PER_GIB_F64
}

pub fn gib_to_bytes(gib: f64) -> u64 {
    (gib * BYTES_PER_GIB_F64) as u64
}
