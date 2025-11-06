# CacheBusting Performance Analysis - Accounting Bug

## Summary

**Finding**: MirrorMove tests are **under-reporting bytes_processed by 33-50%**, making them appear artificially slower than CacheBusting. This is an accounting error, not a genuine performance difference.

## The Bug

### MirrorMove512 Accounting (tests.rs:6765)

```rust
total_bytes_processed += (test_block.test_size * 2) as u64; // Mirror + mirror back
```

**Comment says**: "Mirror + mirror back"
**But the code actually does THREE phases**:

1. **Mirror** (lines 6643-6653): Swap first/last halves - reads + writes
2. **Verify** (lines 6657-6719): Read and verify mirrored pattern - **MISSING FROM ACCOUNTING**
3. **Mirror-back** (lines 6721-6731): Restore original - reads + writes

### CacheBusting Accounting (tests.rs:3160)

```rust
total_bytes_processed += test_block.test_size * 2;  // Write + Verify
```

CacheBusting correctly counts BOTH phases:
1. **Write** (lines 3043-3050): Write with stride pattern
2. **Verify** (lines 3054-3068): Verify with same pattern

## Impact on Reported Throughput

Since throughput is calculated as `bytes_processed / elapsed_time`, under-counting bytes makes tests appear slower.

### Example with 1GB test_size

**MirrorMove512 (CURRENT - WRONG)**:
- Actual work: Mirror (1GB R+W) + Verify (1GB R) + Mirror-back (1GB R+W)
- Reported bytes: `1GB * 2 = 2GB`
- If takes 1 second: Reported throughput = `2GB/s`
- **Verify phase (1GB read) is missing!**

**MirrorMove512 (CORRECTED)**:
- Actual work: Same as above
- Corrected bytes: `1GB * 3 = 3GB`
- If takes 1 second: Corrected throughput = `3GB/s` (**50% increase**)

**CacheBusting (CORRECT)**:
- Actual work: Write (1GB W) + Verify (1GB R)
- Reported bytes: `1GB * 2 = 2GB`
- If takes 0.5 seconds: Reported throughput = `4GB/s`

### Corrected Performance Comparison

If user sees:
- CacheBusting: 10,000 MiB/s
- MirrorMove512: 1,000 MiB/s (10× difference)

After fixing MirrorMove accounting:
- CacheBusting: 10,000 MiB/s (unchanged)
- MirrorMove512: 1,500 MiB/s (**50% increase**, now only 6.7× difference)

The gap narrows significantly, though CacheBusting may still be genuinely faster due to less actual memory operations.

## Why CacheBusting is (Legitimately) Faster

Even with corrected accounting, CacheBusting does LESS work per byte:

### Memory Operations per Element

**CacheBusting** (per u64):
- Write: 1 store
- Verify: 1 load
- **Total: 1 load + 1 store**

**MirrorMove512** (per __m512i = 8 u64s):
- Mirror: ~N/2 swaps = N loads + N stores (for N elements)
- Verify: N loads
- Mirror-back: N loads + N stores
- **Total: 3N loads + 2N stores** (per N elements)
- **Per u64: 0.375 loads + 0.25 stores** (averaged)

Wait, that math suggests MirrorMove does LESS per element. Let me recalculate...

Actually, the two-pointer mirror optimization means:
- For N elements, we do N/2 iterations
- Each iteration: 2 loads + 2 stores
- Total for mirror: N loads + N stores

So:
- Mirror: N loads + N stores
- Verify: N loads
- Mirror-back: N loads + N stores
- **Total: 3N loads + 2N stores**

Compared to CacheBusting:
- Write + Verify: N loads + N stores

**MirrorMove does 3× as many loads and 2× as many stores!**

This explains why CacheBusting is genuinely faster - it does significantly less memory traffic.

## Additional Issues Found

### Misleading Comment in CacheBusting (tests.rs:3159)

```rust
// Update bytes processed for this block (stride coverage ~25%)
total_bytes_processed += test_block.test_size * 2;
```

The comment says "stride coverage ~25%" but the code actually has **100% coverage**!

The strided access pattern (every 4KB = 512 u64s) combined with the outer loop ensures all memory is touched:

```rust
for offset in 0..base_stride.min(chunk_end - processed) {  // Runs 512 times
    let mut i = processed + offset;
    while i < chunk_end {
        *base.add(i) = ...;
        i += base_stride;  // Jump by 512
    }
}
```

- offset=0: writes indices 0, 512, 1024, ...
- offset=1: writes indices 1, 513, 1025, ...
- ...
- offset=511: writes indices 511, 1023, 1535, ...

Every index from 0 to N-1 is written exactly once = 100% coverage.

### Unused total_operations Multiplier (tests.rs:3116, 3142, 3181)

```rust
let total_operations = ((total_bytes_processed / std::mem::size_of::<u64>()) as f64 * 0.25) as u64;
```

This multiplies operations by 0.25 (25%), probably based on the incorrect "25% coverage" assumption. But `total_operations` doesn't affect throughput calculations, so this doesn't impact the reported performance numbers.

## Recommended Fixes

### 1. Fix MirrorMove Accounting (HIGH PRIORITY)

**File**: `src/tests.rs`
**Locations**: All MirrorMove variants (128/256/512, Stream1/StreamN)

**Current (line 6765)**:
```rust
total_bytes_processed += (test_block.test_size * 2) as u64; // Mirror + mirror back
```

**Corrected**:
```rust
total_bytes_processed += (test_block.test_size * 3) as u64; // Mirror + verify + mirror back
```

Apply this fix to:
- `mirror_move_128_stream1_impl` (~line 5400)
- `mirror_move_128_stream_n_impl` (~line 5700)
- `mirror_move_256_stream1_impl` (~line 6100)
- `mirror_move_256_stream_n_impl` (~line 6400)
- `mirror_move_512_stream1_impl` (~line 6765)
- `mirror_move_512_stream_n_impl` (~line 7100)

### 2. Fix Misleading Comment (LOW PRIORITY)

**File**: `src/tests.rs:3159`

**Current**:
```rust
// Update bytes processed for this block (stride coverage ~25%)
```

**Corrected**:
```rust
// Update bytes processed for this block (write + verify = 2×)
```

### 3. Consider Fixing total_operations (OPTIONAL)

If `total_operations` is used elsewhere (not for throughput), fix the 0.25 multiplier:

**Current (line 3181)**:
```rust
let total_operations = ((total_bytes_processed / std::mem::size_of::<u64>()) as f64 * 0.25) as u64;
```

**Corrected**:
```rust
let total_operations = (total_bytes_processed / std::mem::size_of::<u64>()) as u64;
```

## Verification Steps

After applying fixes:

1. Run test suite with both MirrorMove and CacheBusting
2. Compare reported throughput values
3. Verify MirrorMove throughput increases by ~50%
4. Verify CacheBusting throughput remains unchanged
5. Check that performance ratio becomes more reasonable (e.g., 6-7× instead of 10×)

## Why This Bug Exists

Looking at the code history:
1. MirrorMove was implemented with 3 phases (mirror, verify, mirror-back)
2. Accounting was set to `* 2` based on comment "Mirror + mirror back"
3. The verify phase was overlooked in the accounting
4. Comment in CacheBusting about "25% coverage" suggests confusion about stride pattern behavior
5. This propagated to the `* 0.25` multiplier in total_operations

This is a classic case of incomplete accounting after refactoring - the verify phase was added but bytes_processed wasn't updated.

## Impact Assessment

**Severity**: Medium
- Does NOT affect test correctness (memory is tested properly)
- DOES affect performance reporting (misleading throughput numbers)
- Could lead to incorrect conclusions about test performance
- May affect test selection and configuration decisions

**Scope**: All MirrorMove variants (6 functions affected)

**User Impact**: Users may avoid MirrorMove tests thinking they're slower than they actually are

## Related Code

- **Reporting**: `src/reporting/converters.rs:765` - throughput calculation uses bytes_processed
- **Test Registration**: `src/tests.rs:483-491` - OperationMetadata for CacheBusting
- **Chunk Sizing**: `src/tests.rs:615` - CacheBusting uses L3/2 chunk size
- **Chunk Sizing**: `src/tests.rs:722` - MirrorMove512 uses 64MB chunks
