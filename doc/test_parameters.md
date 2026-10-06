# Test Parameters Reference

All per-test parameters are stored in `TestParameterContext` (`src/config.rs`). Each field is `Option<T>` — only the fields relevant to a given test are set; the rest are `None`. Tests panic at startup if their required field is missing.

## Struct Fields

### `raw_parameter: u32`
The original TM5 parameter value, preserved for debugging and display. Not directly consumed by test logic — it's the input that gets interpreted into the typed fields below.

### `stride_cachelines: Option<usize>`
**Used by:** SimpleTest, SimpleNT (`Mem-SimpleV2-*`, `Mem-SimpleNT-*`)

Stride distance in cache lines (64-byte units). Controls how far apart sequential memory accesses are, simulating DDR interleave patterns.

- **Formula:** `channels × parameter - 1` (from TM5 `JumpStep = BlkSize × (Channels × Parameter - 1)`)
- **Example:** channels=2, parameter=8 → stride = 15 cache lines
- **Source:** TM5 `.cfg` parameter field + channels config, or direct value in JSON config
- **Display:** Shown in Test Configuration table as `Stride(15cl/960B)`

### `stride_elements: Option<usize>`
**Used by:** SimpleTest, SimpleNT (same tests as `stride_cachelines`)

Derived field: stride in u64 elements. Calculated as `stride_cachelines × (cache_line_bytes / 8)`. Typically `stride_cachelines × 8` for 64-byte cache lines. This is the value used directly in the test loop for pointer arithmetic.

### `mirror: Option<MirrorMode>`
**Used by:** MirrorMove (`Mem-MirrorV2`, `-128`, `-256`, `-512`, `-Auto`)

How each chunk is mirrored (TODO 85). Every mode is one round trip per test op: the two ends walk
toward each other, swapping, and cross the middle, so each pair is swapped twice and the chunk ends
as it began, as one pass of TM5's MirrorMove or MirrorMove128 does.

- **`whole`** (the default when unset): one mirror over the chunk, one vector per step.
- **`subblocks:2`**, **`subblocks:4`**: the chunk in 2 or 4 equal parts, each mirrored, all in
  lockstep. A chunk is a multiple of 4 KiB, so every part is a whole number of vectors at any width.
  `subblocks:3` is refused with the reason: a third isn't a whole number of vectors, TM5 rounds each
  third down to 128 B and leaves the tail unmirrored, and no shipped config uses it.
- **`jump:N`**: 128 B swaps every (N + 1) x 128 B, the jump capped at a quarter of the chunk; each
  further pass starts 128 B in, last pass first, until every 128 B has been visited (TM5
  MirrorMove128). `jump:0` swaps adjacent 128 B units.
- **TM5 mapping** (the importer, `LegacyConfig::mirror_mode`): MirrorMove Parameter 2 or 4 is that
  many subblocks, 3 is a load error, anything else (0, 1, 16384...) is `whole`; MirrorMove128
  Parameter N is `jump:N`. Both import as `Mem-MirrorV2-Auto`, the widest SIMD the CPU has.
- **JSON config field:** `"mirror": "jump:510"`. A mirror test with `"parameter"` is a load error,
  as is `"mirror"` on any other test.
- **Display:** the same form, e.g. `subblocks:4`.

### `stride_patterns: Option<u32>`
**Used by:** CacheBust (`Mem-CacheBust`)

Number of interleaved stride pattern variants. Each variant uses a different base pattern to write and verify, testing cache coherency under diverse access patterns.

- **Default:** 4
- **Constraint:** Must be >= 1. It picks the pattern for each column, one u64 offset repeated every 4 KiB (`offset % stride_patterns`, once per column), so any count works
- **JSON config field:** `"stride_patterns": 4`

### `rng_sequences: Option<u32>`
**Used by:** RandomTorture (`Mem-Random`)

Number of independent RNG (xorshift) sequences for random access verification. More sequences = more diverse access patterns within each cycle.

- **Default:** 8
- **Constraint:** Must be >= 1, power-of-2 when > 1
- **JSON config field:** `"rng_sequences": 8`

### `subdivisions: Option<u32>`
**Used by:** StrideAccess (`Mem-Stride`)

Number of chunk subdivisions. Each subdivision is processed with different stride distances (1, 16, 64, 256, 1024, 4096 elements), testing how the memory subsystem handles various access granularities.

- **Default:** 4
- **Constraint:** Must be >= 1, power-of-2 when > 1
- **JSON config field:** `"subdivisions": 4`

### `copy_directions: Option<u32>`
**Used by:** BlockMove (`Mem-BlockMove`)

Number of copy direction patterns for memory move operations:
- **1:** Forward copy only
- **2:** Forward + backward (two halves)
- **4:** Four-way interleaved (forward, backward, skip, forward)
- **Other:** Strided round-robin across N directions

- **Default:** 1
- **Constraint:** Must be >= 1 (no power-of-2 requirement)
- **JSON config field:** `"copy_directions": 2`

## Parameter Sources

| Source | How parameters are set |
|--------|----------------------|
| TM5 `.cfg` file | `interpret_tm5_parameter_with_channels()` maps the raw parameter based on test function name |
| JSON config v2.0 | Direct fields: `"parameter"` for SimpleTest's TM5-style stride, or named fields (`"mirror"`, `"stride_patterns"`, etc.) |
| CLI override | `mirror=subblocks:4` (or `whole`, `jump:N`) sets every test's mirror mode and leaves its other parameters |
| Default test suite | Hard-coded in `create_test_definitions()` in `runner.rs` |

## Validation

All parameters are validated before tests run (in `runner.rs`):
- `rng_sequences` and `subdivisions` must be >= 1 and power-of-2 when > 1 (both are used as shifts); `subdivisions` at most 512, since a chunk is a multiple of 4 KiB (512 u64) and more would leave part of it untested
- `stride_patterns` and `copy_directions` must be >= 1 (no power-of-2 constraint)
- `mirror` is checked when it is parsed (`MirrorMode::from_str`, shared by JSON, the CLI and the importer): subblocks 2 or 4, any jump
- Tests panic with a clear error message if their required parameter is missing from `parameter_context`

## Sequencing and the seal (TODO 74)

These sit beside the per-test parameters above:

| Where | Field | Meaning |
|---|---|---|
| test entry | `"id": "12"` | The name `cycle_order` lists the test by; unique in the config. A TM5 import gives each `[TestN]` the id `"N"`, disabled ones too |
| top level | `"cycle_order": ["6", "12", "2", "1", "1"]` | One cycle's steps, as ids in order; an id may repeat. A disabled test's id is skipped (the plan marks it); an unknown id is a load error. Unset is every enabled test once, in file order. A TM5 import's `Test Sequence`, read up to a number of 16 or more |
| test entry | `"seal": false` | This correctness test runs without the seal around its chunks |
| `system` | `"seal": "tmr"` | The seal (`seal.rs`, `doc/test_harness_tiers.md` §3a): `tmr` (the default), `tm5` (TM5's test 0 pattern, bit for bit) or `off`. A TM5 import takes `tmr` when its Test0 (RefreshStable) is enabled, else `off` |
| `system` | `"seal_width": "auto"` | The seal kernels' SIMD width: `auto` (the widest), `128`, `256` or `512`; a width the CPU lacks is a load error |

The command line overrides the last two: `seal=tmr|tm5|off`, `seal-width=auto|128|256|512`. A TM5
`Cycles = 0` runs until stopped. A dependent step (`skip_init`) must directly follow the step that
wrote its data, with no step that takes the seal in between; otherwise the plan is refused.
`Bench-Init-Seal-{TMR,TM5}[-128|-256|-512]` and their `Bench-Verify-Seal-*` measure the seal's
fill and check alone.
