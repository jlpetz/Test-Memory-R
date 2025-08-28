// Window and Block modes are shared between legacy and enhanced systems

use crate::constants::bytes_to_gib_f64;

// WindowMode and ChunkMode moved to tests.rs - they are test configuration concerns, not memory layout concerns

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
    /// Print comprehensive layout information
    pub fn print_layout(&self) {
        // Note: System memory analysis is now shown in consolidated memory report
        
        // Print allocation plan
        self.allocation_result.print_summary(&self.system_memory_info);
        println!();
        
        // Print thread layout
        log::info!("🧵 Thread Memory Layout:");
        log::info!("  Total threads: {}", self.blocks.len());
        
        if !self.blocks.is_empty() {
            let size_gib = bytes_to_gib_f64(self.blocks[0].size_bytes as u64);
            log::info!("  Allocation per thread: {:.2} GiB", size_gib);
            log::info!("  Total planned allocation: {:.2} GiB", 
                      size_gib * self.blocks.len() as f64);
        }
        
        // Print minimum start address recommendation
        log::info!("💡 Memory Allocation Recommendations:");
        log::info!("  Minimum start address: 0x{:016X}", self.allocation_result.min_start_address);
        log::info!("  Strategy: {}", self.allocation_result.allocation_type);
    }
    
}
