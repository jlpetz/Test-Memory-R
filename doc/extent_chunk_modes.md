# Extent and Chunk Modes

(The extent was called the window until TODO 76, 2026-10; TM5's own "Testing Window Size" keeps
its name.)

TMR's three-stage memory hierarchy:

```
Allocation  (per-thread OS-level block — set by memory_strategy.allocation_mode)
└── Extent  (per-test working set inside the allocation)
    └── Chunk (iteration unit inside the extent — controls shutdown responsiveness)
```

`ExtentMode` and `ChunkMode` are the test-time knobs for shaping the inner two stages.

## Quick reference

| Stage  | Mode          | Required field        | Meaning                                                                 |
| ------ | ------------- | --------------------- | ----------------------------------------------------------------------- |
| Extent | `full_allocation` | (none)            | Use the entire per-thread allocation.                                   |
| Extent | `cache`       | `target` (string)     | Tier-aware sizing. Thread-aware. Calibration-aware.                     |
| Extent | `cache_total` | `fraction` (number)   | Coarse `(L1+L2+L3) × fraction`. Naive — not tier- or thread-aware.      |
| Extent | `absolute`    | `size` (string)       | Hard byte size, e.g. `"880MB"`, `"4GiB"`.                               |
| Chunk  | `auto`        | (none)                | Per-test heuristic chunk sizing.                                        |
| Chunk  | `whole`       | (none)                | One chunk, the whole extent (the latency and bandwidth tests).          |
| Chunk  | `cache`       | `target` (string)     | Same target syntax as the extent's `cache` mode.                        |
| Chunk  | `cache_total` | `fraction` (number)   | Coarse `(L1+L2+L3) × fraction`. Mirrors the extent mode.                |
| Chunk  | `absolute`    | `size` (string)       | Hard byte size.                                                         |
| Chunk  | `tm5_block`   | `size`, `divisor`, `granularity` | A TM5 block code 0-3: the smaller of `size` and the extent, / `divisor`, floored to `granularity`. |

**Where the extent lies and how chunks fall (TODO 76).** The extent is the first bytes of the
thread's memory, in order. Under `allocator=stitched` that memory is one span, so the extent is one
piece; under `plan-pagesize-pref` it is whole blocks, then the remainder. The chunk is resolved once
from the extent: the configured size, at least the test's minimum, at most the extent, rounded up to
a multiple of 4 KiB. Every chunk is exactly that size. They spread evenly over the piece: the first
starts at 0, the last ends at the piece's end, and where the chunk doesn't divide the piece the
chunks overlap, by under one chunk in total (`880 MiB x 6, 160 MiB overlap`). When it divides, they
tile with no overlap, as TM5's blocks do. A chunk may cross a 1 GiB to 2 MiB page seam inside a
span. Under `plan-pagesize-pref` a piece shorter than the chunk gets one chunk of its own length.
The per-thread log line lists each piece, its page sizes and its chunks. A fault in an overlap is
found by both chunks and counted twice, as `verify_reps` counts each detection. The plan table
shows each test's extent and chunk resolved for the thread memory (`880.00 MiB x9, 752.00 MiB
overlap`), and a line under it for every chunk the memory made smaller than its spec asks for.

**TM5 `.cfg` imports** cover the full allocation (TODO 76): every TM5 test walks its AWE window
over all of a core's locked memory, so an import's extent is `full_allocation`, not the `.cfg`'s
Testing Window Size. That window, with the Lock Memory Granularity, only sizes Test Block Size
codes 0-3 (`tm5_block`: window/(code + 1) floored to the granularity) and caps larger blocks. A
thread with less memory than the window takes the codes as fractions of its memory, as TM5 does.

An invalid spec stops the run with an error naming the `test_sequence` entry: an unknown mode, a
missing field, a size of 0, or a `cache` target whose scale is malformed or outside 0.01-100. A
default spec is checked for each enabled test that uses it. There is no fallback to another mode or
to the built-in test suite. An unknown JSON key anywhere in the config is an error too.

## JSON shape

Every spec is a nested object: `{ "mode": "...", ... }`. Only the fields required by the
mode need to appear.

```json
{
  "system": {
    "memory_strategy": {
      "allocation_mode": "percentage_reserve",
      "reserve_percent": 10.0,
      "default_extent": { "mode": "full_allocation" },
      "default_chunk":  { "mode": "auto" }
    }
  },
  "test_sequence": [
    {
      "function": "Mem-CacheBust",
      "extent": { "mode": "cache", "target": "L3*2" },
      "chunk":  { "mode": "absolute", "size": "1MB" }
    },
    {
      "function": "Mem-StuckBit",
      "extent": { "mode": "full_allocation" },
      "chunk":  { "mode": "absolute", "size": "512MiB" }
    },
    {
      "function": "Mem-SimpleV2",
      "extent": { "mode": "absolute", "size": "880MB" },
      "chunk":  { "mode": "absolute", "size": "16MB" }
    }
  ]
}
```

When a per-test `extent` or `chunk` is omitted, the global default applies.

## `cache` vs `cache_total` — when to use which

Both reach into the cache hierarchy, but they serve different purposes.

### `cache` — tier-aware, thread-aware, calibration-aware

Targets a **specific tier** with smarts to share that tier correctly across the active
threads. Each tier accepts a `scale` factor (any positive decimal in `0.01..=100.0`):

- **L1 / L2** (per-core): per-core size ÷ active SMT siblings × scale.
- **L3** (shared): per-thread share = (L3 ÷ thread_count) × scale. CPUID fallback
  also applies a `÷2` VM factor when running in a virtual machine.
- **DRAM**: per-thread working set = total L3 × scale. **No** thread divisor — each
  thread independently spills L3, which is what guarantees DRAM access under
  multi-threaded cache competition.
- **DRAM-Full**: sentinel meaning "use the entire per-thread allocation". No scale.

When `tmr-cfg.json` calibration data is available, the calibrated `optimal_size` for the
tier is used directly (not the CPUID heuristic). Calibration captures the true tier
ceiling, so the only post-divisor in the calibrated path is the topology-sharing one
(SMT siblings for L1/L2, thread_count for L3).

Use `cache` when you want **a precise tier** — "I want this test to fit in L2", "I want to
spill exactly 2× L3 worth of working set per thread".

#### `target` syntax

Both `*N` (multiplier) and `/N` (divisor) are accepted on every tier, and `N` may be
decimal. The two forms are equivalent (`L3/2` == `L3*0.5`); use whichever reads cleaner.

```
L1                     # default scale 0.5 (= L1/2)
L1/4                   # ÷4 of per-core L1 share
L2*0.8                 # 80% of per-core L2 share
L3                     # default scale 0.5 (= L3/2 of per-thread L3 share)
L3/2                   # half of per-thread L3 share
L3*0.9                 # 90% of per-thread L3 share
DRAM                   # default scale 4.0 (= DRAM*4, per-thread = 4× total L3)
DRAM*8                 # 8× total L3 per thread (heavy DRAM stress)
DRAM/2                 # 0.5× total L3 — sits *inside* L3 single-threaded but spills
                       # quickly under multi-thread sharing because there is no
                       # thread divisor on DRAM. Useful when you want a working set
                       # measured against the spill threshold rather than per-thread L3.
DRAM-FULL              # entire per-thread allocation
```

#### `L3*N` vs `DRAM/N` — they are not the same

Both produce a working set sized off L3, but they differ in *which* divisors apply:

| Target  | Per-thread size                       | Threads divisor | VM factor (CPUID) |
| ------- | ------------------------------------- | :-------------: | :---------------: |
| `L3*N`  | `(total_L3 / thread_count / vm) × N`  |       yes       |        yes        |
| `DRAM*N`| `total_L3 × N`                        |        no       |         no        |

So `DRAM/2` is `total_L3 × 0.5` per thread (half of *total* L3, per thread), while
`L3*2` is `(total_L3 / thread_count / vm_factor) × 2` per thread. On a 16-thread
non-VM box, those differ by ~8×.

### `cache_total` — coarse, intentionally naive

Computes `(per_core_l1d + per_core_l2 + l3_cache) × fraction`. **Not** tier-aware,
**not** thread-aware. The L1+L2 component is ~5% of L3 on most modern parts, so this is
effectively "a fraction of the cache hierarchy" treated as one number.

Originally designed as a "spill-the-whole-hierarchy" floor — pick a multiplier > 1.0 to
guarantee you exceed cache, regardless of tier topology. Useful for portable cross-machine
ratios where you just want "more than cache, but not full DRAM".

Examples:
- `cache_total fraction=0.5` — half the summed cache (sits inside the hierarchy somewhere).
- `cache_total fraction=2.0` — 2× total cache (definitely DRAM, but not by much).

If you need tier precision, use `cache` instead. `cache_total` is for coarse "more or less
than cache" controls.

## Size strings (`absolute` mode)

`parse_size_string` accepts:

| Suffix             | Meaning            | Example  |
| ------------------ | ------------------ | -------- |
| `B`                | bytes              | `"4096B"` |
| `KB`               | 1000               | `"64KB"` |
| `KiB` / `K`        | 1024               | `"64KiB"` |
| `MB`               | 1000²              | `"880MB"` |
| `MiB` / `M`        | 1024²              | `"64MiB"` |
| `GB`               | 1000³              | `"4GB"`  |
| `GiB` / `G`        | 1024³              | `"4GiB"` |
| (none)             | **defaults to MiB** | `"64"` → 64 MiB |

Parsing is case-insensitive. Underscores are stripped (`"4_096B"` works).

The unsuffixed default is **MiB** specifically for TM5 backward-compat: the legacy field
`testing_window_size_mb=64` is faithfully interpreted as 64 MiB when carried through.

## Migration from old field names

The old flat-field shape (`window_mode`, `window_size_mb`, `window_cache_multiplier`,
`chunk_mode`, `block_size_mb`, `block_window_fraction`) has been replaced. Equivalents:

| Old (flat fields)                                  | New (nested spec)                              |
| -------------------------------------------------- | ---------------------------------------------- |
| `window_mode: "full_allocation"`                   | `extent: { mode: "full_allocation" }`          |
| `window_mode: "fixed_size"`, `window_size_mb: 880` | `extent: { mode: "absolute", size: "880MB" }`  |
| `window_mode: "cache_relative"`, `window_cache_multiplier: 2.0` | `extent: { mode: "cache_total", fraction: 2.0 }` (or `cache` with `target` for tier-aware) |
| `default_window`, `window` (nested spec, until TODO 76) | `default_extent`, `extent` (same shape) |
| `chunk_mode: "auto_optimal"`                       | `chunk: { mode: "auto" }`                      |
| `chunk_mode: "fixed_size"`, `block_size_mb: 16`    | `chunk: { mode: "absolute", size: "16MB" }`    |
| `chunk_mode: "window_fraction"`, `block_window_fraction: 0.125` | none: an `absolute` size (the `fraction` chunk mode was removed 2026-10-03) |
| `chunk_mode: "window_size"` (TM5 0)                | none: an `absolute` size |

There is **no migration shim** — TMR is in dev mode and old configs must be hand-updated.
TM5 `.cfg` files continue to work because the legacy converter (`LegacyConfig::to_modern_config`)
emits the new shape.

## Examples

### Cache-aware refresh test (per-thread L3)

```json
{
  "function": "Mem-Refresh",
  "extent": { "mode": "cache", "target": "L3" },
  "chunk":  { "mode": "absolute", "size": "1MB" }
}
```

### Full-memory stuck-bit sweep

```json
{
  "function": "Mem-StuckBit",
  "extent": { "mode": "full_allocation" },
  "chunk":  { "mode": "absolute", "size": "512MiB" }
}
```

### Heavy DRAM stress

```json
{
  "function": "Mem-CacheBust",
  "extent": { "mode": "cache", "target": "DRAM*8" },
  "chunk":  { "mode": "absolute", "size": "1MB" }
}
```

### Fixed extent and chunk

```json
{
  "function": "Mem-SimpleV2",
  "extent": { "mode": "absolute", "size": "880MB" },
  "chunk":  { "mode": "absolute", "size": "16MB" }
}
```
