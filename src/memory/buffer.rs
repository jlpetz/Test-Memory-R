use crate::memory::backend::{Backend, BackendAllocation};
use crate::constants::BYTES_PER_GIB_USIZE;
use std::sync::Arc;

use windows::Win32::System::SystemInformation::GetPhysicallyInstalledSystemMemory;

/// Single unified buffer type that works with any backend
#[derive(Debug)]
pub struct MemoryBuffer {
    ptr: *mut u8,
    size: usize,
    info: BufferInfo,
    backend: Arc<dyn Backend>,
}

#[derive(Debug, Clone)]
pub struct BufferInfo {
    pub physical_address: Option<u64>,
    pub numa_node: u32,
    pub page_type: PageType,
    pub segments: Vec<SegmentInfo>,  // Empty for simple allocations
}

impl BufferInfo {
    /// Check if this buffer uses large pages (2MB or larger)
    pub fn uses_large_pages(&self) -> bool {
        match &self.page_type {
            PageType::Large(_) | PageType::Huge(_) => true,
            PageType::Mixed(pages) => pages.iter().any(|p| p.size_kb >= 2048),
            PageType::Regular(_) => false,
        }
    }
    
    /// Check if this buffer uses huge pages (1GB)
    pub fn uses_huge_pages(&self) -> bool {
        match &self.page_type {
            PageType::Huge(_) => true,
            PageType::Mixed(pages) => pages.iter().any(|p| p.size_kb >= 1048576),
            _ => false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SegmentInfo {
    pub virtual_address: u64,
    pub physical_address: u64,
    pub size: usize,
    pub page_size_kb: u32,
    pub numa_node: u32,
}

#[derive(Debug, Clone)]
pub enum PageType {
    Regular(usize),              // 4KB
    Large(usize),                // 2MB  
    Huge(usize),                 // 1GB
    Mixed(Vec<PageInfo>),         // For segmented allocations
}

#[derive(Debug, Clone, Copy)]
pub struct PageInfo {
    pub size_kb: u32,
    pub count: usize,
}

#[derive(Debug, Clone, Copy)]
pub enum MemoryType {
    WriteBack,
    WriteCombining,
    Uncached,
    WriteProtected,
    WriteCombined,
}


impl MemoryBuffer {
    pub fn new(allocation: BackendAllocation, backend: Arc<dyn Backend>) -> Self {
        Self {
            ptr: allocation.ptr,
            size: allocation.size,
            info: allocation.info,
            backend,
        }
    }
    
    pub fn as_mut_ptr(&self) -> *mut u8 { 
        self.ptr 
    }
    
    pub fn size(&self) -> usize { 
        self.size 
    }
    
    pub fn info(&self) -> &BufferInfo { 
        &self.info 
    }
    
    
    /// Check if this buffer uses large pages (2MB or larger)
    pub fn uses_large_pages(&self) -> bool {
        match &self.info.page_type {
            PageType::Large(_) | PageType::Huge(_) => true,
            PageType::Mixed(pages) => pages.iter().any(|p| p.size_kb >= 2048),
            PageType::Regular(_) => false,
        }
    }
    
    /// Check if this buffer uses huge pages (1GB)
    pub fn uses_huge_pages(&self) -> bool {
        match &self.info.page_type {
            PageType::Huge(_) => true,
            PageType::Mixed(pages) => pages.iter().any(|p| p.size_kb >= 1048576),
            _ => false,
        }
    }
    
    /// Get the primary page size used by this buffer
    pub fn primary_page_size_kb(&self) -> u32 {
        match &self.info.page_type {
            PageType::Regular(_) => 4,
            PageType::Large(_) => 2048,
            PageType::Huge(_) => 1048576,
            PageType::Mixed(pages) => {
                if let Some(page) = pages.first() {
                    page.size_kb
                } else {
                    4
                }
            }
        }
    }
}

impl Drop for MemoryBuffer {
    fn drop(&mut self) {
        let allocation = BackendAllocation {
            ptr: self.ptr,
            size: self.size,
            info: self.info.clone(),
        };
        
        if let Err(e) = self.backend.free(allocation) {
            eprintln!("Warning: Failed to free memory buffer: {}", e);
        }
    }
}

// Ensure MemoryBuffer is Send + Sync for multi-threading
unsafe impl Send for MemoryBuffer {}
unsafe impl Sync for MemoryBuffer {}



/// Get total system memory
pub fn get_total_system_memory() -> usize {
    
    unsafe {
        let mut total_memory_kb: u64 = 0;
        match GetPhysicallyInstalledSystemMemory(&mut total_memory_kb) {
            Ok(_) => (total_memory_kb * 1024) as usize,
            Err(_) => {
                // Fallback: assume 8GB if we can't detect
                log::warn!("Could not detect system memory, assuming 8 GiB");
                8 * BYTES_PER_GIB_USIZE
            }
        }
    }
}

/// DMA buffer configuration
#[derive(Clone, Debug)]
pub struct DmaConfig {
    pub minimum_page_size: crate::driver::PageSize,
    pub maximum_page_size: crate::driver::PageSize,
    pub prefer_numa_node: Option<u32>,
    pub zero_memory: bool,
    pub memory_type: crate::driver::MemoryType,
    pub contiguous: bool,
    pub timeout_ms: u32,
    pub retry_interval_ms: u32,
    pub max_retries: u32,
    pub strict_numa: bool,
}

impl Default for DmaConfig {
    fn default() -> Self {
        Self {
            minimum_page_size: crate::driver::PageSize::Regular,
            maximum_page_size: crate::driver::PageSize::Huge,
            prefer_numa_node: None,
            zero_memory: true,
            memory_type: crate::driver::MemoryType::WriteBack,
            contiguous: false,
            timeout_ms: 5000,
            retry_interval_ms: 100,
            max_retries: 5,
            strict_numa: false,
        }
    }
}

impl DmaConfig {
    pub fn for_testing() -> Self {
        Self {
            minimum_page_size: crate::driver::PageSize::Regular,
            maximum_page_size: crate::driver::PageSize::Huge,
            prefer_numa_node: None,
            zero_memory: true,
            memory_type: crate::driver::MemoryType::WriteBack,
            contiguous: false,
            timeout_ms: 5000,
            retry_interval_ms: 100,
            max_retries: 5,
            strict_numa: false,
        }
    }

    pub fn for_bandwidth_test() -> Self {
        Self {
            minimum_page_size: crate::driver::PageSize::Large,
            maximum_page_size: crate::driver::PageSize::Huge,
            prefer_numa_node: None,
            zero_memory: false,
            memory_type: crate::driver::MemoryType::WriteBack,
            contiguous: true,
            timeout_ms: 10000,
            retry_interval_ms: 250,
            max_retries: 3,
            strict_numa: false,
        }
    }

    pub fn for_latency_test() -> Self {
        Self {
            minimum_page_size: crate::driver::PageSize::Regular,
            maximum_page_size: crate::driver::PageSize::Large,
            prefer_numa_node: None,
            zero_memory: true,
            memory_type: crate::driver::MemoryType::WriteBack,
            contiguous: false,
            timeout_ms: 2000,
            retry_interval_ms: 50,
            max_retries: 10,
            strict_numa: true,
        }
    }
}


