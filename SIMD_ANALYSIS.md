# SIMD Configuration Analysis - MirrorMove256 Performance Regression

## Summary of Findings

**CRITICAL ISSUE FOUND:** Missing `#[target_feature]` attributes on SIMD functions may be preventing proper SIMD code generation and optimization.

---

## 1. Current Configuration Review

### ✅ Cargo.toml - Release Profile
```toml
[profile.release]
opt-level = 3              ✅ Maximum optimization
lto = true                 ✅ Link-Time Optimization enabled
codegen-units = 1          ✅ Single codegen unit for best optimization
panic = "abort"            ✅ Smaller binary, faster code
strip = true               ✅ Strip symbols
```

**Verdict:** Compiler flags are optimal.

---

## 2. SIMD Implementation Analysis

### ❌ CRITICAL: Missing `#[target_feature]` Attributes

According to Rust's `std::arch` documentation (https://doc.rust-lang.org/nightly/std/arch/):

**Current code:**
```rust
pub unsafe fn mirror_move_256_multi(...) -> TestStats {
    use std::arch::x86_64::*;

    // Runtime feature check
    if !is_x86_feature_detected!("avx2") {
        return zero_stats;
    }

    // AVX2 intrinsics usage
    _mm256_load_si256(...);
    _mm256_store_si256(...);
}
```

**Problem:** Without `#[target_feature(enable = "avx2")]`, the Rust compiler:
1. May NOT generate optimal AVX2 instructions
2. May NOT properly inline SIMD operations
3. May fall back to scalar code paths in loops
4. Loses auto-vectorization opportunities

**According to Rust docs**, there are two approaches:

### Approach 1: Runtime Detection (Current - SUBOPTIMAL)
```rust
// ❌ Current approach - compiler can't optimize aggressively
pub unsafe fn mirror_move_256_multi(...) {
    if !is_x86_feature_detected!("avx2") {
        return;
    }
    // Intrinsics here
}
```

### Approach 2: Compile-Time Feature (RECOMMENDED)
```rust
// ✅ Recommended approach - enables full optimization
#[target_feature(enable = "avx2")]
unsafe fn mirror_move_256_stream1_multi(...) {
    // Intrinsics here - compiler KNOWS AVX2 is available
}

// Public wrapper with runtime check
pub unsafe fn mirror_move_256_multi(...) {
    if !is_x86_feature_detected!("avx2") {
        return zero_stats;
    }
    // Call #[target_feature] function
    mirror_move_256_stream1_multi(...)
}
```

---

## 3. Performance Impact Analysis

### Why MirrorMove256 is SLOWER than scalar:

**Scalar version (MirrorMove):**
```rust
// Lines 8239-8249 - Simple pointer swaps
let mut idx1 = chunk_start;
let mut idx2 = chunk_end - 1;
while idx1 < idx2 {
    let val1 = *ptr.add(idx1);      // 8 bytes
    let val2 = *ptr.add(idx2);      // 8 bytes
    *ptr.add(idx2) = val1;
    *ptr.add(idx1) = val2;
    idx1 += 1;
    idx2 -= 1;
}
```

**AVX2 version (MirrorMove256):**
```rust
// Lines 9096-9107 - SIMD swaps
while idx1 < idx2 {
    let val1 = _mm256_load_si256(base.add(idx1));      // 32 bytes
    let val2 = _mm256_load_si256(base.add(idx2));      // 32 bytes
    _mm256_stream_si256(base.add(idx2), val1);
    _mm256_stream_si256(base.add(idx1), val2);
    idx1 += 1;
    idx2 -= 1;
}
```

### Theoretical Performance:
- **Scalar**: 16 bytes per iteration (2×8-byte loads + 2×8-byte stores)
- **AVX2**: 64 bytes per iteration (2×32-byte loads + 2×32-byte stores)
- **Expected speedup**: 4x faster

### Actual Performance:
- **AVX2 is SLOWER than scalar** ❌

### Root Cause Hypotheses:

#### Hypothesis 1: Missing #[target_feature] (MOST LIKELY)
Without `#[target_feature(enable = "avx2")]`:
- Compiler may not inline SIMD intrinsics properly
- Loop may not be optimized as aggressively
- Extra overhead from runtime checks bleeding into hot loop
- LLVM may not recognize SIMD patterns

#### Hypothesis 2: AVX Frequency Throttling
- AVX2/AVX-512 cause CPU downclocking (known Intel behavior)
- Heavy AVX workloads trigger thermal throttling
- CPU may drop from 5.0GHz → 4.0GHz with AVX-512
- **Test**: Monitor CPU frequency during execution (HWiNFO64)

#### Hypothesis 3: Memory Bandwidth Saturation
- AVX2 consumes 4x bandwidth vs scalar
- Memory controller saturates faster
- NUMA effects amplified
- **Test**: Compare with different memory sizes (L2 cache vs RAM)

#### Hypothesis 4: Non-Temporal Store Overhead
```rust
_mm256_stream_si256(...)  // Non-temporal store
```
- Bypasses cache (WC buffers)
- May be slower for small datasets
- Write combining buffer exhaustion
- **Test**: Replace `_mm256_stream_si256` with `_mm256_store_si256`

---

## 4. Verification Pattern Overhead

**Scalar verification (Line 8254-8264):**
```rust
for idx in chunk_start..chunk_end {
    let mirrored_idx = chunk_start + chunk_end - 1 - idx;
    let expected = (mirrored_idx as u64)... // Simple calculation
    let actual = *ptr.add(idx);
    if actual != expected { error }
}
```

**AVX2 verification (Lines 9109-9177):**
```rust
// Pattern generation IN THE HOT LOOP
while i < chunk_end {
    let mirrored_idx = chunk_sum - i;

    // ❌ EXPENSIVE: Pattern regeneration every iteration
    let expected = _mm256_set_epi32(
        (mirrored_idx as i32).wrapping_add(thread_pattern_base),
        (mirrored_idx as i32).wrapping_add(thread_pattern_base).wrapping_mul(2),
        (mirrored_idx as i32).wrapping_add(thread_pattern_base).wrapping_mul(3),
        (mirrored_idx as i32).wrapping_add(thread_pattern_base).wrapping_mul(4),
        (mirrored_idx as i32).wrapping_add(thread_pattern_base).wrapping_mul(5),
        (mirrored_idx as i32).wrapping_add(thread_pattern_base).wrapping_mul(6),
        (mirrored_idx as i32).wrapping_add(thread_pattern_base).wrapping_mul(7),
        (mirrored_idx as i32).wrapping_add(thread_pattern_base).wrapping_mul(8),
    );

    let actual = _mm256_load_si256(base.add(i));
    let diff = _mm256_xor_si256(expected, actual);
    error_accumulator = _mm256_or_si256(error_accumulator, diff);
    i += 1;
}
```

**Issue:** `_mm256_set_epi32()` is called EVERY iteration with 8 multiplications. This is likely slower than the scalar version's single multiplication!

**Scalar cost per iteration:**
- 1 add + 1 multiply + 1 load + 1 compare = ~4 µops

**AVX2 cost per iteration:**
- 8 multiplies + 8 adds + 1 `_mm256_set_epi32` + 1 load + 1 XOR + 1 OR = ~20+ µops

**The verification loop may be 5x SLOWER than scalar!**

---

## 5. Recommendations (Priority Order)

### 🔴 CRITICAL: Add #[target_feature] Attributes

**Change ALL SIMD implementations from:**
```rust
pub unsafe fn mirror_move_256_stream1_multi(...) -> TestStats {
    use std::arch::x86_64::*;

    if !is_x86_feature_detected!("avx2") {
        return zero_stats;
    }
    // SIMD code
}
```

**To:**
```rust
// Internal SIMD function with target_feature
#[target_feature(enable = "avx2")]
unsafe fn mirror_move_256_stream1_impl(...) -> TestStats {
    use std::arch::x86_64::*;
    // SIMD code - compiler KNOWS AVX2 is available
}

// Public wrapper with runtime check
pub unsafe fn mirror_move_256_stream1_multi(...) -> TestStats {
    if !is_x86_feature_detected!("avx2") {
        log::warn!("AVX2 not available");
        return zero_stats;
    }
    // Call target_feature function
    mirror_move_256_stream1_impl(...)
}
```

**Required for:**
- `mirror_move_128_*` → `#[target_feature(enable = "sse2")]` or `#[target_feature(enable = "sse4.1")]`
- `mirror_move_256_*` → `#[target_feature(enable = "avx2")]`
- `mirror_move_512_*` → `#[target_feature(enable = "avx512f")]`
- ALL StuckBitTest SIMD variants
- ALL RefreshStable SIMD variants

### 🟠 HIGH: Optimize Pattern Generation

**Current (SLOW):**
```rust
let expected = _mm256_set_epi32(
    (mirrored_idx as i32).wrapping_add(thread_pattern_base),
    (mirrored_idx as i32).wrapping_add(thread_pattern_base).wrapping_mul(2),
    // ... 6 more expensive calculations
);
```

**Optimized (FAST):**
```rust
// Pre-compute pattern increments ONCE
let pattern_base = _mm256_set_epi32(
    thread_pattern_base,
    thread_pattern_base.wrapping_mul(2),
    thread_pattern_base.wrapping_mul(3),
    thread_pattern_base.wrapping_mul(4),
    thread_pattern_base.wrapping_mul(5),
    thread_pattern_base.wrapping_mul(6),
    thread_pattern_base.wrapping_mul(7),
    thread_pattern_base.wrapping_mul(8),
);

// In hot loop: just one add
while i < chunk_end {
    let idx_vec = _mm256_set1_epi32(mirrored_idx as i32);
    let expected = _mm256_add_epi32(pattern_base, idx_vec);
    // ... rest of verification
}
```

This reduces 8 multiplies + 8 adds → 1 broadcast + 1 vector add per iteration.

### 🟡 MEDIUM: Test Non-Temporal Store Impact

Replace:
```rust
_mm256_stream_si256(base.add(idx), value);  // Bypass cache
```

With:
```rust
_mm256_store_si256(base.add(idx), value);   // Use cache
```

Test if cache-line fills are faster than WC buffer usage.

### 🟢 LOW: Add Cargo Features for SIMD

```toml
[profile.release]
opt-level = 3
lto = true
codegen-units = 1

# OPTIONAL: Enable AVX2 globally if target CPU supports it
# RUSTFLAGS="-C target-cpu=native" cargo build --release
```

---

## 6. Testing Plan

1. **Add #[target_feature] to ONE function** (mirror_move_256_stream1)
2. **Run --quick-test** comparing before/after
3. **Monitor CPU frequency** (HWiNFO64) during test
4. **If improved**, apply to all SIMD functions
5. **If NOT improved**, investigate Hypothesis 2-4

---

## 7. Expected Outcomes

**If #[target_feature] is the issue:**
- ✅ MirrorMove256 should be 2-4x faster than scalar
- ✅ Bandwidth should increase proportionally
- ✅ CPU utilization patterns should change

**If frequency throttling is the issue:**
- ❌ Performance stays the same or worse
- 📉 CPU frequency drops during AVX2 execution
- 🔧 May need AVX2 offset tuning or power limits adjustment

**If memory bandwidth is saturated:**
- 📊 Performance scales with memory size
- 🔍 Smaller tests (L2/L3 cache) show speedup
- 🔍 Large tests (RAM) show no improvement
