# MirrorMove Test Logic Analysis

## Current Implementation

### Test Flow (All MirrorMove Variants)

**Per cycle:**
1. **Mirror**: Swap first half with last half of memory block
2. **Verify**: Read and verify mirrored pattern is correct
3. **Mirror-back**: Swap again to restore original pattern
4. **❌ MISSING**: No verification that original pattern is restored

### Example with 8 elements [A,B,C,D,E,F,G,H]

```
Initial:  [A,B,C,D,E,F,G,H]
           ↓ Mirror (swap first/last halves)
Mirrored: [H,G,F,E,D,C,B,A]
           ↓ Verify (check pattern is correct)
           ✓ Verification passed
           ↓ Mirror-back (swap again)
Restored: [A,B,C,D,E,F,G,H]
           ↓ ❌ NO VERIFICATION!
```

## The Problem: Missing Final Verify

### What's Not Being Tested

Without a final verification after mirror-back, we fail to detect:

1. **Mirror-back operation errors**
   - If the second swap has bugs (unlikely but possible)
   - If hardware fails during the restore phase specifically

2. **Accumulated bit flips**
   - A bit flip during mirror might be "masked" if the same bit flips back during mirror-back
   - Example: A→H (with flip) → A (with second flip) → looks correct but had 2 errors

3. **Incomplete restoration**
   - If mirror-back doesn't complete fully (shutdown, cache eviction, etc.)
   - Memory could be left in inconsistent state without detection

4. **Cache coherency issues**
   - Problems that only manifest after multiple transformations
   - Write-back cache delays between operations

### User's Concern is Valid

The user correctly identified that without final verification, we're only testing:
- ✅ Initial pattern can be written
- ✅ Mirror operation works
- ✅ Mirrored pattern can be read back
- ❌ Mirror-back operation works
- ❌ Original pattern survives round-trip

## Memory Operation Accounting

### Current Accounting (After Our Fix)

```rust
total_bytes_processed += test_block.test_size * 3;
// Comment: "mirror + verify + mirror back"
```

### What Actually Happens Per Block

Let S = test_block.test_size

**Phase 1: Mirror Operation**
```rust
while idx1 < idx2 {
    let val1 = *ptr.add(idx1);     // READ
    let val2 = *ptr.add(idx2);     // READ
    *ptr.add(idx2) = val1;         // WRITE
    *ptr.add(idx1) = val2;         // WRITE
}
```
- Reads: S bytes
- Writes: S bytes
- **Subtotal: 2S bytes**

**Phase 2: Verify**
```rust
for idx in chunk_start..chunk_end {
    let actual_value = *ptr.add(idx);  // READ
    // compare with expected pattern
}
```
- Reads: S bytes
- Writes: 0 bytes
- **Subtotal: 1S bytes**

**Phase 3: Mirror-back**
```rust
while idx1 < idx2 {
    let val1 = *ptr.add(idx1);     // READ
    let val2 = *ptr.add(idx2);     // READ
    *ptr.add(idx2) = val1;         // WRITE
    *ptr.add(idx1) = val2;         // WRITE
}
```
- Reads: S bytes
- Writes: S bytes
- **Subtotal: 2S bytes**

**Phase 4: Final Verify (MISSING)**
- Would add: 1S bytes (reads)

### Correct Accounting

**Current implementation (3 phases):**
- Total: 2S + 1S + 2S = **5S bytes**
- We're accounting: **3S bytes** ❌ **STILL WRONG!**

**If we add final verify (4 phases):**
- Total: 2S + 1S + 2S + 1S = **6S bytes**
- Should account: **6S bytes**

## The Real Issue: Accounting is Still Wrong!

Our fix changed `* 2` to `* 3`, but it should be `* 5` (current implementation) or `* 6` (if we add final verify)!

### Why the Confusion?

The comment "mirror + verify + mirror back" suggests counting "operations" not "bytes transferred":
- 1 mirror operation
- 1 verify operation
- 1 mirror-back operation
- = 3 operations

But **bytes_processed should count actual memory transfers:**
- Mirror: 1 read + 1 write = 2× bytes
- Verify: 1 read = 1× bytes
- Mirror-back: 1 read + 1 write = 2× bytes
- = 5× bytes

## Recommendations

### Option 1: Add Final Verify (Most Thorough)

**Pros:**
- Complete test coverage
- Detects mirror-back errors
- Detects round-trip accumulated errors
- Symmetrical test pattern

**Cons:**
- Adds 20% more memory operations (5→6)
- Slightly slower test execution

**Implementation:**
```rust
// 3. Mirror back to restore original pattern
idx1 = chunk_start;
idx2 = chunk_end - 1;
while idx1 < idx2 {
    let val1 = *ptr.add(idx1);
    let val2 = *ptr.add(idx2);
    *ptr.add(idx2) = val1;
    *ptr.add(idx1) = val2;
    idx1 += 1;
    idx2 -= 1;
}

std::sync::atomic::fence(Ordering::SeqCst);

// 4. Verify original pattern is restored
for idx in chunk_start..chunk_end {
    let expected_pattern = (idx as u64).wrapping_add(thread_pattern_base).wrapping_mul(0x0123456789ABCDEFu64);
    let actual_value = *ptr.add(idx);
    if actual_value != expected_pattern {
        chunk_errors += 1;
        // ... error handling
    }
}
```

**Accounting:**
```rust
total_bytes_processed += test_block.test_size * 6; // mirror(2) + verify(1) + mirror-back(2) + verify-final(1)
```

### Option 2: Fix Accounting Only (Pragmatic)

**Pros:**
- No performance impact
- Simpler change
- Current test logic still valuable

**Cons:**
- Doesn't add final verification (incomplete test)
- Still not testing mirror-back operation

**Implementation:**
```rust
total_bytes_processed += test_block.test_size * 5; // mirror(2) + verify(1) + mirror-back(2)
```

### Option 3: Remove Mirror-back (Simplified)

**Rationale:**
- If we're not verifying after mirror-back, why do it at all?
- The verify after mirror already tests memory read/write
- Mirror-back is just "cleanup" to restore state, not part of the test

**Implementation:**
```rust
// 1. Mirror operation
// ... swap code ...

// 2. Verify mirrored data
// ... verify code ...

// NO MIRROR-BACK - leave memory in mirrored state
// Next cycle will re-initialize anyway
```

**Accounting:**
```rust
total_bytes_processed += test_block.test_size * 3; // mirror(2) + verify(1)
```

**Pros:**
- Simpler logic
- 40% fewer memory operations (5→3)
- Faster test execution
- Honest accounting

**Cons:**
- Loses symmetry of test
- Doesn't test "round-trip" resilience

## Comparison with TM5 Original

Need to check: Does the original TM5 implementation verify after mirror-back?

**TODO**: Review TM5_Source/MT0.cxx to see if original MirrorMove had final verify.

## Recommended Action

**Short term:** Fix accounting to `* 5` to match current reality

**Long term discussion:** Decide if MirrorMove should:
- Add final verify (more thorough)
- Remove mirror-back (faster, simpler)
- Keep as-is but with honest accounting

## Impact Assessment

**Current bug severity:** Medium
- Reporting is off by 40% (should be 5×, reporting 3×)
- Makes MirrorMove appear artificially slower
- Affects test selection and performance analysis

**Missing final verify severity:** Low-Medium
- Test still catches most memory errors
- Mirror-back errors unlikely to be hardware-specific
- But round-trip testing is best practice

## Files to Modify

If adding final verify:
1. `src/tests.rs` - All 7 MirrorMove implementations
   - mirror_move_multi (scalar, line 4942)
   - mirror_move_128_stream1_impl (line 5259)
   - mirror_move_128_stream_n_impl (line 5549)
   - mirror_move_256_stream1_impl (line 5900)
   - mirror_move_256_stream_n_impl (line 6200)
   - mirror_move_512_stream1_impl (line 6545)
   - mirror_move_512_stream_n_impl (line 6843)

If just fixing accounting:
1. Change all `* 3` to `* 5` (or `* 6` if adding final verify)
2. Update comments to be explicit about byte counts
