# Framework Refactor Implementation Plan - Zero-Cost Abstractions

## Part 1: Zero-Cost SIMD Verification

### The Rust Feature: Monomorphization + Trait Generics

When you write:
```rust
pub trait SIMDVerifier {
    unsafe fn verify(memory: &[u64], expected: &[u64]) -> u64;
    unsafe fn verify_with_pattern_fn<F>(memory: &[u64], pattern_fn: F) -> u64
    where F: Fn(usize) -> u64;
}
```

And use it with generics:
```rust
fn verify_memory<V: SIMDVerifier>(memory: &[u64], expected: &[u64]) -> u64 {
    unsafe { V::verify(memory, expected) }
}
```

**What happens at compile time:**
1. **Monomorphization**: Compiler creates separate function for each concrete type
2. **Inlining**: With `#[inline(always)]`, function bodies are inserted directly
3. **Dead code elimination**: Unused code paths removed
4. **Constant propagation**: Known values compiled in
5. **Auto-vectorization**: LLVM can optimize further

**Result**: Zero overhead! As fast as hand-written inline code.

### Implementation Structure

```rust
// src/simd_verify.rs - Zero-cost abstraction layer

use std::arch::x86_64::*;

/// Zero-cost SIMD verification trait
/// Compiler monomorphizes this to specialized code with no overhead
pub trait SIMDVerifier: Send + Sync {
    /// Verify memory against expected values
    #[inline(always)]
    unsafe fn verify(memory: &[u64], expected: &[u64]) -> u64;

    /// Verify memory using pattern generator function
    #[inline(always)]
    unsafe fn verify_with_pattern<F>(memory: &[u64], pattern_fn: F) -> u64
    where F: Fn(usize) -> u64;

    /// Get SIMD width in elements
    const SIMD_WIDTH: usize;

    /// Get SIMD type name for reporting
    fn simd_type_name() -> &'static str;
}

/// AVX-512 implementation
pub struct AVX512Verifier;

impl SIMDVerifier for AVX512Verifier {
    const SIMD_WIDTH: usize = 8;

    fn simd_type_name() -> &'static str { "AVX-512" }

    #[inline(always)]
    unsafe fn verify(memory: &[u64], expected: &[u64]) -> u64 {
        let mut error_acc = _mm512_setzero_si512();
        let mut error_count = 0u64;

        for (actual_chunk, expected_chunk) in
            memory.chunks_exact(8).zip(expected.chunks_exact(8))
        {
            let actual_vec = _mm512_loadu_si512(actual_chunk.as_ptr() as *const i32);
            let expected_vec = _mm512_loadu_si512(expected_chunk.as_ptr() as *const i32);
            let diff = _mm512_xor_si512(actual_vec, expected_vec);
            error_acc = _mm512_or_si512(error_acc, diff);
        }

        // Extract error bits
        let mask = _mm512_test_epi64_mask(error_acc, error_acc);
        if mask != 0 {
            // Count individual mismatches in tail
            for (a, e) in memory.iter().zip(expected.iter()) {
                if a != e { error_count += 1; }
            }
        }

        error_count
    }

    #[inline(always)]
    unsafe fn verify_with_pattern<F>(memory: &[u64], pattern_fn: F) -> u64
    where F: Fn(usize) -> u64
    {
        let mut error_acc = _mm512_setzero_si512();
        let mut error_count = 0u64;

        for (chunk_idx, actual_chunk) in memory.chunks_exact(8).enumerate() {
            let base_idx = chunk_idx * 8;

            // Generate expected pattern inline (gets optimized!)
            let expected = [
                pattern_fn(base_idx + 0),
                pattern_fn(base_idx + 1),
                pattern_fn(base_idx + 2),
                pattern_fn(base_idx + 3),
                pattern_fn(base_idx + 4),
                pattern_fn(base_idx + 5),
                pattern_fn(base_idx + 6),
                pattern_fn(base_idx + 7),
            ];

            let actual_vec = _mm512_loadu_si512(actual_chunk.as_ptr() as *const i32);
            let expected_vec = _mm512_loadu_si512(expected.as_ptr() as *const i32);
            let diff = _mm512_xor_si512(actual_vec, expected_vec);
            error_acc = _mm512_or_si512(error_acc, diff);
        }

        // Extract and count errors
        let mask = _mm512_test_epi64_mask(error_acc, error_acc);
        if mask != 0 {
            for (idx, &actual) in memory.iter().enumerate() {
                let expected = pattern_fn(idx);
                if actual != expected { error_count += 1; }
            }
        }

        error_count
    }
}

/// AVX2 implementation (similar pattern)
pub struct AVX2Verifier;
impl SIMDVerifier for AVX2Verifier {
    const SIMD_WIDTH: usize = 4;
    fn simd_type_name() -> &'static str { "AVX2" }
    // ... similar implementation with _mm256_* intrinsics
}

/// SSE4.1 implementation
pub struct SSE41Verifier;
impl SIMDVerifier for SSE41Verifier {
    const SIMD_WIDTH: usize = 2;
    fn simd_type_name() -> &'static str { "SSE4.1" }
    // ... similar implementation with _mm_* intrinsics
}

/// Scalar fallback
pub struct ScalarVerifier;
impl SIMDVerifier for ScalarVerifier {
    const SIMD_WIDTH: usize = 1;
    fn simd_type_name() -> &'static str { "Scalar" }

    #[inline(always)]
    unsafe fn verify(memory: &[u64], expected: &[u64]) -> u64 {
        memory.iter().zip(expected.iter())
            .filter(|(a, e)| a != e)
            .count() as u64
    }

    #[inline(always)]
    unsafe fn verify_with_pattern<F>(memory: &[u64], pattern_fn: F) -> u64
    where F: Fn(usize) -> u64
    {
        memory.iter().enumerate()
            .filter(|(idx, &actual)| actual != pattern_fn(*idx))
            .count() as u64
    }
}
```

### Usage in Tests (Zero Overhead!)

```rust
impl MirrorMove<AVX512Verifier> {
    fn verify(&self, memory: &[u64]) -> u64 {
        // Pattern generator (gets inlined!)
        let pattern_fn = |idx: usize| -> u64 {
            (idx as u64)
                .wrapping_mul(0x0123456789ABCDEFu64)
                .wrapping_add(self.thread_id as u64)
        };

        // This becomes a single inline function with no call overhead!
        unsafe { AVX512Verifier::verify_with_pattern(memory, pattern_fn) }
    }
}

// Compiler output: Fully inlined, no function calls, as if you wrote it all inline!
```

**Key Point**: With `#[inline(always)]` and monomorphization, there is ZERO overhead. The trait abstraction disappears at compile time!

---

## Part 2: INIT Phase Accounting Strategy

### The Problem

**Current TMR**: Init happens once inside test loop, but not accounted properly

**TM5 with AWE**: Couldn't access all memory at once, so worked in windows

**TMR without AWE**: Can access ALL memory, but should we?

### How Did TM5 Handle This?

Based on code analysis, TM5 with AWE likely used a **streaming window approach**:

```
For each 1GB window:
    1. Map physical memory to virtual address (AWE)
    2. Call init (write pattern to window)
    3. Call test (e.g., MirrorMove on window)
    4. Call verify (check pattern in window)
    5. Unmap window
    Repeat for next window
```

**Evidence**:
- Test functions operate on single block (pBlock, dBlockSize)
- Framework manages AWE mapping
- Tests are window-agnostic (don't know about AWE)
- Each window processed independently

### TMR Advantages (No AWE Needed)

We can access all memory directly, BUT we still use **windows** for:
1. **Cache efficiency**: Keep working set small
2. **Interleaving**: Multiple threads don't fight over same cache lines
3. **Progress tracking**: Report per-window progress
4. **Error localization**: Know which memory region failed

**Current TMR architecture already does this!** (per-thread windows)

### INIT Phase Accounting Options

#### Option A: Separate Init Phase (TM5-like)

```rust
// Runner orchestration
fn run_test_suite() {
    // === INIT PHASE (separate, before timing) ===
    println!("Initializing memory...");
    let init_start = Instant::now();

    for thread in threads {
        for block in thread.blocks {
            test.init(block);
        }
    }

    let init_elapsed = init_start.elapsed();
    println!("Init complete: {:.2}s, {} GiB written",
        init_elapsed.as_secs_f64(),
        total_bytes / (1024*1024*1024));

    // === TEST PHASE (timed) ===
    let test_start = Instant::now();

    for cycle in 0..cycles {
        for thread in threads {
            for block in thread.blocks {
                let stats = test.test(block);
                // accumulate bytes_processed (does NOT include init!)
            }
        }
    }

    let test_elapsed = test_start.elapsed();

    // === VERIFY PHASE (separate, after timing) ===
    println!("Verifying memory...");
    let verify_start = Instant::now();

    let errors = test.verify_all_blocks();

    let verify_elapsed = verify_start.elapsed();

    // === REPORTING ===
    println!("
┌─────────────────────────────────────────┐
│ Test Results: MirrorMove512             │
├─────────────────────────────────────────┤
│ Init Phase:                             │
│   Time:       {:.2}s                    │
│   Written:    {} GiB                    │
│                                         │
│ Test Phase:                             │
│   Cycles:     {}                        │
│   Time:       {:.2}s                    │
│   Processed:  {} GiB                    │
│   Throughput: {} GiB/s                  │
│                                         │
│ Verify Phase:                           │
│   Time:       {:.2}s                    │
│   Read:       {} GiB                    │
│   Errors:     {}                        │
└─────────────────────────────────────────┘
    ", init_elapsed, init_gib,
       cycles, test_elapsed, test_gib, throughput,
       verify_elapsed, verify_gib, errors);
}
```

**Pros**:
- ✅ Clean separation of concerns
- ✅ TM5 compatible
- ✅ Init doesn't skew throughput calculations
- ✅ Can see init time separately (useful for debugging)
- ✅ Verify time separate (can spot verify bottlenecks)

**Cons**:
- ❌ Requires refactoring test runner
- ❌ Changes reporting format
- ❌ More complex orchestration

#### Option B: Include Init in First Cycle (Current, but Fixed)

```rust
fn run_test() {
    let start = Instant::now();

    for cycle in 0..cycles {
        if cycle == 0 {
            // First cycle: init + test
            test.init(memory);
            bytes_processed += memory.len() * 8;  // Count init!
        }

        // All cycles: test
        let stats = test.test(memory);
        bytes_processed += stats.bytes;
    }

    let elapsed = start.elapsed();

    // Throughput includes init
    let throughput = bytes_processed / elapsed.as_secs_f64();
}
```

**Pros**:
- ✅ Simple to implement
- ✅ No changes to runner
- ✅ Init is accounted for

**Cons**:
- ❌ First cycle slower (skews per-cycle stats)
- ❌ Throughput includes one-time init (misleading for multi-cycle)
- ❌ Not TM5 compatible

#### Option C: Pre-warm Before Timing (Hybrid)

```rust
fn run_test() {
    // Pre-warm: init outside timing
    test.init_all(memory);

    // Test: timed
    let start = Instant::now();

    for cycle in 0..cycles {
        let stats = test.test(memory);
        bytes_processed += stats.bytes;  // Does NOT include init
    }

    let elapsed = start.elapsed();

    // Verify: outside timing
    let errors = test.verify(memory);
}
```

**Pros**:
- ✅ Init doesn't skew throughput
- ✅ Simple to implement
- ✅ Clean semantics (test only measures test operations)

**Cons**:
- ❌ Init time not reported (can't see if init is slow)
- ❌ Not fully TM5 compatible (TM5 times verify)

### My Strong Recommendation: Option A (Separate Phases)

**Why**:
1. **TM5 Compatibility**: Matches original architecture exactly
2. **Clean Metrics**: Each phase measured independently
3. **User Visibility**: See init, test, verify times separately
4. **Debugging**: Can identify slow init vs slow test vs slow verify
5. **Accurate**: Throughput only measures what the test actually does

**Implementation Strategy**:

```rust
// src/test_framework.rs

pub struct TestPhaseStats {
    pub init_time: Duration,
    pub init_bytes: u64,
    pub test_time: Duration,
    pub test_bytes: u64,
    pub test_cycles: u32,
    pub verify_time: Duration,
    pub verify_bytes: u64,
    pub errors: u64,
}

impl TestPhaseStats {
    pub fn test_throughput_gib_s(&self) -> f64 {
        // Throughput ONLY from test phase (excludes init/verify)
        (self.test_bytes as f64 / (1024.0 * 1024.0 * 1024.0)) / self.test_time.as_secs_f64()
    }

    pub fn total_time(&self) -> Duration {
        self.init_time + self.test_time + self.verify_time
    }
}

pub trait MemoryTest: Send + Sync {
    /// Initialize memory (called once before test cycles)
    fn init(&mut self, memory: &mut [u64]) -> u64; // Returns bytes written

    /// Run test operation (called repeatedly)
    fn test(&mut self, memory: &mut [u64]) -> TestOpStats; // Returns bytes processed

    /// Verify memory (called once after test cycles)
    fn verify(&self, memory: &[u64]) -> VerifyStats; // Returns errors + bytes read
}

pub struct TestRunner {
    // ...
}

impl TestRunner {
    pub fn run_test<T: MemoryTest>(&mut self, test: &mut T) -> TestPhaseStats {
        // === PHASE 1: INIT ===
        let init_start = Instant::now();
        let init_bytes: u64 = self.all_blocks
            .par_iter_mut()
            .map(|block| test.init(&mut block.memory))
            .sum();
        let init_time = init_start.elapsed();

        log::info!("Init complete: {:.2}s, {:.2} GiB",
            init_time.as_secs_f64(),
            init_bytes as f64 / (1024.0 * 1024.0 * 1024.0));

        // === PHASE 2: TEST ===
        let test_start = Instant::now();
        let mut test_bytes = 0u64;

        for cycle in 0..self.config.cycles {
            let cycle_bytes: u64 = self.all_blocks
                .par_iter_mut()
                .map(|block| test.test(&mut block.memory).bytes_processed)
                .sum();

            test_bytes += cycle_bytes;

            if self.should_stop() { break; }
        }

        let test_time = test_start.elapsed();

        // === PHASE 3: VERIFY ===
        let verify_start = Instant::now();
        let verify_stats: VerifyStats = self.all_blocks
            .par_iter()
            .map(|block| test.verify(&block.memory))
            .reduce(|| VerifyStats::default(), |a, b| a + b);
        let verify_time = verify_start.elapsed();

        TestPhaseStats {
            init_time, init_bytes,
            test_time, test_bytes, test_cycles: self.config.cycles,
            verify_time, verify_bytes: verify_stats.bytes_read,
            errors: verify_stats.errors,
        }
    }
}
```

### Reporting Format

```
╔═══════════════════════════════════════════════════════════╗
║              MirrorMove512 - Test Results                 ║
╠═══════════════════════════════════════════════════════════╣
║ INITIALIZATION PHASE                                      ║
║   Duration:        2.34s                                  ║
║   Data Written:    32.0 GiB                               ║
║   Write Speed:     13.7 GiB/s                             ║
║                                                           ║
║ TEST PHASE                                                ║
║   Cycles:          10                                     ║
║   Duration:        45.67s                                 ║
║   Data Processed:  640.0 GiB                              ║
║   Throughput:      14.0 GiB/s  ← Pure test performance   ║
║                                                           ║
║ VERIFICATION PHASE                                        ║
║   Duration:        2.15s                                  ║
║   Data Read:       32.0 GiB                               ║
║   Read Speed:      14.9 GiB/s                             ║
║   Errors Found:    0                                      ║
║                                                           ║
║ TOTAL RUNTIME:     50.16s                                 ║
╚═══════════════════════════════════════════════════════════╝
```

**Key insight**: Users see **test throughput** (14.0 GiB/s) which is pure test performance, separate from init/verify overhead!

---

## Part 3: AWE vs Direct Access - What TMR Can Do Better

### TM5 with AWE Limitation

```
Physical Memory: [========== 16 GiB ==========]
Virtual Address: [1 GiB window] ← Can only see this much at once

Test Flow:
  Map window 1 (physical 0-1GB → virtual 0-1GB)
    Init window 1
    Test window 1
    Verify window 1
  Unmap window 1

  Map window 2 (physical 1-2GB → virtual 0-1GB)  ← Reuse same virtual address!
    Init window 2
    Test window 2
    Verify window 2
  Unmap window 2

  ... repeat for all 16 windows
```

**Overhead**:
- Map/unmap calls (kernel overhead)
- Can't parallelize across windows
- Sequential processing

### TMR without AWE Advantage

```
Physical Memory: [========== 16 GiB ==========]
Virtual Address: [========== 16 GiB ==========] ← Can see ALL at once!

Test Flow:
  ALL windows in parallel:
    Thread 1: Init window 1  |  Thread 2: Init window 2  |  ...  |  Thread 16: Init window 16
    Thread 1: Test window 1  |  Thread 2: Test window 2  |  ...  |  Thread 16: Test window 16
    Thread 1: Verify window 1|  Thread 2: Verify window 2|  ...  |  Thread 16: Verify window 16
```

**Benefits**:
- No map/unmap overhead
- Full parallelization across all windows
- Better cache efficiency (threads don't fight for same physical pages)

### Best Practice for TMR

**Keep window-based architecture** (current approach), but process all windows simultaneously:

```rust
// Good: Parallel init across all windows
let init_bytes: u64 = all_windows
    .par_iter_mut()  // Rayon parallel iterator
    .map(|window| test.init(&mut window.memory))
    .sum();

// Good: Parallel test across all windows
let test_bytes: u64 = all_windows
    .par_iter_mut()
    .map(|window| test.test(&mut window.memory))
    .sum();

// Good: Parallel verify across all windows
let errors: u64 = all_windows
    .par_iter()
    .map(|window| test.verify(&window.memory))
    .sum();
```

**Why keep windows?**
1. Cache efficiency (each thread works on its own memory region)
2. NUMA optimization (pin thread to CPU near its memory)
3. Progress tracking (can report per-window completion)
4. Error localization (know which memory region failed)

---

## Part 4: Implementation Roadmap

### Phase 1: Zero-Cost SIMD Abstraction (Week 1)
- [ ] Create `src/simd_verify.rs` with trait
- [ ] Implement AVX512Verifier, AVX2Verifier, SSE41Verifier, ScalarVerifier
- [ ] Add benchmarks to prove zero overhead
- [ ] Write tests for all verifiers

### Phase 2: Framework-Separated Tests (Week 2)
- [ ] Create `MemoryTest` trait
- [ ] Refactor MirrorMove to use trait
- [ ] Update TestRunner for 3-phase execution
- [ ] Update reporting for separate phases

### Phase 3: Migrate Other Tests (Week 3)
- [ ] Migrate StuckBitTest
- [ ] Migrate RefreshStable
- [ ] Migrate SimpleTest
- [ ] Verify all tests use shared SIMD verification

### Phase 4: Polish & Optimize (Week 4)
- [ ] Performance benchmarking
- [ ] Verify zero-cost abstractions in assembly output
- [ ] Update documentation
- [ ] Add TM5 compatibility tests

---

## Part 5: Proving Zero-Cost Abstraction Works

### Benchmark to Run

```rust
#[bench]
fn bench_verify_inline(b: &mut Bencher) {
    let memory = vec![0u64; 1_000_000];
    let expected = vec![1u64; 1_000_000];

    b.iter(|| {
        // Hand-written inline version
        let mut error_acc = unsafe { _mm512_setzero_si512() };
        for (a, e) in memory.chunks_exact(8).zip(expected.chunks_exact(8)) {
            let av = unsafe { _mm512_loadu_si512(a.as_ptr() as *const i32) };
            let ev = unsafe { _mm512_loadu_si512(e.as_ptr() as *const i32) };
            let diff = unsafe { _mm512_xor_si512(av, ev) };
            error_acc = unsafe { _mm512_or_si512(error_acc, diff) };
        }
        error_acc
    });
}

#[bench]
fn bench_verify_trait(b: &mut Bencher) {
    let memory = vec![0u64; 1_000_000];
    let expected = vec![1u64; 1_000_000];

    b.iter(|| {
        // Trait-based version
        unsafe { AVX512Verifier::verify(&memory, &expected) }
    });
}
```

**Expected result**: Both benchmarks should be **identical** (within 1% variance).

### Assembly Check

```bash
# Build with optimizations
cargo rustc --release -- --emit asm

# Check generated assembly - should be identical for both versions!
# No function calls, everything inlined, zero overhead
```

---

## Summary

1. **Zero-Cost Abstraction**: Use trait generics + `#[inline(always)]` + monomorphization = zero overhead

2. **SIMD Verification**: Share the XOR+OR accumulation logic, keep pattern generation test-specific

3. **INIT Phase**: Separate phase (Option A) - cleanest, most TM5-compatible, best reporting

4. **TMR Advantage**: No AWE = parallel init/test/verify across all windows simultaneously

5. **Window Architecture**: Keep it! Cache efficiency + NUMA + progress tracking

**Next Step**: I can create the `src/simd_verify.rs` module with the trait implementation if you'd like to proceed with this plan!
