# TM5 MirrorMove - Definitive Loop Analysis

## Critical Discovery: MirrorMove Does MULTIPLE Swaps Per Call!

### The Code (TM5/bin/mtests0.asm)

```asm
MirrorMove_Check proc
    ; Lines 1134-1145: Calculate loop counter based on test duration
    mov eax, PatternJump.TestLength
    mov edx, dTestTime
    mul edx                         ; Multiply by test time (100% = 100)
    mov ecx, MirrorMove_Div_Down    ; Divide to get iteration count
    div ecx
    mov dLoopCounter, eax
    mov dWriteReadCycleCounter, eax ; ← Sets swap repeat count!

SwapWindow:                         ; ← Start of swap loop
    ; Lines 1152-1431: Do mirror swap (first half ↔ last half)
    mov edi, pBlock
    mov edx, MaxAddr
    ; ... perform swap ...

CycleDone:
    dec dWriteReadCycleCounter      ; ← Decrement counter
    jg SwapWindow                   ; ← LOOP BACK if > 0!

    rdtsc                           ; Return timing
    ret
MirrorMove_Check endp
```

### What This Means

If `dWriteReadCycleCounter = 10`, MirrorMove does:

```
Swap  1: [A,B,C,D,E,F,G,H] → [H,G,F,E,D,C,B,A]  (mirrored)
Swap  2: [H,G,F,E,D,C,B,A] → [A,B,C,D,E,F,G,H]  (restored!)
Swap  3: [A,B,C,D,E,F,G,H] → [H,G,F,E,D,C,B,A]  (mirrored)
Swap  4: [H,G,F,E,D,C,B,A] → [A,B,C,D,E,F,G,H]  (restored!)
Swap  5: [A,B,C,D,E,F,G,H] → [H,G,F,E,D,C,B,A]  (mirrored)
Swap  6: [H,G,F,E,D,C,B,A] → [A,B,C,D,E,F,G,H]  (restored!)
Swap  7: [A,B,C,D,E,F,G,H] → [H,G,F,E,D,C,B,A]  (mirrored)
Swap  8: [H,G,F,E,D,C,B,A] → [A,B,C,D,E,F,G,H]  (restored!)
Swap  9: [A,B,C,D,E,F,G,H] → [H,G,F,E,D,C,B,A]  (mirrored)
Swap 10: [H,G,F,E,D,C,B,A] → [A,B,C,D,E,F,G,H]  (restored! - even count)
```

**Final state after 10 swaps: ORIGINAL pattern restored**

This is what the author means by:
> "With an even number of reflections, the final state of the block does not change (if there are no failures)."

## No Init Inside MirrorMove

**Critical finding**: MirrorMove_Check has ZERO pattern generation calls!

Searching for init:
```bash
grep "RS_GeneratePattern\|ST_GeneratePattern" mtests0.asm
# Results:
#   RS_GeneratePattern - used by RefreshStable
#   ST_GeneratePattern - used by SimpleTest
#   (NO pattern generation in MirrorMove!)
```

**Conclusion**: Init must happen EXTERNALLY via the framework!

## TM5 Framework Flow (With AWE)

### For Each AWE Window:

```
1. Map window (AWE: physical → virtual address space)

2. INIT: Framework calls sub_10001810(window)
   → Writes pattern to entire window
   → Memory: [A,B,C,D,E,F,G,H]

3. TEST: Framework calls MirrorMove_Check(window)
   → Performs N swaps (where N is even)
   → Swap 1: [A,B,C,D,E,F,G,H] → [H,G,F,E,D,C,B,A]
   → Swap 2: [H,G,F,E,D,C,B,A] → [A,B,C,D,E,F,G,H]
   → ... (repeat N/2 times)
   → Final state: [A,B,C,D,E,F,G,H] (restored!)

4. VERIFY: Framework calls sub_10001885(window)
   → Verifies memory matches ORIGINAL pattern
   → If errors: memory didn't survive the swap torture
   → Returns error count

5. Unmap window

6. Repeat for next window
```

**Key insight**: Init/Test/Verify happens PER WINDOW, not once globally!

## Why Init Per Window With AWE?

With AWE, TM5 can only see ONE window at a time:

```
Physical Memory: [=========== 16 GiB ===========]
                  Window 1  Window 2  ... Window 16

Virtual Address: [1 GiB] ← Can only map one window here
```

So TM5 MUST process windows sequentially:
```
Map window 1  → Init → Test (N swaps) → Verify → Unmap
Map window 2  → Init → Test (N swaps) → Verify → Unmap
...
Map window 16 → Init → Test (N swaps) → Verify → Unmap
```

**Each window gets its own Init/Test/Verify cycle!**

## What About Multi-Cycle Testing?

If user configures TM5 to run at 200% duration or multiple cycles:

```
Cycle 1:
    For each window:
        Init → Test (N swaps) → Verify

Cycle 2:
    For each window:
        Init → Test (N swaps) → Verify  (FRESH init!)

... repeat
```

**Evidence**: With errors in cycle 1, you can't just keep testing corrupted data - you NEED fresh init!

## The "Swap Count" Configuration

Looking at MirrorMove constants:

```asm
MirrorMove_Div_Down = 2000  ; Divider for loop count calculation
```

Loop count calculation:
```asm
mov eax, PatternJump.TestLength  ; From config (100 = 100%)
mov edx, dTestTime               ; Global test time
mul edx                          ; 100 * 100 = 10000
mov ecx, MirrorMove_Div_Down     ; = 2000
div ecx                          ; 10000 / 2000 = 5
mov dWriteReadCycleCounter, eax  ; = 5 swaps
```

**So at 100% test length, MirrorMove does 5 swaps!**

At 200%: `(200 * 100) / 2000 = 10 swaps`

**This is likely NOT user-configurable per-swap** - it's derived from test duration percentage.

## Implications for TMR

### Option 1: Match TM5 Exactly (Multi-Swap Per Cycle)

```rust
fn mirror_move_multi(blocks: &[TestBlock], config: &Config) {
    // Calculate swap count based on test duration
    let swap_count = calculate_swap_count(config.test_length_percent);

    for cycle in 0..cycles {
        for block in blocks {
            // === INIT (every cycle, every block) ===
            init_pattern(block);

            // === TEST (N swaps in one timing window) ===
            let start = Instant::now();

            for swap_iter in 0..swap_count {
                mirror_swap(block);  // Swap!
                // After even swaps, memory is restored
            }

            let elapsed = start.elapsed();
            bytes_processed += block.size * swap_count * 2;  // Each swap = 2N

            // === VERIFY ===
            verify_original_pattern(block);  // Should match original!
        }
    }
}
```

**Accounting per cycle**:
- Init: N bytes (write)
- Test: `swap_count * 2N` bytes (each swap reads N + writes N)
- Verify: N bytes (read)
- **Total: `N + (swap_count * 2N) + N = (2 + swap_count * 2)N` bytes**

**For swap_count = 5**: `(2 + 5*2) = 12N bytes per cycle`

### Option 2: Simplified TMR (2 Swaps Always)

```rust
fn mirror_move_multi(blocks: &[TestBlock]) {
    for cycle in 0..cycles {
        for block in blocks {
            // === INIT ===
            init_pattern(block);  // 1N

            // === TEST (always 2 swaps = restore) ===
            mirror_swap(block);     // 2N (mirror)
            mirror_swap(block);     // 2N (restore)

            // === VERIFY ===
            verify_original_pattern(block);  // 1N

            bytes_processed += block.size * 6;  // 1 + 2 + 2 + 1
        }
    }
}
```

**Simpler, but less stress than TM5!**

## Definitive Answer to Your Questions

### Q: Does init run once or every cycle?

**A: Every window, every cycle!** (With AWE, TM5 can only access one window at a time, so init must happen when window is mapped)

### Q: Is there logic where a single pass can be configured to flip 2,4,6,8 times?

**A: YES!** The number of swaps is controlled by:
```
swap_count = (TestLength% * GlobalTestTime) / MirrorMove_Div_Down
```

- At 100% test length: ~5 swaps
- At 200% test length: ~10 swaps
- At 400% test length: ~20 swaps

**NOT directly configurable, but derived from test duration!**

### Q: How does this work with AWE?

**A: Per-window init/test/verify:**

```
For each test cycle:
    For each AWE window (sequential):
        1. Map physical → virtual
        2. Init window
        3. Test window (N swaps)
        4. Verify window
        5. Unmap window
```

### Q: What should TMR do?

**Without AWE, TMR can do better:**

```
For each test cycle:
    === Phase 1: INIT (all windows parallel) ===
    parallel_for_each(window):
        init_pattern(window)

    === Phase 2: TEST (all windows parallel) ===
    parallel_for_each(window):
        for i in 0..swap_count:
            mirror_swap(window)

    === Phase 3: VERIFY (all windows parallel) ===
    parallel_for_each(window):
        verify_original_pattern(window)
```

**Much faster than TM5's sequential AWE window processing!**

## Recommended TMR Implementation

### Simple Version (2 swaps, init every cycle):

```rust
impl MirrorMove {
    fn run_cycle(&mut self, blocks: &[TestBlock]) -> CycleStats {
        let mut total_bytes = 0u64;
        let start = Instant::now();

        for block in blocks {
            // Init (every cycle)
            init_pattern(&mut block.memory);
            total_bytes += block.size;

            // Test (2 swaps = restore)
            mirror_swap(&mut block.memory);
            mirror_swap(&mut block.memory);
            total_bytes += block.size * 4;  // 2 swaps * 2N each

            // Verify
            let errors = verify_original_pattern(&block.memory);
            total_bytes += block.size;

            if errors > 0 { return Err(...); }
        }

        CycleStats {
            elapsed: start.elapsed(),
            bytes_processed: total_bytes,  // init(1N) + swaps(4N) + verify(1N) = 6N
        }
    }
}
```

**Accounting**: `6N bytes per cycle` (includes init every cycle)

**Reporting**: Same as today, just fix accounting!

### Advanced Version (configurable swaps, match TM5):

```rust
impl MirrorMove {
    fn run_cycle(&mut self, blocks: &[TestBlock], config: &Config) -> CycleStats {
        let swap_count = (config.test_length_percent * 100) / 2000;  // Match TM5
        let swap_count = swap_count.max(2) & !1;  // Ensure even, min 2

        let mut total_bytes = 0u64;

        for block in blocks {
            init_pattern(&mut block.memory);
            total_bytes += block.size;

            for _ in 0..swap_count {
                mirror_swap(&mut block.memory);
                total_bytes += block.size * 2;
            }

            verify_original_pattern(&block.memory)?;
            total_bytes += block.size;
        }

        CycleStats {
            bytes_processed: total_bytes,  // 1N + (swap_count*2N) + 1N
        }
    }
}
```

**Accounting**: `(2 + swap_count * 2)N bytes per cycle`

## Summary

1. **TM5 MirrorMove does MULTIPLE swaps per call** (controlled by test duration)
2. **Init happens EXTERNALLY** via framework (sub_10001810)
3. **Verify happens EXTERNALLY** via framework (sub_10001885)
4. **With AWE**: Init/Test/Verify per window (sequential)
5. **Even swaps** ensure data returns to original state
6. **Verify checks ORIGINAL pattern** (not mirrored)
7. **Init happens every cycle** (fresh data, especially needed with AWE window remapping)

**TMR Should**: Init every cycle, do even swaps, verify original, account for all bytes!
