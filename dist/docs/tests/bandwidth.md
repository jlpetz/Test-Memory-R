# The bandwidth tests

These tests measure how fast your CPU reads, writes and copies memory: in each of its three
caches, and in DRAM. They are a benchmark, not a stability test: they check no data. Use them to
see what a memory setting does to throughput, or to check that a machine performs as it should.
For errors, run the [stability tests](stability.md).

## The fifteen tests

Each name is `Spd-<tier>-<operation>-Auto`: five tiers, each read, written and copied
(`Spd-L1-Read-Auto`, `Spd-L1-Write-Auto`, `Spd-L1-Copy-Auto`, then L2, L3, DRAMSmall and
DRAMFull the same way).

| Tier | Each thread's extent, by default | The plan's Extent column, after the size |
|---|---|---|
| L1 | half a core's L1 data cache | `L1/2` |
| L2 | half a core's L2 | `L2/2` |
| L3 | half the L3 a thread reaches while the run's other threads in its L3 group use theirs | `L3(6)/2 [cal]`, or from CPUID `L3/6/2` |
| DRAMSmall | four times one L3 group's L3 | `DRAM*4` |
| DRAMFull | all of the thread's memory | `DRAM-Full` |

The extent is the memory a test covers, from the start of each thread's memory. Every thread runs
the test at the same time, each on its own extent, so a step's figure is the whole CPU's: at L1,
every core's L1 together.

### Where the sizes come from

- **This machine's cache calibration**, when `tmr-cfg.json` holds one; the plan marks those sizes
  `[cal]`. The first run on a machine measures it (about 2 minutes), and `tmr --show-calibration`
  shows it. Under `calibrate=off`, or when that measurement fails (the run stopped, or too little
  memory for it), the sizes are the ones the CPU reports (CPUID).
- **L1 and L2** are a core's. When the run puts two threads on one core (SMT, under the default
  `cputype=threads` with enough threads), each thread takes half. Calibrated, it takes the size
  measured with both of the core's threads running.
- **L3** is shared by the cores of an L3 group. A CPU with more than one L3 (a Ryzen with two
  CCDs, an EPYC) gives each group its own, so the L3 is split by the most threads the run puts in
  one group, not by all of them. Calibrated, the split follows what was measured with one thread
  alone and with every CPU running at once, as an L3 doesn't divide evenly. The extent is never
  smaller than the thread's share of its L2 plus 64 KiB, so it can't fit in L2. From CPUID, in a
  virtual machine, it is halved again.
- **DRAMSmall** has no thread divisor: each thread's extent alone is four times the L3, so the L3
  can't hold it however many threads run. On a Ryzen 9950X (two CCDs of 32 MB) it is 128 MB a
  thread. It doesn't change with `memory=`, unless a thread has less memory than that.
- **DRAMFull** is every byte the thread was given, so it changes with `memory=`, `cpus=` and the
  rest. Over that much memory it also pays for walking the page tables, which DRAMSmall mostly
  doesn't.

Each extent is capped at the thread's memory and rounded down to whole 4 KiB pages. The plan's
Extent column shows each test's size.

## How they work

- **First, an untimed fill.** Each thread writes its extent once (for Copy, the half it copies
  from), so Windows has backed every page before the clock starts. The step report shows it as
  `pattern fill` on its Intermediate line.
- **Then passes, for 10 seconds.** A pass walks the whole extent, start to end, in one go; nothing
  breaks it into chunks. TMR checks the clock and Ctrl+C between passes, never inside one: about
  every 10 ms, or after each pass when a pass takes longer.
- **Read** loads every vector into four independent running XORs, so no load waits for the one
  before it. The result is thrown away.
- **Write** stores a fixed pattern. At L1, L2 and L3 these are ordinary stores, which go into the
  cache. At DRAMSmall and DRAMFull they are non-temporal (streaming) stores, which go to memory
  past the caches, with a store fence after each pass: the memory's own write rate, with no
  line read in first.
- **Copy** splits the extent in two halves and copies the first to the second, then back. Its
  MiB/s counts both the bytes read and the bytes written. At DRAMSmall and DRAMFull its stores are
  non-temporal too.
- **No data checking.** Nothing is compared, so a bandwidth step's own error count is always 0.

## Widths

Each test exists at three SIMD widths: 128 bits (SSE2), 256 (AVX2) and 512 (AVX-512). The plans
take the `-Auto` names, which run the run's width:

- **By default, the widest the CPU has**: 512 with AVX-512 (F, BW, CD, DQ and VL; a Ryzen 7000 or
  8000, for example), else 256 (a Ryzen 5000). TMR needs AVX2, so the default is never 128.
- **`width=128`, `width=256` or `width=512`** sets it for every Auto test, and for the other
  tests that pick their width as they run. A width the CPU lacks is refused before anything runs.

The plan, the step report and the results file name the width a test ran at:
`Spd-L1-Read-512_A` is Spd-L1-Read-Auto at 512 bits. `plan=widths` doesn't take the bandwidth
tests; to compare widths, make two runs, `tmr plan=perf width=256` and `tmr plan=perf width=512`,
or name the widths in a config (see the caveats).

## Which plans run them, and how long

| Plan | What it runs | Cycles |
|---|---|---|
| `plan=perf` | all fifteen, steps 10 to 24 of its 25: after seven [latency tests](latency.md), Lat-Loaded-DRAM and Lat-Turnaround-DRAM, and before Lat-NTW-DRAM-Write-Auto | 1 |
| `plan=all` | every test in the suite, the bandwidth tests last | 3 |
| `plan=stability`, `quick`, `widths` | none of them | |

On their own, run them by name: `tmr test=Spd-*` runs the fifteen once, `test=Spd-L3-*` the
three L3 tests, `test=Spd-DRAM*` the six DRAM ones. A test filter on its own runs one cycle
unless you give `cycles=` or `duration=`.

Each step runs 10 seconds after its fill, so a step takes a little over 10 seconds. The fifteen
take about 2.5 minutes a cycle, a little more with a lot of memory, since the DRAMFull tests fill
each thread's whole memory first.

## Reading the output

### The step report

The console prints one after each step; in the window, a finished step's row opens it. An
illustrative one, on a CPU with six threads of 5 GiB each:

```
📊 Test report - Cycle 1 - Step 22 Spd-DRAMFull-Read-512_A: ✅ 11.6s, 0 errors (0 test + 0 WHEA), Cycles 🟢 20/∞, Time Limit 🔴
   Test: 10.2s, 600.00 GiB @ 60235 MiB/s, 80.53B ops @ 7.90B ops/s
   Intermediate: seal fill n/a; seal check n/a; pattern fill 1.2s, 30.00 GiB @ 25600 MiB/s; chain build n/a
   Config: parameter=-, mode=-, extent=Full Alloc (DRAM-Full), chunk=Whole
```

- **The first line:** ✅ or ❌, the step's whole time (the fill included) and its errors.
  `Cycles 🟢 20/∞` counts passes over the extent (the most any thread made). `Time Limit 🔴` only
  says the 10-second limit ended the step, as it does in the plans: the lights show
  which limit ended a step, and aren't a verdict.
- **Test** is the figure: the test's own time (its slowest thread's, the fill left out), every
  thread's bytes together, and their MiB/s, bytes over that time. `ops` are 8-byte words moved,
  the bytes over 8.
- **Intermediate** is the work outside the figure: the fill, and in a sealed run the seal's check
  of the memory the step is about to overwrite.
- **Config** gives the extent as the test sized it, such as `24.00 KiB (L1/2 [cal])`, and its one
  chunk, `Whole`.

Below it, a table has a row for each thread: where it ran (`L CPU`, `P Core`, `NUMA`), its `Time`,
`Data` and `Speed` (its own MiB/s), how far each is from the average (`Dev T`, `Dev D`, `Dev S`,
in seconds, bytes and MiB/s), its passes (`Cycles`), and `Errors`, `WHEA` and `Pass`. A thread
well below the rest (a large negative `Dev S`) shared its core with other work, or ran on a
slower core, such as a hybrid CPU's small ones.

### Errors and the seal

- **The tests' own errors are always 0**: they check nothing.
- **WHEA:** a hardware error Windows reported during the step counts, and fails it.
- **The seal** (TM5's test 0) is for tests that check data; these never take it. In a sealed
  run (`plan=all`), TMR checks the sealed memory a bandwidth step is about to overwrite before
  the step starts, apart from its figure. What that check finds is the step's `seal` count: a
  fault in data an earlier step left, which the bandwidth test didn't cause. `plan=perf` and
  `test=Spd-*` have no step that takes the seal, so they run unsealed
  (`🔓 Seal: off (no step in this plan takes it)`) and the line reads `(0 test + 0 WHEA)`.
- **A crash, a freeze or a WHEA error** during a bandwidth test still means the system isn't
  stable: the DRAM tests load the memory about as hard as any test does.

### The summary and the results file

The final summary's Per-Test Performance table has a row a test: its Time, Data and Speed averaged
over the cycles it ran in, and its errors summed.

The results file (`results\TMR_<date>_<time>.json`) keeps:

- **Each step**, in `cycles[].tests[]`: `name` (`Spd-L1-Read-512_A`), `timed_ms`,
  `bytes_processed`, `throughput_mib_s` (the Test line's MiB/s), `cycles_completed_min` and
  `cycles_completed_max` (passes), `stopped_by` (`time`), `seal_errors`, `whea_total`, and
  `threads[]`: each thread's `bytes`, `ms`, `cycles`, `cpu`, `core` and `node`.
- **Each test over the cycles**, in `overall_stats.per_step_averages[]`: `avg_throughput_mib_s`.
- **How each test was sized**, in `run_config.tests[]`: `extent_mode`, `extent_bytes` and
  `simd_width_bits`; and in `run_config.calibration`, the calibration the sizes came from.

Speeds are in MiB/s only: divide by 1024 for GiB/s.

## Comparing runs

A bandwidth figure means most beside one from the same machine, the same TMR version and the same
settings.

- **The same threads and memory:** `cpus=`, `skip-cores=`, `cputype=` and `memory=`. They set how
  many cores work, whether two share a core (which halves the L1 and L2 extents), and DRAMFull's
  extent. The same page sizes, too: TMR takes large pages when it can.
- **The same extents.** L1 to DRAMSmall are sized from the calibration. If `calibrate=recheck` or
  `calibrate=always` measured a new one, check that the plan's Extent column still matches.
- **The same width.** An `-Auto` test runs at whatever `width=` or the CPU gives it.
- **The same TMR version.** 0.1.1's L1 tests (and, slightly, L2's) read higher than 0.1.0's, as
  they check the clock less often; L3 and DRAM didn't change.
- **`tmr --compare-results <one> <other>`** (or the window's Compare tab) matches the two runs'
  tests by name, an Auto test at any width, and flags one that ran at another width or with
  another extent. It also lists what differs between the runs: the thread count, threads a core,
  memory per thread, the page-size mix, which CPUs ran, the CPU and the BIOS.
- **Noise.** On the project's test machines, idle and with the same settings, each bandwidth test
  repeated within about half a percent from run to run. A change of a percent or more is likely
  real; re-run an outlier before you trust it. Close other programs: they share the memory.
- **L1 and L2 are a control.** They don't reach the memory, so a memory setting shouldn't move
  them. If they moved along with DRAM, something else changed: CPU clocks, power limits,
  background work.
- **Other tools.** MiB/s is 2^20 bytes a second: for the same speed, a tool that reports MB/s
  (10^6 bytes) reads about 5% higher than MiB/s, and one that reports GB/s about 7% higher than
  GiB/s. TMR's Copy counts the bytes read and the bytes written, and every thread runs at once;
  check how the other tool counts.

What normal looks like depends on the CPU. Spd-DRAMFull on two of the project's machines, in
MiB/s (TMR 0.1.0, whose DRAM figures 0.1.1 keeps; `memory=50% cputype=cores skip-cores=0`):

| CPU and memory | Read | Write | Copy |
|---|---:|---:|---:|
| Ryzen 5 8600G, DDR5 | 59,990 | 60,820 | 75,087 |
| Ryzen 7 5700X, DDR4 | 46,566 | 27,538 | 45,924 |

On the 8600G, writing ran as fast as reading; on the 5700X, at about 60% of it. Copy read above
Read on the 8600G, as it counts both directions.

## Caveats

- **Not a stability test.** A clean bandwidth run says nothing about errors. Use
  `plan=stability`.
- **Write at DRAM can sit well below Read** on some CPUs, as on the 5700X above. That is the CPU's
  and memory's write path, not a fault.
- **The L1 figures are large** because they add every core's L1 together: hundreds of thousands
  of MiB/s is normal.
- **DRAMFull's fill takes longer with more memory**, before its 10 seconds start; it isn't in the
  figure.
- **In a TMR `.json` config**, an entry that names a bandwidth test takes the config's default
  extent, chunk and timing unless it sets its own; the plans give each test its tier's extent,
  one whole chunk and 10 seconds. A test's cycle here is one pass. To match a plan's, give the
  entry `"extent": {"mode": "cache", "target": "L2"}` (or `L1`, `L3`, `DRAM`, `DRAM-FULL`),
  `"chunk": {"mode": "whole"}` and `"duration_secs": 10`, and leave `default_test_cycles` out of
  the config's `timing`, or that many passes stop it first. A config can also name one width,
  `Spd-L2-Read-128`, to put widths side by side in one run; a width the CPU lacks is refused there
  as an unknown test.
- **Latency under load** (what bandwidth costs a read) is a different measurement: see
  [Lat-Loaded-DRAM and Lat-Turnaround-DRAM](loaded-latency.md).

Back to [TMR's tests](README.md).
