// This file contains the redesigned latency tests using pointer chasing
// to defeat prefetchers and ensure accurate latency measurements

use crate::ErrorMode;
use crate::runner::AllocationBlock;
use crate::tests::{TestAction, TestMemoryConfig, TestProgress, TestTiming, TestStats};
use crate::test_scaffolding::TestRunner;
use std::sync::atomic::{fence, Ordering};

#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::__rdtscp;

/// Extended stats for latency tests including percentiles

#[derive(Debug, Clone)]
pub struct LatencyTestStats {
    pub basic_stats: TestStats,
    pub latencies_ns: Vec<f64>,
    pub sample_count: usize,

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


/// One step of the setup RNG (xorshift64), mapped onto `[0, n)` by multiply-high: no division.
#[inline(always)]
pub(crate) fn below(rng: &mut u64, n: usize) -> usize {
    *rng ^= *rng << 13;
    *rng ^= *rng >> 17;
    *rng ^= *rng << 5;
    ((*rng as u128 * n as u128) >> 64) as usize
}

/// A random pointer chase through the `n` u64 slots at `base`: each slot holds the address of
/// the next, and the chain is one cycle through all `n`, so a chase from any slot visits every
/// slot before it repeats, and no prefetcher can follow it. Sattolo's algorithm in insertion form:
/// slot i goes in after a random earlier slot. Every one of the (n-1)! single cycles is equally
/// likely, as with the old shuffle-then-link, but in place, in one ascending pass, with no heap
/// (TODO 76: the heap index was as big as the extent).
pub(crate) unsafe fn build_cycle_u64(base: *mut u64, n: usize, seed: u64) {
    if n == 0 {
        return;
    }
    let mut rng = 0x123456789ABCDEFu64.wrapping_add(seed);
    *base = base as u64;
    for i in 1..n {
        let earlier = base.add(below(&mut rng, i));
        *base.add(i) = *earlier;
        *earlier = base.add(i) as u64;
    }
}

/// Read Latency Test - Uses pointer chasing for true random-access latency
#[doc = include_str!("test_fn_safety.md")]
pub unsafe fn read_latency_multi(
    blocks: &[AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> LatencyTestStats {
    let test_name = "ReadLatency";
    // Use TSC frequency detected at startup
    let cpu_ghz = config.tsc_frequency_ghz;
    if cpu_ghz == 0.0 {
        panic!("TSC frequency not detected! Cannot run latency tests on non-x86_64 platforms.");
    }

    let (mut runner, extent) = TestRunner::new(blocks, thread_id, error_mode, timing, config, progress, test_name, TestAction::Read);
    if extent.test_size == 0 {
        return LatencyTestStats { basic_stats: runner.finish_completed(0), ..LatencyTestStats::calculate_percentiles(vec![]) };
    }

    // The chain covers the extent, which targets the tier (L1/L2/L3/DRAM/DRAM-Full)
    let base = extent.ptr as *mut u64;
    let len = extent.test_size / std::mem::size_of::<u64>();
    let iterations = len.min(1000); // Fixed iterations for consistent measurements

    log::debug!("[Thread {}] {} - Working set: {} bytes ({} elements) targeting {}",
        thread_id, test_name, extent.test_size, len, config.extent_mode.target_level_name());

    build_cycle_u64(base, len, thread_id as u64);

    // Start timing AFTER setup - setup time should not count against test duration
    runner.restart_clock();

    let mut latencies_ns = Vec::with_capacity(10000);
    // Each sample CONTINUES the chase from where the last one stopped, so it walks the whole
    // working set, not a cached subset
    let mut ptr = base;

    // Main measurement loop: one sample per cycle
    loop {
        runner.begin_cycle();

        fence(Ordering::SeqCst);
        let mut aux = 0u32;
        let start_cycles = __rdtscp(&mut aux);

        for _ in 0..iterations {
            let addr = *ptr;  // Read address of next location
            ptr = addr as *mut u64;  // Jump to that location
        }

        let end_cycles = __rdtscp(&mut aux);
        fence(Ordering::SeqCst);

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
        runner.add_bytes(iterations * 8);

        runner.update_progress();
        if runner.shutdown_requested() || !runner.should_continue() {
            break;
        }
    }

    let mut result = LatencyTestStats::calculate_percentiles(latencies_ns);
    result.basic_stats = runner.finish_completed((runner.bytes_processed() / 8) as u64);

    // Log per-thread results with all percentiles
    log::info!("[Thread {}] {} - {} samples | P1={:.1} P5={:.1} P10={:.1} P25={:.1} P50={:.1} P75={:.1} P90={:.1} P95={:.1} P99={:.1} P99.9={:.1} | Spread={:.2}x",
        thread_id, test_name, result.sample_count,
        result.p1_ns, result.p5_ns, result.p10_ns, result.p25_ns, result.p50_ns,
        result.p75_ns, result.p90_ns, result.p95_ns, result.p99_ns, result.p99_9_ns,
        result.spread_ratio);

    result
}

/// Write Latency Test - Uses pointer chasing for addresses, writes simple values
#[doc = include_str!("test_fn_safety.md")]
pub unsafe fn write_latency_multi(
    blocks: &[AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> LatencyTestStats {
    let test_name = "WriteLatency";
    // Use TSC frequency detected at startup
    let cpu_ghz = config.tsc_frequency_ghz;
    if cpu_ghz == 0.0 {
        panic!("TSC frequency not detected! Cannot run latency tests on non-x86_64 platforms.");
    }

    let (mut runner, extent) = TestRunner::new(blocks, thread_id, error_mode, timing, config, progress, test_name, TestAction::Write);
    if extent.test_size == 0 {
        return LatencyTestStats { basic_stats: runner.finish_completed(0), ..LatencyTestStats::calculate_percentiles(vec![]) };
    }

    // The extent in two halves: the pointer chain (read only) and the write targets, so the
    // writes don't destroy the chain
    let chain_base = extent.ptr as *mut u64;
    let chain_len = extent.test_size / std::mem::size_of::<u64>() / 2;
    let write_base = chain_base.add(chain_len);
    let iterations = chain_len.min(1000);

    log::debug!("[Thread {}] {} - Working set: {} bytes ({} elements: {} chain + {} write) targeting {}",
        thread_id, test_name, extent.test_size, chain_len * 2, chain_len, chain_len,
        config.extent_mode.target_level_name());

    build_cycle_u64(chain_base, chain_len, thread_id as u64);

    // Start timing AFTER setup - setup time should not count against test duration
    runner.restart_clock();

    let mut latencies_ns = Vec::with_capacity(10000);
    // CONTINUE from where the last sample stopped
    let mut chain_ptr = chain_base;

    // Main measurement loop: one sample per cycle
    loop {
        runner.begin_cycle();

        fence(Ordering::SeqCst);
        let mut aux = 0u32;
        let start_cycles = __rdtscp(&mut aux);

        // Follow pointer chain to get random offsets, write to parallel region
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

        std::hint::black_box(chain_ptr);

            let delta_cycles = end_cycles - start_cycles;
            let cycles_per_write = delta_cycles as f64 / iterations as f64;
            let latency_ns = cycles_per_write / cpu_ghz;

            if latencies_ns.len() < 10 {
                log::debug!("[Thread {}] {} sample #{}: start={} end={} delta={} iter={} cycles/op={:.2} freq={:.3}GHz latency={:.2}ns",
                    thread_id, test_name, latencies_ns.len() + 1, start_cycles, end_cycles, delta_cycles, iterations, cycles_per_write, cpu_ghz, latency_ns);
            }

            latencies_ns.push(latency_ns);
        runner.add_bytes(iterations * 8);

        runner.update_progress();
        if runner.shutdown_requested() || !runner.should_continue() {
            break;
        }
    }

    let mut result = LatencyTestStats::calculate_percentiles(latencies_ns);
    result.basic_stats = runner.finish_completed((runner.bytes_processed() / 8) as u64);

    // Log per-thread results with all percentiles
    log::info!("[Thread {}] {} - {} samples | P1={:.1} P5={:.1} P10={:.1} P25={:.1} P50={:.1} P75={:.1} P90={:.1} P95={:.1} P99={:.1} P99.9={:.1} | Spread={:.2}x",
        thread_id, test_name, result.sample_count,
        result.p1_ns, result.p5_ns, result.p10_ns, result.p25_ns, result.p50_ns,
        result.p75_ns, result.p90_ns, result.p95_ns, result.p99_ns, result.p99_9_ns,
        result.spread_ratio);

    result
}

/// Copy Latency Test - Uses pointer chasing for source addresses
#[doc = include_str!("test_fn_safety.md")]
pub unsafe fn copy_latency_multi(
    blocks: &[AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> LatencyTestStats {
    let test_name = "CopyLatency";
    // Use TSC frequency detected at startup
    let cpu_ghz = config.tsc_frequency_ghz;
    if cpu_ghz == 0.0 {
        panic!("TSC frequency not detected! Cannot run latency tests on non-x86_64 platforms.");
    }

    let (mut runner, extent) = TestRunner::new(blocks, thread_id, error_mode, timing, config, progress, test_name, TestAction::Copy);
    if extent.test_size == 0 {
        return LatencyTestStats { basic_stats: runner.finish_completed(0), ..LatencyTestStats::calculate_percentiles(vec![]) };
    }

    // The extent in two halves: src (pointer chain) + dst (write target), so the total
    // footprint stays within the target tier. Matches Lat-Write's split for comparable results.
    let src_base = extent.ptr as *mut u64;
    let half_len = extent.test_size / std::mem::size_of::<u64>() / 2;
    let dst_base = src_base.add(half_len);
    let iterations = half_len.min(1000);

    log::debug!("[Thread {}] {} - Working set: {} bytes per buffer ({} elements each, {} total) targeting {}",
        thread_id, test_name, half_len * std::mem::size_of::<u64>(), half_len, half_len * 2,
        config.extent_mode.target_level_name());

    build_cycle_u64(src_base, half_len, thread_id as u64);

    // Start timing AFTER setup - setup time should not count against test duration
    runner.restart_clock();

    let mut latencies_ns = Vec::with_capacity(10000);
    // CONTINUE from where the last sample stopped; the destination walks forward and wraps
    let mut src_ptr = src_base;
    let mut dst_offset = 0usize;

    // Main measurement loop: one sample per cycle
    loop {
        runner.begin_cycle();

        fence(Ordering::SeqCst);
        let mut aux = 0u32;
        let start_cycles = __rdtscp(&mut aux);

        // Use pointer chain for source, write to corresponding destination
        for _ in 0..iterations {
            let value = *src_ptr;  // Read pointer (also serves as data value)
            let dst_ptr = dst_base.add(dst_offset);
            *dst_ptr = value;  // Write to destination
            src_ptr = value as *mut u64;  // Move source (chain follows the value)
            dst_offset += 1;
            if dst_offset >= half_len { dst_offset = 0; }  // Predictable branch — replaces idiv
        }

        let end_cycles = __rdtscp(&mut aux);
        fence(Ordering::SeqCst);

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
        runner.add_bytes(iterations * 16);

        runner.update_progress();
        if runner.shutdown_requested() || !runner.should_continue() {
            break;
        }
    }

    let mut result = LatencyTestStats::calculate_percentiles(latencies_ns);
    result.basic_stats = runner.finish_completed((runner.bytes_processed() / 16) as u64);

    // Log per-thread results with all percentiles
    log::info!("[Thread {}] {} - {} samples | P1={:.1} P5={:.1} P10={:.1} P25={:.1} P50={:.1} P75={:.1} P90={:.1} P95={:.1} P99={:.1} P99.9={:.1} | Spread={:.2}x",
        thread_id, test_name, result.sample_count,
        result.p1_ns, result.p5_ns, result.p10_ns, result.p25_ns, result.p50_ns,
        result.p75_ns, result.p90_ns, result.p95_ns, result.p99_ns, result.p99_9_ns,
        result.spread_ratio);

    result
}

// Note: run_cache_hierarchy_diagnostic() was removed - --cache-latency now uses
// the same ThreadPool infrastructure as --latency-test with cpus=1 for single-thread mode

#[cfg(test)]
mod tests {
    use super::build_cycle_u64;

    /// The chain is one cycle through every slot, for any length, so a chase visits all of the
    /// working set.
    #[test]
    fn the_chain_is_one_cycle_through_every_slot() {
        for n in [1usize, 2, 3, 7, 64, 1000, 4096] {
            let mut mem = vec![0u64; n];
            let base = mem.as_mut_ptr();
            unsafe { build_cycle_u64(base, n, 3) };
            let mut seen = vec![false; n];
            let mut at = base;
            for _ in 0..n {
                let i = (at as usize - base as usize) / 8;
                assert!(!seen[i], "n={n}: slot {i} visited twice");
                seen[i] = true;
                at = unsafe { *at } as *mut u64;
            }
            assert_eq!(at, base, "n={n}: the chase didn't return to the start");
            assert!(seen.iter().all(|&s| s));
        }
    }
}
