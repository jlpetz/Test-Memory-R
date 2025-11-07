# MirrorMove Duplication Analysis

## Current Situation

We have 4 MirrorMove implementations that duplicate ~90% of their code:
1. `mirror_move_multi` (scalar/fallback)
2. `mirror_move_128_stream1_impl` (SSE4.1)
3. `mirror_move_256_stream1_impl` (AVX2) - assumed
4. `mirror_move_512_stream1_impl` (AVX-512)

## Duplicated Code Sections

### 1. Pattern Initialization (~40 lines × 4 = 160 lines)

**Scalar version** (lines 5005-5015):
```rust
for i in 0..len {
    let pattern = (i as u64)
        .wrapping_add(thread_pattern_base)
        .wrapping_mul(0x0123456789ABCDEFu64);
    *ptr.add(i) = pattern;
}
```

**SSE4.1 version** (lines 5327-5339):
```rust
for i in 0..len {
    let idx_broadcast = _mm_set1_epi32(i as i32);
    let idx_scaled = _mm_mullo_epi32(idx_broadcast, multipliers);
    let pattern = _mm_add_epi32(idx_scaled, pattern_base);
    _mm_store_si128(base.add(i), pattern);
}
```

**AVX-512 version** (similar pattern):
```rust
for i in 0..len {
    let idx_broadcast = _mm512_set1_epi32(i as i32);
    let idx_scaled = _mm512_mullo_epi32(idx_broadcast, multipliers);
    let pattern = _mm512_add_epi32(idx_scaled, pattern_base);
    _mm512_store_si512(base.add(i), pattern);
}
```

### 2. Verification Logic (~60 lines × 4 = 240 lines)

**Scalar version** (lines 5051-5108):
```rust
// Calculate mirrored index
let mirrored_idx = chunk_start + chunk_end - 1 - idx;

// Generate expected pattern
let expected_pattern = (mirrored_idx as u64)
    .wrapping_add(thread_pattern_base)
    .wrapping_mul(0x0123456789ABCDEFu64);

// Load actual value
let actual_value = *ptr.add(idx);

// Compare
if actual_value != expected_pattern {
    chunk_errors += 1;
    log::error!("...");
}

// Check errors at configured intervals
if (element_count as u32 & check_mask) == 0 {
    if chunk_errors > 0 {
        block_errors += chunk_errors;
        chunk_errors = 0;
    }
}
```

**AVX-512 version** (lines 6700-6719):
```rust
// Calculate mirrored index
let mirrored_idx = chunk_sum - i;

// Generate expected pattern (vectorized)
let idx_broadcast = _mm512_set1_epi32(mirrored_idx as i32);
let idx_scaled = _mm512_mullo_epi32(idx_broadcast, multipliers);
let expected = _mm512_add_epi32(idx_scaled, pattern_base);

// Load actual value
let actual = _mm512_load_si512(base.add(i));

// XOR compare (0 = match, non-zero = error)
let diff = _mm512_xor_si512(expected, actual);

// OR accumulate errors
error_accumulator = _mm512_or_si512(error_accumulator, diff);

// Final check
let error_mask = _mm512_test_epi32_mask(error_accumulator, error_accumulator);
if error_mask != 0 {
    cycle_errors += 1;
    log::error!("...");
}
```

### 3. Test Boilerplate (~100 lines × 4 = 400 lines)

**All versions duplicate** (lines 4952-5004, 5269-5325, etc.):
```rust
// Empty check
if blocks.is_empty() {
    return TestStats { /* ... */ };
}

// Window calculation
let total_allocated: usize = blocks.iter().map(|b| b.buffer.size()).sum();
let window_size = config.calculate_window_size(test_name, total_allocated);
let test_blocks = prepare_blocks_for_window(blocks, window_size, test_name);

if test_blocks.is_empty() {
    log::warn!("...");
    return TestStats { /* ... */ };
}

// Pre-compute pattern base
let thread_pattern_base = (thread_id as u64) << 16;

// Track totals
let mut total_bytes_processed = 0usize;
let mut total_error_count = 0u64;
let mut total_operations = 0u64;

// Timer setup
let test_start = Instant::now();
let mut cycle = 0u32;

// Progress tracking
let update_interval_ms = 1000;
let mut last_progress_update = Instant::now();
```

### 4. Progress Tracking (~25 lines × 4 = 100 lines)

**All versions duplicate** (lines 6769-6782, etc.):
```rust
if let Some(progress) = progress {
    let now = Instant::now();
    if now.duration_since(last_progress_update).as_millis() >= update_interval_ms {
        progress.cycles_completed.store(cycle, Ordering::Relaxed);
        progress.bytes_processed.store(total_bytes_processed, Ordering::Relaxed);
        progress.errors_found.store(total_error_count, Ordering::Relaxed);
        progress.last_update_ms.store(
            test_start.elapsed().as_millis() as u64,
            Ordering::Relaxed
        );
        last_progress_update = now;
    }
}
```

### 5. Cycle Management & Exit Logic (~40 lines × 4 = 160 lines)

**All versions duplicate** (lines 6784-6804, etc.):
```rust
// Check timing limits
let elapsed_secs = test_start.elapsed().as_secs() as u32;
if !timing.should_continue(cycle, elapsed_secs) {
    let stopped_by_time = timing.cycles.map_or(false, |limit| cycle < limit);
    let elapsed_ms = test_start.elapsed().as_millis();

    return TestStats {
        name: test_name,
        action: TestAction::WriteWaitVerify,
        bytes_processed: total_bytes_processed as usize,
        elapsed_ms,
        thread_id,
        error_count: total_error_count,
        total_operations,
        cycles_completed: cycle,
        cycles_planned: timing.cycles,
        stopped_by_time_limit: stopped_by_time,
    };
}
```

### 6. Error Handling (~30 lines × 4 = 120 lines)

**All versions duplicate** (lines 6738-6761, etc.):
```rust
if cycle_errors > 0 {
    match error_mode {
        ErrorMode::Panic => {
            panic!("...");
        }
        ErrorMode::Halt => {
            return TestStats { /* ... */ };
        }
        ErrorMode::Log => {
            // Already logged
        }
    }
}
```

## Total Duplication

**Rough estimate**: ~1,180 lines of duplicated code across 4 variants

## What Needs Abstraction (Monomorphization)

### Priority 1: Pattern Init & Verify (Performance-Critical)

These need SIMD-specific implementations but can share structure:

```rust
// Trait for pattern initialization
trait PatternInit {
    unsafe fn init_block(ptr: *mut Self, len: usize, thread_pattern_base: u64);
}

impl PatternInit for u64 {
    // Scalar version
}

impl PatternInit for __m128i {
    // SSE4.1 version
}

impl PatternInit for __m256i {
    // AVX2 version
}

impl PatternInit for __m512i {
    // AVX-512 version
}
```

```rust
// Trait for verification
trait PatternVerify {
    unsafe fn verify_chunk(
        ptr: *const Self,
        start: usize,
        end: usize,
        thread_pattern_base: u64,
    ) -> (u64, bool); // (errors, has_errors)
}
```

### Priority 2: Boilerplate (Non-Performance-Critical)

This can be a generic function:

```rust
fn run_mirror_test<T: PatternInit + PatternVerify>(
    blocks: &[AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
    test_name: &'static str,
) -> TestStats {
    // All the boilerplate code here
    // Calls T::init_block() and T::verify_chunk()
}
```

Then each variant becomes:

```rust
pub unsafe fn mirror_move_512_multi(...) -> TestStats {
    run_mirror_test::<__m512i>(..., "MirrorMove512")
}

pub unsafe fn mirror_move_256_multi(...) -> TestStats {
    run_mirror_test::<__m256i>(..., "MirrorMove256")
}

pub unsafe fn mirror_move_128_multi(...) -> TestStats {
    run_mirror_test::<__m128i>(..., "MirrorMove128")
}

pub unsafe fn mirror_move_multi(...) -> TestStats {
    run_mirror_test::<u64>(..., "MirrorMove")
}
```

## Benefits

1. **Code reduction**: ~1,180 lines → ~400 lines (~67% reduction)
2. **Zero cost**: Monomorphization = compiler generates specialized code
3. **Single source of truth**: Bug fixes apply to all variants
4. **Easier TM5 alignment**: Change cycle logic once, applies everywhere
5. **Maintainability**: Add new SIMD variant = implement 2 small traits

## Next Steps

1. Create minimal trait for pattern init (scalar + SSE + AVX2 + AVX512)
2. Create minimal trait for pattern verify (scalar + SSE + AVX2 + AVX512)
3. Extract boilerplate into generic function
4. Verify zero-cost with `cargo asm`
5. Apply same pattern to other tests
