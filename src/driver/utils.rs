use crate::driver::types::{DriverStatus, DriverStatistics};
use crate::driver::interface::{get_global_driver_handle, reset_driver_state, is_driver_connected};
use std::sync::atomic::{AtomicBool, Ordering};
use crate::table::{TableBuilder, Alignment};

// Global setting for remap mode
static USE_REMAP_ALL: AtomicBool = AtomicBool::new(true);

pub fn set_use_remap_all(value: bool) {
    USE_REMAP_ALL.store(value, Ordering::Relaxed);
}

pub fn get_use_remap_all() -> bool {
    USE_REMAP_ALL.load(Ordering::Relaxed)
}

pub fn reset_driver() {
    if let Ok(driver) = get_global_driver_handle() {
        if let Err(e) = driver.reset_all() {
            log::warn!("Failed to reset driver allocations: {}", e);
        } else {
            log::info!("TMR Driver: Reset all allocations");
        }
    } else {
        log::warn!("Cannot reset driver - driver not available");
    }
    
    // Note: We intentionally do NOT reset the driver state here
    // as that would clear the cached handle and break subsequent operations
}

pub fn check_and_display_driver_status() -> DriverStatus {
    match get_global_driver_handle() {
        Ok(driver) => {
            match driver.check_version_compatibility() {
                Ok(version_info) => {
                    DriverStatus::Available(version_info)
                }
                Err(version_error) => {
                    match version_error {
                        crate::driver::types::DriverVersionError::VersionMismatch { 
                            driver_version, 
                            app_version, 
                            min_required, 
                            max_supported 
                        } => {
                            DriverStatus::VersionMismatch {
                                driver_version,
                                app_version,
                                min_required,
                                max_supported,
                            }
                        }
                        _ => DriverStatus::Error(format!("Driver version check failed: {}", version_error))
                    }
                }
            }
        }
        Err(e) => {
            if e.contains("not found") || e.contains("device") {
                DriverStatus::NotFound
            } else {
                DriverStatus::Error(e)
            }
        }
    }
}

pub fn refresh_driver_status() -> DriverStatus {
    // Reset the driver state to force a fresh connection
    reset_driver_state();
    check_and_display_driver_status()
}

pub fn display_driver_info() {
    let status = check_and_display_driver_status();
    
    match status {
        DriverStatus::Available(version) => {
            println!("  DMA Driver: ✅ Available (TMR kernel driver loaded)");
            println!("    Version: {}.{}.{}.{}", 
                version.driver_version_major,
                version.driver_version_minor, 
                version.driver_version_build,
                version.driver_version_revision
            );
            
            let build_date_str = String::from_utf8_lossy(&version.driver_build_date);
            let build_date = build_date_str
                .trim_end_matches('\0')
                .trim();
            let build_time_str = String::from_utf8_lossy(&version.driver_build_time);
            let build_time = build_time_str
                .trim_end_matches('\0')
                .trim();
            
            if !build_date.is_empty() && !build_time.is_empty() {
                println!("    Built: {} {}", build_date, build_time);
            }
            
            // Display compatibility flags
            if version.compatibility_flags != 0 {
                print!("    Features:");
                let flags = version.compatibility_flags;
                if flags & crate::driver::types::CompatibilityFlags::SupportsHugePages as u32 != 0 {
                    print!(" ✓ 1GB SuperPages");
                }
                if flags & crate::driver::types::CompatibilityFlags::SupportsNuma as u32 != 0 {
                    print!(" ✓ NUMA awareness");
                }
                if flags & crate::driver::types::CompatibilityFlags::SupportsMixedPages as u32 != 0 {
                    print!(" ✓ Mixed allocation");
                }
                if flags & crate::driver::types::CompatibilityFlags::SupportsZeroFree as u32 != 0 {
                    print!(" ✓ Zero-free allocation");
                }
                if flags & crate::driver::types::CompatibilityFlags::SupportsBatchAllocation as u32 != 0 {
                    print!(" ✓ Batch API");
                }
                if flags & crate::driver::types::CompatibilityFlags::SupportsETW as u32 != 0 {
                    print!(" ✓ ETW tracing");
                }
                if flags & crate::driver::types::CompatibilityFlags::SupportsMemoryTypes as u32 != 0 {
                    print!(" ✓ Memory type control");
                }
                println!();
            }
            
            // Optionally show runtime statistics if available
            if let Ok(driver) = get_global_driver_handle()
                && let Ok(stats) = driver.get_driver_statistics() {
                    println!("    Runtime: {} calls, {} active allocs, {:.2} MB peak",
                        stats.total_calls,
                        stats.active_allocations,
                        stats.peak_allocated_bytes as f64 / (1024.0 * 1024.0)
                    );
                }
        }
        DriverStatus::VersionMismatch { driver_version, app_version, min_required, max_supported } => {
            println!("  DMA Driver: ⚠️  Version Mismatch");
            println!("    Driver Version: {}", driver_version);
            println!("    App Version: {}", app_version);
            println!("    Required: {} - {}", min_required, max_supported);
        }
        DriverStatus::NotFound => {
            println!("  DMA Driver: ❌ Not Found (kernel driver not loaded)");
            println!("    Install TMR kernel driver for enhanced memory testing");
        }
        DriverStatus::Error(e) => {
            println!("  DMA Driver: ❌ Error: {}", e);
        }
    }
}


pub fn print_driver_info() {
    println!("\n=== TMR Driver Information ===");
    display_driver_info();
    
    if is_driver_connected() {
        let remap_mode = if get_use_remap_all() {
            "Remap All (optimized)"
        } else {
            "Batch Remap (legacy)"
        };
        println!("  Remap Mode: {}", remap_mode);
    }
}

// Additional utility functions for compatibility  
pub fn compare_app_vs_driver_stats() -> Result<(), String> {
    use std::collections::HashMap;
    
    if !is_driver_connected() {
        println!("Driver not connected - cannot compare statistics");
        return Ok(());
    }
    
    if let Ok(driver) = get_global_driver_handle() {
        if let Ok(stats) = driver.get_driver_statistics() {
            let driver_stats = driver_stats_to_hashmap(&stats);
            crate::driver::statistics::compare_app_vs_driver_stats(&driver_stats);
        } else {
            // Fallback to empty driver stats
            let driver_stats = HashMap::new();
            crate::driver::statistics::compare_app_vs_driver_stats(&driver_stats);
        }
    } else {
        let driver_stats = HashMap::new();
        crate::driver::statistics::compare_app_vs_driver_stats(&driver_stats);
    }
    
    Ok(())
}

pub fn display_driver_stats() -> Result<(), String> {
    if !is_driver_connected() {
        println!("Driver not connected - no statistics available");
        return Ok(());
    }
    
    println!("\n=== Driver Statistics ===");
    
    // Show app-side statistics
    display_app_driver_stats_table();
    
    // Show driver-side statistics if available
    if let Ok(driver) = get_global_driver_handle() {
        if let Ok(stats) = driver.get_driver_statistics() {
            println!("\n=== Driver Runtime Statistics ===");
            display_driver_runtime_stats(&stats);
            
            // Compare app vs driver stats
            let driver_stats_map = driver_stats_to_hashmap(&stats);
            crate::driver::statistics::compare_app_vs_driver_stats(&driver_stats_map);
        } else {
            println!("\n⚠️  Failed to retrieve driver runtime statistics");
        }
    }
    
    Ok(())
}

pub fn reset_app_driver_stats() {
    crate::driver::statistics::reset_app_driver_stats();
}

pub fn display_app_driver_stats_table() {
    crate::driver::statistics::display_app_driver_stats_table();
}

fn display_driver_runtime_stats(stats: &DriverStatistics) {
    let mut table = TableBuilder::new()
        .add_header("Metric", Alignment::Left)
        .add_header("Value", Alignment::Right);
    
    table = table.add_row(vec!["Total Calls".to_string(), stats.total_calls.to_string()]);
    table = table.add_row(vec!["Active Allocations".to_string(), stats.active_allocations.to_string()]);
    table = table.add_row(vec!["Total Allocated".to_string(), format!("{:.2} MB", stats.total_allocated_bytes as f64 / (1024.0 * 1024.0))]);
    table = table.add_row(vec!["Peak Allocated".to_string(), format!("{:.2} MB", stats.peak_allocated_bytes as f64 / (1024.0 * 1024.0))]);
    table = table.add_row(vec!["Allocation Failures".to_string(), stats.allocation_failures.to_string()]);
    table = table.add_row(vec!["Driver Uptime".to_string(), format!("{:.2} sec", stats.driver_uptime_ms as f64 / 1000.0)]);
    
    if stats.last_error_code != 0 {
        table = table.add_row(vec!["Last Error Code".to_string(), format!("0x{:08X}", stats.last_error_code)]);
    }
    
    table.print();
}

fn driver_stats_to_hashmap(stats: &DriverStatistics) -> std::collections::HashMap<String, u64> {
    let mut map = std::collections::HashMap::new();
    map.insert("Total Calls".to_string(), stats.total_calls);
    map.insert("Allocate DMA".to_string(), stats.allocate_dma_calls);
    map.insert("Free DMA".to_string(), stats.free_dma_calls);
    map.insert("Batch Allocate".to_string(), stats.batch_allocate_calls);
    map.insert("Get Statistics".to_string(), stats.get_statistics_calls);
    map.insert("Set CPU Affinity".to_string(), stats.set_cpu_affinity_calls);
    map.insert("Get Hardware Info".to_string(), stats.get_hardware_info_calls);
    map.insert("Reset All".to_string(), stats.reset_all_calls);
    map.insert("Get Version".to_string(), stats.get_version_calls);
    map.insert("Batch Remap Memory Type".to_string(), stats.batch_remap_calls);
    map.insert("Remap All Memory Type".to_string(), stats.remap_all_calls);
    map
}