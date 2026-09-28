/// Global constants used throughout the TMR application
/// 
/// This module centralizes magic numbers and commonly used values
/// to improve maintainability and code clarity.
/// Memory page size constants in bytes
pub const HUGE_PAGE_SIZE: u64 = 1024 * 1024 * 1024; // 1GB
pub const LARGE_PAGE_SIZE: u64 = 2 * 1024 * 1024;   // 2MB  
pub const REGULAR_PAGE_SIZE: u64 = 4 * 1024;        // 4KB

/// Memory page size constants as usize (for chunk_size comparisons)
pub const HUGE_PAGE_SIZE_USIZE: usize = 1024 * 1024 * 1024; // 1GB
pub const LARGE_PAGE_SIZE_USIZE: usize = 2 * 1024 * 1024;   // 2MB  

/// Memory size conversion constants
pub const BYTES_PER_MIB: u64 = 1024 * 1024;
pub const BYTES_PER_GIB: u64 = 1024 * 1024 * 1024;

/// Memory size conversion constants as usize
pub const BYTES_PER_MIB_USIZE: usize = 1024 * 1024;

/// Floating point memory conversion constants (for divisions)
pub const BYTES_PER_MIB_F64: f64 = 1024.0 * 1024.0;
pub const BYTES_PER_GIB_F64: f64 = 1024.0 * 1024.0 * 1024.0;

/// Page type classifications for memory allocations
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PageType {
    /// 1GB huge pages
    Huge,
    /// 2MB large pages  
    Large,
    /// 4KB regular pages
    Regular,
}

impl PageType {
    /// Get the display name for this page type
    pub fn display_name(&self) -> &'static str {
        match self {
            PageType::Huge => "1GB huge",
            PageType::Large => "2MB large", 
            PageType::Regular => "4KB regular",
        }
    }
    
    /// Get the page size in bytes
    pub fn page_size(&self) -> u64 {
        match self {
            PageType::Huge => HUGE_PAGE_SIZE,
            PageType::Large => LARGE_PAGE_SIZE,
            PageType::Regular => REGULAR_PAGE_SIZE,
        }
    }
    
    /// Calculate number of pages needed for given byte size
    pub fn pages_needed(&self, bytes: u64) -> u64 {
        bytes.div_ceil(self.page_size())
    }
    
    /// Determine page type from buffer capabilities
    pub fn from_buffer(buffer: &crate::AllocationBlock) -> Self {
        if buffer.buffer.uses_huge_pages() {
            PageType::Huge
        } else if buffer.buffer.uses_large_pages() {
            PageType::Large
        } else {
            PageType::Regular
        }
    }
}

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
