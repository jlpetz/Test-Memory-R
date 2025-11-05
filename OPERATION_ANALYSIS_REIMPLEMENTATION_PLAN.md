# Operation Analysis Report - Re-Implementation Plan

## ⚠️ Important Note on Table Format

**The example output shown below is from an old version and may not match the current data structures.**

The existing `DetailedOperationCount` struct supports more fields than shown in the example:
- ✅ total_reads, total_writes (shown in example)
- ✅ total_simd_ops, total_cache_ops (shown in example)
- ❓ total_verifies (was in footer, might need its own column)
- ❓ total_fence_ops (was in footer, might need its own column)
- ❓ simd_type (was in footer, might need display)
- ❓ access_pattern (not shown in example)

**Recommendation:** Implement the **full detailed version first** with all available fields, then simplify/reorganize columns if the table becomes too wide or cluttered. Better to have too much detail initially than to guess which fields were actually displayed.

Consider:
- Main table with all operation types as columns
- Footer/summary section for aggregate metrics
- Separate small table for SIMD types per test
- Balance between information density and readability

## Overview
This document outlines the plan to restore the "Detailed Operation Analysis" report that was lost during the git checkout rollback. This report provides granular insights into memory operations performed by each test.

## What Was Lost

### Expected Output Location
- Displayed at the end of test run, after "Final Test Summary - Per-Test Performance"
- Showed operation breakdown per test and aggregate totals

### Example Output Format
```
🔍 Detailed Operation Analysis
═══════════════════════════════════════════════════════════════

📊 Operations by Test:
┌─────────────────────────┬──────────────┬───────────────┬──────────────┬─────────────┐
│ Test                    │ Total Reads  │ Total Writes  │ SIMD Ops     │ Cache Ops   │
├─────────────────────────┼──────────────┼───────────────┼──────────────┼─────────────┤
│ StuckBitTest            │            0 │             0 │            0 │           0 │
│ MirrorMove128           │         1.1B │          1.1B │         3.2B │        1.1B │
│ SimpleTest              │       118.8B │        118.8B │            0 │           0 │
├─────────────────────────┼──────────────┼───────────────┼──────────────┼─────────────┤
│ TOTAL                   │       612.2B │        493.3B │        29.0B │        9.7B │
└─────────────────────────┴──────────────┴───────────────┴──────────────┴─────────────┘

📈 Additional Metrics:
   • Total Verifications: 450.6B
   • Memory Fence Operations: 11
   • SIMD Instruction Type: AVX512
```

## What Still Exists ✅

### 1. Data Structures (src/tests.rs)
```rust
pub struct DetailedOperationCount {
    pub total_reads: u64,
    pub total_writes: u64,
    pub total_verifies: u64,
    pub total_simd_ops: u64,
    pub total_fence_ops: u64,
    pub total_cache_ops: u64,
    pub simd_type: SIMDType,
    pub access_pattern: AccessPattern,
}

pub struct OperationMetadata {
    pub reads_per_op: u64,
    pub writes_per_op: u64,
    pub verifies_per_op: u64,
    pub simd_ops_per_op: u64,
    pub fence_ops_per_op: u64,
    pub cache_ops_per_op: u64,
    pub simd_type: SIMDType,
    pub access_pattern: AccessPattern,
}
```

### 2. Reporting Models (src/reporting/models.rs:175)
```rust
pub struct OperationBreakdown {
    pub total_reads: u64,
    pub total_writes: u64,
    pub total_verifies: u64,
    pub total_simd_ops: u64,
    pub total_fence_ops: u64,
    pub total_cache_ops: u64,
    pub simd_type: String,
    pub access_pattern: String,
}
```

### 3. Converter Function (src/reporting/converters.rs:330)
```rust
pub fn create_operation_breakdown(detailed: &crate::tests::DetailedOperationCount) -> OperationBreakdown
```

### 4. TestSummary Integration (src/reporting/models.rs:169)
```rust
pub struct TestSummary {
    // ...
    pub operation_breakdown: Option<OperationBreakdown>,
}
```

## What's Missing ❌

### 1. **Formatter Functions**
**Location:** `src/reporting/formatters.rs`
**Required:**
- `prepare_operation_analysis_table()` - Main table showing per-test breakdown
- Table trait method declaration
- Implementation in DefaultFormatter

**Columns needed:**
- Test Name
- Total Reads (formatted as "1.1B", "450.6M", etc.)
- Total Writes
- SIMD Ops
- Cache Ops
- Verifications (optional column or footer)

**Footer/Summary:**
- Aggregate totals across all tests
- Additional metrics (fence ops, SIMD type)

### 2. **Display Logic**
**Location:** `src/runner.rs` or `src/main.rs`
**Required:**
- Call to collect operation breakdowns from test results
- Aggregate across all tests in the cycle
- Pass to reporting system
- Display after "Final Test Summary - Per-Test Performance"

### 3. **Test Metadata Definition**
**Location:** Each test in `src/tests.rs` or test registration
**Required:**
- Define `OperationMetadata` for each test
- Specify operations per iteration for each test type

**Examples:**
```rust
// MirrorMove: reads entire block, writes entire block, verifies
reads_per_op: block_size / 8,  // 64-bit reads
writes_per_op: block_size / 8,
verifies_per_op: block_size / 8,

// SimpleTest: only writes
reads_per_op: 0,
writes_per_op: block_size / 8,
verifies_per_op: 0,
```

### 4. **Operation Counting Integration**
**Location:** Test execution path
**Required:**
- Calculate `DetailedOperationCount` after each test completes
- Multiply `total_operations` by test's `OperationMetadata`
- Store in `TestSummary.operation_breakdown`

## Implementation Steps

### Phase 1: Survey Current Test Landscape
**Goal:** Determine which tests need operation metadata and verify operation counting

**Tasks:**
1. ✅ Review all active tests in test registry
2. ✅ Identify tests that have changed since original implementation:
   - New SIMD variants (128/256/512/512_A)
   - Modified test logic (multiblock migration)
   - New tests added
3. ✅ Check if tests are currently populating `total_operations` correctly
4. ✅ Verify TestStats structure includes operation count

**Deliverable:** List of all tests with their current operation characteristics

### Phase 2: Define Operation Metadata
**Goal:** Create accurate operation metadata for each test

**Tasks:**
1. For each test, define:
   - Reads per operation
   - Writes per operation
   - Verifies per operation
   - SIMD operations per operation
   - Cache operations (if applicable)
   - Fence operations (if applicable)
   - SIMD type (None/SSE2/AVX2/AVX512)
   - Access pattern (Sequential/Strided/Random/Mirror/etc.)

2. Consider multiblock tests:
   - Operation counts may vary based on interleaving strategy
   - Need to account for multiple blocks in single operation

3. Document assumptions:
   - What constitutes "one operation"? (typically one full pass through assigned memory)
   - How to count SIMD ops? (per instruction or per lane?)

**Deliverable:** `OperationMetadata` struct for each test, documented

### Phase 3: Implement Calculation Logic
**Goal:** Calculate detailed operation counts after test completion

**Location:** `src/runner.rs` or test result aggregation point

**Tasks:**
1. After test completes, retrieve:
   - `total_operations` from TestStats
   - Test's `OperationMetadata`

2. Calculate `DetailedOperationCount`:
   ```rust
   let detailed = DetailedOperationCount {
       total_reads: stats.total_operations * metadata.reads_per_op,
       total_writes: stats.total_operations * metadata.writes_per_op,
       total_verifies: stats.total_operations * metadata.verifies_per_op,
       total_simd_ops: stats.total_operations * metadata.simd_ops_per_op,
       total_fence_ops: stats.total_operations * metadata.fence_ops_per_op,
       total_cache_ops: stats.total_operations * metadata.cache_ops_per_op,
       simd_type: metadata.simd_type.clone(),
       access_pattern: metadata.access_pattern.clone(),
   };
   ```

3. Store in test summary result structure

**Deliverable:** Operation counts populated for each test run

### Phase 4: Create Formatter
**Goal:** Add table formatting for operation analysis

**Location:** `src/reporting/formatters.rs`

**Tasks:**
1. Add to `ReportFormatter` trait:
   ```rust
   fn prepare_operation_analysis_table(&self, report: &OperationAnalysisReport) -> TableData;
   ```

2. Implement in `DefaultFormatter`:
   - Format large numbers (B/M/K notation like "1.1B", "450.6M")
   - Create per-test rows with operation breakdown
   - Add TOTAL aggregate row
   - Add footer with additional metrics

3. Consider table width and readability:
   - Use shortened column headers ("Reads" not "Total Reads")
   - Align numbers right
   - Use consistent formatting

**Deliverable:** Formatted table ready for rendering

### Phase 5: Integrate Display Logic
**Goal:** Display the report in the correct location

**Location:** `src/runner.rs` around line 1318 (after Final Test Summary)

**Tasks:**
1. Collect operation breakdowns from all test summaries
2. Aggregate totals across all tests
3. Create `OperationAnalysisReport` model (may need to define this)
4. Call formatter and renderer:
   ```rust
   if let Some(op_report) = create_operation_analysis_report(&test_summaries) {
       reporter.report_operation_analysis(&op_report)?;
   }
   ```

5. Ensure it displays:
   - After "Final Test Summary - Per-Test Performance"
   - Before the final completion message ("✅ All test cycles completed successfully!")

**Deliverable:** Report displays in correct location with accurate data

### Phase 6: Testing & Validation
**Goal:** Verify accuracy and usefulness

**Tasks:**
1. Run test suite and verify:
   - Operation counts make sense (cross-check with known test behavior)
   - SIMD type is correct for each test
   - Totals sum correctly
   - Large number formatting is readable

2. Compare with historical data if available:
   - Check if operation counts match previous implementation
   - Validate new tests have reasonable counts

3. Edge cases:
   - Tests with zero operations (should display "0" not error)
   - Tests with missing metadata (should handle gracefully)
   - Very large operation counts (formatting should scale)

**Deliverable:** Validated, accurate operation analysis report

## Technical Considerations

### Operation Count Definition
- **"One Operation"** typically means one complete pass through the test memory
- For multiblock tests, this may be one interleaved pass across all blocks
- Tests should document what constitutes one operation in their metadata

### SIMD Operation Counting
- **Option A:** Count SIMD instructions (1 AVX-512 load = 1 SIMD op)
- **Option B:** Count data lanes (1 AVX-512 load = 8 operations, 64-byte vector)
- **Recommendation:** Use Option A (instruction count) for consistency

### Large Number Formatting
- Use human-readable suffixes: K (thousand), M (million), B (billion)
- Format: "1.1B", "450.6M", "3.2K"
- Precision: 1 decimal place is sufficient
- Handle edge case of 0 operations

### Performance Impact
- Operation counting should have minimal overhead
- Calculations are post-test (no runtime impact)
- Metadata should be compile-time constants where possible

## Tests Requiring Metadata

### Current Test List (as of January 2025)
Based on typical test configurations, these tests need operation metadata:

1. **StuckBitTest** + variants (128/256/512/512_A)
   - Reads: High (pattern verification)
   - Writes: High (pattern writing)
   - Verifies: High (bit flip detection)
   - SIMD: Varies by variant

2. **MirrorMove** + variants (128/256/512/512_A)
   - Reads: High (mirror read)
   - Writes: High (mirror write)
   - Verifies: High (mirror verification)
   - SIMD: Varies by variant
   - Cache Ops: Block-sized copies

3. **SimpleTest**
   - Reads: None (write-only test)
   - Writes: Very high (rapid writes)
   - Verifies: None
   - SIMD: None

4. **RefreshStable** + variants (128/256/512/512_A)
   - Reads: High (refresh verification)
   - Writes: High (pattern writes)
   - Verifies: High
   - SIMD: Varies by variant

5. **CacheBusting**
   - Reads: Very high (cache eviction)
   - Writes: Very high (cache pollution)
   - Verifies: Moderate
   - SIMD: None

6. **RandomTorture**
   - Reads: High (random pattern)
   - Writes: High
   - Verifies: High
   - SIMD: None
   - Access: Random

7. **StrideAccess**
   - Reads: Moderate (strided pattern)
   - Writes: Moderate
   - Verifies: Moderate
   - SIMD: None
   - Access: Strided

8. **BandwidthSat**
   - Reads: Very high (bandwidth saturation)
   - Writes: Very high
   - Verifies: None
   - SIMD: Likely (for bandwidth)

9. **BlockMove**
   - Reads: High (block copy)
   - Writes: High (block copy)
   - Verifies: None
   - SIMD: None
   - Cache Ops: Block-sized

**Note:** Exact counts need verification against current test implementations.

## Open Questions

1. **Metadata Storage:**
   - Should metadata be stored with test registration?
   - Or as a separate lookup table?
   - How to associate metadata with test name?

2. **Multiblock Handling:**
   - Do we count operations per-block or aggregate?
   - How does interleaving affect operation counts?

3. **SIMD Variants:**
   - Do 128/256/512 variants have different operation counts?
   - Or just different SIMD types?

4. **Backwards Compatibility:**
   - Should we support old test results without operation data?
   - How to handle missing metadata gracefully?

5. **Report Utility:**
   - Is this report valuable for end users?
   - Or mainly for developers/debugging?
   - Should it be optional/debug-only?

## Priority Assessment

**Value:** Medium-High
- Provides insights into test behavior
- Helps identify performance bottlenecks
- Useful for debugging test issues

**Effort:** Medium
- Core infrastructure exists
- Main work is metadata definition and formatting
- Estimated 4-6 hours for complete implementation

**Risk:** Low
- Non-critical feature (doesn't affect test correctness)
- Can be implemented incrementally
- Easy to test and validate

**Recommendation:** Add to backlog, implement when time permits. Consider implementing after higher-priority items like:
- CPU feature bypass messages
- Non-temporal/temporal evaluation
- Driver cleanup improvements

## Success Criteria

The implementation is complete when:

1. ✅ All active tests have accurate operation metadata defined
2. ✅ Operation counts are calculated and stored in test summaries
3. ✅ Report displays at the correct location (after Per-Test Performance)
4. ✅ Table formatting is clean and readable
5. ✅ Large numbers use K/M/B notation
6. ✅ Totals aggregate correctly across all tests
7. ✅ SIMD types and access patterns are displayed
8. ✅ Report matches expected output format
9. ✅ No performance impact during test execution
10. ✅ Handles edge cases gracefully (zero ops, missing metadata)

## Related Files

**Modified:**
- `src/reporting/formatters.rs` - Add table formatter
- `src/reporting/converters.rs` - May need aggregate function
- `src/runner.rs` - Add display logic
- `src/tests.rs` - May need metadata storage

**Reference:**
- `src/reporting/models.rs` - Existing data structures
- `src/test_framework.rs` - Operation counting
- Test definitions - For determining accurate metadata

## Notes

- This report was working before the git checkout rollback that lost ~1 month of work
- Original implementation details may differ from this plan
- Tests have been significantly modified since original implementation (multiblock migration, SIMD variants)
- Metadata accuracy is critical - better to omit uncertain tests than show wrong data
