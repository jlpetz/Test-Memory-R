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
		
        // Print allocation plan TURNED OFF
        // self.allocation_result.print_summary(&self.system_memory_info);
		
        use crate::reporting::{create_console_reporter, converters::create_consolidated_memory_report};
        
        // Show consolidated memory status and allocation plan
        let consolidated_report = create_consolidated_memory_report(&self.system_memory_info, &self.allocation_result);
        let mut reporter = create_console_reporter();
        
        if let Err(e) = reporter.report_consolidated_memory(&consolidated_report) {
            log::error!("Failed to display consolidated memory report: {}", e);
            // Fallback to basic allocation summary
            self.allocation_result.print_summary(&self.system_memory_info);
        }
        
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
    }

}
