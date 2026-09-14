# TMR Test Restructure Plan

**Created**: 2025-11-28
**Status**: Planning Phase

---

## Executive Summary

This document outlines the comprehensive restructuring of TMR's test framework to:
1. Align with TM5's proven INIT→TEST→VERIFY architecture
2. Introduce the "reps" concept for TM5-compatible inner loop repetitions
3. Standardize on u64 data types across all tests
4. Add granular per-phase timing and bandwidth metrics
5. Implement pure bandwidth tests (Read/Write/Copy) for accurate measurements

---

## 1. Architecture Overview

### 1.1 Current TMR Structure (INCORRECT)

```
┌─────────────────────────────────────────────────────┐
│ Per Cycle:                                          │
│   INIT → (SWAP → VERIFY_MIRRORED → SWAP) × N       │
│                     ↑                               │
│                     └── WRONG: Verifies mirrored    │
│                         pattern, not original       │
└─────────────────────────────────────────────────────┘
```

### 1.2 TM5 Structure (CORRECT - Target)

```
┌─────────────────────────────────────────────────────┐
│ Framework:                                          │
│   RS_Set (INIT) → Test_Check (TEST) → RS_Check     │
│                                        (VERIFY)     │
│                                                     │
│ Inside MirrorMove TEST:                             │
│   (Block0↔Block1 SWAP) × N times                   │
│   └── N = (duration% × 100) / 2000                 │
│                                                     │
│ Final VERIFY checks ORIGINAL pattern (restored)    │
└─────────────────────────────────────────────────────┘
```

### 1.3 New TMR Structure (PROPOSED)

```
┌─────────────────────────────────────────────────────────────┐
│ Hierarchy:                                                  │
│                                                             │
│   suite_cycles      = repeat entire test suite              │
│        ↓                                                    │
│   test_cycles       = repeat full INIT→TEST→VERIFY          │
│        ↓                                                    │
│   test_reps         = repeat TEST phase only (TM5 style)    │
│                                                             │
│ Per test_cycle:                                             │
│   ┌────────┐   ┌──────────────────┐   ┌────────┐           │
│   │  INIT  │ → │ TEST (× reps)    │ → │ VERIFY │           │
│   │ Phase  │   │ Phase            │   │ Phase  │           │
│   └────────┘   └──────────────────┘   └────────┘           │
│       ↑               ↑                    ↑                │
│       │               │                    │                │
│   Write pattern   Duration limit      Check original        │
│   to memory       applies HERE        pattern (XOR)         │
│                                                             │
└─────────────────────────────────────────────────────────────┘
```

---

## 2. Timing Model

### 2.1 Duration Semantics

| Metric | Scope | Purpose |
|--------|-------|---------|
| `elapsed_duration` | End-to-end (INIT+TEST+VERIFY) | Always reported, total wall time |
| `init_duration` | INIT phase only | Debugging/profiling |
| `test_duration` | TEST phase only | Duration limit applies here |
| `verify_duration` | VERIFY phase only | Debugging/profiling |

### 2.2 Duration Limit Application

```rust
// Duration limit ONLY constrains TEST phase
// INIT and VERIFY always run to completion

loop {
    // INIT: Always runs fully
    phase_stats.init_start = Instant::now();
    initialize_pattern(block);
    phase_stats.init_duration = phase_stats.init_start.elapsed();

    // TEST: Duration limit applies here
    phase_stats.test_start = Instant::now();
    while !should_stop && phase_stats.test_start.elapsed() < duration_limit {
        run_test_iteration(block);  // e.g., swap operation
        reps_completed += 1;
    }
    phase_stats.test_duration = phase_stats.test_start.elapsed();

    // VERIFY: Always runs fully (critical for correctness)
    phase_stats.verify_start = Instant::now();
    let errors = verify_pattern(block);
    phase_stats.verify_duration = phase_stats.verify_start.elapsed();
}
```

### 2.3 Bandwidth/Throughput Calculation

```rust
// Throughput calculated from ALL phases (end-to-end)
let total_bytes = init_bytes + test_bytes + verify_bytes;
let elapsed = init_duration + test_duration + verify_duration;
let throughput_gbps = (total_bytes as f64) / elapsed.as_secs_f64() / 1e9;

// Per-phase metrics (for debugging/profiling)
let test_throughput = test_bytes / test_duration;  // TEST phase only
```

---

## 3. Data Type Standardization

### 3.1 Current State (Inconsistent)

| Test Variant | Data Type | Pattern Width |
|--------------|-----------|---------------|
| Mem-Mirror (scalar) | u64 | 64-bit |
| Mem-Mirror-128 | i32 × 4 | 32-bit elements |
| Mem-Mirror-256 | i32 × 8 | 32-bit elements |
| Mem-Mirror-512 | i32 × 16 | 32-bit elements |

### 3.2 Target State (Standardized)

| Test Variant | Data Type | Pattern Width | Rationale |
|--------------|-----------|---------------|-----------|
| Mem-Mirror (scalar) | u64 | 64-bit | Native 64-bit CPU width |
| Mem-Mirror-128 | u64 × 2 | 64-bit elements | SSE2 with 64-bit lanes |
| Mem-Mirror-256 | u64 × 4 | 64-bit elements | AVX2 with 64-bit lanes |
| Mem-Mirror-512 | u64 × 8 | 64-bit elements | AVX-512 with 64-bit lanes |

### 3.3 Benefits of u64 Standardization

1. **CPU Alignment**: Native 64-bit operations on x86-64
2. **Pattern Consistency**: Same pattern formula across all variants
3. **Simplified Verification**: No sign-extension issues
4. **Better Coverage**: Full 64-bit address space per element

---

## 4. Per-Phase Statistics

### 4.1 PhaseStats Structure

```rust
#[derive(Debug, Clone, Default)]
pub struct PhaseStats {
    pub phase: TestPhase,
    pub duration: Duration,
    pub bytes_processed: u64,
    pub iterations: u64,
    pub errors_found: u64,
}

#[derive(Debug, Clone, Copy)]
pub enum TestPhase {
    Init,
    Test,
    Verify,
}
```

### 4.2 CycleStats Structure

```rust
#[derive(Debug, Clone, Default)]
pub struct CycleStats {
    pub cycle_number: u32,
    pub init: PhaseStats,
    pub test: PhaseStats,
    pub verify: PhaseStats,

    // Computed totals
    pub total_duration: Duration,
    pub total_bytes: u64,
    pub total_errors: u64,
}

impl CycleStats {
    pub fn throughput_gbps(&self) -> f64 {
        self.total_bytes as f64 / self.total_duration.as_secs_f64() / 1e9
    }
}
```

### 4.3 Hot Loop Optimization

```rust
// CRITICAL: Minimize hot loop overhead

// BAD: Complex calculations in hot loop
loop {
    process_block();
    bytes += block_size;
    let throughput = bytes as f64 / start.elapsed().as_secs_f64() / 1e9;  // SLOW
    progress.update(throughput);  // SLOW
}

// GOOD: Push raw stats infrequently, let progress thread calculate
let mut bytes_since_update = 0u64;
loop {
    process_block();
    bytes_since_update += block_size;

    // Infrequent update (every N iterations or time threshold)
    if iterations % 1000 == 0 {
        stats_tx.send(RawStats { bytes: bytes_since_update, errors: 0 });
        bytes_since_update = 0;
    }
}
```

---

## 5. Pure Bandwidth Tests (Priority: HIGH)

### 5.1 Motivation

Current `Spd-Saturate` test doesn't accurately measure pure bandwidth because:
- It interleaves read/write operations
- Pattern generation overhead affects measurements
- No separate read-only or write-only modes

### 5.2 Proposed Tests

#### 5.2.1 Pure WRITE Test

```rust
/// Measures write-only bandwidth
/// Pre-condition: Memory block already allocated
/// Uses non-temporal stores (MOVNTDQ/MOVNTPS) to bypass cache
pub fn bandwidth_write_avx512(block: &mut [u8]) -> BandwidthResult {
    let pattern = generate_simple_pattern();  // Constant pattern, no index calc

    let start = Instant::now();
    for chunk in block.chunks_exact_mut(64) {
        // Non-temporal store (writes directly to RAM, bypasses cache)
        unsafe { _mm512_stream_si512(chunk.as_mut_ptr() as *mut _, pattern); }
    }
    // CRITICAL: Memory fence after non-temporal stores
    unsafe { _mm_sfence(); }
    let elapsed = start.elapsed();

    BandwidthResult {
        bytes: block.len() as u64,
        duration: elapsed,
        bandwidth_gbps: block.len() as f64 / elapsed.as_secs_f64() / 1e9,
    }
}
```

#### 5.2.2 Pure READ Test

```rust
/// Measures read-only bandwidth
/// Pre-condition: Memory MUST be initialized first (avoid zero-page optimization)
/// Uses prefetching and streaming loads
pub fn bandwidth_read_avx512(block: &[u8]) -> BandwidthResult {
    let mut accumulator = _mm512_setzero_si512();  // Prevent optimization

    let start = Instant::now();
    for chunk in block.chunks_exact(64) {
        // Prefetch next cache line
        unsafe { _mm_prefetch(chunk.as_ptr().add(512) as *const i8, _MM_HINT_T0); }
        // Load and accumulate (prevents dead code elimination)
        let data = unsafe { _mm512_load_si512(chunk.as_ptr() as *const _) };
        accumulator = _mm512_xor_si512(accumulator, data);
    }
    let elapsed = start.elapsed();

    // Use accumulator to prevent optimization
    std::hint::black_box(accumulator);

    BandwidthResult { ... }
}
```

#### 5.2.3 Pure COPY Test

```rust
/// Measures copy bandwidth (read + write)
/// Similar to AIDA64's Copy test
pub fn bandwidth_copy_avx512(src: &[u8], dst: &mut [u8]) -> BandwidthResult {
    let start = Instant::now();
    for (src_chunk, dst_chunk) in src.chunks_exact(64).zip(dst.chunks_exact_mut(64)) {
        let data = unsafe { _mm512_load_si512(src_chunk.as_ptr() as *const _) };
        unsafe { _mm512_stream_si512(dst_chunk.as_mut_ptr() as *mut _, data); }
    }
    unsafe { _mm_sfence(); }
    let elapsed = start.elapsed();

    BandwidthResult {
        bytes: (src.len() + dst.len()) as u64,  // Read + Write bytes
        duration: elapsed,
        ...
    }
}
```

### 5.3 Implementation Order

1. **Write test first** - Ensures memory is allocated (triggers page faults)
2. **Read test second** - Memory already warm, measures pure read bandwidth
3. **Copy test last** - Measures combined read+write throughput

---

## 6. Test-Specific Restructuring

### 6.1 MirrorMove Restructuring

```rust
// CURRENT (Wrong)
fn mirror_move_cycle() {
    init_pattern();           // INIT
    for _ in 0..cycles {
        swap_blocks();        // TEST
        verify_mirrored();    // WRONG: checks mirrored pattern
        swap_blocks();        // Restore
    }
}

// TARGET (Correct - TM5 aligned)
fn mirror_move_cycle() {
    init_pattern();           // INIT phase (outside timing limit)

    for _ in 0..reps {        // TEST phase (duration limit applies)
        swap_blocks();        // Just swap, no verify
    }

    // After even number of swaps, pattern is restored
    verify_original();        // VERIFY phase (always runs)
}
```

### 6.2 SimpleTest Restructuring

```rust
// TM5 Structure: WRITE → READ×N (N determined by duration)
fn simple_test_cycle() {
    // INIT: Write pattern
    write_pattern(block);

    // TEST: Read and verify N times (duration-limited)
    for _ in 0..reps {
        verify_pattern(block);  // XOR-accumulate, check for errors
    }

    // VERIFY: Final verification (same as TEST iteration)
    // Note: For SimpleTest, TEST and VERIFY are the same operation
}
```

### 6.3 StuckBit Restructuring

```rust
fn stuck_bit_cycle() {
    // INIT: Write all-zeros, then all-ones pattern
    write_zeros(block);
    verify_zeros(block);  // Part of INIT

    write_ones(block);
    verify_ones(block);   // Part of INIT

    // TEST: Repeat with inverted patterns (duration-limited)
    for _ in 0..reps {
        write_pattern(block, pattern);
        verify_pattern(block, pattern);
        pattern = !pattern;
    }

    // VERIFY: Final state check
    final_verify(block);
}
```

---

## 7. Migration Plan

### Phase 1: Foundation (Week 1)

1. **Create PhaseStats/CycleStats structures**
   - Add to `src/test_framework.rs`
   - Implement accumulation and calculation methods

2. **Implement pure bandwidth tests**
   - `Bw-Write-512`, `Bw-Write-256`, `Bw-Write-128`
   - `Bw-Read-512`, `Bw-Read-256`, `Bw-Read-128`
   - `Bw-Copy-512`, `Bw-Copy-256`, `Bw-Copy-128`

3. **Standardize u64 data types**
   - Update SIMD pattern generation to use 64-bit lanes
   - Ensure consistent pattern formula across variants

### Phase 2: MirrorMove (Week 2)

4. **Restructure MirrorMove scalar**
   - Implement INIT→TEST(×reps)→VERIFY structure
   - Add per-phase timing collection
   - Verify TM5 compatibility

5. **Restructure MirrorMove SIMD variants**
   - Apply same structure to 128/256/512 variants
   - Update pattern generation for u64 lanes

### Phase 3: Other Tests (Week 3)

6. **Restructure SimpleTest**
7. **Restructure StuckBit**
8. **Restructure CacheBusting**
9. **Restructure remaining tests**

### Phase 4: Integration (Week 4)

10. **Update progress reporting**
    - Display per-phase metrics when available
    - Maintain backward compatibility with existing output

11. **Update JSON output format**
    - Add phase timing to results
    - Preserve existing fields for compatibility

12. **Testing and validation**
    - Compare results with TM5
    - Verify error detection rates
    - Benchmark performance

---

## 8. Backward Compatibility

### 8.1 Config File Compatibility

- TM5 `.cfg` files continue to work
- `cycles` parameter maps to `test_cycles`
- New `reps` parameter added (defaults to duration-based calculation for TM5 compat)

### 8.2 Output Compatibility

- Existing JSON fields preserved
- New phase timing fields added as optional
- `elapsed_duration` remains end-to-end

### 8.3 CLI Compatibility

- Existing flags unchanged
- New `--verbose-phases` flag for detailed phase output

---

## 9. Success Criteria

1. **MirrorMove produces same memory access pattern as TM5**
2. **Pure bandwidth tests show expected throughput** (within 5% of theoretical max)
3. **All tests use consistent u64 data types**
4. **Per-phase timing available for all tests**
5. **No regression in error detection rates**
6. **No significant performance degradation** (<5%)

---

## 10. Open Questions

1. Should `reps` be configurable per-test or global?
2. How should we handle tests where INIT is very long (e.g., latency tests)?
3. Should per-phase stats be opt-in for performance-critical scenarios?

---

## Appendix A: TM5 Reference

### Key Assembly Functions

| Function | Purpose | TMR Equivalent |
|----------|---------|----------------|
| `RS_Set` | Initialize pattern in memory | `init_pattern()` |
| `RS_Check` | Verify pattern, return errors | `verify_pattern()` |
| `MirrorMove_Check` | Swap blocks N times | `mirror_move_test()` |
| `ST_Check` | SimpleTest read verify | `simple_test()` |

### TM5 Swap Count Formula

```
swaps_per_call = (Duration% × 100) / 2000
```

For Duration=50%: `(50 × 100) / 2000 = 2.5` → 2 swaps per call

---

## Appendix B: SIMD Intrinsics Reference

| Operation | SSE2 | AVX2 | AVX-512 |
|-----------|------|------|---------|
| Load (aligned) | `_mm_load_si128` | `_mm256_load_si256` | `_mm512_load_si512` |
| Store (NT) | `_mm_stream_si128` | `_mm256_stream_si256` | `_mm512_stream_si512` |
| XOR | `_mm_xor_si128` | `_mm256_xor_si256` | `_mm512_xor_si512` |
| Set 64-bit | `_mm_set_epi64x` | `_mm256_set_epi64x` | `_mm512_set_epi64` |
| Fence | `_mm_sfence` | `_mm_sfence` | `_mm_sfence` |
