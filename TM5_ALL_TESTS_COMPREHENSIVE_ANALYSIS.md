# TM5 All Tests - Comprehensive Architecture Analysis

## Executive Summary

TM5 uses **TWO different test architectures**:

1. **Framework-Separated**: Init/Test/Verify are separate functions (RefreshStable, MirrorMove)
2. **Self-Contained**: Write+Verify inline in same function (SimpleTest)

This explains why TMR's current implementation is fundamentally wrong for MirrorMove - it's mixing these two architectures incorrectly.

## Test 0: RefreshStable (RS_Check) - Framework-Separated

**Location**: TM5/bin/mtests0.asm lines 104-188

### Architecture: VERIFY-ONLY

```asm
RS_Check proc
CheckNextPage:
    mov eax, edi
    call RS_GeneratePattern        ; Generate expected pattern → xmm0,1,2
    mov ecx, 85                     ; 85 iterations

CheckPage:
    prefetchnta [edi+1024]
    movaps xmm3, [edi]             ; READ from memory
    movaps xmm4, [edi+16]
    movaps xmm5, [edi+32]
    p_xor xmm3, xmm0                ; XOR with expected
    p_xor xmm4, xmm1
    p_xor xmm5, xmm2
    p_or xmm7, xmm3                 ; Accumulate errors
    p_or xmm7, xmm4
    p_or xmm7, xmm5
    ; Continue loop...

    ; Check if xmm7 == 0 (no errors)
    movups [edx], xmm7
    mov eax, [edx]
    or eax, [edx+4]
    or eax, [edx+8]
    or eax, [edx+12]
    jz DoneOk                       ; No errors
    jmp Abort                       ; Errors found
RS_Check endp
```

### What It Does:
- **Generates** expected pattern based on address
- **Reads** memory
- **XORs** with expected pattern
- **Accumulates** any mismatches in xmm7
- **Reports** success/failure

### What It Does NOT Do:
- Does NOT write initial pattern (assumes already written)
- Does NOT modify memory

### Purpose:
This is the **"Test 0" verification function** that the author mentions:

> "The functions themselves only shake the block, but do not test the integrity of the data. **Test 0 is used for verification.**"

## SimpleTest (ST_Check) - Self-Contained

**Location**: TM5/bin/mtests0.asm lines 198-570

### Architecture: INLINE WRITE + VERIFY

```asm
ST_Check proc
    ; === WRITE PHASE ===
LoopCheckCycle:
    mov eax, edi
    call ST_GeneratePattern         ; Generate pattern → xmm0,1,2
    p_xor xmm6, xmm6

InterleaveFill:
FillNextPage:
    call ST_GeneratePattern2        ; Update pattern for page
WritePage:
    prefetchnta [edi+esi]
    movaps [edi], xmm0              ; WRITE pattern
    db 66h
    PADDD xmm0, xmm6                ; Increment pattern
    movaps [edi+16], xmm1           ; WRITE
    db 66h
    PADDD xmm1, xmm6
    movaps [edi+32], xmm2           ; WRITE
    db 66h
    PADDD xmm2, xmm6
    ; Continue writing...

    ; === VERIFY PHASE ===
LoopRead:
    call ST_GeneratePattern         ; Regenerate same pattern
    p_xor xmm7, xmm7                ; Clear error accumulator
    p_xor xmm6, xmm6

InterleaveCheck:
CheckNextPage:
    call ST_GeneratePattern2
CheckPage:
    prefetchnta [edi+esi]
    movaps xmm3, [edi]              ; READ
    movaps xmm4, [edi+16]
    movaps xmm5, [edi+32]
    p_xor xmm3, xmm0                ; XOR with expected
    db 66h
    PADDD xmm0, xmm6
    p_xor xmm4, xmm1
    db 66h
    PADDD xmm1, xmm6
    p_xor xmm5, xmm2
    db 66h
    PADDD xmm2, xmm6
    p_or xmm7, xmm3                 ; Accumulate errors
    p_or xmm7, xmm4
    p_or xmm7, xmm5
    ; Continue checking...

    jg LoopCheckCycle               ; Repeat write+verify cycles
ST_Check endp
```

### What It Does:
- **Writes** pattern to memory
- **Verifies** pattern immediately after
- **Repeats** write+verify cycles (ST_WriteReadCycles = 4)
- **Self-contained** - doesn't rely on external init/verify

### Flow:
```
Loop N times:
    1. Write pattern to entire block
    2. Verify pattern in entire block
    3. Repeat
```

## MirrorMove (MirrorMove_Check) - Framework-Separated

**Location**: TM5/bin/mtests0.asm lines 1086-1462

### Architecture: SWAP-ONLY

```asm
MirrorMove_Check proc
    ; Calculate loop counter
    mov dLoopCounter, eax
    mov dWriteReadCycleCounter, eax

SwapWindow:
    mov edi, pBlock                 ; Start pointer
    mov edx, MaxAddr                ; End pointer
    sub edx, 16*4
    mov ecx, dBlockSize
    shr ecx, 6                      ; 64-byte chunks

SwapBlk:
    prefetchnta [edi+1024]
    prefetchnta [edx-1024]
    movaps xmm0, [edi]              ; Load from start
    movaps xmm1, [edi+16]
    movaps xmm2, [edi+32]
    movaps xmm3, [edi+48]
    movaps xmm4, [edx]              ; Load from end
    movaps xmm5, [edx+16]
    movaps xmm6, [edx+32]
    movaps xmm7, [edx+48]
    movaps [edx], xmm0              ; Write start → end
    movaps [edx+16], xmm1
    movaps [edx+32], xmm2
    movaps [edx+48], xmm3
    movaps [edi], xmm4              ; Write end → start
    movaps [edi+16], xmm5
    movaps [edi+32], xmm6
    movaps [edi+48], xmm7
    add edi, 64                     ; Move forward
    sub edx, 64                     ; Move backward
    dec ecx
    jg SwapBlk

CycleDone:
    dec dWriteReadCycleCounter
    jg SwapWindow                   ; Repeat swaps

    ; Return timing
    rdtsc
    ; ...
    ret
MirrorMove_Check endp
```

### What It Does:
- **Swaps** first/last halves repeatedly
- **Even swaps** restore original state
- **Odd swaps** leave memory mirrored
- **Returns** timing information

### What It Does NOT Do:
- Does NOT initialize memory
- Does NOT verify correctness
- Does NOT check for errors

### Author's Explanation (TM5_Author_website_notes.txt):
> "The functions themselves **only shake the block**, but do not test the integrity of the data. **Test 0 is used for verification**."

> "With an **even number of reflections, the final state of the block does not change** (if there are no failures)."

## BlockMove (BM_Check) - NOT IMPLEMENTED

**Location**: TM5/bin/mtests0.asm lines 574-715

```asm
BM_Check proc
    ; Setup code...

    ; **** STUB ****
    jmp Abort                       ; Not implemented!

    ; Never reached
Done_Ok:
    ; ...
Abort:
    xor eax, eax
    ret
BM_Check endp
```

TM5's BlockMove is **stubbed out** - it jumps directly to Abort.

## Generic Init/Verify Functions

### sub_10001810 - Write Pattern

**Location**: TM5_Source/MT0.cxx lines 503-549

```c
int sub_10001810(unsigned int size, unsigned int ptr, ..., xmm1, xmm2) {
    for (page = 0; page < size; page += 4096) {
        v6 = generate_pattern(ptr);
        for (i = 0; i < 85; i++) {
            _mm_prefetch(ptr + 1024);
            *(xmm *)ptr = v6;           // WRITE pattern
            *(xmm *)(ptr+16) = xmm1;
            *(xmm *)(ptr+32) = xmm2;
            ptr += 48;
        }
        *(xmm *)ptr = v6;
        ptr += 16;
    }
    return 1;
}
```

**Used by**: Test 0 (RefreshStable) initialization

### sub_10001885 - Verify Pattern

**Location**: TM5_Source/MT0.cxx lines 553-600

```c
int sub_10001885(unsigned int size, unsigned int ptr, ..., xmm1, xmm2) {
    xmm7 = 0;  // Error accumulator

    for (page = 0; page < size; page += 4096) {
        v7 = generate_pattern(ptr);
        for (i = 0; i < 85; i++) {
            _mm_prefetch(ptr + 1024);

            // XOR with expected, accumulate errors
            xmm7 = _mm_or_ps(_mm_or_ps(_mm_or_ps(
                xmm7,
                _mm_xor_ps(*(xmm *)ptr, v7)),        // Compare
                _mm_xor_ps(*(xmm *)(ptr+16), xmm1)),
                _mm_xor_ps(*(xmm *)(ptr+32), xmm2));

            ptr += 48;
        }
        xmm7 = _mm_or_ps(xmm7, _mm_xor_ps(*(xmm *)ptr, v7));
        ptr += 16;
    }

    return check_xmm7_for_errors();  // Any bits set = error
}
```

**Used by**: Test 0 (RefreshStable) verification

## TM5 Test Orchestration

Based on code analysis, TM5 uses a **state machine** (sub_10001782) with command bytes:

### For Framework-Separated Tests (MirrorMove):
```
Framework calls:
    1. Set command = INIT
       → sub_10001782 calls sub_10001810
       → Memory initialized with pattern

    2. Set command = TEST
       → sub_10001782 calls MirrorMove_Check
       → Memory swapped (even count = restore)

    3. Set command = VERIFY
       → sub_10001782 calls sub_10001885
       → Memory verified against original pattern
       → Errors reported if round-trip failed
```

### For Self-Contained Tests (SimpleTest):
```
Framework calls:
    1. Set command = TEST
       → sub_10001782 calls ST_Check
       → ST_Check does its own write+verify cycles
       → Returns success/failure
```

## When Does Init Happen?

### RefreshStable / Test 0:
- **BEFORE first test**: Framework calls sub_10001810 (init)
- **ONCE at start**: Not per-cycle
- **After test**: Framework calls sub_10001885 (verify)

### SimpleTest:
- **EVERY cycle**: Writes pattern then verifies
- **Inline**: Part of test loop
- **4 cycles** per iteration (ST_WriteReadCycles = 4)

### MirrorMove:
- **BEFORE test suite**: Framework calls init (shared with Test 0?)
- **RELIES ON**: Test 0 for verification
- **EVEN swaps**: Returns memory to original state for verification
- **AFTER test**: Framework calls verify (same as Test 0)

## Pattern Generation: Generic or Test-Specific?

### Test 0 (RefreshStable):
- Uses `RS_GeneratePattern` (RefreshStable-specific)
- Address-based pattern generation
- DIMM-aware patterns

### SimpleTest:
- Uses `ST_GeneratePattern` and `ST_GeneratePattern2`
- Incrementing patterns (PADDD - add to pattern each iteration)
- Jump-aware for DIMM interleaving

### MirrorMove:
- Uses `GetPattern_Jump` (common helper)
- **Assumes memory pre-initialized** (probably by Test 0's init)
- **Relies on Test 0's verify** after swaps

### Common:
- All use `GetPattern_Jump` for parameter-based configuration
- All use address-based pattern generation (prevents cache reuse)
- All use DIMM-aware calculations

## TMR Implications - Critical Issues

### Issue 1: TMR MirrorMove Verifies MIRRORED Pattern ❌

**TM5 flow**:
```
INIT pattern → Mirror (swap) → Mirror-back (restore) → VERIFY original pattern
```

**TMR CURRENT (WRONG)**:
```
INIT pattern → Mirror (swap) → VERIFY mirrored pattern ❌ → Mirror-back
```

TMR is checking if the mirrored state is correct, **not** if the round-trip worked!

### Issue 2: TMR Missing Final Verify ❌

**TM5 flow**:
```
Even swaps → Memory restored to original → Verify catches round-trip errors
```

**TMR CURRENT**:
```
Mirror-back → No verify → Errors in mirror-back go undetected ❌
```

### Issue 3: TMR Init Accounting ❌

**TM5**: Init is separate, not counted in test timing

**TMR CURRENT**: Init is inline but not accounted for

### Issue 4: Incorrect Byte Accounting ❌

**TMR reports**: `* 3` (mirror + verify + mirror-back)

**TMR reality**: `* 5` or `* 6` bytes

**TM5 approach**: Separate phases, each accounted independently

## Recommended TMR Fixes

### Option A: Adopt TM5 Framework-Separated Architecture (Best Compatibility)

```rust
// Separate init function (called once before test suite)
fn init_pattern(blocks: &[TestBlock]) {
    for block in blocks {
        for i in 0..len {
            memory[i] = generate_pattern(i);
        }
    }
}

// Test function (called repeatedly)
fn mirror_move_test(blocks: &[TestBlock]) -> u64 {
    let start = Instant::now();

    for cycle in 0..cycles {
        for block in blocks {
            // ONLY swap operations
            mirror_swap(block);
            mirror_swap(block);  // Even swaps = restore
        }
    }

    let elapsed = start.elapsed();
    total_bytes = blocks.iter().sum(|b| b.size) * cycles * 4;  // 2 swaps * 2N each
    return total_bytes;
}

// Separate verify function (called after test suite)
fn verify_pattern(blocks: &[TestBlock]) -> u64 {
    let mut errors = 0;

    for block in blocks {
        for i in 0..len {
            let expected = generate_pattern(i);
            let actual = memory[i];
            if actual != expected { errors += 1; }
        }
    }

    return errors;
}
```

**Pros**:
- Exact TM5 compatibility
- Clean separation of concerns
- Accurate per-phase timing
- Init not included in test timing

**Cons**:
- Major refactoring required
- Changes test registration system
- Affects all tests

### Option B: Fix Inline Architecture (Least Disruptive)

```rust
fn mirror_move_multi(blocks: &[TestBlock]) {
    // Init once (exclude from timing? or account separately?)
    for block in blocks {
        for i in 0..len { memory[i] = pattern; }
    }

    let start = Instant::now();

    loop {
        for block in blocks {
            // 1. Mirror
            mirror_swap(block);  // 2N bytes

            // 2. Mirror-back (restore)
            mirror_swap(block);  // 2N bytes

            // 3. Verify ORIGINAL pattern (FIX: was verifying mirrored!)
            for i in 0..len {
                let expected = generate_pattern(i);  // ORIGINAL pattern
                let actual = memory[i];
                if actual != expected { errors += 1; }
            }
            // 1N bytes

            total_bytes += block.size * 5;  // 2 + 2 + 1
        }
    }

    elapsed = start.elapsed();
}
```

**Pros**:
- Minimal code changes
- Keeps inline architecture
- Fixes verification logic

**Cons**:
- Still mixes concerns
- Init accounting ambiguous
- Less TM5-compatible

### Option C: Hybrid (Separate Init, Inline Test+Verify)

```rust
// Pre-init before timing starts
fn init_all_blocks(blocks: &[TestBlock]) {
    for block in blocks {
        for i in 0..len { memory[i] = pattern; }
    }
}

// Main test
fn mirror_move_multi(blocks: &[TestBlock]) {
    let start = Instant::now();

    loop {
        for block in blocks {
            mirror_swap(block);       // 2N
            mirror_swap(block);       // 2N (restore)
            verify_original(block);   // 1N

            total_bytes += block.size * 5;
        }
    }

    elapsed = start.elapsed();
}
```

**Pros**:
- Clean init separation
- Simple to implement
- Accurate accounting

**Cons**:
- Still not exactly like TM5
- Verification inline vs external

## Comparison Matrix

| Test | Architecture | Init | Verify | Timing Includes |
|------|-------------|------|--------|----------------|
| **TM5 RefreshStable** | Framework-Separated | External (sub_10001810) | External (sub_10001885) | Only verify reads |
| **TM5 SimpleTest** | Self-Contained | Inline (every cycle) | Inline (every cycle) | Write + Verify |
| **TM5 MirrorMove** | Framework-Separated | External (shared w/ Test 0) | External (Test 0) | Only swaps |
| **TMR Current** | Inline (broken) | Inline (once) | Inline (wrong pattern!) | Init + mirror + verify + mirror-back |
| **TMR Proposed (A)** | Framework-Separated | External | External | Only swaps (like TM5) |
| **TMR Proposed (B)** | Inline (fixed) | Inline (once) | Inline (correct pattern) | Mirror + mirror-back + verify |
| **TMR Proposed (C)** | Hybrid | External | Inline | Swaps + verify |

## Summary of TM5 Patterns

1. **Two architectures coexist**:
   - Framework-separated (RefreshStable, MirrorMove)
   - Self-contained (SimpleTest)

2. **Test 0 is the verification framework**:
   - Shared init/verify functions
   - Used by MirrorMove and others

3. **Init happens ONCE** at test suite start (framework-separated)
   - Or **every cycle** (self-contained like SimpleTest)

4. **Verify happens AFTER** all test operations complete
   - External for framework-separated
   - Inline for self-contained

5. **MirrorMove does EVEN swaps** to restore original
   - Verification checks original pattern
   - This validates round-trip worked

6. **TMR's current implementation is fundamentally broken**:
   - Verifies wrong pattern (mirrored instead of original)
   - Missing final verify after mirror-back
   - Wrong byte accounting (* 3 should be * 5 or * 6)
   - Mixes inline and framework-separated concepts incorrectly

## Recommended Next Steps

1. **Decide architecture** for TMR:
   - Full framework-separated (Option A)
   - Fixed inline (Option B)
   - Hybrid (Option C)

2. **Fix MirrorMove immediately** (Option B is quickest):
   - Remove verify of mirrored pattern
   - Add verify of ORIGINAL pattern after mirror-back
   - Fix accounting to * 5

3. **Audit all other tests**:
   - StuckBitTest
   - RefreshStable
   - CacheBusting
   - Ensure all have correct init/verify logic

4. **Consider long-term refactor** to Option A:
   - Separate init/verify framework
   - Exact TM5 compatibility
   - Cleaner architecture
