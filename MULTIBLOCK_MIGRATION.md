# MultiBlock Migration Pattern - Comprehensive Checklist

## Context: Timer Bug Fix via MultiBlock Pattern

**Problem**: Legacy per-block testing runs N× configured duration when memory split into N blocks
**Solution**: MultiBlock pattern with shared timer and interleaved block testing (1,2,3,1,2,3...)

---

## Complete Migration Checklist for Each Test

**Use this checklist for EVERY test migration. Mark each step complete before moving to next test.**

### A. Create Test Implementations

- [ ] **1. Create `test_name_stream1_multi()` - single stream variant**
  - Function signature: `unsafe fn`, NOT `pub`, takes `blocks: &[AllocationBlock]`
  - Returns: `TestStats` with proper fields
  - Parameter order: `blocks, thread_id, error_mode, timing, config, progress`

- [ ] **2. Create `test_name_stream_n_multi()` - N-way stream variant (if applicable)**
  - Function signature: Same as stream1, add `streams: usize, test_name: &'static str` params
  - **CRITICAL**: Test name parameter MUST be `&'static str` for lifetime compatibility
  - Only needed if test supports multiple stream configurations

- [ ] **3. Create `test_name_multi()` - PUBLIC dispatcher**
  - Function signature: `pub unsafe fn`, routes based on `config.streams`
  - Dispatches to stream1_multi or stream_n_multi based on configuration

---

### B. Add Validation (BEFORE Cycle Loop - Outside Hot Loop!)

- [ ] **4. Streams validated in `TestMemoryConfig::with_streams()` (already done globally)**
  - This validates streams are power-of-2 at config creation time
  - No action needed per-test

- [ ] **5. In each stream variant - validate chunk divisibility BEFORE cycle loop**

```rust
// BEFORE cycle loop - validate ALL blocks ONCE (outside hot loop)
for test_block in test_blocks.iter() {
    let ideal_chunk_size = calculate_ideal_chunk_size(config, test_name, test_block.test_size);
    let chunk_size_bytes = get_safe_chunk_size(ideal_chunk_size, test_block.test_size);
    let chunk_size_elements = chunk_size_bytes / std::mem::size_of::<TYPE>();

    validate_chunk_divisibility(test_name, config.streams, chunk_size_elements);
}
```

- [ ] **6. CRITICAL: Validate EACH block (different blocks have different sizes!)**
  - DO NOT calculate chunk size once and validate with same value in loop
  - MUST recalculate for each block's actual size

---

### C. Per-Block Chunk Size Recalculation (INSIDE Loop)

- [ ] **7. DO NOT calculate chunk size once from first block**
  - This is a common mistake caught by "unused variable" warnings

- [ ] **8. MUST recalculate chunk size for EACH block inside the test loop**

```rust
loop {
    cycle += 1;
    for test_block in test_blocks.iter() {
        // Recalculate chunk size for THIS block's size
        let ideal_chunk_size = calculate_ideal_chunk_size(config, test_name, test_block.test_size);
        let chunk_size_bytes = get_safe_chunk_size(ideal_chunk_size, test_block.test_size);
        let chunk_size_elements = chunk_size_bytes / std::mem::size_of::<TYPE>();

        // Use chunk_size_elements for THIS block...
        for chunk_start in (0..len).step_by(chunk_size_elements) {
            // Process chunk...
        }
    }
}
```

- [ ] **9. Why? Blocks can be 16MB to 4GB - each needs appropriate chunk size**
  - Allocator creates different-sized blocks based on memory availability
  - Thread could receive: [4GB block, 1GB block, 512MB block]
  - Each block needs its own chunk size calculation

---

### D. Implement Progress Reporting

- [ ] **10. Actually UPDATE the progress tracker (not just check `Some()`)**

```rust
// Progress tracking variables
let update_interval_ms = 250u128; // 4 updates/sec (or 1000 for less frequent)
let mut last_progress_update = Instant::now();

// Inside cycle loop, AFTER processing all blocks
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

- [ ] **11. Common mistake: Checking `Some(progress)` but never calling `.store()`**
  - Rust will warn about unused variable if you don't actually use it

---

### E. Use Correct Timing Fields

- [ ] **12. CRITICAL: Use `timing.cycles` NOT `timing.cycle_limit` (doesn't exist!)**
  - The struct is `TestTiming { cycles: Option<u32>, duration_secs: Option<u32>, ... }`
  - Field name is `cycles`, not `cycle_limit`

- [ ] **13. Check all TestStats returns use `cycles_planned: timing.cycles`**

```rust
TestStats {
    name: test_name,
    action: TestAction::WriteWaitVerify,
    bytes_processed: total_bytes_processed as usize,
    elapsed_ms,
    thread_id,
    error_count: total_error_count,
    total_operations,
    cycles_completed: cycle,
    cycles_planned: timing.cycles,  // ← CORRECT
    stopped_by_time_limit: stopped_by_time,
}
```

- [ ] **14. Check stopped_by_time calculation uses `timing.cycles`**

```rust
let stopped_by_time = timing.cycles.map_or(false, |limit| cycle < limit);
```

---

### F. Update Registration in runner.rs

- [ ] **15. Find test in `create_test_definitions()` function (around line 730+)**

- [ ] **16. Change registration from WithConfig to MultiBlock**

```rust
// BEFORE (old per-block pattern):
(
    "MirrorMove256",
    TestFunction::WithConfig(|ptr, size, tid, em, timing, config| unsafe {
        mirror_move_256(ptr, size, tid, em, timing, config)
    }),
    validate_streams(TestMemoryConfig::new(...), "MirrorMove256")
        .with_memory_type(None)
),

// AFTER (new MultiBlock pattern):
(
    "MirrorMove256",
    TestFunction::MultiBlock(mirror_move_256_multi),  // ← Changed this line
    validate_streams(TestMemoryConfig::new(...), "MirrorMove256")
        .with_memory_type(None)
),
```

- [ ] **17. Keep all config settings unchanged**
  - WindowMode, ChunkMode, streams, timing, etc. stay the same
  - Only the TestFunction variant changes

---

### G. Update Imports in runner.rs

- [ ] **18. Add `test_name_multi` to the `use crate::tests::{...}` import list (around line 6-13)**

```rust
use crate::tests::{
    mirror_move, mirror_move_128, mirror_move_256, mirror_move_512, mirror_move_auto, mirror_move_multi,
    mirror_move_128_multi, mirror_move_256_multi,  // ← Add your new function here
    stuck_bit_test, stuck_bit_test_128, stuck_bit_test_256, stuck_bit_test_512,
    simple_test_multi, refresh_stable, refresh_stable_128, refresh_stable_256, refresh_stable_512,
    cache_busting_write_test, random_access_torture_test,
    stride_access_test, bandwidth_saturation_test, block_move_test
};
```

- [ ] **19. Ensure alphabetical or logical grouping for readability**

---

### H. Verify Build & Test

- [ ] **20. Build with `cargo build --release` - check for ZERO warnings**

```bash
cd TMR-APP
cargo build --release 2>&1 | grep warning
# Should output nothing if clean
```

- [ ] **21. Common warnings and their meanings:**
  - `unused variable: test_block` → You calculated chunk size once, not per-block
  - `unused variable: progress` → You checked `Some(progress)` but never used it
  - `no field 'cycle_limit'` → Used wrong field name, should be `timing.cycles`
  - `lifetime may not live long enough` → test_name should be `&'static str`

- [ ] **22. Test with `--single-test=TestName`**

```bash
./target/release/tmr.exe --single-test=MirrorMove256
```

- [ ] **23. Verify output shows:**
  - ✅ "Cycles 🟢 X/Y" or "Cycles 🔴 X/Y" (colored indicator)
  - ✅ "Time Limit 🟢" or "Time Limit 🔴" (colored indicator)
  - ✅ Logs from `tmr::tests` module (NOT `tmr::thread_pool`)
  - ✅ Progress tracker updates every 250ms or 1000ms
  - ✅ Per-thread report shows cycle counts and timing

- [ ] **24. Check that window limiting works correctly**
  - Tests should include complete blocks only (never split a block)
  - Log message: "Prepared N block(s), total X MiB (window: Y MiB)"

---

## Common Mistakes & How to Catch Them

| Mistake | Symptom | Fix Location |
|---------|---------|--------------|
| Used `timing.cycle_limit` | Compile error: "no field `cycle_limit`" | All TestStats returns - use `timing.cycles` |
| Calculated chunk once | Warning: unused variable `test_block` in validation | Move calculation inside `for test_block` loop |
| Didn't update progress | Rust warns: unused variable `progress` | Add 4 `.store()` calls in progress block |
| Forgot registration | Test uses old code path, no colored dots | Update `create_test_definitions()` in runner.rs |
| Forgot imports | Compile error: cannot find function | Add to `use crate::tests::{...}` |
| Used `&str` for test_name | Lifetime error: `'1` vs `'static` | Change parameter to `&'static str` |
| Validated wrong stream count | Logic error in multi-stream | Pass correct `streams` or `config.streams` |
| Window splits blocks | Tests half of blocks, incorrect coverage | Use `prepare_blocks_for_window()` correctly |

---

## Pattern Examples

### ✅ CORRECT: Validation Pattern (BEFORE cycle loop)

```rust
// Validate each block's chunk divisibility
for test_block in test_blocks.iter() {
    let ideal_chunk_size = calculate_ideal_chunk_size(config, test_name, test_block.test_size);
    let chunk_size_bytes = get_safe_chunk_size(ideal_chunk_size, test_block.test_size);
    let chunk_size_elements = chunk_size_bytes / std::mem::size_of::<__m256i>();

    validate_chunk_divisibility(test_name, config.streams, chunk_size_elements);
}
```

### ❌ WRONG: Validation Pattern

```rust
// Validates once with pre-calculated chunk (doesn't use test_block!)
let chunk_size_elements = chunk_size_bytes / std::mem::size_of::<__m256i>();
for _test_block in test_blocks.iter() {
    validate_chunk_divisibility(test_name, config.streams, chunk_size_elements);
}
// Warning: unused variable `test_block` → This tells you it's wrong!
```

---

### ✅ CORRECT: Chunk Size Pattern (INSIDE cycle loop)

```rust
loop {
    cycle += 1;

    // Recalculate for each block
    for test_block in test_blocks.iter() {
        let ideal_chunk_size = calculate_ideal_chunk_size(config, test_name, test_block.test_size);
        let chunk_size_bytes = get_safe_chunk_size(ideal_chunk_size, test_block.test_size);
        let chunk_size_elements = chunk_size_bytes / std::mem::size_of::<TYPE>();

        // Use chunk_size_elements for THIS block
        for chunk_start in (0..len).step_by(chunk_size_elements) {
            // Process chunk...
        }
    }
}
```

### ❌ WRONG: Chunk Size Pattern

```rust
// Calculate once from first block
let first_block_size = test_blocks[0].test_size;
let ideal_chunk_size = calculate_ideal_chunk_size(config, test_name, first_block_size);
let chunk_size_bytes = get_safe_chunk_size(ideal_chunk_size, first_block_size);

loop {
    cycle += 1;

    for test_block in test_blocks.iter() {
        let chunk_size_elements = chunk_size_bytes / std::mem::size_of::<TYPE>();
        // chunk_size_bytes is WRONG for blocks with different sizes!
    }
}
```

---

## Tests Successfully Migrated

- ✅ **SimpleTest** (all stream variants: 1, 2, 4, N)
  - Location: tests.rs lines ~4585-5230
  - Dispatcher + 4 stream implementations
  - Properly validates and recalculates chunk sizes

- ✅ **MirrorMove128 (SSE2)** - stream1 and stream_n variants
  - Location: tests.rs lines ~5675-6271
  - Uses `__m128i` (128-bit SIMD)
  - Proper validation and chunk size handling

- ✅ **MirrorMove256 (AVX2)** - stream1 and stream_n variants
  - Location: tests.rs lines ~6273-6887
  - Uses `__m256i` (256-bit SIMD)
  - Fixed timing.cycles and chunk size issues during migration

---

## Tests Pending Migration

- [ ] **MirrorMove512 (AVX-512)** - Next in queue
- [ ] **MirrorMoveAuto resolver**
- [ ] **StuckBitTest (base scalar)**
- [ ] **StuckBitTest128/256/512 (SIMD variants)**
- [ ] **RefreshStable (base scalar)**
- [ ] **RefreshStable128/256/512 (SIMD variants)**
- [ ] **CacheBusting**
- [ ] **RandomAccessTorture**
- [ ] **StrideAccess**
- [ ] **BandwidthSaturation**
- [ ] **BlockMove**

---

## Reference Implementations

**Use these as templates when migrating new tests:**

1. **SimpleTest stream variants** (tests.rs:4585-5230)
   - Best example of complete stream1/stream2/stream4/stream_n implementation
   - Shows proper validation and chunk size handling
   - Good progress tracking example

2. **MirrorMove128 stream_n** (tests.rs:5943-6243)
   - Best example of N-way segmented pattern
   - Shows proper multi-stream mirroring logic
   - Reference for complex stream algorithms

3. **MirrorMove256 stream1** (tests.rs:6277-6558)
   - Clean single-stream implementation
   - Good validation placement example
   - Shows proper AVX2 SIMD pattern

---

## Why These Rules Matter

### Validation Before Cycle Loop
- **Performance**: Validates once per test, not per cycle (100× fewer validations)
- **Correctness**: Catches configuration errors before wasting time on invalid test

### Recalculate Chunk Size Per Block
- **Correctness**: Different block sizes need different chunk sizes
- **Example**: 4GB block needs 128MB chunks, 256MB block needs 32MB chunks
- **Impact**: Using wrong chunk size causes test to process incorrect memory ranges

### Use timing.cycles Not timing.cycle_limit
- **Correctness**: Field doesn't exist, will cause compile error
- **Impact**: Prevents test from compiling at all

### Actually Update Progress Tracker
- **User Experience**: Live progress updates show test is running
- **Debugging**: Progress data helps identify stuck tests
- **Impact**: Without updates, test appears frozen to user

---

## Summary

**Before marking migration complete, verify:**
1. ✅ Zero compiler warnings
2. ✅ Test runs with `--single-test=TestName`
3. ✅ Colored cycle/time indicators show in output
4. ✅ Logs come from `tmr::tests` not `tmr::thread_pool`
5. ✅ Progress updates every 250-1000ms
6. ✅ Window limiting works (complete blocks only)

**If any verification fails, review the checklist - you missed a step!**
