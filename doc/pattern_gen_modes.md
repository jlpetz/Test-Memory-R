# Pattern Generation Modes

TMR supports 6 pattern generation modes, split into two families:

| Mode | Family | Name | Description |
|------|--------|------|-------------|
| 0 | TM5-faithful | Complementary Alternating | Bit dispersion + complement per 4KB page |
| 1 | TM5-faithful | Linear Step + Complement | wrapping_sub step + complement per cache line |
| 2 | TM5-faithful | Two-Level Evolving | Per-cache-line seed+step evolution (PMULLW-style) |
| 10 | TMR-native | Address-Derived XOR | `idx ^ base` (unique per address, fast) |
| 11 | TMR-native | Combined XOR | `idx ^ (param0 ^ param1)` |
| 12 | TMR-native | LCG Chain | `state = state * param0 + param1` (real PRNG) |

## TM5-Faithful Modes (0, 1, 2)

These modes replicate TM5's proven error detection patterns, modernized for 64-bit:

### Mode 0: Complementary Alternating

**TM5 origin**: `ST_GeneratePattern` in `mtests0.asm`. Pattern is static (no per-cache-line evolution). Address-derived with 16-bit ROL per word, complement per 4KB page.

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

**TMR implementation**: Two functions working together:
- `mode2_evolve(seed, step, param0, param1) -> (new_seed, new_step)` — called per cache line boundary
- `mode2_element(page_seed, element_offset, page_step) -> u64` — within-cache-line positional
- Caller manages state: `(seed, step)` pair evolves at each cache line boundary
- Initial step: `base_seed.wrapping_mul(0x5DEECE66D)` (Java LCG constant)
- Stresses: pseudo-random pattern per cache line, excellent at detecting address-dependent faults

## TMR-Native Modes (10, 11, 12)

These are TMR's modern alternatives, optimized for speed and simplicity:

### Mode 10: Address-Derived XOR (default)

`pattern_mode10(idx, base) -> idx ^ base`

Simple, fast, unique per element. Good general-purpose pattern. No state, trivially parallelizable. This is the default fallback for all tests.

### Mode 11: Combined XOR

`pattern_mode11(idx, combined) -> idx ^ combined` where `combined = param0 ^ param1`

Similar to Mode 10 but derives base from two parameters. Used when TM5 configs provide param0/param1.

### Mode 12: LCG Chain (PRNG)

`lcg_next(state, multiplier, addend) -> state * multiplier + addend`

Real linear congruential generator. Produces pseudo-random sequence. Has SIMD variants (`SimdLcgU64x2/x4/x8`) for parallel lane generation. Fixes TM5's broken Mode 2 which constructed a static seed instead of running the chain.

**SIMD LCG**: Pre-computes N independent streams (one per lane). Each lane advances by `multiplier^N` per iteration, maintaining full SIMD throughput with no lane dependencies.

## Mode Selection

### CLI
```bash
tmr.exe pattern-mode=0           # TM5-faithful mode 0
tmr.exe pattern-mode=12          # TMR-native LCG
tmr.exe write-read-cycles=4      # TM5 SimpleTest: (1 write + 5 reads) × 4 per chunk
tmr.exe verify-reps=5            # 5 verify passes per write
```

### TM5 .cfg files
TM5 configs use `Pattern Mode=0/1/2` which map directly to TM5-faithful modes 0/1/2.

### TMR JSON configs
Use `pattern_mode` field in test config. Any mode number (0-2, 10-12) is valid.

## SIMD Support

| Mode | Scalar | SSE2 (128) | AVX2 (256) | AVX-512 (512) |
|------|--------|------------|------------|---------------|
| 0 | Yes | Positional* | Positional* | Positional* |
| 1 | Yes | Positional* | Positional* | Positional* |
| 2 | Yes | Positional* | Positional* | Positional* |
| 10 | Yes | Yes | Yes | Yes |
| 11 | Yes | Yes | Yes | Yes |
| 12 | Yes | Yes (LCG) | Yes (LCG) | Yes (LCG) |

*TM5-faithful modes 0/1/2 use the positional SIMD path (mode 10/11 patterns) as an approximation when running in SIMD test variants. Full TM5-faithful SIMD implementations are planned.

## DDR5 Context

- DDR5 physical page: 1KB (vs DDR4's 512B-2KB depending on density)
- 2 sub-channels per DIMM, 32 banks per rank, BL16 = 64-byte burst
- TM5 "page" in code = 64 bytes (cache line / burst length), NOT an OS page
- TMR's 4KB complement boundary (Mode 0) = 4 DDR5 physical pages
- Cache line complement (Mode 1) = one DDR5 burst length

## Stride Interaction

TM5 SimpleTest stride formula: `JumpStep = BlkSize * (Channels * Parameter - 1)` where `BlkSize=64` bytes.

In TMR: `stride_cachelines = Channels * Parameter - 1` (default Channels=2 for DDR5 dual-channel).

Strided access works with positional modes (0, 1, 10, 11). Stateful modes (2, 12) fall back to Mode 10 when stride is active, since LCG chains don't compose with non-sequential access.
