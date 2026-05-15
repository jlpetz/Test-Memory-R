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

### `subblock_count: Option<u32>`
**Used by:** MirrorMove (`Mem-MirrorV2-*`)

Number of independent subblocks for mirror operations. Each subblock is mirrored (front-to-back copy + verify) independently.

- **Valid values:** 1, 2, 3, or 4
- **TM5 mapping:** Exact match on 2, 3, or 4 only. All other values (0, 1, 16384, etc.) fall through to 1 (single-block mirror)
- **Example:** parameter=3 in 1usmus_v3.cfg → subblock_count=3 → memory split into 3 independently-mirrored regions

### `page_stride_bytes: Option<usize>`
**Used by:** MirrorMove128/256/512 (`Mem-MirrorV2-128`, `Mem-MirrorV2-256`, `Mem-MirrorV2-512`)

Page stride distance in bytes for SIMD mirror operations.

- **Formula:** `(parameter + 1) × 128`
- **Example:** parameter=16384 → page_stride = 16385 × 128 = 2,097,280 bytes
- **Source:** TM5 `.cfg` parameter field

### `stride_patterns: Option<u32>`
**Used by:** CacheBust (`Mem-CacheBust`)

Number of interleaved stride pattern variants. Each variant uses a different base pattern to write and verify, testing cache coherency under diverse access patterns.

- **Default:** 4
- **Constraint:** Must be >= 1, power-of-2 when > 1
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
| JSON config v2.0 | Direct fields: `"parameter"` for TM5-style, or named fields (`"stride_patterns"`, etc.) for TMR-native |
| CLI override | `--parameter=subblocks:3` or `--parameter=stride:8` overrides for TM5-style params |
| Default test suite | Hard-coded in `create_test_definitions()` in `runner.rs` |

## Validation

All parameters are validated before tests run (in `runner.rs`):
- Named TMR-native fields (`stride_patterns`, `rng_sequences`, `subdivisions`) must be >= 1 and power-of-2 when > 1
- `copy_directions` must be >= 1 (no power-of-2 constraint)
- `subblock_count` is validated by the TM5 parameter interpretation (exact match 2/3/4, else 1)
- Tests panic with a clear error message if their required parameter is missing from `parameter_context`
