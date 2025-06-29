use crate::memory;

#[derive(Debug, Clone)]
pub enum MemoryStrategy {
    TM5Compatible {
        testing_window_size_mb: u32,
        reserved_memory_mb: u32,
        test_block_size_mb: u32, // 0 = use full window per thread
    },
    ModernOptimal {
        reserve_gib: Option<f64>, // None = use percentage
    },
    Custom {
        blocks_per_thread: u32,
        min_block_size_mb: u32,
    },
}

impl MemoryStrategy {
    pub fn calculate_stage1_allocation(&self, total_memory_bytes: usize, reserve_percent: f64, thread_count: usize) -> usize {
        match self {
            MemoryStrategy::TM5Compatible { reserved_memory_mb, .. } => {
                // Stage 1: Allocate maximum memory minus OS reserve (TM5 style)
                let os_reserve = (*reserved_memory_mb as usize) * 1024 * 1024;
                let max_memory_for_testing = total_memory_bytes.saturating_sub(os_reserve);
                max_memory_for_testing / thread_count
            }
            MemoryStrategy::ModernOptimal { reserve_gib } => {
                if let Some(gib) = reserve_gib {
                    let reserve_bytes = (gib * 1024.0 * 1024.0 * 1024.0) as usize;
                    let usable = total_memory_bytes.saturating_sub(reserve_bytes);
                    usable / thread_count
                } else {
                    let reserve_bytes = (total_memory_bytes as f64 * reserve_percent / 100.0) as usize;
                    let usable = total_memory_bytes.saturating_sub(reserve_bytes);
                    usable / thread_count
                }
            }
            MemoryStrategy::Custom { .. } => {
                let reserve_bytes = (total_memory_bytes as f64 * reserve_percent / 100.0) as usize;
                let usable = total_memory_bytes.saturating_sub(reserve_bytes);
                usable / thread_count
            }
        }
    }
}

pub struct MemoryLayout {
    pub total_memory: usize,
    pub allocated_memory: usize,      // Stage 1: Total allocated (e.g., 55GB)
    pub reserved_memory: usize,
    pub blocks: Vec<BlockInfo>,
    pub strategy: MemoryStrategy,
}

#[derive(Debug, Clone)]
pub struct BlockInfo {
    pub size_bytes: usize,           // Stage 1: Full allocation per thread (e.g., 4.6GB)
    pub thread_id: usize,
}

impl MemoryLayout {
    pub fn calculate(strategy: MemoryStrategy, thread_count: usize, reserve_percent: f64) -> Self {
        let total_memory = memory::get_total_system_memory();
        
        // Stage 1: Calculate maximum allocation per thread
        let allocated_per_thread = strategy.calculate_stage1_allocation(total_memory, reserve_percent, thread_count);
        let total_allocated = allocated_per_thread * thread_count;
        let reserved_memory = total_memory - total_allocated;

        let blocks = (0..thread_count)
            .map(|thread_id| BlockInfo {
                size_bytes: allocated_per_thread,
                thread_id,
            })
            .collect();

        MemoryLayout {
            total_memory,
            allocated_memory: total_allocated,
            reserved_memory,
            blocks,
            strategy,
        }
    }

    pub fn print_layout(&self) {
        log::info!("Memory Layout (Three-Stage Architecture):");
        log::info!(
            "  Total System Memory: {:.2} GiB",
            self.total_memory as f64 / (1024.0 * 1024.0 * 1024.0)
        );
        log::info!(
            "  Stage 1 - Total Allocated: {:.2} GiB ({:.1}% of system)",
            self.allocated_memory as f64 / (1024.0 * 1024.0 * 1024.0),
            (self.allocated_memory as f64 / self.total_memory as f64) * 100.0
        );
        log::info!(
            "  Reserved for OS: {:.2} GiB",
            self.reserved_memory as f64 / (1024.0 * 1024.0 * 1024.0)
        );
        log::info!("  Strategy: {:?}", self.strategy);
        log::info!("  Threads: {}", self.blocks.len());

        for block in &self.blocks {
            let size_gib = block.size_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
            log::info!("  Thread {}: {:.2} GiB allocated", block.thread_id, size_gib);
        }
        
        log::info!("  Note: Stage 2 (window size) and Stage 3 (block size) will be");
        log::info!("        configured per test within these allocations");
    }
}