use crate::memory;

// Simplified single strategy that handles all cases
#[derive(Debug, Clone)]
pub struct MemoryStrategy {
    // Stage 1: Memory allocation strategy
    pub allocation_mode: AllocationMode,
    
    // Stage 2: Default window sizing (can be overridden per test)
    pub default_window_mode: WindowMode,
    
    // Stage 3: Default block sizing (can be overridden per test)
    pub default_block_mode: BlockMode,
}

#[derive(Debug, Clone)]
pub enum AllocationMode {
    MaxAvailable { reserve_mb: u32 },           // TM5 style: max memory minus fixed reserve
    PercentageReserve { reserve_percent: f64 }, // Modern: reserve percentage
    FixedReserve { reserve_gib: f64 },          // Modern: fixed GiB reserve
}

#[derive(Debug, Clone)]
pub enum WindowMode {
    FullAllocation,                    // Use entire Stage 1 allocation (default for most tests)
    FixedSize { size_mb: u32 },       // Fixed window size (TM5 compatibility)
    CacheRelative { multiplier: f64 }, // Relative to total cache size
}

#[derive(Debug, Clone)]
pub enum BlockMode {
    AutoOptimal,                       // Auto-calculate optimal block size per test
    FixedSize { size_mb: u32 },       // Fixed block size
    WindowFraction { fraction: f64 },  // Fraction of window size
}

impl Default for MemoryStrategy {
    fn default() -> Self {
        Self {
            allocation_mode: AllocationMode::PercentageReserve { reserve_percent: 10.0 },
            default_window_mode: WindowMode::FullAllocation,
            default_block_mode: BlockMode::AutoOptimal,
        }
    }
}

impl MemoryStrategy {
    // TM5-compatible preset
    pub fn tm5_compatible(window_mb: u32, reserve_mb: u32) -> Self {
        Self {
            allocation_mode: AllocationMode::MaxAvailable { reserve_mb },
            default_window_mode: WindowMode::FixedSize { size_mb: window_mb },
            default_block_mode: BlockMode::AutoOptimal,
        }
    }
    
    // Modern optimal preset
    pub fn modern_optimal() -> Self {
        Self::default()
    }
}

pub struct MemoryLayout {
    pub total_memory: usize,
    pub allocated_memory: usize,
    pub reserved_memory: usize,
    pub blocks: Vec<BlockInfo>,
    pub strategy: MemoryStrategy,
}

#[derive(Debug, Clone)]
pub struct BlockInfo {
    pub size_bytes: usize,
    pub thread_id: usize,
}

impl MemoryLayout {
    pub fn calculate(strategy: MemoryStrategy, thread_count: usize) -> Self {
        let total_memory = memory::get_total_system_memory();
        let allocated_per_thread = match &strategy.allocation_mode {
            AllocationMode::MaxAvailable { reserve_mb } => {
                let reserve_bytes = (*reserve_mb as usize) * 1024 * 1024;
                let usable = total_memory.saturating_sub(reserve_bytes);
                usable / thread_count
            }
            AllocationMode::PercentageReserve { reserve_percent } => {
                let reserve_bytes = (total_memory as f64 * reserve_percent / 100.0) as usize;
                let usable = total_memory.saturating_sub(reserve_bytes);
                usable / thread_count
            }
            AllocationMode::FixedReserve { reserve_gib } => {
                let reserve_bytes = (*reserve_gib * 1024.0 * 1024.0 * 1024.0) as usize;
                let usable = total_memory.saturating_sub(reserve_bytes);
                usable / thread_count
            }
        };
        
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
    }
}
