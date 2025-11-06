# MirrorMove Test - Visual Explanation

## Your Question: "What is it reading from?"

**Great question!** You're right to be confused - the test DOES initialize memory first!

## Complete MirrorMove Flow

### ONCE Before All Cycles: Initialization Phase

```
Memory starts as: [????|????|????|????|????|????|????|????]  (uninitialized/random)

Initialization writes sequential pattern:
  for i in 0..N {
      memory[i] = (i * MULTIPLIER) + thread_id_base
  }

After init: [A][B][C][D][E][F][G][H]
            (each letter represents unique pattern based on index)
```

**Code location: tests.rs:5005-5015 (scalar), similar in all SIMD variants**

### Every Cycle: Mirror → Verify → Mirror-back

#### Cycle 1, 2, 3, ... (repeats until cycles_limit or duration_limit)

**Step 1: Mirror Operation (Swap first half with second half)**
```
Before:  [A][B][C][D] | [E][F][G][H]
          ↓    ↓    ↓      ↓    ↓    ↓
Swap:    A↔H  B↔G  C↔F  D↔E
          ↓    ↓    ↓      ↓    ↓    ↓
After:   [H][G][F][E] | [D][C][B][A]
```

**Memory operations:**
- Read A, Read H → Write H to position 0, Write A to position 7
- Read B, Read G → Write G to position 1, Write B to position 6
- etc.
- **Total: N reads + N writes = 2N bytes**

**Step 2: Verify Mirrored Pattern**
```
Current state: [H][G][F][E][D][C][B][A]

For each index i:
    mirrored_index = (N-1) - i
    expected = pattern(mirrored_index)  // What SHOULD be at mirrored position
    actual = memory[i]
    if actual != expected: ERROR!

Example:
  At index 0: expect pattern(7) which is H ✓
  At index 1: expect pattern(6) which is G ✓
  At index 7: expect pattern(0) which is A ✓
```

**Memory operations:**
- Read all N positions: **N bytes**

**Step 3: Mirror-back (Restore original)**
```
Before:  [H][G][F][E] | [D][C][B][A]
          ↓    ↓    ↓      ↓    ↓    ↓
Swap:    H↔A  G↔B  F↔C  E↔D
          ↓    ↓    ↓      ↓    ↓    ↓
After:   [A][B][C][D] | [E][F][G][H]  (back to original!)
```

**Memory operations:**
- Same as Step 1: **2N bytes**

**❌ Step 4: MISSING - Verify Restoration**
```
Should verify: memory == original pattern [A][B][C][D][E][F][G][H]
But we DON'T! We just loop back to Step 1 and assume it's correct.
```

**If we added it:**
- Read all N positions: **N bytes**

## Memory Operations Summary (per cycle)

### Current Implementation
| Phase | Reads | Writes | Total Bytes |
|-------|-------|--------|-------------|
| Init (once) | 0 | N | N |
| Mirror | N | N | 2N |
| Verify | N | 0 | N |
| Mirror-back | N | N | 2N |
| **MISSING: Final Verify** | **N** | **0** | **N** |
| **Total per cycle** | **3N** | **2N** | **5N** |

### If We Add Final Verify
| Total per cycle | 4N | 2N | 6N |

## Why This Test Pattern?

**Purpose**: Detect memory errors by:
1. **Writing known pattern** → Tests memory cells can hold data
2. **Mirroring (complex transformation)** → Tests address decoding and data movement
3. **Verifying mirrored pattern** → Tests reads work and data transformed correctly
4. **Mirroring back** → Tests reversibility (round-trip)
5. **❌ Should verify original restored** → Tests full round-trip worked

## The Problem: What We're NOT Testing

Without Step 4 (final verify), we don't detect:

### Example Failure Scenario
```
Cycle 1:
  Start:  [A][B][C][D][E][F][G][H]
  Mirror: [H][G][F][E][D][C][B][A]  ✓ Verified correctly
  Mirror-back has BUG: [H][G][C][D][E][F][B][A]  ❌ But not detected!

Cycle 2:
  Start:  [H][G][C][D][E][F][B][A]  (wrong, but we don't know!)
  Mirror: [A][B][F][E][D][C][G][H]
  Verify: FAILS! Expected [A][B][F][E]... but we think error is in mirror
         Actually error was in PREVIOUS mirror-back!
```

## Current Accounting Bug

**We report**: `bytes_processed = test_size * 3`

**Reality**:
- Current implementation: `test_size * 5` (mirror:2 + verify:1 + mirror-back:2)
- With final verify: `test_size * 6` (mirror:2 + verify:1 + mirror-back:2 + verify:1)

**Impact**: MirrorMove throughput is under-reported by 40-67%!

## Comparison with Other Tests

### SimpleTest (Simple Pattern)
```
Per cycle:
  1. Write pattern → N bytes
  2. (No verify)
Total: N bytes/cycle
Accounting: test_size * 1  ✓ CORRECT
```

### RefreshStable
```
Per cycle:
  1. Write pattern → N bytes
  2. Verify pattern → N bytes
Total: 2N bytes/cycle
Accounting: Need to check!
```

### CacheBusting
```
Per cycle:
  1. Write strided → N bytes
  2. Verify strided → N bytes
Total: 2N bytes/cycle
Accounting: test_size * 2  ✓ CORRECT
```

## Recommendation

1. **Immediate**: Fix accounting to `test_size * 5` (match current reality)
2. **Short-term**: Add final verify and change to `test_size * 6`
3. **Audit**: Check ALL tests for accurate byte accounting

## Visual: What Mirror Actually Does (8 elements)

```
Initialization (ONCE):
┌────┬────┬────┬────┬────┬────┬────┬────┐
│ A  │ B  │ C  │ D  │ E  │ F  │ G  │ H  │  pattern[0..7]
└────┴────┴────┴────┴────┴────┴────┴────┘
 ↑ idx=0                           idx=7 ↑

Cycle 1 - Mirror (two-pointer swap from ends):
  idx1=0, idx2=7: swap A↔H
  idx1=1, idx2=6: swap B↔G
  idx1=2, idx2=5: swap C↔F
  idx1=3, idx2=4: swap D↔E
  (idx1 >= idx2, stop)

┌────┬────┬────┬────┬────┬────┬────┬────┐
│ H  │ G  │ F  │ E  │ D  │ C  │ B  │ A  │  Mirrored!
└────┴────┴────┴────┴────┴────┴────┴────┘

Verify:
  For i=0: expect pattern[7]=H, read memory[0]=H ✓
  For i=1: expect pattern[6]=G, read memory[1]=G ✓
  ...

Mirror-back (same swap process):
┌────┬────┬────┬────┬────┬────┬────┬────┐
│ A  │ B  │ C  │ D  │ E  │ F  │ G  │ H  │  Restored!
└────┴────┴────┴────┴────┴────┴────┴────┘

❌ MISSING: Should verify memory[i] == pattern[i]
✓ Instead: Loop to Cycle 2 and mirror again
```

## TM5 Original

The TM5 decompiled source (MT0.cxx) is heavily obfuscated by the decompiler. The actual mirror logic is likely in functions called by the test runner (sub_10001782), but it's not easily readable.

**Key functions in TM5:**
- `sub_10001810` - Pattern writing (initialization)
- `sub_10001885` - Verification with XOR comparison
- `sub_10002BCB` / `sub_10002BDA` - MirrorMove wrappers

Without running TM5 under a debugger or disassembling the actual binary, it's hard to confirm if TM5 had the final verify step.

## Analogy: Moving Furniture

Think of MirrorMove like moving furniture:

1. **Init**: Arrange furniture in original positions [A,B,C,D,E,F,G,H]
2. **Mirror**: Swap all furniture (first ↔ last, second ↔ second-last, etc.)
3. **Verify**: Check furniture is now in mirrored positions [H,G,F,E,D,C,B,A]
4. **Mirror-back**: Swap all furniture again to restore original
5. **❌ Missing**: We DON'T check furniture is back in original positions!

If the second swap failed, we'd only notice when we try to swap again (and it would look like the NEXT swap failed, not the previous restoration).
