use crate::ErrorMode;
use crate::constants::MB_F64;
use crate::runner::{AllocationBlock, SHUTDOWN_REQUESTED};
use crate::tests::{TestAction, TestMemoryConfig, TestProgress, TestTiming, TestStats, prepare_blocks_for_window, calculate_ideal_chunk_size, get_safe_chunk_size};
use std::sync::atomic::Ordering;
use std::time::Instant;

/// Extended stats for latency tests including percentiles
#[derive(Debug, Clone)]
pub struct LatencyTestStats {
    /// Basic test stats (for compatibility)
    pub basic_stats: TestStats,
    /// Latency samples in nanoseconds
    pub latencies_ns: Vec<f64>,
    /// Calculated percentiles
    pub avg_ns: f64,
    pub min_ns: f64,
    pub max_ns: f64,
    pub p50_ns: f64,
    pub p80_ns: f64,
    pub p90_ns: f64,
    pub p95_ns: f64,
    pub p99_ns: f64,
}

impl LatencyTestStats {
    fn calculate_percentiles(mut latencies: Vec<f64>) -> Self {
        if latencies.is_empty() {
            return Self {
                basic_stats: TestStats::default(),
                latencies_ns: vec![],
                avg_ns: 0.0,
                min_ns: 0.0,
                max_ns: 0.0,
                p50_ns: 0.0,
                p80_ns: 0.0,
                p90_ns: 0.0,
                p95_ns: 0.0,
                p99_ns: 0.0,
            };
        }

        latencies.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let len = latencies.len();

        let avg = latencies.iter().sum::<f64>() / len as f64;
        let min = latencies[0];
        let max = latencies[len - 1];

        let percentile = |p: f64| -> f64 {
            let idx = ((len as f64 - 1.0) * p / 100.0) as usize;
            latencies[idx.min(len - 1)]
        };

        Self {
            basic_stats: TestStats::default(), // Will be filled by caller
            latencies_ns: latencies.clone(),
            avg_ns: avg,
            min_ns: min,
            max_ns: max,
            p50_ns: percentile(50.0),
            p80_ns: percentile(80.0),
            p90_ns: percentile(90.0),
            p95_ns: percentile(95.0),
            p99_ns: percentile(99.0),
        }
    }
}

/// Node structure for pointer chasing
#[repr(C, align(64))]
struct LatencyNode {
    next: *mut LatencyNode,
    value: u64,
    _padding: [u8; 48], // Ensure 64-byte cache line alignment
}

/// Get CPU frequency in GHz for cycle-to-nanosecond conversion
#[cfg(target_arch = "x86_64")]
unsafe fn get_cpu_frequency_ghz() -> f64 {
    use std::arch::x86_64::_rdtsc;

    // Measure over 100ms for accuracy
    let start_tsc = _rdtsc();
    let start_time = Instant::now();
    std::thread::sleep(std::time::Duration::from_millis(100));
    let end_tsc = _rdtsc();
    let elapsed_ns = start_time.elapsed().as_nanos() as f64;

    let cycles = (end_tsc - start_tsc) as f64;
    cycles / elapsed_ns // GHz = cycles/nanosecond
}

#[cfg(not(target_arch = "x86_64"))]
unsafe fn get_cpu_frequency_ghz() -> f64 {
    // Fallback: assume 3.0 GHz
    3.0
}

/// Raw Read Latency Test - Uses pointer chasing for true latency measurement
pub unsafe fn read_latency_multi(
    blocks: &[AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> LatencyTestStats {
    let test_name = "ReadLatency";
    let start = Instant::now();

    // Get CPU frequency for cycle conversion
    let cpu_ghz = get_cpu_frequency_ghz();

    // Calculate window and prepare blocks
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

    // Setup pointer chain for each block
    for test_block in test_blocks.iter() {
        let base = test_block.block.buffer.as_mut_ptr() as *mut LatencyNode;
        let node_count = test_block.test_size / std::mem::size_of::<LatencyNode>();

        // Create random access pattern to defeat prefetcher
        let mut indices: Vec<usize> = (0..node_count).collect();
        // Simple shuffle using XorShift RNG
        let mut rng_state = 0x123456789ABCDEFu64.wrapping_add(thread_id as u64);
        for i in (1..node_count).rev() {
            rng_state ^= rng_state << 13;
            rng_state ^= rng_state >> 17;
            rng_state ^= rng_state << 5;
            let j = (rng_state as usize) % (i + 1);
            indices.swap(i, j);
        }

        // Link nodes in random order
        for i in 0..node_count - 1 {
            let current = base.add(indices[i]);
            let next = base.add(indices[i + 1]);
            (*current).next = next;
            (*current).value = i as u64;
        }
        // Loop back to start
        let last = base.add(indices[node_count - 1]);
        (*last).next = base.add(indices[0]);
        (*last).value = node_count as u64;
    }

    // Main measurement loop
    loop {
        cycle += 1;

        for test_block in test_blocks.iter() {
            let base = test_block.block.buffer.as_mut_ptr() as *mut LatencyNode;
            let node_count = test_block.test_size / std::mem::size_of::<LatencyNode>();

            // Warmup pass to populate TLB
            let mut current = base;
            for _ in 0..node_count.min(1000) {
                current = (*current).next;
            }

            // Measurement passes with different stride patterns
            let strides = [1, 8, 64, 512, 4096]; // Different access patterns

            for &stride in &strides {
                let iterations = (node_count / stride).min(1000);

                // Measure pointer chasing latency using high-precision timing
                std::sync::atomic::fence(Ordering::SeqCst);
                let start_time = Instant::now();

                let mut ptr = base;
                for _ in 0..iterations {
                    ptr = (*ptr).next;
                }

                let elapsed = start_time.elapsed();
                std::sync::atomic::fence(Ordering::SeqCst);

                let latency_ns = elapsed.as_nanos() as f64 / iterations as f64;

                latencies_ns.push(latency_ns);
                total_bytes_processed += iterations * 8; // Each read is 8 bytes
            }

            // Check for shutdown
            if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
                break;
            }
        }

        // Check timing
        let elapsed_secs = start.elapsed().as_secs() as u32;
        if !timing.should_continue(cycle, elapsed_secs) {
            break;
        }
    }

    // Calculate statistics
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

    log::info!("[Thread {}] {} Latency - Avg: {:.1}ns, P50: {:.1}ns, P90: {:.1}ns, P99: {:.1}ns",
              thread_id, test_name, result.avg_ns, result.p50_ns, result.p90_ns, result.p99_ns);

    result
}

/// Raw Write Latency Test - Measures store completion latency
pub unsafe fn write_latency_multi(
    blocks: &[AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> LatencyTestStats {
    let test_name = "WriteLatency";
    let start = Instant::now();

    let cpu_ghz = get_cpu_frequency_ghz();

    // Calculate window and prepare blocks
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

    // Main measurement loop
    loop {
        cycle += 1;

        for test_block in test_blocks.iter() {
            let base = test_block.block.buffer.as_mut_ptr() as *mut u64;
            let len = test_block.test_size / std::mem::size_of::<u64>();

            // Test different stride patterns
            let strides = [1, 8, 64, 512, 4096];

            for &stride in &strides {
                let iterations = (len / stride).min(1000);

                // Create dependency chain for writes
                let mut value = thread_id as u64;

                std::sync::atomic::fence(Ordering::SeqCst);
                let start_time = Instant::now();

                for i in 0..iterations {
                    let idx = (i * stride) % len;

                    // Write with dependency to prevent reordering
                    *base.add(idx) = value;
                    std::sync::atomic::fence(Ordering::SeqCst); // Ensure write completes

                    // Create dependency for next write
                    value = value.wrapping_mul(0x123456789ABCDEF).wrapping_add(idx as u64);
                }

                let elapsed = start_time.elapsed();
                std::sync::atomic::fence(Ordering::SeqCst);

                let latency_ns = elapsed.as_nanos() as f64 / iterations as f64;

                latencies_ns.push(latency_ns);
                total_bytes_processed += iterations * 8;
            }

            // Check for shutdown
            if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
                break;
            }
        }

        // Check timing
        let elapsed_secs = start.elapsed().as_secs() as u32;
        if !timing.should_continue(cycle, elapsed_secs) {
            break;
        }
    }

    // Calculate statistics
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

    log::info!("[Thread {}] {} Latency - Avg: {:.1}ns, P50: {:.1}ns, P90: {:.1}ns, P99: {:.1}ns",
              thread_id, test_name, result.avg_ns, result.p50_ns, result.p90_ns, result.p99_ns);

    result
}

/// Copy Latency Test - Measures combined read+write latency
pub unsafe fn copy_latency_multi(
    blocks: &[AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> LatencyTestStats {
    let test_name = "CopyLatency";
    let start = Instant::now();

    let cpu_ghz = get_cpu_frequency_ghz();

    // Calculate window and prepare blocks
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

    // Initialize source data
    for test_block in test_blocks.iter() {
        let base = test_block.block.buffer.as_mut_ptr() as *mut u64;
        let len = test_block.test_size / std::mem::size_of::<u64>();

        for i in 0..len/2 {
            *base.add(i) = i as u64 ^ 0xDEADBEEF;
        }
    }

    // Main measurement loop
    loop {
        cycle += 1;

        for test_block in test_blocks.iter() {
            let base = test_block.block.buffer.as_mut_ptr() as *mut u64;
            let len = test_block.test_size / std::mem::size_of::<u64>();
            let half_len = len / 2;

            // Test different stride patterns
            let strides = [1, 8, 64, 512, 4096];

            for &stride in &strides {
                let iterations = (half_len / stride).min(1000);

                _mm_mfence();
                let mut aux = 0u32;
                let start_cycles = __rdtscp(&mut aux);

                for i in 0..iterations {
                    let idx = (i * stride) % half_len;

                    // Copy operation: Read from first half, write to second half
                    let value = *base.add(idx);
                    _mm_mfence(); // Ensure read completes
                    *base.add(half_len + idx) = value;
                    _mm_mfence(); // Ensure write completes
                }

                let end_cycles = __rdtscp(&mut aux);
                _mm_mfence();

                let cycles_per_copy = (end_cycles - start_cycles) as f64 / iterations as f64;
                let latency_ns = cycles_per_copy / cpu_ghz;

                latencies_ns.push(latency_ns);
                total_bytes_processed += iterations * 16; // Read 8 + Write 8
            }

            // Check for shutdown
            if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
                break;
            }
        }

        // Check timing
        let elapsed_secs = start.elapsed().as_secs() as u32;
        if !timing.should_continue(cycle, elapsed_secs) {
            break;
        }
    }

    // Calculate statistics
    let mut result = LatencyTestStats::calculate_percentiles(latencies_ns);
    result.basic_stats = TestStats {
        name: test_name,
        action: TestAction::Copy,
        bytes_processed: total_bytes_processed,
        elapsed_ms: start.elapsed().as_millis(),
        thread_id,
        error_count: 0,
        total_operations: total_bytes_processed as u64 / 16, // Each copy is read+write
        cycles_completed: cycle,
        cycles_planned: timing.cycles,
        stopped_by_time_limit: timing.cycles.map_or(false, |limit| cycle < limit),
    };

    log::info!("[Thread {}] {} Latency - Avg: {:.1}ns, P50: {:.1}ns, P90: {:.1}ns, P99: {:.1}ns",
              thread_id, test_name, result.avg_ns, result.p50_ns, result.p90_ns, result.p99_ns);

    result
}