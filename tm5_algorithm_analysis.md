# TM5 MirrorMove Parameter Algorithm Analysis

Based on decompiled TM5 source code (MT0.cxx lines 910-1048), here's exactly what each Parameter value does:

## TM5 MirrorMove Parameter-Based Algorithm Selection

### Parameter=1 (default case): End-to-End Memory Reversal
```c
// TM5 default case (lines 1017-1048)
char *start_ptr = memory_base;
char *end_ptr = memory_base + size - 64;  // Points to last 64 bytes

do {
    // Load 64 bytes from start and end locations
    load_64_bytes_from(start_ptr);      // [0,1,2,3,4,5,6,7,8...63]
    load_64_bytes_from(end_ptr);        // [Size-64...Size-1]
    
    // Reverse order WITHIN each 64-byte chunk, then swap locations
    reverse_chunk_internally_and_swap_positions();
    
    start_ptr += 64;  // Move start forward
    end_ptr -= 64;    // Move end backward
} while (start_ptr < end_ptr);
```

**Memory Access Pattern**: 
```
Memory: [0......................Size-1]
         ^                      ^
         |                      |
    start_ptr              end_ptr
         |                      |
         +------- SWAP ---------+
```

**Example Result**: 
```
Input:  [0,1,2,3,4,5,6,7,8,9,A,B,C,D,E,F]
Output: [F,E,D,C,B,A,9,8,7,6,5,4,3,2,1,0]
```

### Parameter=2 (case 2): Dual-Half Cross-Mirror
```c
// TM5 case 2 (lines 914-946)  
char *ptr1 = memory_base;
char *ptr2 = memory_base + (size >> 1);  // Middle of memory
int half_size = size >> 1;

do {
    // Load from 4 strategic positions
    load_32_bytes(ptr1);                    // Start of first half
    load_32_bytes(ptr1 + half_size);        // Start of second half
    load_32_bytes(ptr2 - 32);               // End of first half  
    load_32_bytes(ptr2 + half_size - 32);   // End of second half
    
    // CROSS-SWAP between memory halves
    write_first_half_start_to_second_half_end();
    write_second_half_start_to_first_half_end();
    write_first_half_end_to_second_half_start();
    write_second_half_end_to_first_half_start();
    
    ptr1 += 32;
    ptr2 -= 32;
} while (condition);
```

**Memory Access Pattern**: 
```
Memory: [First Half     ][Second Half    ]
        [0....Mid-1     ][Mid....Size-1  ]
         ^      ^         ^        ^
         |      |         |        |
         A------+----C----+        |
         |      |    |             |
         |      B----+--------D----+
         |           |
         +-----------+
    A ↔ D cross-swap
    B ↔ C cross-swap
```

**Example Result**: 
```
Input:  [0,1,2,3,4,5,6,7][8,9,A,B,C,D,E,F]
        First Half       Second Half

After Cross-Swap:
Output: [F,E,D,C,B,A,9,8][7,6,5,4,3,2,1,0]
         ^Second→First^   ^First→Second^
```

### Parameter=4 (case 4): Quad-Quarter Cross-Mirroring
```c
// TM5 case 4 (lines 978-1015)
char *ptr1 = memory_base;
char *ptr2 = memory_base + (size >> 2);      // Quarter point
int q1_size = size >> 2;                     // Quarter size
int q2_size = 2 * (size >> 2);               // Half size
int q3_size = 3 * (size >> 2);               // 3/4 size

do {
    // Load from 8 different quarter positions (16 bytes each)
    load_16_bytes(ptr1);               // Q1 start
    load_16_bytes(ptr1 + q1_size);     // Q2 start  
    load_16_bytes(ptr1 + q2_size);     // Q3 start
    load_16_bytes(ptr1 + q3_size);     // Q4 start
    load_16_bytes(ptr2 - 16);          // Q1 end
    load_16_bytes(ptr2 + q1_size - 16); // Q2 end
    load_16_bytes(ptr2 + q2_size - 16); // Q3 end
    load_16_bytes(ptr2 + q3_size - 16); // Q4 end
    
    // COMPLEX 4-WAY CROSS-SWAPPING
    write_q1_start_to_q4_end();
    write_q2_start_to_q3_end();
    write_q3_start_to_q2_end();
    write_q4_start_to_q1_end();
    write_q1_end_to_q4_start();
    write_q2_end_to_q3_start();
    write_q3_end_to_q2_start();
    write_q4_end_to_q1_start();
} while (condition);
```

**Memory Access Pattern**: 
```
Memory: [Q1   ][Q2   ][Q3   ][Q4   ]
        [0-25%][25-50%][50-75%][75-100%]
         A  B   C  D   E  F   G  H
         |  |   |  |   |  |   |  |
         +--|---|--+   +--|---|--+
            |   |         |   |
            +---|---------+   |
                |             |
                +-------------+
```

**Cross-Swap Pattern**:
- A ↔ H (Q1_start ↔ Q4_end)
- B ↔ G (Q1_end ↔ Q4_start)  
- C ↔ F (Q2_start ↔ Q3_end)
- D ↔ E (Q2_end ↔ Q3_start)

**Example Result**: 
```
Input:  [0,1][2,3][4,5][6,7]
         Q1   Q2   Q3   Q4

After 4-way Cross-Swap:
Output: [7,6][5,4][3,2][1,0]
         ^Q4  ^Q3  ^Q2  ^Q1
```

### Parameter=3 (case 3): Triple-Stream Complex Division
```c
// TM5 case 3 (lines 947-977)
// Divides memory by 0x180 (384), works with 3-way partitioning
int division_size = (size / 0x180) << 7;  // Complex calculation
// Creates 3-stream access pattern with mathematical division
```

**Memory Access Pattern**: 
```
Memory divided into 3 unequal mathematical partitions based on 384-byte boundaries
More complex than simple thirds - uses specific mathematical divisions
```

## Key TM5 vs TMR Algorithm Differences

### TM5 Algorithms Test Different Memory Stress Patterns:

1. **Parameter=1**: Tests memory controller's ability to handle **long-distance memory operations** (start ↔ end)
2. **Parameter=2**: Tests **cross-half memory bandwidth** and addressing logic  
3. **Parameter=4**: Tests **complex multi-quarter cross-referencing** and cache behavior
4. **Parameter=3**: Tests **mathematical partition access** patterns

### TMR Algorithms Are All Variations of Local Mirroring:

1. **streams=1**: Mirror entire chunk `[0,1,2,3,4,5,6,7] → [7,6,5,4,3,2,1,0]`
2. **streams=2**: Mirror two halves `[0,1,2,3,4,5,6,7] → [3,2,1,0,7,6,5,4]`
3. **streams=4**: Mirror four quarters `[0,1,2,3,4,5,6,7] → [1,0,3,2,5,4,7,6]`

## The Critical Difference

**TM5**: Tests memory controller's ability to handle **cross-region data movement** at various distances and complexities

**TMR**: Tests memory with **localized mirroring patterns** that don't stress long-distance memory operations

## Potential Coverage Gap

TM5's algorithms specifically test:
- Long-distance memory access (start to end)
- Cross-regional bandwidth (half-to-half, quarter-to-quarter)
- Complex addressing patterns that stress memory controllers differently

TMR's algorithms test:
- Local pattern consistency 
- SIMD optimization efficiency
- Parallel stream performance

**The question**: Do memory errors manifest differently under long-distance cross-regional access patterns vs. localized pattern verification?

## Implementation Plan for TMR Cross-Regional Algorithms

### Priority: High - This is a critical feature gap

Based on analysis, TMR should implement TM5's cross-regional memory access patterns to maintain compatibility and ensure equivalent error detection coverage.

### Proposed Design: Branch Once, Optimize Each Path

```rust
pub unsafe fn mirror_move_128(
    ptr: *mut u8, 
    size: usize, 
    thread_id: usize, 
    error_mode: ErrorMode, 
    timing: &TestTiming, 
    config: &TestMemoryConfig
) -> TestStats {
    // Branch ONCE on algorithm type (not in hot loop)
    match config.parameter {
        Some(2) => mirror_move_128_cross_half(ptr, size, thread_id, error_mode, timing, config),
        Some(4) => mirror_move_128_cross_quarter(ptr, size, thread_id, error_mode, timing, config), 
        Some(param) if param > 100 => mirror_move_128_tm5_variant(ptr, size, thread_id, error_mode, timing, config, param),
        _ => {
            // Use existing stream-based dispatch for small parameters
            let streams = config.streams.max(1) as usize;
            // ... existing stream logic
        }
    }
}
```

### Algorithm Mode Configuration

```rust
#[derive(Debug, Clone, PartialEq)]
pub enum AlgorithmMode {
    TMRStreams,        // Use TMR's stream-based approach
    TM5CrossHalf,      // Use TM5's cross-half algorithm  
    TM5CrossQuarter,   // Use TM5's cross-quarter algorithm
    TM5EndToEnd,       // Use TM5's end-to-end algorithm
}

impl TestMemoryConfig {
    pub fn algorithm_from_parameter(parameter: Option<u32>) -> AlgorithmMode {
        match parameter {
            Some(2) => AlgorithmMode::TM5CrossHalf,
            Some(4) => AlgorithmMode::TM5CrossQuarter,
            Some(1) => AlgorithmMode::TM5EndToEnd,
            Some(param) if param > 100 => AlgorithmMode::TM5EndToEnd, // TM5 variants
            _ => AlgorithmMode::TMRStreams, // Use TMR's approach
        }
    }
}
```

### SIMD-Optimized Cross-Half Example

```rust
unsafe fn mirror_move_128_cross_half(/* ... */) -> TestStats {
    // Pre-compute pointers outside hot loop
    let first_half_base = base;
    let second_half_base = base.add(half_len);
    
    // Hot loop: Cross-swap between halves with SIMD
    let mut start_idx = processed;
    let mut end_idx = chunk_end - 1;
    
    while start_idx < end_idx {
        // Load from both halves
        let first_half_start = _mm_load_si128(first_half_base.add(start_idx));
        let first_half_end = _mm_load_si128(first_half_base.add(end_idx));
        let second_half_start = _mm_load_si128(second_half_base.add(start_idx));
        let second_half_end = _mm_load_si128(second_half_base.add(end_idx));
        
        // TM5-style cross-swap between halves (no branching)
        _mm_store_si128(first_half_base.add(start_idx), second_half_end);
        _mm_store_si128(first_half_base.add(end_idx), second_half_start);
        _mm_store_si128(second_half_base.add(start_idx), first_half_end);
        _mm_store_si128(second_half_base.add(end_idx), first_half_start);
        
        start_idx += 1;
        end_idx -= 1;
    }
}
```

### Implementation Strategy

1. **Phase 1**: Implement Parameter=2 (cross-half) with SIMD optimization
2. **Phase 2**: Add Parameter=4 (cross-quarter) 
3. **Phase 3**: Add Parameter=1 (end-to-end) and other variants
4. **Phase 4**: Benchmark and validate error detection vs TM5

### Performance Requirements

- Branch once outside hot loops
- Maintain SIMD optimizations (AVX2/AVX-512)
- Keep chunked processing for responsive shutdown
- Use existing error accumulation patterns
- Add prefetching for cross-regional memory access

### Expected Benefits

- **Compatibility**: True TM5 Parameter behavior replication
- **Coverage**: Cross-regional memory stress testing
- **Validation**: Head-to-head error detection comparison possible
- **Performance**: SIMD-optimized cross-regional algorithms