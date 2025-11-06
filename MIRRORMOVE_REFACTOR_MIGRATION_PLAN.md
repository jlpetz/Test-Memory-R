# MirrorMove Refactor - Migration Plan

## Executive Summary

Refactor MirrorMove and related tests to match TM5's architecture using zero-cost abstractions for shared code (init/verify), while maintaining TMR's modern responsiveness advantages.

## Core Design Principles

### 1. Zero-Cost Abstraction via Monomorphization
**No code duplication** - Use trait generics + `#[inline(always)]` for shared init/verify logic.

```rust
// Compiler generates specialized code at compile time (zero runtime cost)
init_block::<RefreshStablePattern, Avx512Verifier>(memory)  // Specialized for this combo
verify_block::<SimpleTestPattern, Avx2Verifier>(memory)     // Different specialization
```

Benefits:
- Single implementation of init/verify logic
- Zero runtime overhead (no vtables, no dynamic dispatch)
- Type safety at compile time
- Easy to add new patterns/SIMD variants

### 2. TM5-Style Test Architecture

**Key Insight from TM5**: Cycles only impact the TEST phase, not init/verify.

```rust
// TM5 Architecture (match this)
for cycle in 0..cycles {
    INIT(memory);           // Once per cycle

    for _ in 0..test_iterations {  // ← Cycles/duration controls THIS
        TEST(memory);       // Multiple times (e.g., 5 swaps for MirrorMove)
    }

    VERIFY(memory);         // Once per cycle
}
```

**TMR Current (WRONG)**:
```rust
// Everything repeats based on cycles - NOT how TM5 works!
loop {  // ← This controls everything
    init();
    mirror_swap();
    verify_mirrored();  // ← Verifies WRONG pattern!
    mirror_swap();
    // ← Missing final verify!
}
```

**TMR Target (CORRECT)**:
```rust
for cycle in 0..cycles {
    // INIT phase (once)
    init_block::<Pattern, Simd>(memory);

    // TEST phase (N iterations based on test_duration)
    let swap_count = calculate_swap_count(test_duration_percent);
    for _ in 0..swap_count {
        mirror_swap(memory);  // Just swap, no verify here!
    }

    // VERIFY phase (once, checks ORIGINAL pattern)
    verify_block::<Pattern, Simd>(memory);
}
```

### 3. Modern Responsiveness Design

**Keep TMR's advantages**:
- Shutdown check at block/chunk boundaries (every 4KB)
- Progress updates during all phases
- Per-phase indicators for UI (🔧 Init, 📊 Test, ✅ Verify)
- Non-blocking progress tracking

**Don't sacrifice**:
- Low latency shutdown (< 100ms)
- Granular progress visibility
- Per-phase performance stats for debugging

## TM5 Architecture Reference

From definitive analysis (TM5_MIRRORMOVE_DEFINITIVE_LOOP_ANALYSIS.md):

### What TM5 Does Per Cycle

```
MainThread.asm:681  → CallFunction(dRunTestNumber, Cmd_Check)  → MirrorMove_Check
                                                                   ↓
                                                    Does N swaps in a loop!
                                                    (swap_count = duration/2000)
                                                                   ↓
MainThread.asm:690  → CallFunction(0, Cmd_Check)               → RS_Check (verify)
MainThread.asm:696  → CallFunction(0, Cmd_Set)                 → RS_Set (init next)
```

**Key Points**:
1. Init happens via RefreshStable (`RS_Set`) - external to MirrorMove
2. MirrorMove only does swaps (`MirrorMove_Check`) - multiple per call
3. Verify happens via RefreshStable (`RS_Check`) - external to MirrorMove
4. Loop counter inside `MirrorMove_Check` determines swap count

### Swap Count Formula (TM5)

```asm
; mtests0.asm:1134-1145
mov eax, PatternJump.TestLength  ; Test duration % (100 = 100%)
mov edx, dTestTime                ; Global test time (100)
mul edx                           ; 100 × 100 = 10000
mov ecx, MirrorMove_Div_Down      ; 2000
div ecx                           ; 10000 / 2000 = 5
mov dWriteReadCycleCounter, eax   ; 5 swaps
```

Examples:
- 100% duration: 5 swaps
- 200% duration: 10 swaps
- 400% duration: 20 swaps

## Implementation Plan

### Phase 1: Zero-Cost Abstractions (Days 1-2)

#### 1.1 Pattern Generation Trait

**File**: `TMR-APP/src/pattern.rs` (new)

```rust
/// Pattern generator trait - zero-cost via monomorphization
pub trait PatternGenerator: Send + Sync {
    /// Generate pattern value for memory index
    /// MUST be #[inline(always)] for zero-cost abstraction
    #[inline(always)]
    fn generate(&self, index: usize) -> u64;

    fn pattern_name(&self) -> &'static str;
}

/// RefreshStable pattern (TM5 compatible)
/// Based on RS_GeneratePattern (mtests0.asm:1714-1757)
pub struct RefreshStablePattern {
    seed: u64,
}

impl PatternGenerator for RefreshStablePattern {
    #[inline(always)]
    fn generate(&self, index: usize) -> u64 {
        // TM5-compatible address-based pattern
        let addr = index as u64 * 8;
        let mut val = addr.wrapping_add(self.seed);
        val ^= val >> 17;
        val = val.wrapping_mul(0x27d4eb2d);
        val
    }

    fn pattern_name(&self) -> &'static str {
        "RefreshStable"
    }
}

/// SimpleTest pattern (self-contained, different from RefreshStable)
pub struct SimpleTestPattern {
    counter: u64,
}

impl PatternGenerator for SimpleTestPattern {
    #[inline(always)]
    fn generate(&self, index: usize) -> u64 {
        self.counter.wrapping_add(index as u64)
    }

    fn pattern_name(&self) -> &'static str {
        "SimpleTest"
    }
}
```

#### 1.2 SIMD Verification Trait

**File**: `TMR-APP/src/verification.rs` (new)

```rust
use std::arch::x86_64::*;
use crate::pattern::PatternGenerator;

/// SIMD verifier trait - zero-cost via monomorphization
pub trait SimdVerifier: Send + Sync {
    const SIMD_WIDTH: usize;
    const SIMD_NAME: &'static str;

    /// Verify memory matches pattern using SIMD
    /// Returns: number of errors detected
    /// MUST be #[inline(always)] for zero-cost abstraction
    #[inline(always)]
    unsafe fn verify_chunk<P: PatternGenerator>(
        memory: &[u64],
        pattern: &P,
        start_index: usize,
    ) -> u64;
}

/// AVX-512 verifier (8 × u64 = 64 bytes per iteration)
pub struct Avx512Verifier;

impl SimdVerifier for Avx512Verifier {
    const SIMD_WIDTH: usize = 64;
    const SIMD_NAME: &'static str = "AVX-512";

    #[inline(always)]
    unsafe fn verify_chunk<P: PatternGenerator>(
        memory: &[u64],
        pattern: &P,
        start_index: usize,
    ) -> u64 {
        let mut error_acc = _mm512_setzero_si512();

        // Process 8 u64s at a time
        for (chunk_idx, chunk) in memory.chunks_exact(8).enumerate() {
            let mem_idx = start_index + chunk_idx * 8;

            // Generate expected pattern (compiler will inline this!)
            let expected = [
                pattern.generate(mem_idx + 0),
                pattern.generate(mem_idx + 1),
                pattern.generate(mem_idx + 2),
                pattern.generate(mem_idx + 3),
                pattern.generate(mem_idx + 4),
                pattern.generate(mem_idx + 5),
                pattern.generate(mem_idx + 6),
                pattern.generate(mem_idx + 7),
            ];

            // Load actual values from memory
            let actual = _mm512_loadu_si512(chunk.as_ptr() as *const _);
            let expected_vec = _mm512_loadu_si512(expected.as_ptr() as *const _);

            // XOR: 0 if match, non-zero if error (TM5 style)
            let diff = _mm512_xor_si512(actual, expected_vec);

            // OR accumulate errors
            error_acc = _mm512_or_si512(error_acc, diff);
        }

        // Check if any bits set (errors detected)
        let mask = _mm512_test_epi64_mask(error_acc, error_acc);
        if mask != 0 { 1 } else { 0 }
    }
}

/// AVX2 verifier (4 × u64 = 32 bytes per iteration)
pub struct Avx2Verifier;

impl SimdVerifier for Avx2Verifier {
    const SIMD_WIDTH: usize = 32;
    const SIMD_NAME: &'static str = "AVX2";

    #[inline(always)]
    unsafe fn verify_chunk<P: PatternGenerator>(
        memory: &[u64],
        pattern: &P,
        start_index: usize,
    ) -> u64 {
        let mut error_acc = _mm256_setzero_si256();

        for (chunk_idx, chunk) in memory.chunks_exact(4).enumerate() {
            let mem_idx = start_index + chunk_idx * 4;

            let expected = [
                pattern.generate(mem_idx + 0),
                pattern.generate(mem_idx + 1),
                pattern.generate(mem_idx + 2),
                pattern.generate(mem_idx + 3),
            ];

            let actual = _mm256_loadu_si256(chunk.as_ptr() as *const _);
            let expected_vec = _mm256_loadu_si256(expected.as_ptr() as *const _);

            let diff = _mm256_xor_si256(actual, expected_vec);
            error_acc = _mm256_or_si256(error_acc, diff);
        }

        !_mm256_testz_si256(error_acc, error_acc) as u64
    }
}
```

#### 1.3 Generic Init/Verify Functions

**File**: `TMR-APP/src/test_phases.rs` (new)

```rust
use crate::pattern::PatternGenerator;
use crate::verification::SimdVerifier;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Default, Clone, Copy)]
pub struct PhaseStats {
    pub bytes_processed: u64,
    pub duration_ns: u64,
    pub errors: u64,
}

/// Generic init function - works with ANY pattern via monomorphization
#[inline(always)]
pub unsafe fn init_block<P: PatternGenerator>(
    memory: &mut [u64],
    pattern: &P,
    start_index: usize,
    shutdown_flag: &AtomicBool,
) -> PhaseStats {
    let start = std::time::Instant::now();
    let mut bytes = 0u64;

    const CHUNK_SIZE: usize = 4096 / 8; // 4KB chunks for responsiveness

    for (chunk_idx, chunk) in memory.chunks_mut(CHUNK_SIZE).enumerate() {
        // Modern responsiveness: check shutdown every 4KB
        if shutdown_flag.load(Ordering::Relaxed) {
            break;
        }

        let mem_idx = start_index + chunk_idx * CHUNK_SIZE;

        // Compiler inlines pattern.generate() here!
        for (i, slot) in chunk.iter_mut().enumerate() {
            *slot = pattern.generate(mem_idx + i);
        }

        bytes += chunk.len() as u64 * 8;
    }

    PhaseStats {
        bytes_processed: bytes,
        duration_ns: start.elapsed().as_nanos() as u64,
        errors: 0,
    }
}

/// Generic verify function - works with ANY pattern + SIMD combo via monomorphization
#[inline(always)]
pub unsafe fn verify_block<P: PatternGenerator, V: SimdVerifier>(
    memory: &[u64],
    pattern: &P,
    start_index: usize,
    shutdown_flag: &AtomicBool,
) -> PhaseStats {
    let start = std::time::Instant::now();
    let mut bytes = 0u64;
    let mut errors = 0u64;

    const CHUNK_SIZE: usize = 4096 / 8; // 4KB chunks

    for (chunk_idx, chunk) in memory.chunks(CHUNK_SIZE).enumerate() {
        // Modern responsiveness: check shutdown every 4KB
        if shutdown_flag.load(Ordering::Relaxed) {
            break;
        }

        let mem_idx = start_index + chunk_idx * CHUNK_SIZE;

        // Compiler generates specialized code for this P+V combo!
        errors += V::verify_chunk(chunk, pattern, mem_idx);
        bytes += chunk.len() as u64 * 8;
    }

    PhaseStats {
        bytes_processed: bytes,
        duration_ns: start.elapsed().as_nanos() as u64,
        errors,
    }
}
```

### Phase 2: Refactor MirrorMove (Days 3-4)

#### 2.1 MirrorMove Structure (TM5-Style)

**File**: `TMR-APP/src/tests.rs` (modify existing)

```rust
/// MirrorMove512 - Framework-separated (like TM5)
pub struct MirrorMove512 {
    pattern: RefreshStablePattern,
    swap_count: usize,  // How many swaps per TEST phase
}

impl MirrorMove512 {
    pub fn new(test_duration_percent: u32) -> Self {
        // Match TM5 formula: (duration% × 100) / 2000
        let swap_count = ((test_duration_percent * 100) / 2000).max(2) as usize;
        let swap_count = swap_count & !1; // Ensure even (data restored)

        Self {
            pattern: RefreshStablePattern { seed: 0x12345678 },
            swap_count,
        }
    }

    /// Run one complete cycle: INIT → TEST → VERIFY
    pub unsafe fn run_cycle(
        &self,
        memory: &mut [u64],
        shutdown_flag: &AtomicBool,
    ) -> CycleResult {
        let mut stats = CycleStats::default();

        // === PHASE 1: INIT (once per cycle) ===
        stats.init = init_block(
            memory,
            &self.pattern,
            0,
            shutdown_flag,
        );

        if shutdown_flag.load(Ordering::Relaxed) {
            return CycleResult::Shutdown(stats);
        }

        // === PHASE 2: TEST (N swaps, like TM5) ===
        let test_start = Instant::now();

        for swap_iter in 0..self.swap_count {
            // Modern responsiveness: check every swap
            if shutdown_flag.load(Ordering::Relaxed) {
                stats.test.duration_ns = test_start.elapsed().as_nanos() as u64;
                return CycleResult::Shutdown(stats);
            }

            // Pure swap operation (no verify here!)
            self.mirror_swap_avx512(memory);

            // Accounting: each swap reads N + writes N
            stats.test.bytes_processed += memory.len() as u64 * 8 * 2;
        }

        stats.test.duration_ns = test_start.elapsed().as_nanos() as u64;

        // After even swaps, memory is restored to ORIGINAL pattern

        // === PHASE 3: VERIFY (once per cycle, check ORIGINAL) ===
        stats.verify = if has_avx512!() {
            verify_block::<_, Avx512Verifier>(
                memory,
                &self.pattern,
                0,
                shutdown_flag,
            )
        } else {
            verify_block::<_, Avx2Verifier>(
                memory,
                &self.pattern,
                0,
                shutdown_flag,
            )
        };

        if stats.verify.errors > 0 {
            CycleResult::Errors(stats)
        } else {
            CycleResult::Success(stats)
        }
    }

    /// Mirror swap (pure operation, no init/verify)
    #[inline(always)]
    unsafe fn mirror_swap_avx512(&self, memory: &mut [u64]) {
        // Keep existing implementation
        let len = memory.len();
        let mut idx1 = 0;
        let mut idx2 = len - 8;

        while idx1 < idx2 {
            let first = _mm512_loadu_si512(memory.as_ptr().add(idx1) as *const _);
            let last = _mm512_loadu_si512(memory.as_ptr().add(idx2) as *const _);

            _mm512_storeu_si512(memory.as_mut_ptr().add(idx1) as *mut _, last);
            _mm512_storeu_si512(memory.as_mut_ptr().add(idx2) as *mut _, first);

            idx1 += 8;
            idx2 -= 8;
        }
    }
}

#[derive(Default)]
pub struct CycleStats {
    pub init: PhaseStats,
    pub test: PhaseStats,
    pub verify: PhaseStats,
}

pub enum CycleResult {
    Success(CycleStats),
    Errors(CycleStats),
    Shutdown(CycleStats),
}
```

#### 2.2 Thread Integration (TM5-Style Cycles)

**File**: `TMR-APP/src/tests.rs` (modify)

```rust
pub fn mirror_move_512(
    test_blocks: &[TestBlock],
    runtime_config: &RuntimeConfig,
    thread_info: &ThreadInfo,
    result: &mut TestResult,
) -> TestExitReason {
    let test = MirrorMove512::new(runtime_config.test_duration_percent);

    let max_cycles = runtime_config.max_cycles.unwrap_or(usize::MAX);

    // TM5-style: Cycles control outer loop
    for cycle in 0..max_cycles {
        // Modern responsiveness: check between cycles
        if runtime_config.shutdown_flag.load(Ordering::Relaxed) {
            return TestExitReason::Shutdown;
        }

        // Process all blocks for this cycle
        for test_block in test_blocks {
            let memory = unsafe {
                std::slice::from_raw_parts_mut(
                    test_block.address as *mut u64,
                    test_block.test_size / 8,
                )
            };

            // Run one cycle: INIT → TEST (N swaps) → VERIFY
            let cycle_result = unsafe {
                test.run_cycle(memory, &runtime_config.shutdown_flag)
            };

            match cycle_result {
                CycleResult::Shutdown(stats) => {
                    result.accumulate_stats(stats);
                    return TestExitReason::Shutdown;
                }

                CycleResult::Errors(stats) => {
                    result.accumulate_stats(stats);
                    result.chunk_errors += 1;

                    if runtime_config.error_mode == ErrorMode::Halt {
                        return TestExitReason::ErrorDetected;
                    }
                }

                CycleResult::Success(stats) => {
                    result.accumulate_stats(stats);
                }
            }

            // Modern responsiveness: progress updates every N blocks
            if test_block.block_index % 16 == 0 {
                report_progress(thread_info, result);
            }
        }

        result.cycles_completed = cycle + 1;
    }

    TestExitReason::Completed
}
```

### Phase 3: Results & Progress (Day 5)

#### 3.1 Per-Phase Statistics

**File**: `TMR-APP/src/results.rs` (modify)

```rust
#[derive(Clone)]
pub struct TestResult {
    // Per-phase accounting (match TM5 architecture)
    pub init_bytes: u64,
    pub init_duration_ns: u64,

    pub test_bytes: u64,
    pub test_duration_ns: u64,

    pub verify_bytes: u64,
    pub verify_duration_ns: u64,
    pub verify_errors: u64,

    pub cycles_completed: u64,
    pub chunk_errors: u64,
}

impl TestResult {
    pub fn accumulate_stats(&mut self, stats: CycleStats) {
        self.init_bytes += stats.init.bytes_processed;
        self.init_duration_ns += stats.init.duration_ns;

        self.test_bytes += stats.test.bytes_processed;
        self.test_duration_ns += stats.test.duration_ns;

        self.verify_bytes += stats.verify.bytes_processed;
        self.verify_duration_ns += stats.verify.duration_ns;
        self.verify_errors += stats.verify.errors;
    }

    pub fn total_bytes(&self) -> u64 {
        self.init_bytes + self.test_bytes + self.verify_bytes
    }

    pub fn phase_throughputs(&self) -> (f64, f64, f64) {
        let init_gbps = throughput_gbps(self.init_bytes, self.init_duration_ns);
        let test_gbps = throughput_gbps(self.test_bytes, self.test_duration_ns);
        let verify_gbps = throughput_gbps(self.verify_bytes, self.verify_duration_ns);
        (init_gbps, test_gbps, verify_gbps)
    }
}
```

## Accounting Correction

### Before (Broken)
```rust
// WRONG - verifies mirrored pattern, missing final verify
bytes_processed = memory.len() * 3;  // ???
```

### After (Correct, TM5-Style)

```rust
// Per cycle, per block:
init_bytes   = N              // Write pattern once
test_bytes   = swap_count × 2N  // Each swap: read N + write N
verify_bytes = N              // Read pattern once

total = N + (swap_count × 2N) + N
      = (2 + swap_count × 2) × N

// Examples:
// swap_count = 5:  total = 12N per cycle
// swap_count = 10: total = 22N per cycle
```

## Migration Checklist

### Days 1-2: Zero-Cost Abstractions
- [ ] Create `pattern.rs` with PatternGenerator trait
- [ ] Create `verification.rs` with SimdVerifier trait
- [ ] Create `test_phases.rs` with init_block/verify_block
- [ ] Verify zero-cost via `cargo asm` (check monomorphization)
- [ ] Unit tests for patterns and verifiers

### Days 3-4: MirrorMove Refactor
- [ ] Refactor MirrorMove512 (TM5-style: INIT → TEST → VERIFY)
- [ ] Fix swap count calculation (match TM5 formula)
- [ ] Remove incorrect mirrored pattern verify
- [ ] Add final verify (original pattern)
- [ ] Fix byte accounting
- [ ] Refactor MirrorMove256, MirrorMove128

### Day 5: Progress & Stats
- [ ] Add per-phase stats to TestResult
- [ ] Update progress reporting
- [ ] Add phase indicators to display
- [ ] Test shutdown responsiveness during all phases

### Day 6-7: Testing & Validation
- [ ] Compare with TM5 (same error detection)
- [ ] Benchmark throughput (should exceed TM5)
- [ ] Verify zero-cost abstraction (no overhead)
- [ ] Update documentation

## Success Criteria

1. **Zero Duplication**: Init/verify code shared via traits (single implementation)
2. **Zero Cost**: Monomorphization verified via assembly inspection
3. **TM5 Architecture**: Cycles control TEST phase only, init/verify once per cycle
4. **Correctness**: Same error detection as TM5
5. **Performance**: >= TM5 throughput (better due to parallel init)
6. **Responsiveness**: CTRL+C < 100ms during any phase

## Key Benefits

### vs. Current TMR
- **Correct verification**: Checks ORIGINAL pattern (not mirrored)
- **Complete testing**: Final verify after swaps
- **Accurate accounting**: Per-phase byte counts
- **No duplication**: Shared init/verify via traits

### vs. TM5
- **Parallel init**: All memory initialized simultaneously (vs. sequential AWE)
- **Modern responsiveness**: Shutdown/progress during all phases
- **Type safety**: Compile-time verification
- **Better observability**: Per-phase performance stats
