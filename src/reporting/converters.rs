/// Simplified conversion functions that work with the actual codebase structure
/// This bridges the gap between the current codebase and the new reporting system
use super::models::*;
use crate::memory::allocation_strategy::{SystemMemoryInfo, AllocationResult};
use crate::progress::TestSummary;
use crate::constants::{PageType, bytes_to_gib_f64};
use std::collections::HashSet;

/// A test's parameters for the plan and report: the first one set, in the input's own form
pub(crate) fn format_parameter_context(ctx: &Option<crate::config::TestParameterContext>) -> String {
    match ctx {
        Some(c) => {
            if let Some(mirror) = c.mirror {
                return mirror.to_string();
            }
            if let Some(stride_cl) = c.stride_cachelines
                && stride_cl > 0 {
                    let stride_bytes = stride_cl * 64; // cache line = 64 bytes
                    return format!("Stride({}cl/{}B)", stride_cl, stride_bytes);
                }
            // TMR-native test parameters
            if let Some(sp) = c.stride_patterns {
                return format!("StridePat({})", sp);
            }
            if let Some(rng) = c.rng_sequences {
                return format!("RngSeq({})", rng);
            }
            if let Some(sub) = c.subdivisions {
                return format!("Subdiv({})", sub);
            }
            if let Some(cd) = c.copy_directions {
                return format!("CopyDir({})", cd);
            }
            "-".to_string()
        }
        None => "-".to_string(),
    }
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
        memory_load_percent: mem_info.memory_load_percent,
        reference_bytes: allocation_result.reference_bytes,
        reference_name: allocation_result.reference_name,
        requested_reserve_bytes: allocation_result.requested_reserve_bytes,
        target_bytes: allocation_result.target_bytes,
        raw_allocation_bytes: allocation_result.raw_allocation_bytes,
        thread_count: allocation_result.thread_count,
        per_thread_raw_bytes: allocation_result.per_thread_raw_bytes,
        rounding_step_bytes: allocation_result.rounding_step_bytes,
        rounding_direction: allocation_result.rounding_direction.to_string(),
        rounding_asked: allocation_result.rounding_asked
            .map(|asked| (asked.step_bytes, asked.direction.to_string())),
        per_thread_bytes: allocation_result.per_thread_bytes,
        allocation_bytes: allocation_result.allocation_bytes,
        reserve_bytes: allocation_result.reserve_bytes,
        warnings,
    }
}

/// Convert TestDefinition structs to test configuration report (updated version)
/// Now accepts cache_info and thread_count to calculate actual extent sizes for cache targets
/// `thread_memory` is each thread's bytes (planned or allocated): extents and chunks are shown
/// resolved for the thread with the least, the range when threads differ. `tm5` is a TM5
/// import's `.cfg` window and lock granularity (MiB), for a header line.
/// What the plan shows of the cycle beyond its tests (TODO 74).
pub struct PlanSequence<'a> {
    /// `cycle_order` as written, each id with whether it runs; `None` without one
    pub order: Option<&'a [(String, bool)]>,
    /// The seal's kernel, when the run is sealed
    pub seal: Option<crate::seal::SealKernel>,
    /// A TM5 import: tests go by the `.cfg`'s numbers
    pub tm5: bool,
}

/// The plan: one row per test, in the order of its first step, then the cycle's sequence when it
/// isn't simply each test once, and the seal (TODO 74).
pub fn create_test_configuration_report_v2(
    test_definitions: &[crate::runner::TestDefinition],
    suite_timing: &crate::runner::TestSuiteTiming,
    cache_info: &crate::cache::CacheInfo,
    thread_memory: &[usize],
    tm5: Option<(u32, u32)>,
    sequence: &PlanSequence,
) -> TestConfigurationReport {
    use crate::reporting::formatters::{ReportFormatter, DefaultFormatter};
    use crate::test_memory::{ChunkSpread, GRANULE};
    use crate::tests::{ChunkMode, ExtentMode};
    let formatter = DefaultFormatter::new();
    let size = |bytes: usize| formatter.format_bytes(bytes as u64);
    let thread_count = thread_memory.len();
    let least = thread_memory.iter().copied().min().unwrap_or(0);
    let most = thread_memory.iter().copied().max().unwrap_or(0);

    let mut notes = Vec::new();
    if let Some((window, lock)) = tm5 {
        notes.push(format!("ℹ  TM5 import: every test covers the full allocation. The .cfg's {window} MiB Testing Window and \
                            {lock} MiB Lock Memory Granularity only size Test Block Size codes 0-3 and cap larger blocks."));
    }

    let suite_timing_str = match (suite_timing.global_cycles, suite_timing.global_duration_secs) {
        (Some(cycles), Some(duration)) => format!("{} cycles or {}s max", cycles, duration),
        (Some(cycles), None) => format!("{} cycles", cycles),
        (None, Some(duration)) => format!("{}s duration", duration),
        (None, None) => "Unlimited".to_string(),
    };

    // Each test once, at its first step, with how many steps of the cycle run it
    let mut firsts: Vec<(&crate::runner::TestDefinition, usize)> = Vec::new();
    for def in test_definitions {
        match firsts.iter_mut().find(|(d, _)| d.test_index == def.test_index) {
            Some((_, steps)) => *steps += 1,
            None => firsts.push((def, 1)),
        }
    }
    let unique = firsts.len();

    let tests = firsts.iter().enumerate().map(|(i, &(test_def, steps))| {
        let config = &test_def.config;

        let timing = match (&config.timing.cycles, &config.timing.duration_secs) {
            (Some(c), Some(d)) => format!("{}cycles/{}s", c, d),
            (Some(c), None) => format!("{}cycles", c),
            (None, Some(d)) => format!("{}s", d),
            (None, None) => "default".to_string(),  // Match old behavior
        };

        let per_chunk = if crate::tests::reads_rep_knobs(test_def.actual_name) {
            format!("{}×({}+{})", config.write_read_cycles, config.test_reps, config.verify_reps)
        } else {
            "-".to_string()
        };

        // Use the new formatter with calculated sizes for CacheLevel targets
        let mut extent_mode = formatter.format_extent_mode_with_size(&config.extent_mode, cache_info, thread_count);
        let mut chunk_mode = formatter.format_chunk_mode_with_size(&config.chunk_mode, cache_info, thread_count);

        // Resolved as the test resolves them (`TestRunner::new`): the extent floored to 4 KiB,
        // then the chunk from it, spread over it
        if least > 0 {
            let mut sized = config.clone();
            sized.thread_count = thread_count;
            let name = test_def.actual_name;
            // Below one granule TestRunner tests one granule (and logs it)
            let extent_at = |memory: usize| (sized.calculate_extent_size(name, memory) / GRANULE * GRANULE).max(GRANULE.min(memory));
            let (lo, hi) = (extent_at(least), extent_at(most));
            let sizes = if lo == hi { size(lo) } else { format!("{}-{}", size(lo), size(hi)) };
            extent_mode = match &config.extent_mode {
                ExtentMode::FullAllocation => format!("Full ({sizes}/thread)"),
                ExtentMode::Absolute { .. } => sizes,
                ExtentMode::CacheTotal { fraction } => format!("{sizes} (CacheTotal {fraction:.2}x)"),
                ExtentMode::Cache { target } => {
                    format!("{sizes} ({})", target.name_with_context(thread_count, cache_info.is_virtual_machine))
                }
            };
            let resolution = sized.resolve_chunk(name, lo);
            let spread = ChunkSpread::new(lo, resolution.resolved);
            let overlap = if spread.overlap() > 0 { format!(", {} overlap", size(spread.overlap())) } else { String::new() };
            let source = match &config.chunk_mode {
                ChunkMode::Tm5Block { divisor: 1, .. } => " (.cfg window)".to_string(),
                ChunkMode::Tm5Block { divisor, .. } => format!(" (.cfg window/{divisor})"),
                ChunkMode::Cache { target } => format!(" ({})", target.name()),
                ChunkMode::Auto => " (auto)".to_string(),
                ChunkMode::Whole => " (whole)".to_string(),
                ChunkMode::Absolute { .. } | ChunkMode::CacheTotal { .. } => String::new(),
            };
            chunk_mode = format!("{} x{}{overlap}{source}", size(spread.chunk()), spread.count());
            // Only when the memory is what limits the extent: a cache-sized extent smaller than
            // its chunk is the test's design (Mem-Refresh's 2 GiB chunk means "the whole extent")
            let memory_bounds_extent = sized.calculate_extent_size(name, 1 << 60) > lo;
            if resolution.shrunk_by_memory() && memory_bounds_extent {
                notes.push(format!("⚠  Chunk size was adjusted for test {} ({}) from {} -> {}, as the memory allocation ({} per thread) can't support it",
                                   i + 1, test_def.display_name, size(resolution.requested), size(resolution.resolved), size(lo)));
            }
        }

        use crate::runner::SealUse;
        let seal = match test_def.seal_use {
            _ if !config.seal.on => "",
            SealUse::Wrap if config.seal.wrap => "✓",
            SealUse::Data => "data",
            SealUse::Check => "check",
            _ => "–",
        }.to_string();

        let parameter = format_parameter_context(&config.parameter_context);
        let mut name = test_def.step_label(sequence.tm5);
        if steps > 1 {
            name.push_str(&format!(" ×{steps}"));
        }

        TestConfigurationEntry {
            number: i + 1,
            name,
            timing,
            per_chunk,
            parameter,
            extent_mode,
            chunk_mode,
            seal,
        }
    }).collect();

    // The sequence, when it isn't each test once in row order
    let label = |id: &str| if sequence.tm5 { id.to_string() } else { format!("\"{id}\"") };
    match sequence.order {
        Some(order) => {
            let steps: Vec<String> = order.iter()
                .map(|(id, runs)| if *runs { label(id) } else { format!("({} off)", label(id)) })
                .collect();
            let repeats: Vec<String> = firsts.iter().filter(|(_, n)| *n > 1)
                .map(|(d, n)| format!("{} ×{n}", d.id.as_deref().map_or(d.display_name.clone(), |id| if sequence.tm5 { format!("Test {id}") } else { label(id) })))
                .collect();
            let extra = if repeats.is_empty() { String::new() } else { format!("; {}", repeats.join(", ")) };
            notes.push(format!("ℹ  Cycle: {} ({} steps{extra})", steps.join(" → "), test_definitions.len()));
        }
        None if unique != test_definitions.len() => {
            notes.push(format!("ℹ  Cycle: {} steps", test_definitions.len()));
        }
        None => {}
    }
    notes.push(match sequence.seal {
        Some(kernel) => format!("🔒 Seal ({}): all memory is sealed at each cycle's start and checked at its end. ✓: the seal is checked \
                                 before every chunk and resealed after. data: the test works on the seal. –: never sealed; what it \
                                 overwrites is checked first and resealed before the next sealed step.", kernel.name()),
        None => "🔓 Seal: off (seal=off)".to_string(),
    });

    TestConfigurationReport {
        suite_timing: suite_timing_str,
        test_count: unique,
        step_count: test_definitions.len(),
        sealed: sequence.seal.is_some(),
        tests,
        notes,
    }
}

/// Convert test summaries to cycle report
#[expect(dead_code, reason = "TODO #29: the per-cycle report is to be revived, not deleted (from TODO #69 F)")]
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
            whea_total: summary.whea_total,
            whea_corrected: summary.whea_corrected,
        }
    }).collect();
    
    CycleReport {
        cycle_number: cycle,
        duration_secs,
        test_performances,
    }
}

/// Create block allocation report from Windows VirtualAlloc2 allocation results. `requested` is
/// the layout the allocator was given, each thread's blocks summing to its target.
pub fn create_block_allocation_report_from_windows(
    allocations: &std::collections::HashMap<usize, Vec<crate::AllocationBlock>>,
    requested: &std::collections::HashMap<usize, Vec<crate::BlockInfo>>,
) -> BlockAllocationReport {
    let allocator_backend = "Windows VirtualAlloc2".to_string();
    
    analyze_block_allocations(allocations, requested, allocator_backend)
}

/// Shared block-allocation analysis, independent of which backend allocated.
fn analyze_block_allocations(
    allocations: &std::collections::HashMap<usize, Vec<crate::AllocationBlock>>,
    requested: &std::collections::HashMap<usize, Vec<crate::BlockInfo>>,
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
        let numa_node = blocks.first().map(|b| b.buffer.info().numa_node).unwrap_or(0);
        
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
        
        let target_bytes = requested.get(&thread_id)
            .map_or(0, |blocks| blocks.iter().map(|b| b.size_bytes as u64).sum());
        thread_allocations.push(ThreadBlockAllocation {
            thread_id,
            cpu_id,
            numa_node,
            target_bytes,
            total_bytes: thread_total_bytes,
            block_sizes: thread_block_sizes,
            page_type_breakdown: thread_page_breakdown,
        });
    }
    thread_allocations.sort_by_key(|t| t.thread_id);
    
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
    block_size_distribution.sort_by_key(|b| std::cmp::Reverse(b.block_size_mb)); // Largest first
    
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
    numa_distribution.sort_by_key(|a| a.node_id);
    
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
    let fairness = calculate_allocation_fairness(&thread_allocations);
    
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
fn calculate_allocation_fairness(threads: &[ThreadBlockAllocation]) -> AllocationFairness {
    if threads.is_empty() {
        return AllocationFairness {
            coefficient_of_variation: 0.0,
            min_allocation_bytes: 0,
            max_allocation_bytes: 0,
            mean_allocation_bytes: 0.0,
            min_huge_pages: 0,
            max_huge_pages: 0,
        };
    }
    
    let allocation_sizes: Vec<u64> = threads.iter().map(|t| t.total_bytes).collect();
    let huge_pages = threads.iter().map(|t| t.page_type_breakdown.huge_pages_count);
    
    let min_allocation = *allocation_sizes.iter().min().unwrap();
    let max_allocation = *allocation_sizes.iter().max().unwrap();
    let mean_allocation = allocation_sizes.iter().sum::<u64>() as f64 / allocation_sizes.len() as f64;
    
    // Calculate coefficient of variation (standard deviation / mean)
    let variance = allocation_sizes.iter()
        .map(|&size| (size as f64 - mean_allocation).powi(2))
        .sum::<f64>() / allocation_sizes.len() as f64;
    let std_dev = variance.sqrt();
    let coefficient_of_variation = if mean_allocation > 0.0 { std_dev / mean_allocation } else { 0.0 };
    
    AllocationFairness {
        coefficient_of_variation,
        min_allocation_bytes: min_allocation,
        max_allocation_bytes: max_allocation,
        mean_allocation_bytes: mean_allocation,
        min_huge_pages: huge_pages.clone().min().unwrap_or(0),
        max_huge_pages: huge_pages.max().unwrap_or(0),
    }
}

/// Convert thread stats to thread timing report
///
/// `whea` is the count attributed to *this test* (the delta across it), not the run total — it is
/// what lets the aggregate row fail when the per-thread rows are all clean.
pub fn create_thread_timing_report(
    stats: &[(usize, usize, u64, u128, u64, u64, u32)],  // (thread_id, cpu_id, bytes, elapsed_ms, errors, operations, cycles_completed)
    whea: crate::whea::WheaCounts,
) -> super::models::ThreadTimingReport {
    use super::models::{ThreadTimingReport, ThreadTiming};

    if stats.is_empty() {
        return ThreadTimingReport {
            average_elapsed_ms: 0,
            thread_timings: Vec::new(),
            whea,
        };
    }

    // Calculate averages
    let total_elapsed: u128 = stats.iter().map(|(_, _, _, elapsed, _, _, _)| *elapsed).sum();
    let avg_elapsed = total_elapsed / stats.len() as u128;

    let total_bytes: u64 = stats.iter().map(|(_, _, bytes, _, _, _, _)| *bytes).sum();
    let avg_bytes = total_bytes / stats.len() as u64;

    // Build thread timing entries with deviations
    let mut thread_timings: Vec<ThreadTiming> = stats.iter().map(|&(thread_id, cpu_id, bytes, elapsed, errors, _operations, cycles_completed)| {
        let deviation_ms = elapsed as i128 - avg_elapsed as i128;
        let deviation_data_bytes = bytes as i64 - avg_bytes as i64;

        // Calculate throughput
        let throughput_mib_s = if elapsed > 0 {
            (bytes as f64 / (1024.0 * 1024.0)) / (elapsed as f64 / 1000.0)
        } else {
            0.0
        };

        // Get physical core and NUMA node info from the real detected topology
        // (NOT cpu_id/2 — that hardcoded 2-way SMT and mislabeled cores on non-SMT CPUs).
        let physical_core_id = crate::cpu_topology::get_physical_core_for_cpu(cpu_id);
        let numa_node = crate::cpu_topology::get_numa_node_for_cpu(cpu_id);

        ThreadTiming {
            thread_id,
            cpu_id,
            physical_core_id,
            numa_node,
            runtime_ms: elapsed,
            deviation_ms,
            data_bytes: bytes,
            deviation_data_bytes,
            throughput_mib_s,
            deviation_speed_mib_s: 0.0, // Will calculate after we know avg speed
            errors,
            cycles_completed,
        }
    }).collect();

    // Calculate average speed and then update speed deviations
    let avg_speed_mib_s = thread_timings.iter()
        .map(|t| t.throughput_mib_s)
        .sum::<f64>() / thread_timings.len() as f64;

    for timing in &mut thread_timings {
        timing.deviation_speed_mib_s = timing.throughput_mib_s - avg_speed_mib_s;
    }

    // Sort by deviation descending (highest deviation first)
    thread_timings.sort_by_key(|b| std::cmp::Reverse(b.deviation_ms));

    ThreadTimingReport {
        average_elapsed_ms: avg_elapsed,
        thread_timings,
        whea,
    }
}

/// The final summary (TODO 74): one row per test, its steps combined (a test that runs twice a
/// cycle is one row with twice the runs), the seal's stages, and the steps that found errors.
pub fn create_overall_stats_summary_report(
    result: &crate::results::TestRunResult,
) -> super::models::FinalTestSummaryReport {
    use super::models::{FinalTestSummaryReport, SealStagesSummary, StepErrorsEntry, TestSummaryEntry};
    let overall_stats = &result.overall_stats;

    // Format runtime in fixed HH:MM:SS format for machine comparison
    let secs = overall_stats.total_runtime_secs;
    let hours = secs / 3600;
    let minutes = (secs % 3600) / 60;
    let seconds = secs % 60;
    let total_runtime = format!("{:02}:{:02}:{:02}", hours, minutes, seconds);

    // Steps of one test, combined: times and data per run, errors summed. A step's figures are
    // already per run (`TestAverage`), so the test's are its steps' run-weighted means.
    let mut per_test_summaries: Vec<TestSummaryEntry> = Vec::new();
    let mut seen: Vec<usize> = Vec::new();
    for avg in &overall_stats.per_step_averages {
        let label = match &avg.id {
            Some(_) => avg.label.clone(),
            None => avg.name.clone(),
        };
        let at = match seen.iter().position(|&t| t == avg.test_index) {
            Some(at) => at,
            None => {
                seen.push(avg.test_index);
                per_test_summaries.push(TestSummaryEntry {
                    name: label,
                    runs: 0,
                    seal_errors: 0,
                    average_duration_secs: 0.0,
                    average_data_gib: 0.0,
                    average_throughput_mib_s: 0.0,
                    total_errors: 0,
                    whea_total: 0,
                    whea_corrected: 0,
                    // Copy latency data from the test's first step
                    latency_samples: avg.latency_samples,
                    latency_p5_ns: avg.latency_p5_ns,
                    latency_p10_ns: avg.latency_p10_ns,
                    latency_p25_ns: avg.latency_p25_ns,
                    latency_p50_ns: avg.latency_p50_ns,
                    latency_p75_ns: avg.latency_p75_ns,
                    latency_p90_ns: avg.latency_p90_ns,
                    latency_p95_ns: avg.latency_p95_ns,
                    latency_p99_ns: avg.latency_p99_ns,
                    latency_p99_9_ns: avg.latency_p99_9_ns,
                    latency_spread: avg.latency_spread,
                });
                per_test_summaries.len() - 1
            }
        };
        let entry = &mut per_test_summaries[at];
        let (old, new) = (entry.runs as f64, avg.runs as f64);
        let mean = |a: f64, b: f64| (a * old + b * new) / (old + new);
        entry.average_duration_secs = mean(entry.average_duration_secs, avg.avg_duration_ms as f64 / 1000.0);
        entry.average_data_gib = mean(entry.average_data_gib, avg.avg_bytes_processed as f64 / (1024.0 * 1024.0 * 1024.0));
        entry.average_throughput_mib_s = mean(entry.average_throughput_mib_s, avg.avg_throughput_mib_s);
        entry.runs += avg.runs;
        entry.total_errors += avg.total_errors;
        entry.seal_errors += avg.total_seal_errors;
        entry.whea_total += avg.whea_total;
        entry.whea_corrected += avg.whea_corrected;
    }

    let sealed: Vec<crate::results::CycleSeal> = result.cycles.iter().filter_map(|c| c.seal).collect();
    let seal = (!sealed.is_empty()).then(|| SealStagesSummary {
        secs: sealed.iter().map(|s| (s.seal_ms + s.between_steps_ms + s.final_check_ms) as f64 / 1000.0).sum(),
        data_gib: sealed.iter().map(|s| (s.seal_bytes + s.final_check_bytes) as f64).sum::<f64>() / (1024.0 * 1024.0 * 1024.0),
        final_check_errors: sealed.iter().map(|s| s.final_check_errors).sum(),
    });
    let errors_by_step = overall_stats.per_step_averages.iter()
        .filter(|s| s.total_errors + s.total_seal_errors > 0)
        .map(|s| StepErrorsEntry { step: s.step, label: s.label.clone(), errors: s.total_errors, seal_errors: s.total_seal_errors })
        .collect();

    FinalTestSummaryReport {
        total_runtime,
        cycles_completed: overall_stats.cycles_completed as usize,
        total_data_processed_gib: overall_stats.total_data_processed_gib,
        overall_throughput_mib_s: overall_stats.overall_throughput_mib_s,
        overall_throughput_gib_s: overall_stats.overall_throughput_mib_s / 1024.0,
        total_errors: overall_stats.total_errors,
        // Exact run-level counters (see TestRunResult::set_whea_totals) — not a sum of the
        // per-test figures, which miss events logged outside a test's execution window.
        whea_total: overall_stats.whea_total,
        whea_corrected: overall_stats.whea_corrected,
        whea_monitored: overall_stats.whea_monitored,
        per_test_summaries,
        seal,
        errors_by_step,
    }
}
