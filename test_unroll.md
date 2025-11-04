# Loop Unrolling Analysis for StuckBitTest Performance

## Current Loop Structure (All Variants):
```rust
// Sequential processing - one vector at a time
for i in processed..chunk_end {
    _mm256_store_si256(base.add(i), pattern1);
}
```

## Issue:
- Each store must complete before next one starts
- No instruction-level parallelism
- Pipeline stalls waiting for memory

## Potential Fix: 4-way Unrolling
```rust
// Process 4 vectors per iteration
let mut i = processed;
while i + 4 <= chunk_end {
    _mm256_store_si256(base.add(i), pattern1);
    _mm256_store_si256(base.add(i + 1), pattern1);
    _mm256_store_si256(base.add(i + 2), pattern1);
    _mm256_store_si256(base.add(i + 3), pattern1);
    i += 4;
}
// Handle remainder
while i < chunk_end {
    _mm256_store_si256(base.add(i), pattern1);
    i += 1;
}
```

## Benefits:
- CPU can issue multiple stores in parallel
- Better use of memory bandwidth
- Reduced loop overhead
- Better instruction scheduling

## Testing Approach:
1. Try 2-way unrolling first
2. Test 4-way unrolling
3. Compare performance

Would you like me to implement loop unrolling for StuckBitTest256 to test this theory?