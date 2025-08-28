/// Simplified conversion functions that work with the actual codebase structure
/// This bridges the gap between the current codebase and the new reporting system
use super::models::*;
use crate::memory::allocation_strategy::{SystemMemoryInfo, AllocationResult};
use crate::driver::DriverStatus;
use crate::progress::{TestSummary, CycleStats};
use crate::constants::{PageType, bytes_to_gib_f64};
use std::collections::HashSet;

impl From<&SystemMemoryInfo> for MemoryInfo {
    fn from(info: &SystemMemoryInfo) -> Self {
        MemoryInfo {
            total_installed: info.total_installed_bytes,
            total_physical: info.total_physical_bytes,
            available_physical: info.available_physical_bytes,
            large_pages_available: false, // Will be set by caller
            huge_pages_available: false,  // Will be set by caller
            numa_nodes: 1,                // Will be set by caller
        }
    }
}

impl From<&AllocationResult> for MemoryAllocationReport {
    fn from(result: &AllocationResult) -> Self {
        // Note: Some fields need to be filled by the caller with additional context
        MemoryAllocationReport {
            total_installed_bytes: 0,      // Needs SystemMemoryInfo
            total_physical_bytes: 0,       // Needs SystemMemoryInfo
            available_physical_bytes: 0,   // Needs SystemMemoryInfo
            used_physical_bytes: 0,        // Needs SystemMemoryInfo
            allocation_bytes: result.allocation_bytes,
            reserve_bytes: result.reserve_bytes,
            reference_bytes: result.reference_bytes,
            allocation_percent: (result.allocation_bytes as f64 / result.reference_bytes as f64) * 100.0,
            reserve_percent: (result.reserve_bytes as f64 / result.reference_bytes as f64) * 100.0,
            memory_load_percent: 0,        // Needs SystemMemoryInfo
            allocation_type: result.allocation_type.clone(),
            min_start_address: result.min_start_address,
            actual_start_address: result.min_start_address, // Same for now
            split_reserve: None,               // Will be populated if available
            warnings: Vec::new(),
        }
    }
}

/// Convert allocation result with full system memory info
pub fn create_memory_allocation_report(
    allocation_result: &AllocationResult,
    mem_info: &SystemMemoryInfo,
) -> MemoryAllocationReport {
    let mut report = MemoryAllocationReport::from(allocation_result);
    
    // Fill in system memory info
    report.total_installed_bytes = mem_info.total_installed_bytes;
    report.total_physical_bytes = mem_info.total_physical_bytes;
    report.available_physical_bytes = mem_info.available_physical_bytes;
    report.used_physical_bytes = mem_info.used_physical_bytes;
    report.memory_load_percent = mem_info.memory_load_percent;
    
    // Add split reserve details if available
    if let Some(details) = &allocation_result.split_details {
        report.split_reserve = Some(SplitReserveDetails {
            pre_percent: details.pre_percent,
            post_percent: details.post_percent,
            pre_buffer_bytes: details.pre_buffer_bytes,
            post_reserve_bytes: details.post_reserve_bytes,
        });
    }
    
    report
}

/// Create a consolidated memory report combining system info and allocation planning
pub fn create_consolidated_memory_report(
    mem_info: &SystemMemoryInfo, 
    allocation_result: &AllocationResult
) -> ConsolidatedMemoryReport {
    let mut warnings = Vec::new();
    
    // Check for failure mode testing
    if allocation_result.allocation_type.contains("from Total") || allocation_result.allocation_type.contains("Target") {
        warnings.push("🧪 FAILURE MODE TESTING DETECTED - This configuration may intentionally cause allocation failures".to_string());
        warnings.push("Use only for development/testing purposes!".to_string());
        
        if allocation_result.allocation_bytes > mem_info.available_physical_bytes * 2 {
            warnings.push("🚨 EXTREME: Allocation >2x available memory - will likely fail".to_string());
        }
    }
    
    ConsolidatedMemoryReport {
        total_installed_bytes: mem_info.total_installed_bytes,
        total_physical_bytes: mem_info.total_physical_bytes,
        available_physical_bytes: mem_info.available_physical_bytes,
        used_physical_bytes: mem_info.used_physical_bytes,
        total_virtual_bytes: mem_info.total_virtual_bytes,
        available_virtual_bytes: mem_info.available_virtual_bytes,
        memory_load_percent: mem_info.memory_load_percent,
        allocation_bytes: allocation_result.allocation_bytes,
        reserve_bytes: allocation_result.reserve_bytes,
        reference_bytes: allocation_result.reference_bytes,
        allocation_type: allocation_result.allocation_type.clone(),
        min_start_address: allocation_result.min_start_address,
        split_reserve: allocation_result.split_details.as_ref().map(|details| SplitReserveDetails {
            pre_percent: details.pre_percent,
            post_percent: details.post_percent,
            pre_buffer_bytes: details.pre_buffer_bytes,
            post_reserve_bytes: details.post_reserve_bytes,
        }),
        warnings,
    }
}

/// Convert driver status to reporting format
pub fn create_driver_status_report(status: &DriverStatus) -> DriverStatusReport {
    match status {
        DriverStatus::Available(version_info) => {
            let version = Some(DriverVersion {
                major: version_info.driver_version_major as u16,
                minor: version_info.driver_version_minor as u16,
                build: version_info.driver_version_build as u16,
                revision: version_info.driver_version_revision as u16,
            });
            
            DriverStatusReport {
                available: true,
                version,
                error_message: None,
                statistics: None,
            }
        }
        DriverStatus::VersionMismatch { driver_version, app_version, min_required, max_supported } => {
            DriverStatusReport {
                available: false,
                version: None,
                error_message: Some(format!(
                    "Version mismatch: driver {}, app {}, requires {}-{}",
                    driver_version, app_version, min_required, max_supported
                )),
                statistics: None,
            }
        }
        DriverStatus::NotFound => DriverStatusReport {
            available: false,
            version: None,
            error_message: Some("Driver not found".to_string()),
            statistics: None,
        },
        DriverStatus::Error(msg) => DriverStatusReport {
            available: false,
            version: None,
            error_message: Some(msg.clone()),
            statistics: None,
        },
    }
}

/// Convert test definitions to test configuration report
pub fn create_test_configuration_report(
    test_definitions: &[(&'static str, crate::runner::TestFunction, crate::tests::TestMemoryConfig)],
    suite_timing: &crate::runner::TestSuiteTiming,
) -> TestConfigurationReport {
    use crate::reporting::formatters::{ReportFormatter, DefaultFormatter};
    let formatter = DefaultFormatter::new();
    
    let suite_timing_str = match (suite_timing.global_cycles, suite_timing.global_duration_secs) {
        (Some(cycles), Some(duration)) => format!("{} cycles or {}s max", cycles, duration),
        (Some(cycles), None) => format!("{} cycles", cycles),
        (None, Some(duration)) => format!("{}s duration", duration),
        (None, None) => "Unlimited".to_string(),
    };
    
    let tests = test_definitions.iter().enumerate().map(|(i, (name, _, config))| {
        let timing = match (&config.timing.cycles, &config.timing.duration_secs) {
            (Some(c), Some(d)) => format!("{}cycles/{}s", c, d),
            (Some(c), None) => format!("{}cycles", c),
            (None, Some(d)) => format!("{}s", d),
            (None, None) => "unlimited".to_string(),
        };
        
        let window_mode = formatter.format_window_mode(&config.window_mode);
        let chunk_mode = formatter.format_chunk_mode(&config.chunk_mode);
        
        let mut flags = Vec::new();
        if config.allow_misaligned {
            flags.push("Misaligned".to_string());
        }
        if config.requires_locality {
            flags.push("Locality".to_string());
        }
        
        TestConfigurationEntry {
            number: i + 1,
            name: name.to_string(),
            timing,
            streams: config.streams as usize,
            window_mode,
            chunk_mode,
            flags,
        }
    }).collect();
    
    TestConfigurationReport {
        suite_timing: suite_timing_str,
        test_count: test_definitions.len(),
        tests,
    }
}

/// Convert test summaries to cycle report
pub fn create_cycle_report(
    cycle: u32,
    duration_secs: u32,
    summaries: &[TestSummary],
) -> CycleReport {
    let test_performances = summaries.iter().enumerate().map(|(i, summary)| {
        TestPerformanceEntry {
            number: i + 1,
            name: summary.name.clone(),
            duration_secs: summary.duration_ms as f64 / 1000.0,
            data_processed_gib: bytes_to_gib_f64(summary.bytes_processed),
            throughput_mib_s: summary.throughput_mib_s,
            throughput_gib_s: summary.throughput_mib_s / 1024.0,
            errors: summary.errors,
        }
    }).collect();
    
    CycleReport {
        cycle_number: cycle,
        duration_secs,
        test_performances,
    }
}

/// Convert CPU stats to variance report
pub fn create_cpu_variance_report(
    cpu_stats: &std::collections::HashMap<usize, crate::runner::CpuPerformanceStats>,
) -> CpuVarianceReport {
    // Calculate average performance
    let total_elapsed: u128 = cpu_stats.values().map(|s| s.total_elapsed_ms).sum();
    let total_bytes: u64 = cpu_stats.values().map(|s| s.total_bytes).sum();
    let avg_throughput = if total_elapsed > 0 {
        (total_bytes as f64 / (1024.0 * 1024.0)) / (total_elapsed as f64 / 1000.0)
    } else {
        0.0
    };
    
    let cpu_performances = cpu_stats.iter().map(|(cpu_id, stats)| {
        let cpu_throughput = if stats.total_elapsed_ms > 0 {
            (stats.total_bytes as f64 / (1024.0 * 1024.0)) / (stats.total_elapsed_ms as f64 / 1000.0)
        } else {
            0.0
        };
        
        let variance_pct = if avg_throughput > 0.0 {
            ((cpu_throughput - avg_throughput) / avg_throughput) * 100.0
        } else {
            0.0
        };
        
        CpuPerformanceEntry {
            cpu_id: *cpu_id,
            throughput_mib_s: cpu_throughput,
            variance_percent: variance_pct,
            test_count: stats.thread_count as u32,
        }
    }).collect();
    
    CpuVarianceReport {
        average_throughput_mib_s: avg_throughput,
        cpu_performances,
    }
}

/// Convert DetailedOperationCount to OperationBreakdown for reporting
pub fn create_operation_breakdown(detailed: &crate::tests::DetailedOperationCount) -> OperationBreakdown {
    OperationBreakdown {
        total_reads: detailed.total_reads,
        total_writes: detailed.total_writes,
        total_verifies: detailed.total_verifies,
        total_simd_ops: detailed.total_simd_ops,
        total_fence_ops: detailed.total_fence_ops,
        total_cache_ops: detailed.total_cache_ops,
        simd_type: match &detailed.simd_type {
            crate::tests::SIMDType::None => "None".to_string(),
            crate::tests::SIMDType::SSE2_128 => "SSE2 (128-bit)".to_string(),
            crate::tests::SIMDType::AVX2_256 => "AVX2 (256-bit)".to_string(),
            crate::tests::SIMDType::AVX512_512 => "AVX-512 (512-bit)".to_string(),
        },
        access_pattern: match &detailed.access_pattern {
            crate::tests::AccessPattern::Sequential => "Sequential".to_string(),
            crate::tests::AccessPattern::Strided(s) => format!("Strided ({}B)", s),
            crate::tests::AccessPattern::Random => "Random".to_string(),
            crate::tests::AccessPattern::Mirror => "Mirror".to_string(),
            crate::tests::AccessPattern::BlockCopy => "Block Copy".to_string(),
            crate::tests::AccessPattern::CacheBusting => "Cache Busting".to_string(),
        },
    }
}

/// Convert final test data to summary report
pub fn create_final_test_summary_report(
    total_time: std::time::Duration,
    cycle_stats: &[CycleStats],
    total_bytes: u64,
    total_errors: u64,
    test_definitions: &[(&'static str, crate::runner::TestFunction, crate::tests::TestMemoryConfig)],
) -> FinalTestSummaryReport {
    let cycles_completed = cycle_stats.len();
    let total_data_processed_gib = bytes_to_gib_f64(total_bytes);
    
    let overall_throughput_mib_s = if total_time.as_secs() > 0 {
        (total_bytes as f64 / (1024.0 * 1024.0)) / total_time.as_secs() as f64
    } else {
        0.0
    };
    
    // Aggregate per-test performance
    let mut test_aggregates: std::collections::HashMap<String, (u64, u128, u64)> = std::collections::HashMap::new();
    
    for cycle in cycle_stats {
        for test_summary in &cycle.test_stats {
            let entry = test_aggregates.entry(test_summary.name.clone()).or_insert((0, 0, 0));
            entry.0 += test_summary.bytes_processed;
            entry.1 += test_summary.duration_ms;
            entry.2 += test_summary.errors;
        }
    }
    
    let mut per_test_summaries = Vec::new();
    for (test_name, _, _) in test_definitions.iter() {
        if let Some((total_bytes, total_duration_ms, total_errors)) = test_aggregates.get(*test_name) {
            let cycle_count = cycle_stats.len() as u64;
            let avg_bytes = *total_bytes / cycle_count;
            let avg_duration_ms = *total_duration_ms / cycle_count as u128;
            let avg_throughput_mib_s = if avg_duration_ms > 0 {
                (avg_bytes as f64 / (1024.0 * 1024.0)) / (avg_duration_ms as f64 / 1000.0)
            } else {
                0.0
            };
            
            per_test_summaries.push(TestSummaryEntry {
                name: test_name.to_string(),
                average_duration_secs: avg_duration_ms as f64 / 1000.0,
                total_data_gib: bytes_to_gib_f64(avg_bytes),
                average_throughput_mib_s: avg_throughput_mib_s,
                average_throughput_gib_s: avg_throughput_mib_s / 1024.0,
                total_errors: *total_errors,
            });
        }
    }
    
    FinalTestSummaryReport {
        total_runtime: format_duration(total_time),
        cycles_completed,
        total_data_processed_gib,
        overall_throughput_mib_s,
        overall_throughput_gib_s: overall_throughput_mib_s / 1024.0,
        total_errors,
        per_test_summaries,
    }
}

/// Create block allocation report from Windows VirtualAlloc2 allocation results
pub fn create_block_allocation_report_from_windows(
    allocations: &std::collections::HashMap<usize, Vec<crate::AllocationBlock>>,
) -> BlockAllocationReport {
    let allocator_backend = "Windows VirtualAlloc2".to_string();
    
    analyze_block_allocations(allocations, allocator_backend)
}

/// Create block allocation report from driver batch allocation results
pub fn create_block_allocation_report_from_driver(
    allocations: &std::collections::HashMap<usize, Vec<crate::AllocationBlock>>,
) -> BlockAllocationReport {
    let allocator_backend = "TMR Kernel Driver".to_string();
    
    analyze_block_allocations(allocations, allocator_backend)
}

/// Common analysis function that works with both Windows and driver allocations
fn analyze_block_allocations(
    allocations: &std::collections::HashMap<usize, Vec<crate::AllocationBlock>>,
    allocator_backend: String,
) -> BlockAllocationReport {
    use std::collections::HashMap;
    
    let total_threads = allocations.len();
    let mut total_allocated_bytes = 0u64;
    let mut block_size_stats = HashMap::new();
    let mut numa_stats = HashMap::new();
    let mut thread_allocations = Vec::new();
    let mut all_allocations = Vec::new();
    
    // Analyze each thread's allocations
    for (&thread_id, blocks) in allocations {
        let mut thread_total_bytes = 0u64;
        let mut thread_block_sizes: Vec<ThreadBlockSize> = Vec::new();
        let mut thread_page_breakdown = ThreadPageTypeBreakdown {
            huge_pages_count: 0,
            large_pages_count: 0,
            regular_pages_count: 0,
            huge_pages_bytes: 0,
            large_pages_bytes: 0,
            regular_pages_bytes: 0,
        };
        
        let cpu_id = blocks.first().map(|b| b.block_info.thread_id).unwrap_or(thread_id);
        let numa_node = blocks.first().map(|b| b.numa_node).unwrap_or(0);
        
        for block in blocks {
            let block_size_mb = (block.buffer.size() as f64 / (1024.0 * 1024.0)).ceil() as u32;
            let block_size_bytes = block.buffer.size() as u64;
            
            thread_total_bytes += block_size_bytes;
            total_allocated_bytes += block_size_bytes;
            
            // Determine page type and update breakdowns
            let page_type = PageType::from_buffer(block);
            let page_count = page_type.pages_needed(block_size_bytes);
            
            match page_type {
                PageType::Huge => {
                    thread_page_breakdown.huge_pages_count += page_count;
                    thread_page_breakdown.huge_pages_bytes += block_size_bytes;
                }
                PageType::Large => {
                    thread_page_breakdown.large_pages_count += page_count;
                    thread_page_breakdown.large_pages_bytes += block_size_bytes;
                }
                PageType::Regular => {
                    thread_page_breakdown.regular_pages_count += page_count;
                    thread_page_breakdown.regular_pages_bytes += block_size_bytes;
                }
            }
            
            // Track block size distribution
            let size_key = block_size_mb;
            let size_entry = block_size_stats.entry(size_key).or_insert((0, 0u64, HashSet::new(), HashSet::new()));
            size_entry.0 += 1; // block count
            size_entry.1 += block_size_bytes; // total bytes
            size_entry.2.insert(page_type.display_name().to_string()); // page types for this size
            size_entry.3.insert(thread_id); // threads with this size
            
            // Track NUMA distribution
            let numa_entry = numa_stats.entry(numa_node).or_insert((std::collections::HashSet::new(), 0u64, Vec::new(), NumaPageTypeStats {
                huge_pages_count: 0,
                large_pages_count: 0,
                regular_pages_count: 0,
                huge_pages_bytes: 0,
                large_pages_bytes: 0,
                regular_pages_bytes: 0,
            }));
            numa_entry.0.insert(thread_id);
            numa_entry.1 += block_size_bytes;
            if !numa_entry.2.contains(&block_size_mb) {
                numa_entry.2.push(block_size_mb);
            }
            
            // Update NUMA page type stats
            match page_type {
                PageType::Huge => {
                    numa_entry.3.huge_pages_count += page_count as u32;
                    numa_entry.3.huge_pages_bytes += block_size_bytes;
                }
                PageType::Large => {
                    numa_entry.3.large_pages_count += page_count as u32;
                    numa_entry.3.large_pages_bytes += block_size_bytes;
                }
                PageType::Regular => {
                    numa_entry.3.regular_pages_count += page_count as u32;
                    numa_entry.3.regular_pages_bytes += block_size_bytes;
                }
            }
            
            // Track individual block size for thread
            let existing_size = thread_block_sizes.iter_mut().find(|bs| bs.size_mb == block_size_mb);
            if let Some(existing) = existing_size {
                existing.count += 1;
                existing.total_bytes += block_size_bytes;
            } else {
                thread_block_sizes.push(ThreadBlockSize {
                    size_mb: block_size_mb,
                    count: 1,
                    total_bytes: block_size_bytes,
                    page_type: page_type.display_name().to_string(),
                });
            }
            
            all_allocations.push((thread_id, block_size_bytes));
        }
        
        thread_allocations.push(ThreadBlockAllocation {
            thread_id,
            cpu_id,
            numa_node,
            total_bytes: thread_total_bytes,
            block_sizes: thread_block_sizes,
            page_type_breakdown: thread_page_breakdown,
        });
    }
    
    // Build block size distribution
    let mut block_size_distribution = Vec::new();
    for (size_mb, (count, total_bytes, page_types, threads)) in block_size_stats {
        // Format page types - sort for consistency
        let mut page_type_vec: Vec<String> = page_types.into_iter().collect();
        page_type_vec.sort();
        let page_type = if page_type_vec.len() == 1 {
            page_type_vec[0].clone()
        } else {
            format!("Mixed ({})", page_type_vec.join(", "))
        };
        
        block_size_distribution.push(BlockSizeDistribution {
            block_size_mb: size_mb,
            block_count: count,
            total_bytes,
            page_type,
            threads_with_this_size: threads.len() as u32,
            average_per_thread: count as f64 / threads.len() as f64,
        });
    }
    block_size_distribution.sort_by(|a, b| b.block_size_mb.cmp(&a.block_size_mb)); // Largest first
    
    // Build NUMA distribution
    let mut numa_distribution = Vec::new();
    for (node_id, (threads, total_bytes, mut block_sizes, page_types)) in numa_stats {
        block_sizes.sort_by(|a, b| b.cmp(a)); // Largest first
        numa_distribution.push(NumaNodeDistribution {
            node_id,
            thread_count: threads.len() as u32,
            total_bytes,
            block_sizes,
            page_types,
        });
    }
    numa_distribution.sort_by(|a, b| a.node_id.cmp(&b.node_id));
    
    // Calculate page type summary
    let mut huge_stats = PageTypeStats { page_count: 0, total_bytes: 0, block_count: 0, threads_using: 0, percentage_of_total: 0.0 };
    let mut large_stats = PageTypeStats { page_count: 0, total_bytes: 0, block_count: 0, threads_using: 0, percentage_of_total: 0.0 };
    let mut regular_stats = PageTypeStats { page_count: 0, total_bytes: 0, block_count: 0, threads_using: 0, percentage_of_total: 0.0 };
    
    for thread_alloc in &thread_allocations {
        let breakdown = &thread_alloc.page_type_breakdown;
        
        if breakdown.huge_pages_count > 0 {
            huge_stats.page_count += breakdown.huge_pages_count;
            huge_stats.total_bytes += breakdown.huge_pages_bytes;
            huge_stats.threads_using += 1;
        }
        if breakdown.large_pages_count > 0 {
            large_stats.page_count += breakdown.large_pages_count;
            large_stats.total_bytes += breakdown.large_pages_bytes;
            large_stats.threads_using += 1;
        }
        if breakdown.regular_pages_count > 0 {
            regular_stats.page_count += breakdown.regular_pages_count;
            regular_stats.total_bytes += breakdown.regular_pages_bytes;
            regular_stats.threads_using += 1;
        }
        
        // Count blocks by page type
        for block_size in &thread_alloc.block_sizes {
            if block_size.page_type.contains("huge") {
                huge_stats.block_count += block_size.count;
            } else if block_size.page_type.contains("large") {
                large_stats.block_count += block_size.count;
            } else {
                regular_stats.block_count += block_size.count;
            }
        }
    }
    
    // Calculate percentages
    if total_allocated_bytes > 0 {
        huge_stats.percentage_of_total = (huge_stats.total_bytes as f64 / total_allocated_bytes as f64) * 100.0;
        large_stats.percentage_of_total = (large_stats.total_bytes as f64 / total_allocated_bytes as f64) * 100.0;
        regular_stats.percentage_of_total = (regular_stats.total_bytes as f64 / total_allocated_bytes as f64) * 100.0;
    }
    
    // Calculate allocation fairness
    let allocation_sizes: Vec<u64> = thread_allocations.iter().map(|t| t.total_bytes).collect();
    let fairness = calculate_allocation_fairness(allocation_sizes, &thread_allocations);
    
    BlockAllocationReport {
        allocator_backend,
        total_threads,
        total_allocated_bytes,
        block_size_distribution,
        page_type_summary: PageTypeSummary {
            huge_pages: huge_stats,
            large_pages: large_stats,
            regular_pages: regular_stats,
        },
        per_thread_allocation: thread_allocations,
        numa_distribution,
        allocation_fairness: fairness,
    }
}

/// Calculate allocation fairness statistics
fn calculate_allocation_fairness(
    allocation_sizes: Vec<u64>,
    thread_allocations: &[ThreadBlockAllocation],
) -> AllocationFairness {
    if allocation_sizes.is_empty() {
        return AllocationFairness {
            coefficient_of_variation: 0.0,
            min_allocation_bytes: 0,
            max_allocation_bytes: 0,
            mean_allocation_bytes: 0.0,
            median_allocation_bytes: 0,
            unfair_threads: Vec::new(),
        };
    }
    
    let min_allocation = *allocation_sizes.iter().min().unwrap();
    let max_allocation = *allocation_sizes.iter().max().unwrap();
    let mean_allocation = allocation_sizes.iter().sum::<u64>() as f64 / allocation_sizes.len() as f64;
    
    // Calculate median
    let mut sorted_sizes = allocation_sizes.clone();
    sorted_sizes.sort();
    let median_allocation = if sorted_sizes.len().is_multiple_of(2) {
        let mid = sorted_sizes.len() / 2;
        (sorted_sizes[mid - 1] + sorted_sizes[mid]) / 2
    } else {
        sorted_sizes[sorted_sizes.len() / 2]
    };
    
    // Calculate coefficient of variation (standard deviation / mean)
    let variance = allocation_sizes.iter()
        .map(|&size| (size as f64 - mean_allocation).powi(2))
        .sum::<f64>() / allocation_sizes.len() as f64;
    let std_dev = variance.sqrt();
    let coefficient_of_variation = if mean_allocation > 0.0 { std_dev / mean_allocation } else { 0.0 };
    
    // Identify unfair threads (>20% deviation from mean)
    let mut unfair_threads = Vec::new();
    for thread_alloc in thread_allocations {
        let deviation_percent = ((thread_alloc.total_bytes as f64 - mean_allocation) / mean_allocation) * 100.0;
        
        if deviation_percent.abs() > 20.0 {
            let reason = if thread_alloc.page_type_breakdown.huge_pages_count > 0 && 
                        thread_alloc.page_type_breakdown.large_pages_count == 0 && 
                        thread_alloc.page_type_breakdown.regular_pages_count == 0 {
                "got all huge pages".to_string()
            } else if thread_alloc.page_type_breakdown.huge_pages_count == 0 && 
                     thread_alloc.page_type_breakdown.large_pages_count > 0 && 
                     thread_alloc.page_type_breakdown.regular_pages_count == 0 {
                "got all large pages".to_string()
            } else if thread_alloc.page_type_breakdown.huge_pages_count == 0 && 
                     thread_alloc.page_type_breakdown.large_pages_count == 0 {
                "only regular pages".to_string()
            } else if deviation_percent > 0.0 {
                "received extra chunks".to_string()
            } else {
                "received fewer chunks".to_string()
            };
            
            unfair_threads.push(UnfairThreadAllocation {
                thread_id: thread_alloc.thread_id,
                allocated_bytes: thread_alloc.total_bytes,
                deviation_from_mean_percent: deviation_percent,
                reason,
            });
        }
    }
    
    AllocationFairness {
        coefficient_of_variation,
        min_allocation_bytes: min_allocation,
        max_allocation_bytes: max_allocation,
        mean_allocation_bytes: mean_allocation,
        median_allocation_bytes: median_allocation,
        unfair_threads,
    }
}

/// Format duration in human-readable format
fn format_duration(duration: std::time::Duration) -> String {
    let secs = duration.as_secs();
    let hours = secs / 3600;
    let minutes = (secs % 3600) / 60;
    let seconds = secs % 60;
    
    if hours > 0 {
        format!("{}h {}m {}s", hours, minutes, seconds)
    } else if minutes > 0 {
        format!("{}m {}s", minutes, seconds)
    } else {
        format!("{}s", seconds)
    }
}