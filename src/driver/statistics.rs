use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use crate::table::{TableBuilder, Alignment};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DriverStatType {
    TotalCalls,
    AllocateDma,
    Free,
    BatchAllocate,
    FreeAll,
    GetStatistics,
    SetCpuAffinity,
    GetHardwareInfo,
    ResetAll,
    GetVersion,
}

// Stat definition with all metadata
struct StatDefinition {
    stat_type: DriverStatType,
    display_name: &'static str,
    counter: AtomicU64,
}

pub struct AppDriverStats {
    stats: Vec<StatDefinition>,
}

impl AppDriverStats {
    fn new() -> Self {
        // Define all stats in one place with their display names
        let stat_defs = vec![
            (DriverStatType::TotalCalls, "Total Calls"),
            (DriverStatType::AllocateDma, "Allocate DMA"),
            (DriverStatType::Free, "Free Memory"),
            (DriverStatType::BatchAllocate, "Batch Allocate"),
            (DriverStatType::GetStatistics, "Get Statistics"),
            (DriverStatType::SetCpuAffinity, "Set CPU Affinity"),
            (DriverStatType::GetHardwareInfo, "Get Hardware Info"),
            (DriverStatType::ResetAll, "Reset All"),
            (DriverStatType::GetVersion, "Get Version"),
            (DriverStatType::FreeAll, "Free All Memory"),
        ];
        
        let stats = stat_defs
            .into_iter()
            .map(|(stat_type, display_name)| StatDefinition {
                stat_type,
                display_name,
                counter: AtomicU64::new(0),
            })
            .collect();
        
        Self { stats }
    }
    
    fn increment(&self, stat_type: DriverStatType) {
        // Find and increment the specific stat
        if let Some(stat) = self.stats.iter().find(|s| s.stat_type == stat_type) {
            stat.counter.fetch_add(1, Ordering::Relaxed);
        }
        // Always increment total calls (except for total calls itself)
        if stat_type != DriverStatType::TotalCalls
            && let Some(total) = self.stats.iter().find(|s| s.stat_type == DriverStatType::TotalCalls) {
                total.counter.fetch_add(1, Ordering::Relaxed);
            }
    }
    
    fn reset(&self) {
        for stat in &self.stats {
            stat.counter.store(0, Ordering::Relaxed);
        }
    }
    
    pub fn get_summary(&self) -> HashMap<String, u64> {
        self.stats
            .iter()
            .map(|stat| (stat.display_name.to_string(), stat.counter.load(Ordering::Relaxed)))
            .collect()
    }
    
    pub fn get_value(&self, stat_type: DriverStatType) -> u64 {
        self.stats
            .iter()
            .find(|s| s.stat_type == stat_type)
            .map(|s| s.counter.load(Ordering::Relaxed))
            .unwrap_or(0)
    }
}

// Global application stats
static APP_DRIVER_STATS: OnceLock<AppDriverStats> = OnceLock::new();

fn get_app_stats() -> &'static AppDriverStats {
    APP_DRIVER_STATS.get_or_init(AppDriverStats::new)
}

// Helper macro to track calls
macro_rules! track_driver_call {
    ($stat_type:expr) => {
        {
            let stats = get_app_stats();
            stats.increment($stat_type);
        }
    };
}

// Public interface functions
pub fn reset_app_driver_stats() {
    get_app_stats().reset();
}

pub fn display_app_driver_stats_table() {
    let stats = get_app_stats().get_summary();
    
    let mut table = TableBuilder::new()
        .add_header("Driver Call", Alignment::Left)
        .add_header("Count", Alignment::Right);
    
    for (call_name, count) in stats {
        table = table.add_row(vec![call_name, count.to_string()]);
    }
    
    println!("\n=== Application Driver Call Statistics ===");
    table.print();
}

pub fn compare_app_vs_driver_stats(driver_stats: &HashMap<String, u64>) {
    let app_stats = get_app_stats().get_summary();
    
    println!("\n=== Driver Call Statistics Comparison ===");
    let mut table = TableBuilder::new()
        .add_header("Call Type", Alignment::Left)
        .add_header("App Count", Alignment::Right)
        .add_header("Driver Count", Alignment::Right)
        .add_header("Difference", Alignment::Right);
    
    for (call_name, app_count) in app_stats {
        let driver_count = driver_stats.get(&call_name).unwrap_or(&0);
        let diff = if *driver_count >= app_count {
            format!("+{}", driver_count - app_count)
        } else {
            format!("-{}", app_count - driver_count)
        };
        
        table = table.add_row(vec![
            call_name,
            app_count.to_string(),
            driver_count.to_string(),
            diff,
        ]);
    }
    
    table.print();
}

// Export the macro for use in other modules
pub(crate) use track_driver_call;

/// Public function to manually track driver calls (for testing/debugging)
pub fn track_call(stat_type: DriverStatType) {
    track_driver_call!(stat_type);
}

/// Input structure for remap all request
#[repr(C)]
#[derive(Copy, Clone)]
pub struct RemapAllInput {
    pub new_memory_type: u32,  // MemoryType as u32
}

/// Output structure for remap all result
#[repr(C)]
#[derive(Copy, Clone)]
#[derive(Default)]
pub struct RemapAllOutput {
    pub success: bool,
    pub allocations_remapped: u32,
    pub allocations_failed: u32,
    pub total_time_us: u32,
}

impl Default for RemapAllInput {
    fn default() -> Self {
        Self {
            new_memory_type: crate::driver::MemoryType::WriteBack as u32,
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_driver_statistics_tracking() {
        // Reset stats to start clean
        reset_app_driver_stats();
        
        // Test macro usage
        track_driver_call!(DriverStatType::GetVersion);
        track_driver_call!(DriverStatType::AllocateDma);
        track_driver_call!(DriverStatType::AllocateDma);
        
        let stats = get_app_stats();
        
        // Verify individual stats
        assert_eq!(stats.get_value(DriverStatType::GetVersion), 1);
        assert_eq!(stats.get_value(DriverStatType::AllocateDma), 2);
        
        // Verify total calls (should be sum of individual calls)
        assert_eq!(stats.get_value(DriverStatType::TotalCalls), 3);
        
        // Test reset functionality
        reset_app_driver_stats();
        assert_eq!(stats.get_value(DriverStatType::GetVersion), 0);
        assert_eq!(stats.get_value(DriverStatType::TotalCalls), 0);
    }
}