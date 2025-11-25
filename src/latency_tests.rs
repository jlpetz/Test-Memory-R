// This file contains the redesigned latency tests using pointer chasing
// to defeat prefetchers and ensure accurate latency measurements

use crate::ErrorMode;
use crate::runner::{AllocationBlock, SHUTDOWN_REQUESTED};
use crate::tests::{TestAction, TestMemoryConfig, TestProgress, TestTiming, TestStats, prepare_blocks_for_window};
use std::sync::atomic::{fence, Ordering};
use std::time::Instant;

#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::__rdtscp;

/// Extended stats for latency tests including percentiles

#[derive(Debug, Clone)]
pub struct LatencyTestStats {
    pub basic_stats: TestStats,
    pub latencies_ns: Vec<f64>,
    pub sample_count: usize,
    pub avg_ns: f64,

    // Full percentile breakdown
    pub p1_ns: f64,     // 1st percentile
    pub p5_ns: f64,     // 5th percentile
    pub p10_ns: f64,    // 10th percentile
    pub p25_ns: f64,    // Lower quartile
    pub p50_ns: f64,    // Median
    pub p75_ns: f64,    // Upper quartile
    pub p90_ns: f64,    // 90th percentile
    pub p95_ns: f64,    // 95th percentile
    pub p99_ns: f64,    // 99th percentile
    pub p99_9_ns: f64,  // 99.9th percentile

    // Spread ratio (P95/P5) - high values suggest cache level spills
    pub spread_ratio: f64,
}

impl LatencyTestStats {
    fn calculate_percentiles(mut latencies: Vec<f64>) -> Self {
        if latencies.is_empty() {
            return Self {
                basic_stats: TestStats {
                    name: "",
                    action: TestAction::Latency,
                    bytes_processed: 0,
                    elapsed_ms: 0,
                    thread_id: 0,
                    error_count: 0,
                    total_operations: 0,
                    cycles_completed: 0,
                    cycles_planned: None,
                    stopped_by_time_limit: false,
                },
                latencies_ns: vec![],
                sample_count: 0,
                avg_ns: 0.0,
                p1_ns: 0.0,
                p5_ns: 0.0,
                p10_ns: 0.0,
                p25_ns: 0.0,
                p50_ns: 0.0,
                p75_ns: 0.0,
                p90_ns: 0.0,
                p95_ns: 0.0,
                p99_ns: 0.0,
                p99_9_ns: 0.0,
                spread_ratio: 0.0,
            };
        }

        latencies.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let len = latencies.len();

        let avg = latencies.iter().sum::<f64>() / len as f64;

        let percentile = |p: f64| -> f64 {
            let idx = ((len as f64 - 1.0) * p / 100.0) as usize;
            latencies[idx.min(len - 1)]
        };

        let p1 = percentile(1.0);
        let p5 = percentile(5.0);
        let p10 = percentile(10.0);
        let p25 = percentile(25.0);
        let p50 = percentile(50.0);
        let p75 = percentile(75.0);
        let p90 = percentile(90.0);
        let p95 = percentile(95.0);
        let p99 = percentile(99.0);
        let p99_9 = percentile(99.9);

        // Spread ratio: high value suggests mixed cache levels / spills
        let spread_ratio = if p5 > 0.0 { p95 / p5 } else { 0.0 };

        Self {
            basic_stats: TestStats {
                name: "",
                action: TestAction::Latency,
                bytes_processed: 0,
                elapsed_ms: 0,
                thread_id: 0,
                error_count: 0,
                total_operations: 0,
                cycles_completed: 0,
                cycles_planned: None,
                stopped_by_time_limit: false,
            },
            latencies_ns: latencies,
            sample_count: len,
            avg_ns: avg,
            p1_ns: p1,
            p5_ns: p5,
            p10_ns: p10,
            p25_ns: p25,
            p50_ns: p50,
            p75_ns: p75,
            p90_ns: p90,
            p95_ns: p95,
            p99_ns: p99,
            p99_9_ns: p99_9,
            spread_ratio,
        }
    }
}


/// Setup random pointer-chasing pattern in memory
/// Each u64 location stores the address (as usize cast to u64) of the next random location
/// This creates a random walk through memory that defeats prefetchers
unsafe fn setup_pointer_chase(base: *mut u64, len: usize, thread_id: usize) {
    // Create array of indices
    let mut indices: Vec<usize> = (0..len).collect();

    // Fisher-Yates shuffle using XorShift RNG
    let mut rng_state = 0x123456789ABCDEFu64.wrapping_add(thread_id as u64);
    for i in (1..len).rev() {
        rng_state ^= rng_state << 13;
        rng_state ^= rng_state >> 17;
        rng_state ^= rng_state << 5;
        let j = (rng_state as usize) % (i + 1);
        indices.swap(i, j);
    }

    // Link: each location points to next in shuffled order
    for i in 0..len - 1 {
        let current_addr = base.add(indices[i]);
        let next_addr = base.add(indices[i + 1]) as usize as u64;
        *current_addr = next_addr;
    }

    // Loop back to start
    let last_addr = base.add(indices[len - 1]);
    let first_addr = base.add(indices[0]) as usize as u64;
    *last_addr = first_addr;
}

/// Read Latency Test - Uses pointer chasing for true random-access latency
pub unsafe fn read_latency_multi(
    blocks: &[AllocationBlock],
    thread_id: usize,
    _error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    _progress: Option<&TestProgress>,
) -> LatencyTestStats {
    let test_name = "ReadLatency";
    let start = Instant::now();

    // Use TSC frequency detected at startup
    let cpu_ghz = config.tsc_frequency_ghz;
    if cpu_ghz == 0.0 {
        panic!("TSC frequency not detected! Cannot run latency tests on non-x86_64 platforms.");
    }

    let total_allocated: usize = blocks.iter().map(|b| b.buffer.size()).sum();
    let window_size = config.calculate_window_size(test_name, total_allocated);
    let test_blocks = prepare_blocks_for_window(blocks, window_size, test_name);

    if test_blocks.is_empty() {
        return LatencyTestStats {
            basic_stats: TestStats {
                name: test_name,
                action: TestAction::Read,
                bytes_processed: 0,
                elapsed_ms: 0,
                thread_id,
                error_count: 0,
                total_operations: 0,
                cycles_completed: 0,
                cycles_planned: timing.cycles,
                stopped_by_time_limit: false,
            },
            ..LatencyTestStats::calculate_percentiles(vec![])
        };
    }

    let mut latencies_ns = Vec::with_capacity(10000);
    let mut total_bytes_processed = 0usize;
    let mut cycle = 0u32;

    // Use window size to determine working set - this allows targeting different cache levels:
    // - L1 Data: ~16-32KB window
    // - L2 Cache: ~512KB-1MB window
    // - L3 Cache: ~8-16MB window
    // - DRAM: 4-8x L3 size window (64-128MB typical for 16MB L3)
    let working_set_bytes = window_size;
    let working_set_u64 = working_set_bytes / std::mem::size_of::<u64>();
    let iterations_per_sample = 1000usize; // Fixed iterations for consistent measurements

    log::info!("[Thread {}] {} - Working set: {} bytes ({} elements) targeting {}",
        thread_id, test_name, working_set_bytes, working_set_u64,
        if working_set_bytes <= 32 * 1024 { "L1 cache" }
        else if working_set_bytes <= 1024 * 1024 { "L2 cache" }
        else if working_set_bytes <= 16 * 1024 * 1024 { "L3 cache" }
        else { "DRAM" });

    // Setup pointer-chasing pattern for each block (limited to working set size)
    // Also initialize starting positions for continuous traversal
    let mut chain_positions: Vec<*mut u64> = Vec::new();
    for test_block in test_blocks.iter() {
        let base = test_block.block.buffer.as_mut_ptr() as *mut u64;
        let block_len = test_block.test_size / std::mem::size_of::<u64>();
        let len = block_len.min(working_set_u64);
        setup_pointer_chase(base, len, thread_id);
        chain_positions.push(base); // Start at beginning
    }

    // Main measurement loop
    loop {
        cycle += 1;

        for (block_idx, test_block) in test_blocks.iter().enumerate() {
            let block_len = test_block.test_size / std::mem::size_of::<u64>();
            let len = block_len.min(working_set_u64);
            let iterations = len.min(iterations_per_sample);

            fence(Ordering::SeqCst);
            let mut aux = 0u32;
            let start_cycles = __rdtscp(&mut aux);

            // Pointer chase - CONTINUE from where we left off (don't restart!)
            // This ensures we traverse the entire working set, not just cached subset
            let mut ptr = chain_positions[block_idx];
            for _ in 0..iterations {
                let addr = *ptr;  // Read address of next location
                ptr = addr as *mut u64;  // Jump to that location
            }

            let end_cycles = __rdtscp(&mut aux);
            fence(Ordering::SeqCst);

            // Save position for next sample - continue traversing the chain
            chain_positions[block_idx] = ptr;

            // Use result to prevent optimization
            std::hint::black_box(ptr);

            let delta_cycles = end_cycles - start_cycles;
            let cycles_per_read = delta_cycles as f64 / iterations as f64;
            let latency_ns = cycles_per_read / cpu_ghz;

            if latencies_ns.len() < 10 {
                log::debug!("[Thread {}] {} sample #{}: start={} end={} delta={} iter={} cycles/op={:.2} freq={:.3}GHz latency={:.2}ns",
                    thread_id, test_name, latencies_ns.len() + 1, start_cycles, end_cycles, delta_cycles, iterations, cycles_per_read, cpu_ghz, latency_ns);
            }

            latencies_ns.push(latency_ns);
            total_bytes_processed += iterations * 8;

            if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
                break;
            }
        }

        let elapsed_secs = start.elapsed().as_secs() as u32;
        if !timing.should_continue(cycle, elapsed_secs) {
            break;
        }
    }

    let mut result = LatencyTestStats::calculate_percentiles(latencies_ns);
    result.basic_stats = TestStats {
        name: test_name,
        action: TestAction::Read,
        bytes_processed: total_bytes_processed,
        elapsed_ms: start.elapsed().as_millis(),
        thread_id,
        error_count: 0,
        total_operations: total_bytes_processed as u64 / 8,
        cycles_completed: cycle,
        cycles_planned: timing.cycles,
        stopped_by_time_limit: timing.cycles.map_or(false, |limit| cycle < limit),
    };

    // Log per-thread results with all percentiles
    log::info!("[Thread {}] {} - {} samples | P1={:.1} P5={:.1} P10={:.1} P25={:.1} P50={:.1} P75={:.1} P90={:.1} P95={:.1} P99={:.1} P99.9={:.1} | Spread={:.2}x",
        thread_id, test_name, result.sample_count,
        result.p1_ns, result.p5_ns, result.p10_ns, result.p25_ns, result.p50_ns,
        result.p75_ns, result.p90_ns, result.p95_ns, result.p99_ns, result.p99_9_ns,
        result.spread_ratio);

    result
}

/// Write Latency Test - Uses pointer chasing for addresses, writes simple values
pub unsafe fn write_latency_multi(
    blocks: &[AllocationBlock],
    thread_id: usize,
    _error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    _progress: Option<&TestProgress>,
) -> LatencyTestStats {
    let test_name = "WriteLatency";
    let start = Instant::now();

    // Use TSC frequency detected at startup
    let cpu_ghz = config.tsc_frequency_ghz;
    if cpu_ghz == 0.0 {
        panic!("TSC frequency not detected! Cannot run latency tests on non-x86_64 platforms.");
    }

    let total_allocated: usize = blocks.iter().map(|b| b.buffer.size()).sum();
    let window_size = config.calculate_window_size(test_name, total_allocated);
    let test_blocks = prepare_blocks_for_window(blocks, window_size, test_name);

    if test_blocks.is_empty() {
        return LatencyTestStats {
            basic_stats: TestStats {
                name: test_name,
                action: TestAction::Write,
                bytes_processed: 0,
                elapsed_ms: 0,
                thread_id,
                error_count: 0,
                total_operations: 0,
                cycles_completed: 0,
                cycles_planned: timing.cycles,
                stopped_by_time_limit: false,
            },
            ..LatencyTestStats::calculate_percentiles(vec![])
        };
    }

    let mut latencies_ns = Vec::with_capacity(10000);
    let mut total_bytes_processed = 0usize;
    let mut cycle = 0u32;

    // Use window size to determine working set - allows targeting different cache levels
    // For write test: split into two halves - pointer chain (read-only) and write targets
    // This prevents destroying the pointer chain when we write
    let working_set_bytes = window_size;
    let half_working_set_u64 = (working_set_bytes / std::mem::size_of::<u64>()) / 2;
    let iterations_per_sample = 1000usize;

    log::info!("[Thread {}] {} - Working set: {} bytes ({} elements: {} chain + {} write) targeting {}",
        thread_id, test_name, working_set_bytes, half_working_set_u64 * 2, half_working_set_u64, half_working_set_u64,
        if working_set_bytes <= 32 * 1024 { "L1 cache" }
        else if working_set_bytes <= 1024 * 1024 { "L2 cache" }
        else if working_set_bytes <= 16 * 1024 * 1024 { "L3 cache" }
        else { "DRAM" });

    // Setup pointer-chasing pattern in first half (chain region - read only)
    // Second half is used for writes so we don't destroy the chain
    // Also initialize starting positions for continuous traversal
    let mut chain_positions: Vec<*mut u64> = Vec::new();
    let mut chain_bases: Vec<*mut u64> = Vec::new();
    let mut write_bases: Vec<*mut u64> = Vec::new();
    for test_block in test_blocks.iter() {
        let base = test_block.block.buffer.as_mut_ptr() as *mut u64;
        let block_len = test_block.test_size / std::mem::size_of::<u64>();
        let len = (block_len / 2).min(half_working_set_u64);
        setup_pointer_chase(base, len, thread_id);
        chain_positions.push(base); // Start at beginning
        chain_bases.push(base);
        write_bases.push(base.add(len));
    }

    // Main measurement loop
    loop {
        cycle += 1;

        for (block_idx, test_block) in test_blocks.iter().enumerate() {
            let block_len = test_block.test_size / std::mem::size_of::<u64>();
            let chain_len = (block_len / 2).min(half_working_set_u64);
            let iterations = chain_len.min(iterations_per_sample);

            // First half = pointer chain (read-only), Second half = write targets
            let chain_base = chain_bases[block_idx];
            let write_base = write_bases[block_idx];

            fence(Ordering::SeqCst);
            let mut aux = 0u32;
            let start_cycles = __rdtscp(&mut aux);

            // Follow pointer chain to get random offsets, write to parallel region
            // CONTINUE from where we left off (don't restart!)
            let mut chain_ptr = chain_positions[block_idx];
            for i in 0..iterations {
                let next_chain_addr = *chain_ptr;  // Read next from chain (doesn't modify chain)
                // Calculate offset from chain position, write to parallel location
                let offset = (chain_ptr as usize - chain_base as usize) / 8;
                let write_ptr = write_base.add(offset);
                *write_ptr = i as u64;  // Write to parallel location (random address)
                chain_ptr = next_chain_addr as *mut u64;  // Move to next in chain
            }

            let end_cycles = __rdtscp(&mut aux);
            fence(Ordering::SeqCst);

            // Save position for next sample
            chain_positions[block_idx] = chain_ptr;

            std::hint::black_box(chain_ptr);

            let delta_cycles = end_cycles - start_cycles;
            let cycles_per_write = delta_cycles as f64 / iterations as f64;
            let latency_ns = cycles_per_write / cpu_ghz;

            if latencies_ns.len() < 10 {
                log::debug!("[Thread {}] {} sample #{}: start={} end={} delta={} iter={} cycles/op={:.2} freq={:.3}GHz latency={:.2}ns",
                    thread_id, test_name, latencies_ns.len() + 1, start_cycles, end_cycles, delta_cycles, iterations, cycles_per_write, cpu_ghz, latency_ns);
            }

            latencies_ns.push(latency_ns);
            total_bytes_processed += iterations * 8;

            if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
                break;
            }
        }

        let elapsed_secs = start.elapsed().as_secs() as u32;
        if !timing.should_continue(cycle, elapsed_secs) {
            break;
        }
    }

    let mut result = LatencyTestStats::calculate_percentiles(latencies_ns);
    result.basic_stats = TestStats {
        name: test_name,
        action: TestAction::Write,
        bytes_processed: total_bytes_processed,
        elapsed_ms: start.elapsed().as_millis(),
        thread_id,
        error_count: 0,
        total_operations: total_bytes_processed as u64 / 8,
        cycles_completed: cycle,
        cycles_planned: timing.cycles,
        stopped_by_time_limit: timing.cycles.map_or(false, |limit| cycle < limit),
    };

    // Log per-thread results with all percentiles
    log::info!("[Thread {}] {} - {} samples | P1={:.1} P5={:.1} P10={:.1} P25={:.1} P50={:.1} P75={:.1} P90={:.1} P95={:.1} P99={:.1} P99.9={:.1} | Spread={:.2}x",
        thread_id, test_name, result.sample_count,
        result.p1_ns, result.p5_ns, result.p10_ns, result.p25_ns, result.p50_ns,
        result.p75_ns, result.p90_ns, result.p95_ns, result.p99_ns, result.p99_9_ns,
        result.spread_ratio);

    result
}

/// Copy Latency Test - Uses pointer chasing for source addresses
pub unsafe fn copy_latency_multi(
    blocks: &[AllocationBlock],
    thread_id: usize,
    _error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    _progress: Option<&TestProgress>,
) -> LatencyTestStats {
    let test_name = "CopyLatency";
    let start = Instant::now();

    // Use TSC frequency detected at startup
    let cpu_ghz = config.tsc_frequency_ghz;
    if cpu_ghz == 0.0 {
        panic!("TSC frequency not detected! Cannot run latency tests on non-x86_64 platforms.");
    }

    let total_allocated: usize = blocks.iter().map(|b| b.buffer.size()).sum();
    let window_size = config.calculate_window_size(test_name, total_allocated);
    let test_blocks = prepare_blocks_for_window(blocks, window_size, test_name);

    if test_blocks.is_empty() {
        return LatencyTestStats {
            basic_stats: TestStats {
                name: test_name,
                action: TestAction::Copy,
                bytes_processed: 0,
                elapsed_ms: 0,
                thread_id,
                error_count: 0,
                total_operations: 0,
                cycles_completed: 0,
                cycles_planned: timing.cycles,
                stopped_by_time_limit: false,
            },
            ..LatencyTestStats::calculate_percentiles(vec![])
        };
    }

    let mut latencies_ns = Vec::with_capacity(10000);
    let mut total_bytes_processed = 0usize;
    let mut cycle = 0u32;

    // Use window size to determine working set - allows targeting different cache levels
    // For copy, we use half for source and half for destination
    let working_set_bytes = window_size;
    let working_set_u64 = (working_set_bytes / std::mem::size_of::<u64>()) / 2; // Half for src, half for dst
    let iterations_per_sample = 1000usize;

    log::info!("[Thread {}] {} - Working set: {} bytes ({} elements src + dst) targeting {}",
        thread_id, test_name, working_set_bytes, working_set_u64 * 2,
        if working_set_bytes <= 32 * 1024 { "L1 cache" }
        else if working_set_bytes <= 1024 * 1024 { "L2 cache" }
        else if working_set_bytes <= 16 * 1024 * 1024 { "L3 cache" }
        else { "DRAM" });

    // Setup pointer-chasing pattern in first half (source)
    // Also initialize starting positions for continuous traversal
    let mut chain_positions: Vec<*mut u64> = Vec::new();
    let mut dst_bases: Vec<*mut u64> = Vec::new();
    for test_block in test_blocks.iter() {
        let base = test_block.block.buffer.as_mut_ptr() as *mut u64;
        let block_len = test_block.test_size / std::mem::size_of::<u64>();
        let half_len = (block_len / 2).min(working_set_u64);
        setup_pointer_chase(base, half_len, thread_id);
        chain_positions.push(base); // Start at beginning
        dst_bases.push(base.add(half_len));
    }

    // Track destination position separately (wraps around)
    let mut dst_positions: Vec<usize> = vec![0; test_blocks.len()];

    // Main measurement loop
    loop {
        cycle += 1;

        for (block_idx, test_block) in test_blocks.iter().enumerate() {
            let block_len = test_block.test_size / std::mem::size_of::<u64>();
            let half_len = (block_len / 2).min(working_set_u64);
            let iterations = half_len.min(iterations_per_sample);

            let dst_base = dst_bases[block_idx];

            fence(Ordering::SeqCst);
            let mut aux = 0u32;
            let start_cycles = __rdtscp(&mut aux);

            // Use pointer chain for source, write to corresponding destination
            // CONTINUE from where we left off (don't restart!)
            let mut src_ptr = chain_positions[block_idx];
            let mut dst_offset = dst_positions[block_idx];
            for _ in 0..iterations {
                let next_addr = *src_ptr;  // Read next source address
                let value = *src_ptr;  // Read value from source
                let dst_ptr = dst_base.add(dst_offset % half_len);
                *dst_ptr = value;  // Write to destination
                src_ptr = next_addr as *mut u64;  // Move source
                dst_offset += 1;  // Move destination (wrap around)
            }

            let end_cycles = __rdtscp(&mut aux);
            fence(Ordering::SeqCst);

            // Save positions for next sample
            chain_positions[block_idx] = src_ptr;
            dst_positions[block_idx] = dst_offset;

            std::hint::black_box(src_ptr);
            std::hint::black_box(dst_offset);

            let delta_cycles = end_cycles - start_cycles;
            let cycles_per_copy = delta_cycles as f64 / iterations as f64;
            let latency_ns = cycles_per_copy / cpu_ghz;

            if latencies_ns.len() < 10 {
                log::debug!("[Thread {}] {} sample #{}: start={} end={} delta={} iter={} cycles/op={:.2} freq={:.3}GHz latency={:.2}ns",
                    thread_id, test_name, latencies_ns.len() + 1, start_cycles, end_cycles, delta_cycles, iterations, cycles_per_copy, cpu_ghz, latency_ns);
            }

            latencies_ns.push(latency_ns);
            total_bytes_processed += iterations * 16;

            if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
                break;
            }
        }

        let elapsed_secs = start.elapsed().as_secs() as u32;
        if !timing.should_continue(cycle, elapsed_secs) {
            break;
        }
    }

    let mut result = LatencyTestStats::calculate_percentiles(latencies_ns);
    result.basic_stats = TestStats {
        name: test_name,
        action: TestAction::Copy,
        bytes_processed: total_bytes_processed,
        elapsed_ms: start.elapsed().as_millis(),
        thread_id,
        error_count: 0,
        total_operations: total_bytes_processed as u64 / 16,
        cycles_completed: cycle,
        cycles_planned: timing.cycles,
        stopped_by_time_limit: timing.cycles.map_or(false, |limit| cycle < limit),
    };

    // Log per-thread results with all percentiles
    log::info!("[Thread {}] {} - {} samples | P1={:.1} P5={:.1} P10={:.1} P25={:.1} P50={:.1} P75={:.1} P90={:.1} P95={:.1} P99={:.1} P99.9={:.1} | Spread={:.2}x",
        thread_id, test_name, result.sample_count,
        result.p1_ns, result.p5_ns, result.p10_ns, result.p25_ns, result.p50_ns,
        result.p75_ns, result.p90_ns, result.p95_ns, result.p99_ns, result.p99_9_ns,
        result.spread_ratio);

    result
}

/// Analyze spread ratio and return a summary message
/// High spread (>1.5x) suggests spills between cache levels / DRAM
pub fn analyze_spread(spread_ratio: f64) -> String {
    if spread_ratio > 3.0 {
        format!("⚠️ High variability (spread {:.2}x) - likely spilling between cache levels and DRAM", spread_ratio)
    } else if spread_ratio > 2.0 {
        format!("⚠️ Moderate variability (spread {:.2}x) - possible cache level transitions", spread_ratio)
    } else if spread_ratio > 1.5 {
        format!("ℹ️ Some variability (spread {:.2}x) - minor latency fluctuations", spread_ratio)
    } else {
        format!("✅ Consistent latencies (spread {:.2}x) - stable cache/memory access pattern", spread_ratio)
    }
}

/// Print consolidated latency summary for all threads
pub fn print_latency_summary(test_name: &str, results: &[LatencyTestStats]) {
    if results.is_empty() {
        return;
    }

    // Aggregate all latencies across threads
    let mut all_latencies: Vec<f64> = Vec::new();
    for r in results {
        all_latencies.extend(&r.latencies_ns);
    }

    if all_latencies.is_empty() {
        return;
    }

    all_latencies.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let len = all_latencies.len();

    let percentile = |p: f64| -> f64 {
        let idx = ((len as f64 - 1.0) * p / 100.0) as usize;
        all_latencies[idx.min(len - 1)]
    };

    let p5 = percentile(5.0);
    let p50 = percentile(50.0);
    let p95 = percentile(95.0);
    let p99 = percentile(99.0);
    let spread = if p5 > 0.0 { p95 / p5 } else { 0.0 };

    println!("\n📊 {} Summary (all {} threads, {} total samples):",
        test_name, results.len(), len);
    println!("   Median: {:.1}ns | P5-P95 range: {:.1}-{:.1}ns | P99: {:.1}ns",
        p50, p5, p95, p99);
    println!("   {}", analyze_spread(spread));
}

// Wrappers for runner integration
pub unsafe fn read_latency_multi_wrapper(
    blocks: &[AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> TestStats {
    read_latency_multi(blocks, thread_id, error_mode, timing, config, progress).basic_stats
}

pub unsafe fn write_latency_multi_wrapper(
    blocks: &[AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> TestStats {
    write_latency_multi(blocks, thread_id, error_mode, timing, config, progress).basic_stats
}

pub unsafe fn copy_latency_multi_wrapper(
    blocks: &[AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> TestStats {
    copy_latency_multi(blocks, thread_id, error_mode, timing, config, progress).basic_stats
}

// Note: run_cache_hierarchy_diagnostic() was removed - --cache-latency now uses
// the same ThreadPool infrastructure as --latency-test with cpus=1 for single-thread mode
