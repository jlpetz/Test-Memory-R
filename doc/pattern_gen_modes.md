# Pattern Generation Modes

TMR supports 8 pattern generation modes, split into two families:

| Mode | Family | Name | Description |
|------|--------|------|-------------|
| 0 | TM5-faithful | Complementary Alternating | Bit dispersion + complement per 4KB page |
| 1 | TM5-faithful | Linear Step + Complement | wrapping_sub step + complement per cache line |
| 2 | TM5-faithful | Two-Level Evolving | Per-64 B-line seed+step evolution (PMULLW-style), restarted every 4 KiB page |
| 10 | TMR-native | Address-Derived XOR | `idx ^ base` (unique per address, fast) |
| 11 | TMR-native | Combined XOR | `idx ^ (param0 ^ param1)` |
| 12 | TMR-native | Hashed Lines | Mode 2's lines, seed and step hashed from each line's address |
| 13 | TMR-native | Hash per word | splitmix64 of `idx + param0` |
| 14 | TMR-native | Bus flip | Each beat of a line the inverse of the one before, at the memory type's beat width |

## TM5-Faithful Modes (0, 1, 2)

These modes replicate TM5's proven error detection patterns, modernized for 64-bit:

### Mode 0: Complementary Alternating

**TM5 origin**: `ST_GeneratePattern` in `mtests0.asm` (1805-1869), which is test 0's
`RS_GeneratePattern` plus one `xchg`.
- It runs once per block: a 16-bit word `d` from the block's first page number becomes a 48 B
  motif `[q0, !q0, q1, !q1, q2, !q2]`, each `q` four rotations of `d`. On an even page `d` and its
  inverse swap. The motif then fills the whole block, the same on all 4 write passes (the `+wrc`
  can't change a page-aligned page number): 6 distinct words per block.
- On a 64-bit bus every other beat flips all 64 lines (46.7 of 64 on average). On DDR5's 32-bit
  beats it flips 15.7 of 32, the same as random data.

**TMR's version is not TM5's bits.** It writes a pseudo-random word everywhere, seeded per thread
and cycle, so on DDR5 it flips as many lines (16.0 of 32) and every word differs. Measured
2026-10-09 by generating both.

**TMR implementation**: `pattern_mode0(idx, block_seed)`
- `block_seed` derived via Murmur hash of `(block_addr, thread_id, cycle)` — replaces TM5's `shr addr, 12`
- Bit dispersion: `block_seed ^ idx.wrapping_mul(0x9E3779B97F4A7C15)` (golden ratio prime, SIMD-friendly)
- Branchless complement every 4KB page (512 u64 elements): `value ^ mask` where `mask = 0u64.wrapping_sub((idx >> 9) & 1)`
- Stresses: adjacent 4KB pages see complementary patterns, detecting stuck-at faults at page boundaries

### Mode 1: Linear Step + Complement

**TM5 origin**: `ST_GeneratePattern2` Mode 1. XOR complement with `Const_m1` every 64 bytes + `PADDD` step with `Const_m3=[-3,-1,0,0]` per element.

**TMR implementation**: `pattern_mode1(idx, block_seed, cl_shift)`
- Linear step: `block_seed.wrapping_sub(idx.wrapping_mul(3))` — simplified from TM5's packed dword [-3,-1,0,0]
- Branchless complement per cache line: `value ^ mask` where `mask = 0u64.wrapping_sub(cache_line & 1)`
- `cl_shift` = `trailing_zeros(cache_line_bytes / 8)` — dynamic, not hardcoded to 64 bytes
- Stresses: adjacent cache lines see complementary patterns, detecting coupling between cache line buffers

### Mode 2: Two-Level Evolving Pattern

**TM5 origin**: `ST_GeneratePattern2` Mode 2. `PMULLW` per-page evolution where both seed AND step evolve.

**TMR implementation** (`pattern_gen.rs`, the loops in `tests.rs` `Mode2`):
- Each 64 B line (TM5's block length, fixed, not the CPU's cache line) is `line_words(seed, step)`:
  `seed + j * step` for j in 0..8
- `mode2_evolve(seed, step, param0, param1) -> (new_seed, new_step)` — called after each line
- The chain restarts at every 4 KiB page: `mode2_page_start(page_addr, thread, cycle)` gives
  `seed = block_seed(page_addr, ...)`, `step = seed * 0x5DEECE66D`. TM5 seeds once per block (its
  chunk) from the block's address; TMR does it per page so a page depends only on its address and
  chunks may overlap (TODO 76).
- Known defect: `step` is a running product of seeds, so it gains low zero bits and reaches 0 near
  the end of a page (line ~62 of 64 for 0x5DEECE66D/0xB); those lines hold 8 equal words (TODO 3).
- Verify: XOR+OR into 4 accumulators, and a cold rescan counts and logs only if one is set
- Stresses: pseudo-random pattern per line, excellent at detecting address-dependent faults

## TMR-Native Modes (10, 11, 12, 13)

These are TMR's modern alternatives, optimized for speed and simplicity:

### Mode 10: Address-Derived XOR (default)

`pattern_mode10(idx, base) -> idx ^ base`

Simple, fast, unique per element. Good general-purpose pattern. No state, trivially parallelizable. This is the default fallback for all tests.

### Mode 11: Combined XOR

`pattern_mode11(idx, combined) -> idx ^ combined` where `combined = param0 ^ param1`

Similar to Mode 10 but derives base from two parameters. Used when TM5 configs provide param0/param1.

### Mode 12: Hashed Lines (TMR's mode 2)

`mode12_line(line_addr, key) -> (seed, step)`, then `line_words(seed, step)`

Mode 2's line shape without the chain: each 64 B line is `seed + j * step`, with seed and an odd
step hashed (splitmix64) from the line's address and a key made once from the thread, the cycle
and both parameters (`mode12_key`). Any line can be computed alone, so the SIMD variants write and
verify it at full width, strided access works, and there is no step collapse. It replaced an LCG
chain (`state = state * param0 + param1`) on 2026-10-05, which carried state across chunks and
broke when chunks overlap (TODO 76).

### Mode 13: Hash per word

`pattern_mode13(idx, param0)`: splitmix64 of `idx + param0`. Pseudo-random per word, no state.

### Mode 14: Bus flip

`pattern_gen::mode14_line(line_addr, key, beat)`: each beat of a 64 B line's burst is the bitwise
inverse of the one before, so every data line toggles on every beat.
- **The beat width** comes from the DIMMs' SMBIOS memory type: 64-bit for DDR to DDR4, 32-bit for
  DDR5 (two subchannels a DIMM), 16-bit for LPDDR4 and LPDDR5. With no type, DDR5's 32 is
  assumed. The run logs which.
- **Why a width at all:** a pattern tuned for one width is close to the worst case for another.
  Measured 2026-10-09: TM5's mode 0 flips 46.7 of 64 lines a beat on DDR4 but 15.7 of 32 on DDR5;
  a 32-bit-tuned pattern flips 31.0 of 32 on DDR5 and 4.0 of 64 on DDR4.
- **One value a line**, hashed from its address and `mode12_key` (thread, cycle, params). Each byte
  holds four 1s and four 0s, so data bus inversion (DBI), which inverts a byte with more than four
  0s, never undoes a flip.
- **The trade-off:** the first beat fixes the whole burst, so a line carries 8, 16 or 32 bits of
  its hash (16-, 32-, 64-bit beats). Two lines can hold the same value, and a misdirected access
  between them goes unseen; the other modes cover address faults.
- **Scrambling:** a controller that scrambles data (Intel does by default) turns this into random
  data on the bus, no worse than mode 13. TMR can't tell, so it assumes scrambling is off.

## Mode Selection

### CLI
```bash
tmr.exe pattern-mode=0           # TM5-faithful mode 0
tmr.exe pattern-mode=12          # TMR-native hashed lines
tmr.exe write-read-cycles=4      # TM5 SimpleTest: (1 write + 5 reads) × 4 per chunk
tmr.exe verify-reps=5            # 5 verify passes per write
```

### TM5 .cfg files
TM5 configs use `Pattern Mode=0/1/2` which map directly to TM5-faithful modes 0/1/2.

### TMR JSON configs
Use `pattern_mode` field in test config. Any mode number (0-2, 10-14) is valid.

## SIMD Support

| Mode | Scalar | SSE2 (128) | AVX2 (256) | AVX-512 (512) |
|------|--------|------------|------------|---------------|
| 0 | Yes | Positional* | Positional* | Positional* |
| 1 | Yes | Positional* | Positional* | Positional* |
| 2 | Yes | Positional* | Positional* | Positional* |
| 10 | Yes | Yes | Yes | Yes |
| 11 | Yes | Yes | Yes | Yes |
| 12 | Yes | Yes | Yes | Yes |
| 13 | Yes | Positional* | Positional* | Positional* |
| 14 | Yes | Scalar† | Scalar† | Scalar† |

*Modes 0/1/2 and 13 use the positional SIMD path (mode 10/11 patterns) as an approximation when running in SIMD test variants. Full TM5-faithful SIMD implementations are planned (TODO 3).

†Mode 14's SIMD variants run the scalar build: a line is one or two values, which it already stores at the baseline's full width (two 32 B stores a line; four lines a loop iteration sequentially).

## DDR5 Context

- DDR5 physical page: 1KB (vs DDR4's 512B-2KB depending on density)
- 2 sub-channels per DIMM, 32 banks per rank, BL16 = 64-byte burst
- TM5 "page" in code = 64 bytes (cache line / burst length), NOT an OS page
- TMR's 4KB complement boundary (Mode 0) = 4 DDR5 physical pages
- Cache line complement (Mode 1) = one DDR5 burst length

## Stride Interaction

TM5 SimpleTest stride formula: `JumpStep = BlkSize * (Channels * Parameter - 1)` where `BlkSize=64` bytes.

In TMR: `stride_cachelines = Channels * Parameter - 1` (default Channels=2 for DDR5 dual-channel).

Strided access moves a whole 64 B line per jump, as TM5 does (TODO 93), so every mode works strided with the sequential image, except that the scalar test's strided mode 2 writes mode 12's lines: mode 2's chain runs a page in order, and the walk visits a page's lines a period apart.
