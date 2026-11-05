# TMR's tests

TMR's tests do one of two jobs. The `Mem-` tests look for memory errors: they write patterns,
read them back and count every bit that differs. The `Lat-`, `Spd-` and `Bench-` tests measure:
latency, bandwidth and how fast TMR writes and checks its patterns. These pages say what each test
does, which plans run it, how long it takes and how to read what it prints. Installing and running
TMR are in the [user guide](../guide.md).

## The plans

A plan picks which of TMR's built-in tests a run takes, and in what order. Give it as `plan=` on
the command line, or in the window's box. A plain `tmr` runs `plan=stability`. A test keeps its
own settings in every plan, so `plan=stability` and `test=Mem-MovingInv` run the same
Mem-MovingInv.

| Plan | What it runs | Cycles | How long |
|---|---|---|---|
| `stability` (the default) | the 13 stability tests, at the widest width | 3 | about 5 min a cycle |
| `quick` | the same 13 | 1 | about 5 min |
| `widths` | 20 tests: five families, every width side by side | 3 | about 6 min a cycle |
| `perf` | 25 tests: latency, latency under load, bandwidth | 1 | about 5.5 min |
| `all` | every test, 277 | 3 | about an hour a cycle |

- **stability** runs, in this order: Mem-StuckBitAuto, Mem-SimpleV2-Auto, Mem-SimpleNT-Auto,
  Mem-PageExchange, Mem-MovingInv, Mem-ModuloX, Mem-Sequence, Mem-Random,
  Mem-PageExchange-Rewrite, Mem-RefreshAuto, Mem-MirrorV2-Auto, Mem-Stride and Mem-BlockMove.
  Mem-StuckBit goes first, so a hard fault shows in seconds. The two page exchange tests sit
  apart, so a fault that comes and goes meets one or the other. It leaves out Mem-CacheBust,
  which works mostly inside the caches, and the Flush tests: at these chunk sizes the data has
  left the cache anyway.
- **quick** is the stability plan once. `tmr --quick-test` is `plan=quick`, leaving 60% of the
  free memory for Windows.
- **widths** runs Mem-StuckBit, Mem-SimpleNT, Mem-Refresh, Mem-SimpleV2 and Mem-MirrorV2 at 128,
  256 and 512 bits and Auto, all in one run, so every width tests the same memory in the same
  cycle. Compare widths there: separate runs drift. A CPU without AVX-512 leaves the five 512-bit
  tests out (about 5 min a cycle then).
- **perf** runs seven unloaded latency tests, Lat-L1-Read, Lat-L2-Read, Lat-L3-Read,
  Lat-DRAM-Read, Lat-DRAM-Write, Lat-DRAMFull-Read and Lat-DRAMFull-Write; then Lat-Loaded-DRAM
  and Lat-Turnaround-DRAM (about a minute together); then the 15 `Spd-…-Auto` bandwidth tests;
  and Lat-NTW-DRAM-Write-Auto last. It leaves out the cache tiers' Write and Copy latency and the
  DRAM tiers' Copy: in the caches the read chain sets the time and the stores hit the cache, so
  they read as the Read does; in DRAM the stores' line fetches add, and Copy adds nothing Write
  doesn't. Run them by name. No perf test takes the seal, so a perf run is unsealed.
- **all** runs every test in the suite's order: the `Mem-` tests, then Bench, then latency, the
  bandwidth tests last. About half of its hour is the 180 latency tests. A CPU without AVX-512
  leaves out the 50 512-bit tests, and one without AMX leaves out Mem-TileTransform; the run says
  what it left out.

The times are rough, worked out from the steps' lengths. Most steps stop on the clock, so the
times hold on most PCs. The one-pass tests (Mem-StuckBit, Mem-Stride, the Bench tests) and the
fill before each DRAMFull test take longer with more memory. A step stops at the end of the pass
it is in, so it runs a little over its time.

### Choosing tests yourself

- **`test=`** runs tests by name, with `*` and `?` wildcards and commas: `tmr test=Mem-MovingInv`,
  `tmr test=Mem-PageExchange*`, `tmr test=Spd-L3-*,Lat-L3-Read`. On its own it picks from every
  test and runs **one cycle**, unless you give `cycles=` or `duration=`. With a `plan=`, it picks
  from that plan's tests.
- **`cycles=10`** or **`duration=3600`** replaces a plan's cycles. `duration=` alone runs cycles
  until the time is up.
- **`config=`** runs a TM5 `.cfg` or a TMR `.json` config, which brings its own tests. It can't be
  given with `plan=`.
- **`tmr --cache-latency`** runs the Read, Write and Copy latency of L1, L2, L3 and DRAM (12 tests)
  once, on one CPU. **`tmr --ram-latency`** runs the six Lat-DRAM and Lat-DRAMFull tests once, on
  half the CPUs.

## The families

| Page | Tests | What they're for | Plans |
|---|---|---|---|
| [The stability tests](stability.md) | 24 | memory errors: stuck bits, faults that depend on the data, writes that disturb other cells or land at the wrong address | quick, stability, widths, all |
| [Refresh, page exchange and coherence storm](refresh-and-exchange.md) | 9 | data left alone, data passed between cores, cache lines every core writes | quick, stability, widths, all |
| [Mem-TileTransform, the AMX tile test](amx.md) | 1 | memory errors while the CPU's matrix unit works on the data (Xeon with AMX only) | all |
| [The seal and the Bench tests](seal-and-bench.md) | 46 | the seal around every `Mem-` step; the Bench tests time each pattern | all |
| [The latency tests](latency.md) | 180 | how long a read waits, at each cache level and in DRAM | perf, all |
| [Latency under load](loaded-latency.md) | 2 | a read's latency while the other cores keep the memory busy | perf, all |
| [The bandwidth tests](bandwidth.md) | 15 | how fast the CPU reads, writes and copies, at each cache level and in DRAM | perf, all |

**The seal** (TM5's test 0) isn't a test you pick. A run with a test that takes it (most `Mem-`
tests do) writes the seal over all memory at each cycle's start. Each of those steps checks that
its chunks still hold the seal before it works them, and reseals them after, and the cycle ends
with a check of all memory. Its errors have a column of their own. See
[the seal](seal-and-bench.md).

## Reading the names

- **The prefix.** `Mem-` tests check data and find errors. `Lat-` and `Spd-` tests measure and
  check no data. `Bench-` tests time a pattern (a Bench-Verify does check its pattern).
- **The tier**, in the latency and bandwidth names: L1, L2 or L3; DRAM, four times one L3, so past
  the caches but not all of memory (the bandwidth tests call it DRAMSmall); or DRAMFull, all of
  each thread's memory.
- **The width.** 128, 256 and 512 at the end of a name (Mem-StuckBit256, Mem-SimpleV2-256) are
  SIMD widths in bits (SSE2, AVX2 and AVX-512). Each runs at its width only, and a CPU that lacks
  it leaves the test out of the run, and says so. Auto runs the widest the CPU has: 512 with
  AVX-512 (F, BW, CD, DQ and VL), else 256, or the width `width=` gives. TMR needs AVX2, so Auto
  is never 128 unless you ask.
- **What you see.** An Auto test is shown under the width it ran at, with `_A` after it:
  Mem-SimpleV2-Auto shows as `Mem-SimpleV2-512_A` on a CPU with AVX-512. `test=` takes any of its
  names. `test=Mem-SimpleV2-Auto` (or `test=Mem-SimpleV2-512_A`) picks the Auto test alone, and
  `test=Mem-SimpleV2-512` picks the fixed one, and the Auto one too where Auto runs at 512.

## Reading a step's report

Every step ends with a report, on the console and in the window (a finished step's row opens it):

```
📊 Test report - Cycle 1 - Step 2 Mem-SimpleV2-256_A: ❌ 31.4s, 1 error (1 test + 0 seal + 0 WHEA), Cycles 🟢 9/100, Time Limit 🔴
```

- **✅ or ❌** is the step's verdict. Any error fails it: the test's own, the seal's, or a WHEA
  event (a hardware error Windows recorded during the step). An unsealed run's line reads
  `(0 test + 0 WHEA)`.
- **Cycles** counts the test's own passes in this step (samples, for a latency test), not the
  run's cycles. 🔴 marks the limit that ended the step.
- The lines under it and the per-thread table differ from family to family: see each page.

The run ends with a final summary and a verdict. Each run's results go in
`results\TMR_<date>_<time>.json`, and `tmr --compare-results <one> <other>` compares two runs.

## Every test

The Plans column names every plan that runs a test, `all` included, as every test is in it.
Where a plan runs only some of a row's tests, the brackets say which. In the names, a slash gives
another ending of the same name: Mem-SimpleV2-128/256 is Mem-SimpleV2-128 and Mem-SimpleV2-256,
and Mem-StuckBit128/256 is Mem-StuckBit128 and Mem-StuckBit256. `<tier>` is each of L1, L2, L3,
DRAM and DRAMFull. Step lengths are the defaults.

### [The stability tests](stability.md)

| Tests | What they do | Plans |
|---|---|---|
| Mem-StuckBit128/256/512/Auto | write `0xAA55…`, its inverse and `0xAA55…` again over all memory, checking each: bits stuck at 0 or 1; one pass | quick (Auto), stability (Auto), widths, all |
| Mem-StuckBit-FlushAuto | the same in L2-sized chunks, each flushed from the cache before its check | all |
| Mem-SimpleV2-128/256/512/Auto | TM5's SimpleTest: TMR's pattern program, each pattern and its inverse, over 4 MiB chunks; 30 s or 100 passes | quick (Auto), stability (Auto), widths, all |
| Mem-SimpleNT-128/256/512/Auto | write, then check, with non-temporal stores that go straight to DRAM; 30 s or 100 passes | quick (Auto), stability (Auto), widths, all |
| Mem-MovingInv | memtest86's moving inversions, up then down, each word checked just before it is overwritten; 30 s | quick, stability, all |
| Mem-ModuloX | memtest86's modulo-20: every 20th line held while the lines around it are rewritten; 30 s | quick, stability, all |
| Mem-Sequence | write, read and read-write passes, forward, reversed and in random line order; 30 s | quick, stability, all |
| Mem-Random | 8-byte reads at random addresses, then a sweep of every word; 25 s and the sweep | quick, stability, all |
| Mem-MirrorV2-128/256/512/Auto | TM5's MirrorMove: swaps in place, over the sealed data; 10 s | quick (Auto), stability (Auto), widths, all |
| Mem-Stride | six passes over all memory, 8 B to 32 KiB apart; one round | quick, stability, all |
| Mem-BlockMove | 8 MiB block copies through DRAM, a read and a write stream at once; 20 s | quick, stability, all |
| Mem-CacheBust | writes a page apart, mostly inside the caches; 20 s | all |

### [Refresh, page exchange and coherence storm](refresh-and-exchange.md)

| Tests | What they do | Plans |
|---|---|---|
| Mem-Refresh128/256/512/Auto | write, wait 500 ms with nothing touching the data, then check: data that sits still keeps its value; 15 s | quick (Auto), stability (Auto), widths, all |
| Mem-Refresh-FlushAuto | the same, the data flushed from the cache before the wait | all |
| Mem-PageExchange | 2 MiB tiles passed from thread to thread, each checked and copied by whichever draws it; 30 s | quick, stability, all |
| Mem-PageExchange-Rewrite | the same, each tile checked, then written a new pattern; 30 s | quick, stability, all |
| Mem-PageExchange-Invert | the same, each word checked and inverted in place; 30 s | all |
| Mem-CoherenceStorm | every core writes its own byte of a few shared cache lines, so the lines move from core to core; 30 s | all |

### [The AMX tile test](amx.md)

| Tests | What they do | Plans |
|---|---|---|
| Mem-TileTransform | writes 1 KiB tiles, multiplies each in place on the matrix unit, then checks a checksum for each group of tiles; Xeon with AMX only; 30 s | all |

### [The Bench tests](seal-and-bench.md)

Each Bench-Init writes its pattern over all of each thread's memory 10 times, checking nothing.
Each Bench-Verify writes it once, outside its time, then reads and checks it 10 times.

| Tests | The pattern | Plans |
|---|---|---|
| Bench-Init-TM5-0/1/2, Bench-Verify-TM5-0/1/2 | pattern modes 0, 1 and 2, after TM5's | all |
| Bench-Init-TMR-10/11/13/20/30, Bench-Verify-TMR-10/11/13/20/30 | TMR's numbered pattern modes | all |
| Bench-Init-Ident/Random/Switch/Lines/Linepair/Quiet/Header, and the same Bench-Verify-… | TMR's seven pattern families, each alone | all |
| Bench-Init-Seal-TMR, Bench-Verify-Seal-TMR | the seal's `tmr` pattern, at the run's width (`width=`, else the seal's own) | all |
| Bench-Init-Seal-TMR-128/256/512, Bench-Verify-Seal-TMR-128/256/512 | the same at one width | all |
| Bench-Init-Seal-TM5, Bench-Verify-Seal-TM5 | the seal's `tm5` pattern, TM5's own test 0 pattern | all |
| Bench-Init-Seal-TM5-128/256/512, Bench-Verify-Seal-TM5-128/256/512 | the same at one width | all |

### [The latency tests](latency.md)

Each step takes 10 s of samples, after an untimed build of its chain.

| Tests | What they do | Plans |
|---|---|---|
| Lat-L1-Read, Lat-L2-Read, Lat-L3-Read, Lat-DRAM-Read, Lat-DRAMFull-Read | a pointer chase: one dependent read at a time, each waiting for the one before | perf, all |
| Lat-L1-Write, Lat-L2-Write, Lat-L3-Write, Lat-DRAM-Write, Lat-DRAMFull-Write | the chase, plus a store to a random line each step | perf (DRAM, DRAMFull), all |
| Lat-L1-Copy, Lat-L2-Copy, Lat-L3-Copy, Lat-DRAM-Copy, Lat-DRAMFull-Copy | the chase, each value read also stored, in order | all |
| Lat-V2-\<tier>-Read-128/256/512/Auto | the chase, each chain cell on a line of its own, plus one SIMD load of a random line each step | all |
| Lat-V2-\<tier>-Write-128/256/512/Auto | the same chase, plus one SIMD store to a random line each step | all |
| Lat-V2-\<tier>-Copy-128/256/512/Auto | the same chase, plus one load, change and store of a random line each step | all |
| Lat-V2P-\<tier>-Read-128/256/512/Auto | the V2 chase, plus six independent loads of random lines each step | all |
| Lat-V2P-\<tier>-Write-128/256/512/Auto | the same with six stores | all |
| Lat-V2P-\<tier>-Copy-128/256/512/Auto | the same with six loads, changes and stores | all |
| Lat-V2P-\<tier>-WriteFull-128/256/512/Auto | Write, covering each whole 64-byte line at every width | all |
| Lat-V2P-\<tier>-CopyFull-128/256/512/Auto | Copy, the same | all |
| Lat-NTW-DRAM-Write-Scalar/128/256/512/Auto | 4,096 non-temporal stores to random lines, then a fence: the time per store (Scalar's stores are 8 bytes) | perf (Auto), all |

### [Latency under load](loaded-latency.md)

| Tests | What they do | Plans |
|---|---|---|
| Lat-Loaded-DRAM | a read's latency idle, while every other core writes to memory flat out, and while they read flat out; about 10 s | perf, all |
| Lat-Turnaround-DRAM | the same, then writes and reads held to 25%, 50% and 75% of the lower flat-out speed: the turnaround, what writes cost a read; about 50 s | perf, all |

### [The bandwidth tests](bandwidth.md)

Each step makes passes over its tier for 10 s, after an untimed fill.

| Tests | Each thread's memory | Plans |
|---|---|---|
| Spd-L1-Read-Auto, Spd-L1-Write-Auto, Spd-L1-Copy-Auto | half of its share of a core's L1 | perf, all |
| Spd-L2-Read-Auto, Spd-L2-Write-Auto, Spd-L2-Copy-Auto | half of its share of a core's L2 | perf, all |
| Spd-L3-Read-Auto, Spd-L3-Write-Auto, Spd-L3-Copy-Auto | half of its share of its L3 group's L3 | perf, all |
| Spd-DRAMSmall-Read-Auto, Spd-DRAMSmall-Write-Auto, Spd-DRAMSmall-Copy-Auto | four times one L3 | perf, all |
| Spd-DRAMFull-Read-Auto, Spd-DRAMFull-Write-Auto, Spd-DRAMFull-Copy-Auto | all of it | perf, all |

The fixed widths, `Spd-<tier>-<operation>-128`, `-256` and `-512`, aren't in the suite. A TMR
`.json` config can name them, to put the widths side by side in one run.
