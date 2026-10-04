// Latency Tests v2 — redesigned read latency measurement using cache-line-granular layouts.
//
// Three test families share this file:
//   - Layout A (Lat-V2-*-Read): minimum-parallelism — 1 chain cell + 1 data cell pair per block,
//                                1 chain step per iteration, 1 SIMD-width data load per step
//   - Layout B (Lat-V2P-*-Read): packed/parallel — 1 chain cell + 6 data cells per block,
//                                 1 chain step per iteration, 6 SIMD-width data loads per step
//                                 (CPU pipelines the 6 data loads through its load buffer)
//   - Layout C (Lat-NTW-DRAM-Write): NT streaming write saturation (sustained NT-write commit
//                                     time at memory controller saturation)
//
// SIMD width variants per layout: 128-bit (SSE2), 256-bit (AVX2), 512-bit (AVX-512), Auto-dispatch.
// The chain pointer reads stay scalar (loading addresses, not vectors). Data cell reads use
// the configured SIMD width — they exercise the data cell address space without affecting
// the chain step's latency-bearing dependency.
//
// All chain cells are 64-byte aligned (cache-line aligned). Data cells are pure 64-byte payloads
// — no metadata embedded — so SIMD ops up to AVX-512 work directly on them.

use crate::ErrorMode;
use crate::runner::{AllocationBlock, SHUTDOWN_REQUESTED};
use crate::tests::{TestAction, TestMemoryConfig, TestProgress, TestTiming, TestStats, prepare_blocks_for_window};
use crate::latency_tests::LatencyTestStats;
use std::sync::atomic::{fence, Ordering};
use std::time::Instant;

#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::__rdtscp;

const CACHE_LINE_BYTES: usize = 64;

// ============================================================================
// Layout A — Single chain cell + single data cell (1 chain step per iteration)
// ============================================================================
//
// Block of 2 cache lines (128 bytes total):
//   Line 0 (chain cell): [prev: 8B][next: 8B][d_target: 8B][unused: 40B]
//   Line 1 (data cell):  64-byte clean payload (writable as full AVX-512)
//
// Per-step work: 1 chain read (cache miss → tier latency) + 1 data load at SIMD width.
// The data load pipelines behind the chain miss — it doesn't gate the chain step.

const LAYOUT_A_BLOCK_BYTES: usize = 2 * CACHE_LINE_BYTES; // chain + 1 data = 128B

// ============================================================================
// Layout B — Packed: 1 chain cell + 6 data cells per block (high MLP)
// ============================================================================
//
// Block of 7 cache lines (448 bytes total):
//   Line 0 (chain cell): [prev: 8B][next: 8B][d0..d5: 6×8B = 48B]
//   Lines 1-6 (data cells): 6× 64-byte clean payloads
//
// Per-step work: 1 chain read + 6 SIMD-width data loads. The 6 data loads are independent
// random accesses (separate random permutation), CPU-pipelined through the load buffer.

const LAYOUT_B_BLOCK_BYTES: usize = 7 * CACHE_LINE_BYTES; // 1 chain + 6 data = 448B

// ============================================================================
// Setup helpers (shared across all width variants — no SIMD needed for setup)
// ============================================================================

unsafe fn fisher_yates(slice: &mut [usize], seed: usize) {
    let len = slice.len();
    if len < 2 { return; }
    let mut state = 0x123456789ABCDEFu64.wrapping_add(seed as u64);
    for i in (1..len).rev() {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        let j = (state as usize) % (i + 1);
        slice.swap(i, j);
    }
}

unsafe fn setup_layout_a(base: *mut u8, working_bytes: usize, thread_id: usize) -> *mut u64 {
    let block_count = working_bytes / LAYOUT_A_BLOCK_BYTES;
    if block_count < 2 {
        return base as *mut u64;
    }

    let chain_addr = |i: usize| -> *mut u64 {
        base.add(i * LAYOUT_A_BLOCK_BYTES) as *mut u64
    };
    let data_addr = |i: usize| -> *mut u64 {
        base.add(i * LAYOUT_A_BLOCK_BYTES + CACHE_LINE_BYTES) as *mut u64
    };

    let mut chain_perm: Vec<usize> = (0..block_count).collect();
    let mut data_perm: Vec<usize> = (0..block_count).collect();
    fisher_yates(&mut chain_perm, thread_id ^ 0xA1);
    fisher_yates(&mut data_perm, thread_id ^ 0xA2);

    for i in 0..block_count {
        let cur_block = chain_perm[i];
        let next_block = chain_perm[(i + 1) % block_count];
        let prev_block = chain_perm[(i + block_count - 1) % block_count];
        let dtarget_block = data_perm[i];

        let cell = chain_addr(cur_block);
        *cell.add(0) = chain_addr(prev_block) as u64;
        *cell.add(1) = chain_addr(next_block) as u64;
        *cell.add(2) = data_addr(dtarget_block) as u64;
    }

    chain_addr(chain_perm[0])
}

unsafe fn setup_layout_b(base: *mut u8, working_bytes: usize, thread_id: usize) -> *mut u64 {
    let block_count = working_bytes / LAYOUT_B_BLOCK_BYTES;
    if block_count < 2 {
        return base as *mut u64;
    }

    let chain_addr = |i: usize| -> *mut u64 {
        base.add(i * LAYOUT_B_BLOCK_BYTES) as *mut u64
    };
    let data_addr = |block: usize, slot: usize| -> *mut u64 {
        base.add(block * LAYOUT_B_BLOCK_BYTES + (1 + slot) * CACHE_LINE_BYTES) as *mut u64
    };

    let total_data_cells = block_count * 6;
    let mut chain_perm: Vec<usize> = (0..block_count).collect();
    let mut data_perm: Vec<usize> = (0..total_data_cells).collect();
    fisher_yates(&mut chain_perm, thread_id ^ 0xB1);
    fisher_yates(&mut data_perm, thread_id ^ 0xB2);

    let mut data_idx = 0usize;
    for i in 0..block_count {
        let cur_block = chain_perm[i];
        let next_block = chain_perm[(i + 1) % block_count];
        let prev_block = chain_perm[(i + block_count - 1) % block_count];

        let cell = chain_addr(cur_block);
        *cell.add(0) = chain_addr(prev_block) as u64;
        *cell.add(1) = chain_addr(next_block) as u64;

        for slot in 0..6 {
            let target_idx = data_perm[data_idx % total_data_cells];
            data_idx += 1;
            let target_block = target_idx / 6;
            let target_slot = target_idx % 6;
            *cell.add(2 + slot) = data_addr(target_block, target_slot) as u64;
        }
    }

    chain_addr(chain_perm[0])
}

// ============================================================================
// Layout A macro — stamps out one read test for a given SIMD width
// ============================================================================
//
// The data load consumes the data cell payload via a SIMD-width load. The result is XOR-
// accumulated into a register-width scratch, then black_boxed at the end so the optimizer
// can't elide the load. Chain pointer reads stay scalar.

macro_rules! lat_v2_read_impl {
    (
        $pub_fn:ident,
        $target_feature:literal,
        $vec_type:ty,
        $load_fn:path,
        $xor_fn:path,
        $zero_fn:path,
        $reduce:expr  // closure: |vec| -> u64 to fold for black_box
    ) => {
        #[target_feature(enable = $target_feature)]
        #[doc = include_str!("test_fn_safety.md")]
        pub unsafe fn $pub_fn(
            blocks: &[AllocationBlock],
            thread_id: usize,
            _error_mode: ErrorMode,
            timing: &TestTiming,
            config: &TestMemoryConfig,
            _progress: Option<&TestProgress>,
        ) -> LatencyTestStats {
            let test_name = "Lat-V2-Read";
            let cpu_ghz = config.tsc_frequency_ghz;
            if cpu_ghz == 0.0 {
                panic!("TSC frequency not detected");
            }

            let total_allocated: usize = blocks.iter().map(|b| b.buffer.size()).sum();
            let window_size = config.calculate_window_size(test_name, total_allocated);
            let test_blocks = prepare_blocks_for_window(blocks, window_size, test_name);

            if test_blocks.is_empty() {
                return empty_stats(test_name, thread_id, timing);
            }

            const ITER: usize = 1000;

            let mut chain_positions: Vec<*mut u64> = Vec::new();
            for tb in test_blocks.iter() {
                let base = tb.ptr;
                let working_bytes = tb.test_size.min(window_size);
                let aligned = (working_bytes / LAYOUT_A_BLOCK_BYTES) * LAYOUT_A_BLOCK_BYTES;
                let start = setup_layout_a(base, aligned, thread_id);
                chain_positions.push(start);
            }

            let mut latencies_ns = Vec::with_capacity(10000);
            let mut total_ops = 0u64;
            let mut cycle = 0u32;
            let start_time = Instant::now();

            loop {
                cycle += 1;

                for (block_idx, _tb) in test_blocks.iter().enumerate() {
                    fence(Ordering::SeqCst);
                    let mut aux = 0u32;
                    let start_cyc = __rdtscp(&mut aux);

                    let mut chain = chain_positions[block_idx];
                    let mut accum: $vec_type = $zero_fn();
                    for _ in 0..ITER {
                        let next = *chain.add(1) as *mut u64;
                        let dtarget = *chain.add(2) as *const $vec_type;
                        // SIMD-width load from data cell — pipelines behind chain miss
                        let v = $load_fn(dtarget);
                        accum = $xor_fn(accum, v);
                        chain = next;
                    }

                    let end_cyc = __rdtscp(&mut aux);
                    fence(Ordering::SeqCst);
                    // Reduce the SIMD accumulator to a u64 via the supplied closure, then
                    // hand it to black_box so the SIMD load isn't optimized away.
                    let scalar_accum: u64 = ($reduce)(accum);
                    std::hint::black_box(scalar_accum);

                    chain_positions[block_idx] = chain;

                    let delta = end_cyc - start_cyc;
                    // 1 op per iteration: chain read is the latency-bearing operation. The
                    // SIMD data load pipelines in parallel — it exercises data cell address
                    // space without gating the chain step, so we don't count it.
                    let ops = ITER as u64;
                    let cycles_per_op = delta as f64 / ops as f64;
                    let ns_per_op = cycles_per_op / cpu_ghz;

                    latencies_ns.push(ns_per_op);
                    total_ops += ops;

                    if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) { break; }
                }

                let elapsed_secs = start_time.elapsed().as_secs() as u32;
                if !timing.should_continue(cycle, elapsed_secs) { break; }
            }

            finalize_stats(test_name, thread_id, total_ops, start_time, cycle, timing, latencies_ns)
        }
    }
}

// ============================================================================
// Layout A Write macro — single SIMD-width store per chain step (baseline)
// ============================================================================
//
// Same chain structure as Layout A Read, but each chain step issues one SIMD-width
// cached store to the data cell. Predicted to closely track Lat-V2-Read at every tier
// because a single in-flight write has plenty of slack to retire during the chain miss.
// Validates that the store-buffer pressure observed in Lat-V2P-Write requires the
// concurrency density of 6 simultaneous stores per step.

macro_rules! lat_v2_write_impl {
    (
        $pub_fn:ident,
        $target_feature:literal,
        $vec_type:ty,
        $set1_fn:path,
        $store_fn:path
    ) => {
        #[target_feature(enable = $target_feature)]
        #[doc = include_str!("test_fn_safety.md")]
        pub unsafe fn $pub_fn(
            blocks: &[AllocationBlock],
            thread_id: usize,
            _error_mode: ErrorMode,
            timing: &TestTiming,
            config: &TestMemoryConfig,
            _progress: Option<&TestProgress>,
        ) -> LatencyTestStats {
            let test_name = "Lat-V2-Write";
            let cpu_ghz = config.tsc_frequency_ghz;
            if cpu_ghz == 0.0 {
                panic!("TSC frequency not detected");
            }

            let total_allocated: usize = blocks.iter().map(|b| b.buffer.size()).sum();
            let window_size = config.calculate_window_size(test_name, total_allocated);
            let test_blocks = prepare_blocks_for_window(blocks, window_size, test_name);

            if test_blocks.is_empty() {
                return empty_stats(test_name, thread_id, timing);
            }

            const ITER: usize = 1000;
            let pattern: $vec_type = $set1_fn(0x5A5A5A5A_5A5A5A5Au64 as i64);

            let mut chain_positions: Vec<*mut u64> = Vec::new();
            for tb in test_blocks.iter() {
                let base = tb.ptr;
                let working_bytes = tb.test_size.min(window_size);
                let aligned = (working_bytes / LAYOUT_A_BLOCK_BYTES) * LAYOUT_A_BLOCK_BYTES;
                let start = setup_layout_a(base, aligned, thread_id);
                chain_positions.push(start);
            }

            let mut latencies_ns = Vec::with_capacity(10000);
            let mut total_ops = 0u64;
            let mut cycle = 0u32;
            let start_time = Instant::now();

            loop {
                cycle += 1;

                for (block_idx, _tb) in test_blocks.iter().enumerate() {
                    fence(Ordering::SeqCst);
                    let mut aux = 0u32;
                    let start_cyc = __rdtscp(&mut aux);

                    let mut chain = chain_positions[block_idx];
                    for _ in 0..ITER {
                        let next = *chain.add(1) as *mut u64;
                        let dtarget = *chain.add(2) as *mut $vec_type;
                        // Single SIMD-width cached store — fires into store buffer behind
                        // the chain miss, retires before next iteration needs the slot.
                        $store_fn(dtarget, pattern);
                        chain = next;
                    }

                    let end_cyc = __rdtscp(&mut aux);
                    fence(Ordering::SeqCst);

                    chain_positions[block_idx] = chain;

                    let delta = end_cyc - start_cyc;
                    let ops = ITER as u64;
                    let cycles_per_op = delta as f64 / ops as f64;
                    let ns_per_op = cycles_per_op / cpu_ghz;

                    latencies_ns.push(ns_per_op);
                    total_ops += ops;

                    if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) { break; }
                }

                let elapsed_secs = start_time.elapsed().as_secs() as u32;
                if !timing.should_continue(cycle, elapsed_secs) { break; }
            }

            finalize_stats(test_name, thread_id, total_ops, start_time, cycle, timing, latencies_ns)
        }
    }
}

// ============================================================================
// Layout A Copy macro — single read-modify-write per chain step (baseline)
// ============================================================================
//
// Same chain structure as Layout A Read, but each chain step does a load-XOR-store on
// the single data cell. Predicted to closely track Lat-V2-Read because the RMW chain is
// short (one cell) and easily fits behind the chain miss. Validates that the L1
// dependency-chain limit observed in Lat-V2P-Copy requires multiple simultaneous RMW
// pipelines.

macro_rules! lat_v2_copy_impl {
    (
        $pub_fn:ident,
        $target_feature:literal,
        $vec_type:ty,
        $load_fn:path,
        $store_fn:path,
        $xor_fn:path,
        $set1_fn:path
    ) => {
        #[target_feature(enable = $target_feature)]
        #[doc = include_str!("test_fn_safety.md")]
        pub unsafe fn $pub_fn(
            blocks: &[AllocationBlock],
            thread_id: usize,
            _error_mode: ErrorMode,
            timing: &TestTiming,
            config: &TestMemoryConfig,
            _progress: Option<&TestProgress>,
        ) -> LatencyTestStats {
            let test_name = "Lat-V2-Copy";
            let cpu_ghz = config.tsc_frequency_ghz;
            if cpu_ghz == 0.0 {
                panic!("TSC frequency not detected");
            }

            let total_allocated: usize = blocks.iter().map(|b| b.buffer.size()).sum();
            let window_size = config.calculate_window_size(test_name, total_allocated);
            let test_blocks = prepare_blocks_for_window(blocks, window_size, test_name);

            if test_blocks.is_empty() {
                return empty_stats(test_name, thread_id, timing);
            }

            const ITER: usize = 1000;
            let mask: $vec_type = $set1_fn(0x5A5A5A5A_5A5A5A5Au64 as i64);

            let mut chain_positions: Vec<*mut u64> = Vec::new();
            for tb in test_blocks.iter() {
                let base = tb.ptr;
                let working_bytes = tb.test_size.min(window_size);
                let aligned = (working_bytes / LAYOUT_A_BLOCK_BYTES) * LAYOUT_A_BLOCK_BYTES;
                let start = setup_layout_a(base, aligned, thread_id);
                chain_positions.push(start);
            }

            let mut latencies_ns = Vec::with_capacity(10000);
            let mut total_ops = 0u64;
            let mut cycle = 0u32;
            let start_time = Instant::now();

            loop {
                cycle += 1;

                for (block_idx, _tb) in test_blocks.iter().enumerate() {
                    fence(Ordering::SeqCst);
                    let mut aux = 0u32;
                    let start_cyc = __rdtscp(&mut aux);

                    let mut chain = chain_positions[block_idx];
                    for _ in 0..ITER {
                        let next = *chain.add(1) as *mut u64;
                        let dtarget = *chain.add(2) as *mut $vec_type;
                        // Single read-modify-write: load → XOR → store on same cell.
                        let v = $load_fn(dtarget);
                        $store_fn(dtarget, $xor_fn(v, mask));
                        chain = next;
                    }

                    let end_cyc = __rdtscp(&mut aux);
                    fence(Ordering::SeqCst);

                    chain_positions[block_idx] = chain;

                    let delta = end_cyc - start_cyc;
                    let ops = ITER as u64;
                    let cycles_per_op = delta as f64 / ops as f64;
                    let ns_per_op = cycles_per_op / cpu_ghz;

                    latencies_ns.push(ns_per_op);
                    total_ops += ops;

                    if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) { break; }
                }

                let elapsed_secs = start_time.elapsed().as_secs() as u32;
                if !timing.should_continue(cycle, elapsed_secs) { break; }
            }

            finalize_stats(test_name, thread_id, total_ops, start_time, cycle, timing, latencies_ns)
        }
    }
}

// ============================================================================
// Layout B macro — stamps out one packed read test for a given SIMD width
// ============================================================================

macro_rules! lat_v2p_read_impl {
    (
        $pub_fn:ident,
        $target_feature:literal,
        $vec_type:ty,
        $load_fn:path,
        $xor_fn:path,
        $zero_fn:path,
        $reduce:expr
    ) => {
        #[target_feature(enable = $target_feature)]
        #[doc = include_str!("test_fn_safety.md")]
        pub unsafe fn $pub_fn(
            blocks: &[AllocationBlock],
            thread_id: usize,
            _error_mode: ErrorMode,
            timing: &TestTiming,
            config: &TestMemoryConfig,
            _progress: Option<&TestProgress>,
        ) -> LatencyTestStats {
            let test_name = "Lat-V2P-Read";
            let cpu_ghz = config.tsc_frequency_ghz;
            if cpu_ghz == 0.0 {
                panic!("TSC frequency not detected");
            }

            let total_allocated: usize = blocks.iter().map(|b| b.buffer.size()).sum();
            let window_size = config.calculate_window_size(test_name, total_allocated);
            let test_blocks = prepare_blocks_for_window(blocks, window_size, test_name);

            if test_blocks.is_empty() {
                return empty_stats(test_name, thread_id, timing);
            }

            const ITER: usize = 1000;

            let mut chain_positions: Vec<*mut u64> = Vec::new();
            for tb in test_blocks.iter() {
                let base = tb.ptr;
                let working_bytes = tb.test_size.min(window_size);
                let aligned = (working_bytes / LAYOUT_B_BLOCK_BYTES) * LAYOUT_B_BLOCK_BYTES;
                let start = setup_layout_b(base, aligned, thread_id);
                chain_positions.push(start);
            }

            let mut latencies_ns = Vec::with_capacity(10000);
            let mut total_ops = 0u64;
            let mut cycle = 0u32;
            let start_time = Instant::now();

            loop {
                cycle += 1;

                for (block_idx, _tb) in test_blocks.iter().enumerate() {
                    fence(Ordering::SeqCst);
                    let mut aux = 0u32;
                    let start_cyc = __rdtscp(&mut aux);

                    let mut chain = chain_positions[block_idx];
                    let mut accum: $vec_type = $zero_fn();
                    for _ in 0..ITER {
                        let next = *chain.add(1) as *mut u64;
                        let d0 = *chain.add(2) as *const $vec_type;
                        let d1 = *chain.add(3) as *const $vec_type;
                        let d2 = *chain.add(4) as *const $vec_type;
                        let d3 = *chain.add(5) as *const $vec_type;
                        let d4 = *chain.add(6) as *const $vec_type;
                        let d5 = *chain.add(7) as *const $vec_type;
                        // 6 SIMD-width loads — CPU pipelines them through the load buffer
                        let v0 = $load_fn(d0);
                        let v1 = $load_fn(d1);
                        let v2 = $load_fn(d2);
                        let v3 = $load_fn(d3);
                        let v4 = $load_fn(d4);
                        let v5 = $load_fn(d5);
                        accum = $xor_fn(accum, v0);
                        accum = $xor_fn(accum, v1);
                        accum = $xor_fn(accum, v2);
                        accum = $xor_fn(accum, v3);
                        accum = $xor_fn(accum, v4);
                        accum = $xor_fn(accum, v5);
                        chain = next;
                    }

                    let end_cyc = __rdtscp(&mut aux);
                    fence(Ordering::SeqCst);
                    let scalar_accum: u64 = ($reduce)(accum);
                    std::hint::black_box(scalar_accum);

                    chain_positions[block_idx] = chain;

                    let delta = end_cyc - start_cyc;
                    // 1 op per iteration: chain read is latency-bearing. The 6 data loads
                    // pipeline in parallel through the CPU's load buffer.
                    let ops = ITER as u64;
                    let cycles_per_op = delta as f64 / ops as f64;
                    let ns_per_op = cycles_per_op / cpu_ghz;

                    latencies_ns.push(ns_per_op);
                    total_ops += ops;

                    if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) { break; }
                }

                let elapsed_secs = start_time.elapsed().as_secs() as u32;
                if !timing.should_continue(cycle, elapsed_secs) { break; }
            }

            finalize_stats(test_name, thread_id, total_ops, start_time, cycle, timing, latencies_ns)
        }
    }
}

// ============================================================================
// Layout B Write macro — packed 6 SIMD-width WRITES per chain step (Path B PoC)
// ============================================================================
//
// Same chain structure as Layout B Read, but each chain step issues 6 SIMD-width *cached*
// stores to the 6 random data cells. The stores enter the store buffer (~56-72 entries
// on Intel) and pipeline behind the chain miss. Hypothesis: under sustained pressure at
// DRAM tier (each RFO ~140 ns), 6 outstanding writes per iteration could fill the store
// buffer faster than it drains, making per-step latency rise above pure Lat-V2P-Read.
//
// If results match Read closely → cached writes are still hidden by store buffer (expected).
// If DRAM tier rises notably → store-buffer pressure becomes visible (informative).

macro_rules! lat_v2p_write_impl {
    (
        $pub_fn:ident,
        $target_feature:literal,
        $vec_type:ty,
        $set1_fn:path,
        $store_fn:path
    ) => {
        #[target_feature(enable = $target_feature)]
        #[doc = include_str!("test_fn_safety.md")]
        pub unsafe fn $pub_fn(
            blocks: &[AllocationBlock],
            thread_id: usize,
            _error_mode: ErrorMode,
            timing: &TestTiming,
            config: &TestMemoryConfig,
            _progress: Option<&TestProgress>,
        ) -> LatencyTestStats {
            let test_name = "Lat-V2P-Write";
            let cpu_ghz = config.tsc_frequency_ghz;
            if cpu_ghz == 0.0 {
                panic!("TSC frequency not detected");
            }

            let total_allocated: usize = blocks.iter().map(|b| b.buffer.size()).sum();
            let window_size = config.calculate_window_size(test_name, total_allocated);
            let test_blocks = prepare_blocks_for_window(blocks, window_size, test_name);

            if test_blocks.is_empty() {
                return empty_stats(test_name, thread_id, timing);
            }

            const ITER: usize = 1000;
            let pattern: $vec_type = $set1_fn(0x5A5A5A5A_5A5A5A5Au64 as i64);

            let mut chain_positions: Vec<*mut u64> = Vec::new();
            for tb in test_blocks.iter() {
                let base = tb.ptr;
                let working_bytes = tb.test_size.min(window_size);
                let aligned = (working_bytes / LAYOUT_B_BLOCK_BYTES) * LAYOUT_B_BLOCK_BYTES;
                let start = setup_layout_b(base, aligned, thread_id);
                chain_positions.push(start);
            }

            let mut latencies_ns = Vec::with_capacity(10000);
            let mut total_ops = 0u64;
            let mut cycle = 0u32;
            let start_time = Instant::now();

            loop {
                cycle += 1;

                for (block_idx, _tb) in test_blocks.iter().enumerate() {
                    fence(Ordering::SeqCst);
                    let mut aux = 0u32;
                    let start_cyc = __rdtscp(&mut aux);

                    let mut chain = chain_positions[block_idx];
                    for _ in 0..ITER {
                        let next = *chain.add(1) as *mut u64;
                        let d0 = *chain.add(2) as *mut $vec_type;
                        let d1 = *chain.add(3) as *mut $vec_type;
                        let d2 = *chain.add(4) as *mut $vec_type;
                        let d3 = *chain.add(5) as *mut $vec_type;
                        let d4 = *chain.add(6) as *mut $vec_type;
                        let d5 = *chain.add(7) as *mut $vec_type;
                        // 6 SIMD-width cached stores — fire into store buffer, pipeline
                        // behind the chain miss. Hypothesis test: at DRAM tier with 6 RFOs
                        // outstanding per iteration, does the store buffer become a
                        // measurable bottleneck?
                        $store_fn(d0, pattern);
                        $store_fn(d1, pattern);
                        $store_fn(d2, pattern);
                        $store_fn(d3, pattern);
                        $store_fn(d4, pattern);
                        $store_fn(d5, pattern);
                        chain = next;
                    }

                    let end_cyc = __rdtscp(&mut aux);
                    fence(Ordering::SeqCst);

                    chain_positions[block_idx] = chain;

                    let delta = end_cyc - start_cyc;
                    // 1 op per iteration: chain read is the latency-bearing operation. The
                    // 6 writes pipeline through the store buffer. If they ever gate the
                    // chain step (store buffer fills), it shows up here.
                    let ops = ITER as u64;
                    let cycles_per_op = delta as f64 / ops as f64;
                    let ns_per_op = cycles_per_op / cpu_ghz;

                    latencies_ns.push(ns_per_op);
                    total_ops += ops;

                    if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) { break; }
                }

                let elapsed_secs = start_time.elapsed().as_secs() as u32;
                if !timing.should_continue(cycle, elapsed_secs) { break; }
            }

            finalize_stats(test_name, thread_id, total_ops, start_time, cycle, timing, latencies_ns)
        }
    }
}

// ============================================================================
// Layout B Copy macro — packed 6 SIMD-width READ-MODIFY-WRITE per chain step
// ============================================================================
//
// Same chain structure as Layout B Read/Write, but each chain step issues 6 SIMD-width
// read-modify-write operations: load → XOR → store on each of the 6 random data cells.
// This produces realistic "copy" memory traffic — each cell incurs both a load and a
// store on the same cache line.
//
// vs Lat-V2P-Read: same load pressure, plus 6 stores
// vs Lat-V2P-Write: stores write to lines that were just loaded (already in L1, no RFO)
//
// Hypothesis: Copy should be FASTER than Write at most tiers because the loads warm the
// lines into L1 first, eliminating the RFO cost of the stores. The store-buffer pressure
// pattern from Lat-V2P-Write should mostly disappear, replaced by a simpler "Read +
// trivial stores" profile.

macro_rules! lat_v2p_copy_impl {
    (
        $pub_fn:ident,
        $target_feature:literal,
        $vec_type:ty,
        $load_fn:path,
        $store_fn:path,
        $xor_fn:path,
        $set1_fn:path
    ) => {
        #[target_feature(enable = $target_feature)]
        #[doc = include_str!("test_fn_safety.md")]
        pub unsafe fn $pub_fn(
            blocks: &[AllocationBlock],
            thread_id: usize,
            _error_mode: ErrorMode,
            timing: &TestTiming,
            config: &TestMemoryConfig,
            _progress: Option<&TestProgress>,
        ) -> LatencyTestStats {
            let test_name = "Lat-V2P-Copy";
            let cpu_ghz = config.tsc_frequency_ghz;
            if cpu_ghz == 0.0 {
                panic!("TSC frequency not detected");
            }

            let total_allocated: usize = blocks.iter().map(|b| b.buffer.size()).sum();
            let window_size = config.calculate_window_size(test_name, total_allocated);
            let test_blocks = prepare_blocks_for_window(blocks, window_size, test_name);

            if test_blocks.is_empty() {
                return empty_stats(test_name, thread_id, timing);
            }

            const ITER: usize = 1000;
            let mask: $vec_type = $set1_fn(0x5A5A5A5A_5A5A5A5Au64 as i64);

            let mut chain_positions: Vec<*mut u64> = Vec::new();
            for tb in test_blocks.iter() {
                let base = tb.ptr;
                let working_bytes = tb.test_size.min(window_size);
                let aligned = (working_bytes / LAYOUT_B_BLOCK_BYTES) * LAYOUT_B_BLOCK_BYTES;
                let start = setup_layout_b(base, aligned, thread_id);
                chain_positions.push(start);
            }

            let mut latencies_ns = Vec::with_capacity(10000);
            let mut total_ops = 0u64;
            let mut cycle = 0u32;
            let start_time = Instant::now();

            loop {
                cycle += 1;

                for (block_idx, _tb) in test_blocks.iter().enumerate() {
                    fence(Ordering::SeqCst);
                    let mut aux = 0u32;
                    let start_cyc = __rdtscp(&mut aux);

                    let mut chain = chain_positions[block_idx];
                    for _ in 0..ITER {
                        let next = *chain.add(1) as *mut u64;
                        let d0 = *chain.add(2) as *mut $vec_type;
                        let d1 = *chain.add(3) as *mut $vec_type;
                        let d2 = *chain.add(4) as *mut $vec_type;
                        let d3 = *chain.add(5) as *mut $vec_type;
                        let d4 = *chain.add(6) as *mut $vec_type;
                        let d5 = *chain.add(7) as *mut $vec_type;
                        // Read-Modify-Write each data cell. Loads warm the lines into L1
                        // first, so the subsequent stores hit a cached line (no RFO needed).
                        // Tests the "copy traffic" path — load + store on same cache line.
                        let v0 = $load_fn(d0); $store_fn(d0, $xor_fn(v0, mask));
                        let v1 = $load_fn(d1); $store_fn(d1, $xor_fn(v1, mask));
                        let v2 = $load_fn(d2); $store_fn(d2, $xor_fn(v2, mask));
                        let v3 = $load_fn(d3); $store_fn(d3, $xor_fn(v3, mask));
                        let v4 = $load_fn(d4); $store_fn(d4, $xor_fn(v4, mask));
                        let v5 = $load_fn(d5); $store_fn(d5, $xor_fn(v5, mask));
                        chain = next;
                    }

                    let end_cyc = __rdtscp(&mut aux);
                    fence(Ordering::SeqCst);

                    chain_positions[block_idx] = chain;

                    let delta = end_cyc - start_cyc;
                    // 1 op per iteration: chain read is the latency-bearing operation.
                    // The 6 RMW ops pipeline behind the chain miss.
                    let ops = ITER as u64;
                    let cycles_per_op = delta as f64 / ops as f64;
                    let ns_per_op = cycles_per_op / cpu_ghz;

                    latencies_ns.push(ns_per_op);
                    total_ops += ops;

                    if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) { break; }
                }

                let elapsed_secs = start_time.elapsed().as_secs() as u32;
                if !timing.should_continue(cycle, elapsed_secs) { break; }
            }

            finalize_stats(test_name, thread_id, total_ops, start_time, cycle, timing, latencies_ns)
        }
    }
}

// ============================================================================
// Layout B WriteFull macro — sized to write the entire 64-byte cache line per cell
// ============================================================================
//
// Same chain structure as Lat-V2P-Write, but instead of a single SIMD-width store per
// data cell (which writes 16/32/64 bytes depending on width), this variant repeats the
// store at consecutive offsets within the same cell so that every width writes the FULL
// 64-byte cache line per cell.
//
// 128-bit: 4 stores per cell at offsets 0, 16, 32, 48
// 256-bit: 2 stores per cell at offsets 0, 32
// 512-bit: 1 store per cell at offset 0 (already full-line)
//
// Comparing to Lat-V2P-Write isolates the byte-coverage variable. Same total cache lines
// touched, but every line gets fully overwritten regardless of width.

macro_rules! lat_v2p_write_full_impl {
    (
        $pub_fn:ident,
        $target_feature:literal,
        $vec_type:ty,
        $set1_fn:path,
        $store_fn:path,
        $stores_per_cell:expr  // 4 for 128-bit, 2 for 256-bit, 1 for 512-bit
    ) => {
        #[target_feature(enable = $target_feature)]
        #[doc = include_str!("test_fn_safety.md")]
        pub unsafe fn $pub_fn(
            blocks: &[AllocationBlock],
            thread_id: usize,
            _error_mode: ErrorMode,
            timing: &TestTiming,
            config: &TestMemoryConfig,
            _progress: Option<&TestProgress>,
        ) -> LatencyTestStats {
            let test_name = "Lat-V2P-WriteFull";
            let cpu_ghz = config.tsc_frequency_ghz;
            if cpu_ghz == 0.0 {
                panic!("TSC frequency not detected");
            }

            let total_allocated: usize = blocks.iter().map(|b| b.buffer.size()).sum();
            let window_size = config.calculate_window_size(test_name, total_allocated);
            let test_blocks = prepare_blocks_for_window(blocks, window_size, test_name);

            if test_blocks.is_empty() {
                return empty_stats(test_name, thread_id, timing);
            }

            const ITER: usize = 1000;
            let pattern: $vec_type = $set1_fn(0x5A5A5A5A_5A5A5A5Au64 as i64);

            let mut chain_positions: Vec<*mut u64> = Vec::new();
            for tb in test_blocks.iter() {
                let base = tb.ptr;
                let working_bytes = tb.test_size.min(window_size);
                let aligned = (working_bytes / LAYOUT_B_BLOCK_BYTES) * LAYOUT_B_BLOCK_BYTES;
                let start = setup_layout_b(base, aligned, thread_id);
                chain_positions.push(start);
            }

            let mut latencies_ns = Vec::with_capacity(10000);
            let mut total_ops = 0u64;
            let mut cycle = 0u32;
            let start_time = Instant::now();

            loop {
                cycle += 1;

                for (block_idx, _tb) in test_blocks.iter().enumerate() {
                    fence(Ordering::SeqCst);
                    let mut aux = 0u32;
                    let start_cyc = __rdtscp(&mut aux);

                    let mut chain = chain_positions[block_idx];
                    for _ in 0..ITER {
                        let next = *chain.add(1) as *mut u64;
                        let d0 = *chain.add(2) as *mut $vec_type;
                        let d1 = *chain.add(3) as *mut $vec_type;
                        let d2 = *chain.add(4) as *mut $vec_type;
                        let d3 = *chain.add(5) as *mut $vec_type;
                        let d4 = *chain.add(6) as *mut $vec_type;
                        let d5 = *chain.add(7) as *mut $vec_type;
                        // Write the full 64-byte cache line per cell. Loop unrolled by
                        // $stores_per_cell so 128/256/512 all touch every byte of every
                        // target line.
                        for i in 0..$stores_per_cell {
                            $store_fn(d0.add(i), pattern);
                            $store_fn(d1.add(i), pattern);
                            $store_fn(d2.add(i), pattern);
                            $store_fn(d3.add(i), pattern);
                            $store_fn(d4.add(i), pattern);
                            $store_fn(d5.add(i), pattern);
                        }
                        chain = next;
                    }

                    let end_cyc = __rdtscp(&mut aux);
                    fence(Ordering::SeqCst);

                    chain_positions[block_idx] = chain;

                    let delta = end_cyc - start_cyc;
                    let ops = ITER as u64;
                    let cycles_per_op = delta as f64 / ops as f64;
                    let ns_per_op = cycles_per_op / cpu_ghz;

                    latencies_ns.push(ns_per_op);
                    total_ops += ops;

                    if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) { break; }
                }

                let elapsed_secs = start_time.elapsed().as_secs() as u32;
                if !timing.should_continue(cycle, elapsed_secs) { break; }
            }

            finalize_stats(test_name, thread_id, total_ops, start_time, cycle, timing, latencies_ns)
        }
    }
}

// ============================================================================
// Layout B CopyFull macro — read-modify-write the entire 64-byte cache line per cell
// ============================================================================
//
// Same as Lat-V2P-Copy but each cell is fully covered by RMW operations regardless of
// SIMD width. Comparing to Lat-V2P-Copy isolates whether the L3 bimodality observed at
// 256/512 widths is due to (a) byte coverage of the target line, or (b) the SIMD path
// itself (single wide instruction vs multiple narrow instructions doing the same total
// work).

macro_rules! lat_v2p_copy_full_impl {
    (
        $pub_fn:ident,
        $target_feature:literal,
        $vec_type:ty,
        $load_fn:path,
        $store_fn:path,
        $xor_fn:path,
        $set1_fn:path,
        $ops_per_cell:expr
    ) => {
        #[target_feature(enable = $target_feature)]
        #[doc = include_str!("test_fn_safety.md")]
        pub unsafe fn $pub_fn(
            blocks: &[AllocationBlock],
            thread_id: usize,
            _error_mode: ErrorMode,
            timing: &TestTiming,
            config: &TestMemoryConfig,
            _progress: Option<&TestProgress>,
        ) -> LatencyTestStats {
            let test_name = "Lat-V2P-CopyFull";
            let cpu_ghz = config.tsc_frequency_ghz;
            if cpu_ghz == 0.0 {
                panic!("TSC frequency not detected");
            }

            let total_allocated: usize = blocks.iter().map(|b| b.buffer.size()).sum();
            let window_size = config.calculate_window_size(test_name, total_allocated);
            let test_blocks = prepare_blocks_for_window(blocks, window_size, test_name);

            if test_blocks.is_empty() {
                return empty_stats(test_name, thread_id, timing);
            }

            const ITER: usize = 1000;
            let mask: $vec_type = $set1_fn(0x5A5A5A5A_5A5A5A5Au64 as i64);

            let mut chain_positions: Vec<*mut u64> = Vec::new();
            for tb in test_blocks.iter() {
                let base = tb.ptr;
                let working_bytes = tb.test_size.min(window_size);
                let aligned = (working_bytes / LAYOUT_B_BLOCK_BYTES) * LAYOUT_B_BLOCK_BYTES;
                let start = setup_layout_b(base, aligned, thread_id);
                chain_positions.push(start);
            }

            let mut latencies_ns = Vec::with_capacity(10000);
            let mut total_ops = 0u64;
            let mut cycle = 0u32;
            let start_time = Instant::now();

            loop {
                cycle += 1;

                for (block_idx, _tb) in test_blocks.iter().enumerate() {
                    fence(Ordering::SeqCst);
                    let mut aux = 0u32;
                    let start_cyc = __rdtscp(&mut aux);

                    let mut chain = chain_positions[block_idx];
                    for _ in 0..ITER {
                        let next = *chain.add(1) as *mut u64;
                        let d0 = *chain.add(2) as *mut $vec_type;
                        let d1 = *chain.add(3) as *mut $vec_type;
                        let d2 = *chain.add(4) as *mut $vec_type;
                        let d3 = *chain.add(5) as *mut $vec_type;
                        let d4 = *chain.add(6) as *mut $vec_type;
                        let d5 = *chain.add(7) as *mut $vec_type;
                        // Full-line RMW: cover every byte of every target line.
                        for i in 0..$ops_per_cell {
                            let v0 = $load_fn(d0.add(i)); $store_fn(d0.add(i), $xor_fn(v0, mask));
                            let v1 = $load_fn(d1.add(i)); $store_fn(d1.add(i), $xor_fn(v1, mask));
                            let v2 = $load_fn(d2.add(i)); $store_fn(d2.add(i), $xor_fn(v2, mask));
                            let v3 = $load_fn(d3.add(i)); $store_fn(d3.add(i), $xor_fn(v3, mask));
                            let v4 = $load_fn(d4.add(i)); $store_fn(d4.add(i), $xor_fn(v4, mask));
                            let v5 = $load_fn(d5.add(i)); $store_fn(d5.add(i), $xor_fn(v5, mask));
                        }
                        chain = next;
                    }

                    let end_cyc = __rdtscp(&mut aux);
                    fence(Ordering::SeqCst);

                    chain_positions[block_idx] = chain;

                    let delta = end_cyc - start_cyc;
                    let ops = ITER as u64;
                    let cycles_per_op = delta as f64 / ops as f64;
                    let ns_per_op = cycles_per_op / cpu_ghz;

                    latencies_ns.push(ns_per_op);
                    total_ops += ops;

                    if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) { break; }
                }

                let elapsed_secs = start_time.elapsed().as_secs() as u32;
                if !timing.should_continue(cycle, elapsed_secs) { break; }
            }

            finalize_stats(test_name, thread_id, total_ops, start_time, cycle, timing, latencies_ns)
        }
    }
}

// ============================================================================
// Width-specific reduction helpers — fold a SIMD vector to u64 for black_box
// ============================================================================

#[inline(always)]
unsafe fn reduce_m128i(v: std::arch::x86_64::__m128i) -> u64 {
    use std::arch::x86_64::*;
    let lo = _mm_extract_epi64::<0>(v) as u64;
    let hi = _mm_extract_epi64::<1>(v) as u64;
    lo ^ hi
}

#[inline(always)]
unsafe fn reduce_m256i(v: std::arch::x86_64::__m256i) -> u64 {
    use std::arch::x86_64::*;
    let lo = _mm256_extract_epi64::<0>(v) as u64;
    let m1 = _mm256_extract_epi64::<1>(v) as u64;
    let m2 = _mm256_extract_epi64::<2>(v) as u64;
    let hi = _mm256_extract_epi64::<3>(v) as u64;
    lo ^ m1 ^ m2 ^ hi
}

#[inline(always)]
unsafe fn reduce_m512i(v: std::arch::x86_64::__m512i) -> u64 {
    use std::arch::x86_64::*;
    // _mm512_reduce_or_epi64 isn't available on stable — extract via store-to-array
    let mut buf = [0u64; 8];
    _mm512_storeu_si512(buf.as_mut_ptr() as *mut __m512i, v);
    buf.iter().fold(0u64, |a, &b| a ^ b)
}

// ============================================================================
// Layout A — three SIMD width variants + auto-dispatch
// ============================================================================

lat_v2_read_impl!(
    lat_v2_read_128_multi,
    "sse4.2,sse4.1,ssse3,sse3,sse2,popcnt",
    std::arch::x86_64::__m128i,
    std::arch::x86_64::_mm_load_si128,
    std::arch::x86_64::_mm_xor_si128,
    std::arch::x86_64::_mm_setzero_si128,
    |v| reduce_m128i(v)
);

lat_v2_read_impl!(
    lat_v2_read_256_multi,
    "avx2,avx,fma,bmi1,bmi2",
    std::arch::x86_64::__m256i,
    std::arch::x86_64::_mm256_load_si256,
    std::arch::x86_64::_mm256_xor_si256,
    std::arch::x86_64::_mm256_setzero_si256,
    |v| reduce_m256i(v)
);

lat_v2_read_impl!(
    lat_v2_read_512_multi,
    "avx512f,avx512bw,avx512cd,avx512dq,avx512vl,avx2,avx,fma,bmi1,bmi2",
    std::arch::x86_64::__m512i,
    std::arch::x86_64::_mm512_load_si512,
    std::arch::x86_64::_mm512_xor_si512,
    std::arch::x86_64::_mm512_setzero_si512,
    |v| reduce_m512i(v)
);

/// Auto-dispatch wrapper for Layout A — picks the best available SIMD width at runtime.
/// Returns LatencyTestStats (not TestStats) so it works with TestFunction::Latency.
#[doc = include_str!("test_fn_safety.md")]
pub unsafe fn lat_v2_read_auto_multi(
    blocks: &[AllocationBlock], thread_id: usize, error_mode: ErrorMode,
    timing: &TestTiming, config: &TestMemoryConfig, progress: Option<&TestProgress>,
) -> LatencyTestStats {
    if is_x86_feature_detected!("avx512f") {
        return lat_v2_read_512_multi(blocks, thread_id, error_mode, timing, config, progress);
    }
    if is_x86_feature_detected!("avx2") {
        return lat_v2_read_256_multi(blocks, thread_id, error_mode, timing, config, progress);
    }
    lat_v2_read_128_multi(blocks, thread_id, error_mode, timing, config, progress)
}

// ============================================================================
// Layout A Write — three SIMD width variants + auto-dispatch
// ============================================================================

lat_v2_write_impl!(
    lat_v2_write_128_multi,
    "sse2",
    std::arch::x86_64::__m128i,
    std::arch::x86_64::_mm_set1_epi64x,
    std::arch::x86_64::_mm_store_si128
);

lat_v2_write_impl!(
    lat_v2_write_256_multi,
    "avx2,avx,fma,bmi1,bmi2",
    std::arch::x86_64::__m256i,
    std::arch::x86_64::_mm256_set1_epi64x,
    std::arch::x86_64::_mm256_store_si256
);

lat_v2_write_impl!(
    lat_v2_write_512_multi,
    "avx512f,avx512bw,avx512cd,avx512dq,avx512vl,avx2,avx,fma,bmi1,bmi2",
    std::arch::x86_64::__m512i,
    std::arch::x86_64::_mm512_set1_epi64,
    std::arch::x86_64::_mm512_store_si512
);

#[doc = include_str!("test_fn_safety.md")]
pub unsafe fn lat_v2_write_auto_multi(
    blocks: &[AllocationBlock], thread_id: usize, error_mode: ErrorMode,
    timing: &TestTiming, config: &TestMemoryConfig, progress: Option<&TestProgress>,
) -> LatencyTestStats {
    if is_x86_feature_detected!("avx512f") {
        return lat_v2_write_512_multi(blocks, thread_id, error_mode, timing, config, progress);
    }
    if is_x86_feature_detected!("avx2") {
        return lat_v2_write_256_multi(blocks, thread_id, error_mode, timing, config, progress);
    }
    lat_v2_write_128_multi(blocks, thread_id, error_mode, timing, config, progress)
}

// ============================================================================
// Layout A Copy — three SIMD width variants + auto-dispatch
// ============================================================================

lat_v2_copy_impl!(
    lat_v2_copy_128_multi,
    "sse2",
    std::arch::x86_64::__m128i,
    std::arch::x86_64::_mm_load_si128,
    std::arch::x86_64::_mm_store_si128,
    std::arch::x86_64::_mm_xor_si128,
    std::arch::x86_64::_mm_set1_epi64x
);

lat_v2_copy_impl!(
    lat_v2_copy_256_multi,
    "avx2,avx,fma,bmi1,bmi2",
    std::arch::x86_64::__m256i,
    std::arch::x86_64::_mm256_load_si256,
    std::arch::x86_64::_mm256_store_si256,
    std::arch::x86_64::_mm256_xor_si256,
    std::arch::x86_64::_mm256_set1_epi64x
);

lat_v2_copy_impl!(
    lat_v2_copy_512_multi,
    "avx512f,avx512bw,avx512cd,avx512dq,avx512vl,avx2,avx,fma,bmi1,bmi2",
    std::arch::x86_64::__m512i,
    std::arch::x86_64::_mm512_load_si512,
    std::arch::x86_64::_mm512_store_si512,
    std::arch::x86_64::_mm512_xor_si512,
    std::arch::x86_64::_mm512_set1_epi64
);

#[doc = include_str!("test_fn_safety.md")]
pub unsafe fn lat_v2_copy_auto_multi(
    blocks: &[AllocationBlock], thread_id: usize, error_mode: ErrorMode,
    timing: &TestTiming, config: &TestMemoryConfig, progress: Option<&TestProgress>,
) -> LatencyTestStats {
    if is_x86_feature_detected!("avx512f") {
        return lat_v2_copy_512_multi(blocks, thread_id, error_mode, timing, config, progress);
    }
    if is_x86_feature_detected!("avx2") {
        return lat_v2_copy_256_multi(blocks, thread_id, error_mode, timing, config, progress);
    }
    lat_v2_copy_128_multi(blocks, thread_id, error_mode, timing, config, progress)
}

// ============================================================================
// Layout B — three SIMD width variants + auto-dispatch
// ============================================================================

lat_v2p_read_impl!(
    lat_v2p_read_128_multi,
    "sse4.2,sse4.1,ssse3,sse3,sse2,popcnt",
    std::arch::x86_64::__m128i,
    std::arch::x86_64::_mm_load_si128,
    std::arch::x86_64::_mm_xor_si128,
    std::arch::x86_64::_mm_setzero_si128,
    |v| reduce_m128i(v)
);

lat_v2p_read_impl!(
    lat_v2p_read_256_multi,
    "avx2,avx,fma,bmi1,bmi2",
    std::arch::x86_64::__m256i,
    std::arch::x86_64::_mm256_load_si256,
    std::arch::x86_64::_mm256_xor_si256,
    std::arch::x86_64::_mm256_setzero_si256,
    |v| reduce_m256i(v)
);

lat_v2p_read_impl!(
    lat_v2p_read_512_multi,
    "avx512f,avx512bw,avx512cd,avx512dq,avx512vl,avx2,avx,fma,bmi1,bmi2",
    std::arch::x86_64::__m512i,
    std::arch::x86_64::_mm512_load_si512,
    std::arch::x86_64::_mm512_xor_si512,
    std::arch::x86_64::_mm512_setzero_si512,
    |v| reduce_m512i(v)
);

#[doc = include_str!("test_fn_safety.md")]
pub unsafe fn lat_v2p_read_auto_multi(
    blocks: &[AllocationBlock], thread_id: usize, error_mode: ErrorMode,
    timing: &TestTiming, config: &TestMemoryConfig, progress: Option<&TestProgress>,
) -> LatencyTestStats {
    if is_x86_feature_detected!("avx512f") {
        return lat_v2p_read_512_multi(blocks, thread_id, error_mode, timing, config, progress);
    }
    if is_x86_feature_detected!("avx2") {
        return lat_v2p_read_256_multi(blocks, thread_id, error_mode, timing, config, progress);
    }
    lat_v2p_read_128_multi(blocks, thread_id, error_mode, timing, config, progress)
}

// ============================================================================
// Layout B Write — three SIMD width variants + auto-dispatch (Path B PoC)
// ============================================================================

lat_v2p_write_impl!(
    lat_v2p_write_128_multi,
    "sse2",
    std::arch::x86_64::__m128i,
    std::arch::x86_64::_mm_set1_epi64x,
    std::arch::x86_64::_mm_store_si128
);

lat_v2p_write_impl!(
    lat_v2p_write_256_multi,
    "avx2,avx,fma,bmi1,bmi2",
    std::arch::x86_64::__m256i,
    std::arch::x86_64::_mm256_set1_epi64x,
    std::arch::x86_64::_mm256_store_si256
);

lat_v2p_write_impl!(
    lat_v2p_write_512_multi,
    "avx512f,avx512bw,avx512cd,avx512dq,avx512vl,avx2,avx,fma,bmi1,bmi2",
    std::arch::x86_64::__m512i,
    std::arch::x86_64::_mm512_set1_epi64,
    std::arch::x86_64::_mm512_store_si512
);

#[doc = include_str!("test_fn_safety.md")]
pub unsafe fn lat_v2p_write_auto_multi(
    blocks: &[AllocationBlock], thread_id: usize, error_mode: ErrorMode,
    timing: &TestTiming, config: &TestMemoryConfig, progress: Option<&TestProgress>,
) -> LatencyTestStats {
    if is_x86_feature_detected!("avx512f") {
        return lat_v2p_write_512_multi(blocks, thread_id, error_mode, timing, config, progress);
    }
    if is_x86_feature_detected!("avx2") {
        return lat_v2p_write_256_multi(blocks, thread_id, error_mode, timing, config, progress);
    }
    lat_v2p_write_128_multi(blocks, thread_id, error_mode, timing, config, progress)
}

// ============================================================================
// Layout B Copy — three SIMD width variants + auto-dispatch (read-modify-write)
// ============================================================================

lat_v2p_copy_impl!(
    lat_v2p_copy_128_multi,
    "sse2",
    std::arch::x86_64::__m128i,
    std::arch::x86_64::_mm_load_si128,
    std::arch::x86_64::_mm_store_si128,
    std::arch::x86_64::_mm_xor_si128,
    std::arch::x86_64::_mm_set1_epi64x
);

lat_v2p_copy_impl!(
    lat_v2p_copy_256_multi,
    "avx2,avx,fma,bmi1,bmi2",
    std::arch::x86_64::__m256i,
    std::arch::x86_64::_mm256_load_si256,
    std::arch::x86_64::_mm256_store_si256,
    std::arch::x86_64::_mm256_xor_si256,
    std::arch::x86_64::_mm256_set1_epi64x
);

lat_v2p_copy_impl!(
    lat_v2p_copy_512_multi,
    "avx512f,avx512bw,avx512cd,avx512dq,avx512vl,avx2,avx,fma,bmi1,bmi2",
    std::arch::x86_64::__m512i,
    std::arch::x86_64::_mm512_load_si512,
    std::arch::x86_64::_mm512_store_si512,
    std::arch::x86_64::_mm512_xor_si512,
    std::arch::x86_64::_mm512_set1_epi64
);

#[doc = include_str!("test_fn_safety.md")]
pub unsafe fn lat_v2p_copy_auto_multi(
    blocks: &[AllocationBlock], thread_id: usize, error_mode: ErrorMode,
    timing: &TestTiming, config: &TestMemoryConfig, progress: Option<&TestProgress>,
) -> LatencyTestStats {
    if is_x86_feature_detected!("avx512f") {
        return lat_v2p_copy_512_multi(blocks, thread_id, error_mode, timing, config, progress);
    }
    if is_x86_feature_detected!("avx2") {
        return lat_v2p_copy_256_multi(blocks, thread_id, error_mode, timing, config, progress);
    }
    lat_v2p_copy_128_multi(blocks, thread_id, error_mode, timing, config, progress)
}

// ============================================================================
// Layout B WriteFull — three SIMD width variants + auto-dispatch
// ============================================================================

lat_v2p_write_full_impl!(
    lat_v2p_write_full_128_multi,
    "sse2",
    std::arch::x86_64::__m128i,
    std::arch::x86_64::_mm_set1_epi64x,
    std::arch::x86_64::_mm_store_si128,
    4_usize  // 4 × 16 bytes = full 64-byte cache line
);

lat_v2p_write_full_impl!(
    lat_v2p_write_full_256_multi,
    "avx2,avx,fma,bmi1,bmi2",
    std::arch::x86_64::__m256i,
    std::arch::x86_64::_mm256_set1_epi64x,
    std::arch::x86_64::_mm256_store_si256,
    2_usize  // 2 × 32 bytes = full 64-byte cache line
);

lat_v2p_write_full_impl!(
    lat_v2p_write_full_512_multi,
    "avx512f,avx512bw,avx512cd,avx512dq,avx512vl,avx2,avx,fma,bmi1,bmi2",
    std::arch::x86_64::__m512i,
    std::arch::x86_64::_mm512_set1_epi64,
    std::arch::x86_64::_mm512_store_si512,
    1_usize  // 1 × 64 bytes = full 64-byte cache line
);

#[doc = include_str!("test_fn_safety.md")]
pub unsafe fn lat_v2p_write_full_auto_multi(
    blocks: &[AllocationBlock], thread_id: usize, error_mode: ErrorMode,
    timing: &TestTiming, config: &TestMemoryConfig, progress: Option<&TestProgress>,
) -> LatencyTestStats {
    if is_x86_feature_detected!("avx512f") {
        return lat_v2p_write_full_512_multi(blocks, thread_id, error_mode, timing, config, progress);
    }
    if is_x86_feature_detected!("avx2") {
        return lat_v2p_write_full_256_multi(blocks, thread_id, error_mode, timing, config, progress);
    }
    lat_v2p_write_full_128_multi(blocks, thread_id, error_mode, timing, config, progress)
}

// ============================================================================
// Layout B CopyFull — three SIMD width variants + auto-dispatch
// ============================================================================

lat_v2p_copy_full_impl!(
    lat_v2p_copy_full_128_multi,
    "sse2",
    std::arch::x86_64::__m128i,
    std::arch::x86_64::_mm_load_si128,
    std::arch::x86_64::_mm_store_si128,
    std::arch::x86_64::_mm_xor_si128,
    std::arch::x86_64::_mm_set1_epi64x,
    4_usize
);

lat_v2p_copy_full_impl!(
    lat_v2p_copy_full_256_multi,
    "avx2,avx,fma,bmi1,bmi2",
    std::arch::x86_64::__m256i,
    std::arch::x86_64::_mm256_load_si256,
    std::arch::x86_64::_mm256_store_si256,
    std::arch::x86_64::_mm256_xor_si256,
    std::arch::x86_64::_mm256_set1_epi64x,
    2_usize
);

lat_v2p_copy_full_impl!(
    lat_v2p_copy_full_512_multi,
    "avx512f,avx512bw,avx512cd,avx512dq,avx512vl,avx2,avx,fma,bmi1,bmi2",
    std::arch::x86_64::__m512i,
    std::arch::x86_64::_mm512_load_si512,
    std::arch::x86_64::_mm512_store_si512,
    std::arch::x86_64::_mm512_xor_si512,
    std::arch::x86_64::_mm512_set1_epi64,
    1_usize
);

#[doc = include_str!("test_fn_safety.md")]
pub unsafe fn lat_v2p_copy_full_auto_multi(
    blocks: &[AllocationBlock], thread_id: usize, error_mode: ErrorMode,
    timing: &TestTiming, config: &TestMemoryConfig, progress: Option<&TestProgress>,
) -> LatencyTestStats {
    if is_x86_feature_detected!("avx512f") {
        return lat_v2p_copy_full_512_multi(blocks, thread_id, error_mode, timing, config, progress);
    }
    if is_x86_feature_detected!("avx2") {
        return lat_v2p_copy_full_256_multi(blocks, thread_id, error_mode, timing, config, progress);
    }
    lat_v2p_copy_full_128_multi(blocks, thread_id, error_mode, timing, config, progress)
}

// ============================================================================
// Lat-NTW-DRAM-Write — Non-temporal streaming write saturation experiment
// ============================================================================
//
// Hypothesis: per-write hardware commit latency is independently measurable if we saturate
// the WCBs / store buffer with NT stores, then sfence to force commit before stopping the
// timer. Each sample issues N NT stores then sfences and divides total time by N.
//
// NT stores bypass cache hierarchy → there's no per-tier difference, only one number exists:
// sustained per-write commit time at memory controller saturation. So we register only a
// single test (Lat-NTW-DRAM-Write) rather than per-tier variants.
//
// SIMD widths could matter for write-throughput (wider stores fewer instructions) but the
// commit time per cache line is what we're measuring — width should not change the per-line
// commit cost. We register width variants for comparison anyway.

macro_rules! lat_ntw_write_impl {
    (
        $pub_fn:ident,
        $target_feature:literal,
        $vec_type:ty,
        $set1_fn:path,
        $stream_fn:path,
        $vec_size:expr
    ) => {
        #[target_feature(enable = $target_feature)]
        #[doc = include_str!("test_fn_safety.md")]
        pub unsafe fn $pub_fn(
            blocks: &[AllocationBlock],
            thread_id: usize,
            _error_mode: ErrorMode,
            timing: &TestTiming,
            config: &TestMemoryConfig,
            _progress: Option<&TestProgress>,
        ) -> LatencyTestStats {
            use std::arch::x86_64::_mm_sfence;

            let test_name = "Lat-NTW-DRAM-Write";
            let cpu_ghz = config.tsc_frequency_ghz;
            if cpu_ghz == 0.0 {
                panic!("TSC frequency not detected");
            }

            let total_allocated: usize = blocks.iter().map(|b| b.buffer.size()).sum();
            let window_size = config.calculate_window_size(test_name, total_allocated);
            let test_blocks = prepare_blocks_for_window(blocks, window_size, test_name);

            if test_blocks.is_empty() {
                return empty_stats(test_name, thread_id, timing);
            }

            // Stores per sample. Large enough to saturate WCBs and dilute the warmup phase
            // (first ~12 stores fit in empty WCBs and appear instant).
            const STORES_PER_SAMPLE: usize = 4096;
            let pattern_word = 0x5A5A5A5A_5A5A5A5Au64 as i64;
            let pattern: $vec_type = $set1_fn(pattern_word);

            // Build a randomized address table per block. Each address is a 64-byte-aligned
            // cache line — we issue one SIMD-width store per address (writes $vec_size bytes
            // at the start of the line). The vec_size doesn't change per-line commit cost
            // (still one cache line) but changes how many instructions we issue.
            let mut addr_tables: Vec<Vec<*mut $vec_type>> = Vec::new();
            for tb in test_blocks.iter() {
                let base = tb.ptr as *mut u8;
                let working_bytes = tb.test_size.min(window_size);
                let line_count = working_bytes / CACHE_LINE_BYTES;
                if line_count < 2 {
                    addr_tables.push(Vec::new());
                    continue;
                }
                let mut perm: Vec<usize> = (0..line_count).collect();
                fisher_yates(&mut perm, thread_id ^ 0xC1);

                let mut table = Vec::with_capacity(STORES_PER_SAMPLE);
                for i in 0..STORES_PER_SAMPLE {
                    let line = perm[i % line_count];
                    let addr = base.add(line * CACHE_LINE_BYTES) as *mut $vec_type;
                    table.push(addr);
                }
                addr_tables.push(table);
            }

            let mut latencies_ns = Vec::with_capacity(10000);
            let mut total_ops = 0u64;
            let mut cycle = 0u32;
            let start_time = Instant::now();

            loop {
                cycle += 1;

                for (block_idx, _tb) in test_blocks.iter().enumerate() {
                    let table = &addr_tables[block_idx];
                    if table.is_empty() { continue; }

                    fence(Ordering::SeqCst);
                    let mut aux = 0u32;
                    let start_cyc = __rdtscp(&mut aux);

                    for &addr in table.iter() {
                        $stream_fn(addr, pattern);
                    }
                    _mm_sfence();

                    let end_cyc = __rdtscp(&mut aux);
                    fence(Ordering::SeqCst);

                    let delta = end_cyc - start_cyc;
                    let cycles_per_op = delta as f64 / STORES_PER_SAMPLE as f64;
                    let ns_per_op = cycles_per_op / cpu_ghz;

                    latencies_ns.push(ns_per_op);
                    total_ops += STORES_PER_SAMPLE as u64;

                    if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) { break; }
                }

                let elapsed_secs = start_time.elapsed().as_secs() as u32;
                if !timing.should_continue(cycle, elapsed_secs) { break; }
            }

            // Suppress unused-vec_size warning; the value is documentary and may be used in
            // future variants that issue multiple stores per cache line.
            let _ = $vec_size;
            finalize_stats(test_name, thread_id, total_ops, start_time, cycle, timing, latencies_ns)
        }
    }
}

/// Scalar (8-byte) NT-write variant. `_mm_stream_si64` writes a single u64 (8 bytes) per
/// store — fills only 1/8 of a cache line. Tests the lower bound of the WCB-fill-ratio
/// hypothesis: stores that leave the most empty bytes per cache line should produce the
/// slowest sustained commit time. Validates the progression Scalar < 128 ≤ 256 ≪ 512.
#[doc = include_str!("test_fn_safety.md")]
#[target_feature(enable = "sse4.2,sse4.1,ssse3,sse3,sse2,popcnt")]
pub unsafe fn lat_ntw_write_scalar_multi(
    blocks: &[AllocationBlock],
    thread_id: usize,
    _error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    _progress: Option<&TestProgress>,
) -> LatencyTestStats {
    use std::arch::x86_64::{_mm_sfence, _mm_stream_si64};

    let test_name = "Lat-NTW-DRAM-Write";
    let cpu_ghz = config.tsc_frequency_ghz;
    if cpu_ghz == 0.0 {
        panic!("TSC frequency not detected");
    }

    let total_allocated: usize = blocks.iter().map(|b| b.buffer.size()).sum();
    let window_size = config.calculate_window_size(test_name, total_allocated);
    let test_blocks = prepare_blocks_for_window(blocks, window_size, test_name);

    if test_blocks.is_empty() {
        return empty_stats(test_name, thread_id, timing);
    }

    const STORES_PER_SAMPLE: usize = 4096;
    const PATTERN: i64 = 0x5A5A5A5A_5A5A5A5Au64 as i64;

    let mut addr_tables: Vec<Vec<*mut i64>> = Vec::new();
    for tb in test_blocks.iter() {
        let base = tb.ptr;
        let working_bytes = tb.test_size.min(window_size);
        let line_count = working_bytes / CACHE_LINE_BYTES;
        if line_count < 2 {
            addr_tables.push(Vec::new());
            continue;
        }
        let mut perm: Vec<usize> = (0..line_count).collect();
        fisher_yates(&mut perm, thread_id ^ 0xC1);

        let mut table = Vec::with_capacity(STORES_PER_SAMPLE);
        for i in 0..STORES_PER_SAMPLE {
            let line = perm[i % line_count];
            let addr = base.add(line * CACHE_LINE_BYTES) as *mut i64;
            table.push(addr);
        }
        addr_tables.push(table);
    }

    let mut latencies_ns = Vec::with_capacity(10000);
    let mut total_ops = 0u64;
    let mut cycle = 0u32;
    let start_time = Instant::now();

    loop {
        cycle += 1;

        for (block_idx, _tb) in test_blocks.iter().enumerate() {
            let table = &addr_tables[block_idx];
            if table.is_empty() { continue; }

            fence(Ordering::SeqCst);
            let mut aux = 0u32;
            let start_cyc = __rdtscp(&mut aux);

            for &addr in table.iter() {
                _mm_stream_si64(addr, PATTERN);
            }
            _mm_sfence();

            let end_cyc = __rdtscp(&mut aux);
            fence(Ordering::SeqCst);

            let delta = end_cyc - start_cyc;
            let cycles_per_op = delta as f64 / STORES_PER_SAMPLE as f64;
            let ns_per_op = cycles_per_op / cpu_ghz;

            latencies_ns.push(ns_per_op);
            total_ops += STORES_PER_SAMPLE as u64;

            if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) { break; }
        }

        let elapsed_secs = start_time.elapsed().as_secs() as u32;
        if !timing.should_continue(cycle, elapsed_secs) { break; }
    }

    finalize_stats(test_name, thread_id, total_ops, start_time, cycle, timing, latencies_ns)
}

lat_ntw_write_impl!(
    lat_ntw_write_128_multi,
    "sse4.2,sse4.1,ssse3,sse3,sse2,popcnt",
    std::arch::x86_64::__m128i,
    std::arch::x86_64::_mm_set1_epi64x,
    std::arch::x86_64::_mm_stream_si128,
    16usize
);

lat_ntw_write_impl!(
    lat_ntw_write_256_multi,
    "avx2,avx,fma,bmi1,bmi2",
    std::arch::x86_64::__m256i,
    std::arch::x86_64::_mm256_set1_epi64x,
    std::arch::x86_64::_mm256_stream_si256,
    32usize
);

lat_ntw_write_impl!(
    lat_ntw_write_512_multi,
    "avx512f,avx512bw,avx512cd,avx512dq,avx512vl,avx2,avx,fma,bmi1,bmi2",
    std::arch::x86_64::__m512i,
    std::arch::x86_64::_mm512_set1_epi64,
    std::arch::x86_64::_mm512_stream_si512,
    64usize
);

#[doc = include_str!("test_fn_safety.md")]
pub unsafe fn lat_ntw_write_auto_multi(
    blocks: &[AllocationBlock], thread_id: usize, error_mode: ErrorMode,
    timing: &TestTiming, config: &TestMemoryConfig, progress: Option<&TestProgress>,
) -> LatencyTestStats {
    if is_x86_feature_detected!("avx512f") {
        return lat_ntw_write_512_multi(blocks, thread_id, error_mode, timing, config, progress);
    }
    if is_x86_feature_detected!("avx2") {
        return lat_ntw_write_256_multi(blocks, thread_id, error_mode, timing, config, progress);
    }
    lat_ntw_write_128_multi(blocks, thread_id, error_mode, timing, config, progress)
}

// ============================================================================
// Common stats helpers
// ============================================================================

fn empty_stats(test_name: &'static str, thread_id: usize, timing: &TestTiming) -> LatencyTestStats {
    LatencyTestStats {
        basic_stats: TestStats {
            name: test_name,
            action: TestAction::Latency,
            bytes_processed: 0,
            elapsed_ms: 0,
            thread_id,
            error_count: 0,
            total_operations: 0,
            cycles_completed: 0,
            cycles_planned: timing.cycles,
            stopped_by_time_limit: false,
        },
        latencies_ns: vec![],
        sample_count: 0,
        p1_ns: 0.0, p5_ns: 0.0, p10_ns: 0.0, p25_ns: 0.0, p50_ns: 0.0,
        p75_ns: 0.0, p90_ns: 0.0, p95_ns: 0.0, p99_ns: 0.0, p99_9_ns: 0.0,
        spread_ratio: 0.0,
    }
}

fn finalize_stats(
    test_name: &'static str,
    thread_id: usize,
    total_ops: u64,
    start_time: Instant,
    cycle: u32,
    timing: &TestTiming,
    latencies_ns: Vec<f64>,
) -> LatencyTestStats {
    let mut sorted = latencies_ns;
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let len = sorted.len();
    let percentile = |slice: &[f64], p: f64| -> f64 {
        if slice.is_empty() { return 0.0; }
        let n = slice.len();
        let idx = ((n as f64 - 1.0) * p / 100.0) as usize;
        slice[idx.min(n - 1)]
    };
    let p1 = percentile(&sorted, 1.0);
    let p5 = percentile(&sorted, 5.0);
    let p10 = percentile(&sorted, 10.0);
    let p25 = percentile(&sorted, 25.0);
    let p50 = percentile(&sorted, 50.0);
    let p75 = percentile(&sorted, 75.0);
    let p90 = percentile(&sorted, 90.0);
    let p95 = percentile(&sorted, 95.0);
    let p99 = percentile(&sorted, 99.0);
    let p99_9 = percentile(&sorted, 99.9);
    let spread = if p5 > 0.0 { p95 / p5 } else { 0.0 };

    let result = LatencyTestStats {
        basic_stats: TestStats {
            name: test_name,
            action: TestAction::Latency,
            bytes_processed: (total_ops * 8) as usize,
            elapsed_ms: start_time.elapsed().as_millis(),
            thread_id,
            error_count: 0,
            total_operations: total_ops,
            cycles_completed: cycle,
            cycles_planned: timing.cycles,
            stopped_by_time_limit: timing.cycles.is_none_or(|limit| cycle < limit),
        },
        latencies_ns: sorted,
        sample_count: len,
        p1_ns: p1, p5_ns: p5, p10_ns: p10, p25_ns: p25, p50_ns: p50,
        p75_ns: p75, p90_ns: p90, p95_ns: p95, p99_ns: p99, p99_9_ns: p99_9,
        spread_ratio: spread,
    };

    log::info!("[Thread {}] {} - {} samples | P5={:.2} P50={:.2} P95={:.2} P99={:.2} | Spread={:.2}x",
        thread_id, test_name, result.sample_count,
        result.p5_ns, result.p50_ns, result.p95_ns, result.p99_ns, result.spread_ratio);

    result
}
