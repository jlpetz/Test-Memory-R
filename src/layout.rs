// Extent and chunk modes are shared between legacy and enhanced systems

use crate::constants::bytes_to_gib_f64;

// ExtentMode and ChunkMode moved to tests.rs - they are test configuration concerns, not memory layout concerns

#[derive(Debug, Clone)]
pub struct BlockInfo {
    pub size_bytes: usize,
    pub thread_id: usize,
}

/// Enhanced memory layout using the new allocation strategy system
#[derive(Debug, Clone)]
pub struct EnhancedMemoryLayout {
    /// Detailed system memory information
    pub system_memory_info: crate::memory::allocation_strategy::SystemMemoryInfo,
    
    /// Allocation calculation result
    pub allocation_result: crate::memory::allocation_strategy::AllocationResult,
    
    /// Per-thread memory blocks
    pub blocks: Vec<BlockInfo>,
    
    /// Strategy used to create this layout
    pub strategy: crate::memory::allocation_strategy::EnhancedMemoryStrategy,
}

impl EnhancedMemoryLayout {
    /// The memory status and the allocation plan, the run's first event (TODO 29)
    pub fn memory_plan(&self) -> crate::reporting::models::ConsolidatedMemoryReport {
        crate::reporting::converters::create_consolidated_memory_report(&self.system_memory_info, &self.allocation_result)
    }

    /// The thread layout, to the log, after the plan is shown
    pub fn log_thread_layout(&self) {
        log::info!("🧵 Thread Memory Layout:");
        log::info!("  Total threads: {}", self.blocks.len());
        
        if !self.blocks.is_empty() {
            let size_gib = bytes_to_gib_f64(self.blocks[0].size_bytes as u64);
            log::info!("  Allocation per thread: {:.2} GiB", size_gib);
            log::info!("  Total planned allocation: {:.2} GiB", 
                      size_gib * self.blocks.len() as f64);
        }
    }

}
