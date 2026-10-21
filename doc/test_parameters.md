# Test Parameters Reference

All per-test parameters are stored in `TestParameterContext` (`src/config.rs`). Each field is `Option<T>` — only the fields relevant to a given test are set; the rest are `None`. Tests panic at startup if their required field is missing.

## Struct Fields

### `raw_parameter: u32`
The original TM5 parameter value, preserved for debugging and display. Not directly consumed by test logic — it's the input that gets interpreted into the typed fields below.

### `stride_cachelines: Option<usize>`
**Used by:** SimpleTest (`Mem-SimpleV2-128/-256/-512/-Auto`, every width; `"parameter"` sets it for each)

The jump in cache lines (64-byte units) after each line the test writes or reads, simulating DDR interleave patterns.

- **Formula:** `channels × parameter - 1` (from TM5 `JumpStep = BlkSize × (Channels × Parameter - 1)`)
- **Example:** channels=2, parameter=8 → stride = 15 cache lines
- **The walk** (TODO 93, TM5's `mtests0.asm` ~336-394): each step writes or reads one whole 64 B line, then jumps the stride, so the period is `stride + 1` lines (`64 × channels × parameter` bytes). Each line of a period starts an interleave pass, the last line first, and a pass steps a period at a time to the chunk's end, so every line is touched once per pass over the chunk. Reads walk the same way. The image is the sequential test's (strided mode 2 writes mode 20's lines, at every width). Each step prefetches the line the walk reaches 128 steps on (TM5 prefetches 8 periods on, which can land outside the chunk), so requests stay queued while every access still opens a new row. Measurements: `doc/todo/93-strided-simpletest-lines.md`
- **Source:** TM5 `.cfg` parameter field + channels config, or direct value in JSON config
- **Display:** Shown in Test Configuration table as `Stride(15cl/960B)`

### `stride_elements: Option<usize>`
**Used by:** SimpleTest (same tests as `stride_cachelines`)

Derived field: stride in u64 elements. Calculated as `stride_cachelines × (cache_line_bytes / 8)`. Typically `stride_cachelines × 8` for 64-byte cache lines. This is the value used directly in the test loop for pointer arithmetic.

### `mirror: Option<MirrorMode>`
**Used by:** MirrorMove (`Mem-MirrorV2-128`, `-256`, `-512`, `-Auto`)

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

Number of independent RNG (xorshift) sequences for random access verification. More sequences = more diverse access patterns within each cycle. A cycle reads as many random words as the extent holds, split evenly over the sequences (TODO 99), so a cycle takes about as long as an extent's worth of random 8-byte reads.

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

## SimpleTest patterns (pattern spec 2)

| Where | Field | Meaning |
|---|---|---|
| test entry (Mem-SimpleV2*, Mem-Sequence) | `"program": ["random", "switch"]` | The native program: families in order, each run as a P/~P pair (`doc/pattern_spec.md`). Unset with no `pattern_mode`, `["random", "header", "random", "switch", "random", "quiet"]`. Not with `pattern_mode`, and only on a SimpleTest or Mem-Sequence |
| test entry | `"pattern_mode": 30` | One of TMR's numbered modes (spec 1, `doc/pattern_gen_modes.md`: 0-2, 10, 11, 13, 20, 30) instead of a program, or a native family's number (15 `ident`, 16 `random`, 17 `linepair`, 21 `lines`, 31 `switch`, 32 `quiet`, 33 `header`) for a program of it alone |
| command line | `seed=0x1234` | The native patterns' run seed (default from the clock); logged, printed and saved, so a run's bytes can be replayed on the same layout |
| command line | `beat=auto\|12\|16\|24\|32\|64` or a memory type | The transfer width `switch` (and mode 30) flips at; auto takes SMBIOS's memory type, else `switch` alternates 64 and 32 and mode 30 takes 32. A memory type names its width: `ddr3`/`ddr4` 64, `ddr5`/`lpddr3` 32, `lpddr4`/`lpddr5` 16, `ddr6` 24, `lpddr6` 12. The last two are models (`doc/pattern_spec.md`): SMBIOS has no code TMR knows for them, so set `beat=` there |
| command line | `rescan=on\|off` | Whether a failed check is rescanned to name each bad word (on); off counts one error a failed check |
| command line | `patterns=native\|legacy` | How a TM5 `.cfg`'s SimpleTests import: native programs on `Mem-SimpleV2-Auto` (the default), or the numbered modes on the same test |

**Repetition must test something** (checked when the config loads): `verify_reps` 0 on a SimpleTest
or mirror test, `write_read_cycles` 0 on any `run_phased_test` test, and `test_reps` 0 on a mirror
are refused; so is `test_reps` 0 with a mode that keys its image by the cycle (0, 1, 2, 20, 30),
whose fill and verifies would disagree. Each knob is at most 10,000. A knob the test doesn't read is
logged, not refused. A TM5 `.cfg` value that doesn't parse is an error naming it.

**A native SimpleTest's rounds come in pairs.** Each pattern is written twice in a row on a
chunk: P, then ~P (every bit flipped), so every bit is checked holding a 0 and a 1. An odd
`write_read_cycles` on one that writes first (`test_reps` above 0) rounds up to the next even
number, with a warning: 3 runs as 4, 1 as 2. A config's SimpleTest and a TM5 import take 4 by
default, the built-in suite's 2. A step that verifies its fill (`test_reps` 0) writes once a
suite cycle, so it alternates P and ~P between suite cycles instead (`doc/pattern_spec.md`).


## Mem-Sequence

A chunk's passes, given as steps: an optional count, then `W` (write the program's next round), `R`
(check what the chunk holds) or `RW` (check it and write its inverse in the same visit, so the reads
are checked in a stream with writes in it), then an optional `@order`. Up to 16 steps, separated by
spaces or commas; `write_read_cycles` is how many times the sequence runs a chunk (2 when unset).

| Where | Field | Meaning |
|---|---|---|
| test entry | `"sequence": "W 2R RW 2R@random"` | The passes, in order. Unset: `W 5R` (the built-in suite's: `W 2R RW 2R@random`, twice) |
| test entry | `"order": "forward"` | The order of the steps that name none: `forward`, `reverse`, `random` (every line once, scrambled anew each pass) or `stride` (TM5's walk, from the entry's `parameter`, as SimpleTest's; a stride of 0 is in order) |
| command line | `sequence=W,4R,RW@random,R`, `order=random` | The same, on every Mem-Sequence step; `stride` needs a config entry's `parameter` |

The rounds come in pairs, as SimpleTest's: a `W` writes the next round, and an `RW` the inverse of
what it read, which is the next round when it read a pattern's P (so no `W` rewrites what an `RW`
left). A sequence whose rounds a cycle are odd runs once more where that makes them even, with a
warning. Checked when the config loads: a sequence that never reads, a step of 0 or more than 64
passes, and `@stride` with no `parameter` are refused, as is a numbered `pattern_mode`
(`pattern-mode=` with one leaves Mem-Sequence's program). `WR` isn't a step: a read straight after
a write of the same line is answered by the cache. A sequence that starts with a read reads a fill.
A write no read checks (`2W R`, or a write last) is allowed, as write stress, with a warning. It runs
at 512 bits or 256 (the widest the CPU has, or `width=`'s; it has no 128-bit kernel, so
`width=128` runs it at 256).

## SIMD width (`width=`, TODO 99)

| Where | Field | Meaning |
|---|---|---|
| `system` | `"width": "auto"` | The SIMD width of every test that picks its width at run time: `auto` (the default: the widest the CPU has, 512 bits with AVX-512, else 256), `128`, `256` or `512` |
| command line | `width=auto\|128\|256\|512` | The same, over the config's |

The tests that pick their width at run time are every `-Auto` test, which resolves to the test
named for the width (`Mem-StuckBitAuto` under `width=256` runs `Mem-StuckBit256`, shown as
`Mem-StuckBit256_A` in the plan and the results file), Mem-Sequence (at 256 for 128, above), the
native and numbered Bench pairs (`Bench-Init-Ident`, `Bench-Verify-TMR-10`, ...) and
`Bench-{Init,Verify}-Seal-{TMR,TM5}`. The seal takes it too, unless `seal-width=` (or the config's
`seal_width`) sets its own. A test named for a width (`-128`, `-256`, `-512`) keeps that one. A
width the CPU lacks is refused before anything runs, with what it needs (`width=`, `seal-width=`,
the seal off too). A test the CPU can't run, a 512-bit one without AVX-512 (F, BW, CD, DQ and
VL), is left out of the run however it was picked (a plan, `test=`, a config), and the run says
so (`ℹ  Left out N test(s) this CPU can't run ...`); it never runs at another width in its
place. The kernels don't check: one that got past would stop the run (a `bug:` panic). The plan
says what was given and where (`ℹ  Width: 256 bits (width=)`). The results file records the run's
width and where it came from (`execution.simd_width_bits`, `execution.simd_width_source`:
`width=`, `the config's width` or `auto`), and each test that takes it the width it ran at
(`run_config.tests[].simd_width_bits`): Mem-Sequence 256 under `width=128`, and the Bench pairs,
whose names carry none. A test named for a width runs at that one and records none.
`--compare-results` flags a test that ran at another width under the same name. Auto stays the
default: measured on Intel and AMD, the widest suits the write-and-check tests; `width=` is there
to compare widths or to step down.

## Parameter Sources

| Source | How parameters are set |
|--------|----------------------|
| TM5 `.cfg` file | `interpret_tm5_parameter_with_channels()` maps the raw parameter based on test function name |
| JSON config v2.0 | Direct fields: `"parameter"` for SimpleTest's TM5-style stride, or named fields (`"mirror"`, `"stride_patterns"`, etc.) |
| CLI override | `mirror=subblocks:4` (or `whole`, `jump:N`) sets every test's mirror mode and leaves its other parameters |
| Default test suite | Hard-coded in `create_test_definitions()` in `runner.rs`; `plan=` (`plans.rs`, TODO 25) picks which of its tests run, keeping their settings: `stability` by default, `quick`, `widths`, `perf`, `all` |

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
| `system` | `"seal_width": "auto"` | The seal kernels' SIMD width: `auto` (this machine's seal calibration, else the rule: 256 with AVX2; below), `128`, `256` or `512`; unset, `width`'s when that is set, else `auto`. A width the CPU lacks is a load error |

The command line overrides the last two: `seal=tmr|tm5|off`, `seal-width=auto|128|256|512`. A TM5
`Cycles = 0` runs until stopped. A dependent step (`skip_init`) must directly follow the step that
wrote its data, with no step that takes the seal in between; otherwise the plan is refused. A
dependent native SimpleTest can't only verify (`test_reps` 0): its image is keyed by its own
step, which no other step writes. One that isn't dependent verifies its own fill.
`Bench-Init-Seal-{TMR,TM5}[-128|-256|-512]` and their `Bench-Verify-Seal-*` measure the seal's
fill and check alone.

### The seal's width under `auto` (TODO 99)

`seal-width=auto` runs the width of this machine's seal calibration, else the rule's (256 with
AVX2, else 128: one Intel measurement, 2026-10-06, `SealWidth::resolve_on`). A width set wins over both:
`seal-width=` or the config's `seal_width`, else `width=` or the config's `width`. The plan names
the kernel and where its width came from (`🔒 Seal (tmr-256, calibrated)`; `rule`, or the setting:
`seal-width=`, `width=`, `the config's seal_width`, `the config's width`), and the results file
records both (`execution.seal`, `execution.seal_width_source`; null when the run is unsealed).

`tmr.exe --calibrate-seal` measures it; `--calibrate-all` runs `--calibrate-cache` and then it.
One thread (as the latency calibration, so no other thread's traffic biases a width) runs the
seal's own kernels, `SealKernel::fill` and `check` with the default pattern `tmr`, at every width
the CPU has, over a buffer 4 times the L3 (at least 512 MiB) allocated as the cache calibration
allocates, so each check reads DRAM. It asks for the page size `maxpage=` gives (2 MiB by default),
strictly, then for each smaller one in turn where a size is refused; the report and the saved
calibration name the page size it got. Each round fills and
checks the whole buffer at every width, in an order rotated each round; the first round is a
warm-up, then 5 are timed. It reports each width's fill and check speed (MiB/s at the median) and
its fill + check time (median [min..max]), and picks the fastest by median, or a narrower width
that runs at the same speed, since the narrower draws less power. The same speed is a median no
slower than the tighter of the fastest's upper quartile (of 5 runs, the second slowest) and 1%
over its median (`choose_seal_width`). The quartile is the fastest's own run-to-run spread, which
one preempted run can't widen (the slowest run, which this used to take, let 40 ms [39.5..55] pass
a width at 50 ms, 25% slower); the 1% caps what noise can cost, since two disturbed runs still lift
the quartile and the choice is saved for every later run. A check that finds a bad word fails the
calibration (memory errors). The result is saved to `tmr-cfg.json` (`seal_calibration`) after a
prompt comparing it with the one stored, as the cache calibration's is, and is used only on the
machine it was measured on. It keeps its own fingerprint of that machine, everything the cache
calibration's check compares (`is_calibration_valid`: the machine ID, the memory configuration,
the system UUID and the TSC frequency), and is used only while all of it is this machine's. Its
save leaves the config's identity, which the cache calibrations are checked against, alone, and a
cache calibration's save leaves the seal's alone: a CPU swap followed by only `--calibrate-seal`
keeps the old cache calibration stale, and one followed by only `--calibrate-cache` keeps the old
seal width stale. `--no-calibration` skips it.

A `tm5` seal takes the same width: only `tmr` is measured. Its kernels hold a page's three vectors
in registers and only store or load, XOR and OR them, where `tmr`'s compute a hash per vector, so
`tm5` is the more purely memory-bound of the two and less likely to separate the widths (Rule 3 of
`doc/simd_codegen_rules.md`); measuring it too would cost a few seconds more. Measure it if a
`Bench-*-Seal-TM5` comparison shows `tm5` preferring another width than `tmr` on some machine.
