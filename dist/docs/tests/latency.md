# The latency tests

These tests time how long the CPU waits for memory: a read from each level of cache and from the
memory itself, in nanoseconds. They are measurements, not error tests: they check no data, so they
find no memory errors of their own. Use them to see what a memory setting (a timing, the
frequency) or a BIOS option does to latency, and to compare one machine with another.

This page covers 180 tests in four families, the unloaded latency tests: every thread of the run
chases at once, a light load, not idle. The two loaded tests, Lat-Loaded-DRAM and
Lat-Turnaround-DRAM, which time a read while the other cores keep the memory busy, are on
[their own page](loaded-latency.md).

## At a glance

| Family | Tests | What each step does | Plans |
|---|---|---|---|
| `Lat-<tier>-Read`, `-Write`, `-Copy` | 15 | a read that must finish before the next one's address is known; Write adds a store to a random line, Copy a store in order | perf (every Read; Write at DRAM and DRAMFull), all |
| `Lat-V2-<tier>-<op>-<width>` | 60 | the same read, each on its own cache line, plus one SIMD read, write or read-and-write of a random line | all |
| `Lat-V2P-<tier>-<op>-<width>` | 100 | the same with six random lines a step; WriteFull and CopyFull cover each line whole | all |
| `Lat-NTW-DRAM-Write-<width>` | 5 | 4,096 stores that skip the cache, then a fence | perf (Auto), all |

`<tier>` is L1, L2, L3, DRAM or DRAMFull. `<op>` is Read, Write or Copy, and for V2P also
WriteFull and CopyFull. `<width>` is 128, 256, 512 or Auto, and for NTW also Scalar.

## How they measure

Before its clock starts, a test builds a pointer chain in each thread's memory: every cell holds
the address of the next, in one random loop through all of them. Following it is a chase. Each read
must arrive before the CPU knows where the next one is, so the reads can't overlap, and no
prefetcher can guess the next address. The ticker shows "Building pointer chain" while it builds,
and the step report shows how long it took.

A **sample** is one timed run of 1,000 chained reads (fewer on a chain shorter than that) between
two reads of the CPU's time-stamp counter. Its value is the time per step. The next sample carries
on where the last one stopped, so the chase walks the whole chain. A step takes samples back to
back for 10 seconds.

Every thread of the run chases its own chain in its own memory, all at the same time. A
Lat-<tier>-Read thread has one read in flight, so at DRAM this is a light load, not an idle
machine: that is what unloaded means here. For one thread alone, use `--cache-latency` (below).

## The tiers

The tier sets each thread's extent: how much memory its chain covers, from the start of the
thread's memory.

| Tier | Each thread's extent | Shown in the plan as |
|---|---|---|
| L1 | half the L1 a thread has: a core's L1, split between the run's threads on that core | `L1/2` |
| L2 | half the L2 a thread has, the same way | `L2/2` |
| L3 | half the thread's share of its L3 (on a CPU with several L3s, its own group's), and never less than its L2 plus 64 KiB | `L3/6/2`, or `L3(6)/2 [cal]` |
| DRAM | four times one L3, whatever the thread count | `DRAM*4` |
| DRAMFull | the thread's whole memory | `DRAM-Full` |

**`[cal]`** means the size came from this machine's cache calibration in `tmr-cfg.json`, which the
first run on a machine measures (about 2 minutes). Calibrated, an L3 extent is half of what one
thread reaches with the run's threads in its group running: `L3(6)/2 [cal]` for 6 threads in a
group. Without a calibration the sizes come from CPUID's cache sizes, and in a virtual machine the
L3 share is halved again. `tmr --show-calibration` shows what is stored.

Each extent sits inside its tier with room to spare, so a tier's figure is mostly, not only, that
tier's. Part of an L3 chain still hits the L2, and part of a DRAM chain hits the L3. DRAMFull
covers the whole memory, so more of its reads also miss the TLB, the CPU's cache of page
addresses: on three desktops on 2026-11-03, on 1 GB and 2 MB pages, Lat-DRAMFull-Read was 2-4 ns
slower than Lat-DRAM-Read.

## Lat-L1 to Lat-DRAMFull: Read, Write and Copy

The usual latency figure: one dependent read at a time, as other latency tools measure it. The
chain runs through every 8-byte word of its memory. Their reads are addresses, so these tests have
no width.

- **Read**: the chase alone.
- **Write**: half the extent is the chain and half is the targets. Each step reads the chain, then
  writes a word at the matching place in the other half, so on a random line. A cached store needs
  its line in the cache first. The store doesn't hold up the chase while the core can hide it, so at
  the cache tiers Write reads within about 10% of Read. At DRAM a step can need two lines from
  memory, the chain's and the store's, and Write reads higher: 73 ns against Read's 65 on a Ryzen 5
  8600G (DDR5-6000), and 78 against 65 on a Ryzen 7 5700X (DDR4-3600), all threads running
  (2026-11-03).
- **Copy**: half the extent is the chain. Each value read is also stored in the other half, in
  order, one word a step, so the stores stream while the reads chase.

Write's and Copy's chains are half the size of Read's, so compare each test with itself, between
runs.

## Lat-V2 and Lat-V2P

These ask what other accesses beside the chase cost. Each chain cell has a cache line of its own,
and the data lines lie beside the cells. Each step reads the next cell, then accesses data lines at
random at the test's SIMD width. The figure is still the time per chain step.

- **Lat-V2**, one data line a step: Read loads it, Write stores to it, Copy loads it, changes it
  and stores it back. One access fits in the time the chase waits anyway: on an Intel Xeon
  6975P-C (AVX-512, 6 threads, 2026-05), Read, Write and Copy agreed within about 5% at every tier.
- **Lat-V2P**, six data lines a step, all independent: six accesses in flight beside the chain.
  This shows where the core can no longer hide its stores. On the same Xeon, at 512 bits:

  | Tier | Read | Write | Copy |
  |---|---:|---:|---:|
  | L1 | 2.9 ns | 3.2 ns | 4.1 ns |
  | L2 | 7.0 ns | 12.7 ns | 12.6 ns |
  | L3 | 42.2 ns | 53.8 ns | 40.9 ns |
  | DRAM | 166.7 ns | 168.6 ns | 167.4 ns |

  Writes cost most at L2, where the chase's wait is too short to cover six stores' lines, and
  nothing at DRAM, where the wait covers everything. At L1, Copy is slowest: each
  load-change-store waits for its own load.
- **WriteFull and CopyFull** (V2P only): Write and Copy touch 16, 32 or 64 bytes of each line,
  depending on the width. The Full tests cover the whole 64-byte line at every width (four 128-bit
  stores, two 256-bit, or one 512-bit), so the widths can be compared on the same bytes. On the
  Xeon, Lat-V2P-L3-CopyFull-128 read 56.7 ns and -512 40.9 ns: the same bytes, in four 128-bit
  changes a line against one 512-bit change.

## Lat-NTW-DRAM-Write

This one isn't a chain. Each sample issues 4,096 non-temporal (NT) stores, which skip the caches
and go to memory. Each store goes to the start of one of 4,096 random lines of a DRAM-sized extent
(the same lines in every sample). The sample then waits on a fence until the CPU has passed every
store on. Its figure is the time per store at the rate a core can push them out, with every
thread doing the same, not one store's delay.

Here the width matters a lot. A 512-bit store fills a whole 64-byte line; a narrower one leaves the
line partly written, which costs far more. On the Xeon (2026-05): Scalar (an 8-byte store, kept to
show this), 128 and 256 each took 25.6 ns, and 512 took 5.6 ns.

## Widths and Auto

The V2, V2P and NTW tests come at 128, 256 and 512 bits (SSE2, AVX2, AVX-512) and as Auto. Auto
runs the widest the CPU has, 512 with AVX-512 and 256 without, or the width `width=` gives. The
plan and the results show an Auto test under the width it ran at, as in
`Lat-V2-L2-Read-512_A`. A 512-bit test on a CPU without AVX-512, such as a Ryzen 7 5700X, is left
out of the run, which says so (`Left out N test(s) this CPU can't run`).

## Which plans run them, and for how long

- **`plan=perf`** (one cycle, 25 tests, about 5.5 min): Lat-L1-Read, Lat-L2-Read, Lat-L3-Read,
  Lat-DRAM-Read, Lat-DRAM-Write, Lat-DRAMFull-Read and Lat-DRAMFull-Write, then the two loaded
  tests, the 15 [bandwidth tests](bandwidth.md), and Lat-NTW-DRAM-Write-Auto last. It leaves out
  L1's, L2's and L3's Write and Copy, and DRAM's and DRAMFull's Copy: in the caches the chain's
  reads set the time and the stores hit the cache, so they read as the Read does; in DRAM the
  stores' line fetches add, and Copy adds nothing Write doesn't. They run by name.
- **`plan=all`**: every one of them. `stability`, `quick` and `widths` run none.
- **`tmr --cache-latency`**: the Read, Write and Copy tests of L1, L2, L3 and DRAM (12), once, on
  one thread, the core Windows ranks best.
- **`tmr --ram-latency`**: Lat-DRAM-* and Lat-DRAMFull-* (6), once, on half the CPUs.
- **By name**, with globs: `tmr test=Lat-V2P-L2-*`. A `test=` filter alone picks from every test.

Each step takes 10 s of samples plus its chain's build, which is quick except for DRAMFull's: it
builds over all of the thread's memory (on a 192-thread server with 2 GB a thread,
Lat-DRAMFull-Read's build took 9 s). The 180 tests under `plan=all` come to about half an hour a
cycle; fewer without AVX-512, where the 41 tests at 512 bits are left out.

## Reading the output

**The plan table** shows each test's Timing (`10s`) and its Extent with its tier (for example
`16.00 KiB (L1/2 [cal])`). In a sealed run its Seal column has `–` for them: these tests never
take the seal.

**The step report** (the console's, and the window's for the step):

- The first line has the verdict and the errors. Cycles counts samples, and a red `Time Limit` says
  the 10 s ended the step, as it should.
- `Test:` ends with `P50` and `Spread`, the step's median and spread over every thread's samples.
  Its MiB/s and ops/s count 8 bytes a chain step (16 for the Lat-<tier>-Copy tests, and 8 a store
  for NTW whatever its width): read them as steps per second, not bandwidth.
- `Intermediate:` gives the chain build's time; `Config:` the extent in bytes.
- Under the thread table, the 🔍 Latency table:

| Column | Meaning |
|---|---|
| Thread, CPU | the thread and the logical CPU it ran on; `ALL` is the step |
| Samples | samples taken |
| Min | the fastest sample |
| P5 to P99.9 | percentiles of the samples, in ns: P50 is the median, the figure to quote |
| Spread | P95 / P5 |

`ALL` is taken from every thread's samples together, not an average of the threads, so a thread
with more samples (a faster core) weighs more. On a CPU with two kinds of core, read the threads'
rows: on a Ryzen 5 8500G, its four Zen 4c cores set the `ALL` median.

A sample averages 1,000 reads, so one slow read hardly moves it: the slowest samples, at P99 and
P99.9, are stretches that ran slow as a whole, such as a thread interrupted by Windows. **Spread**
near 1.0x means steady samples. A larger one means two kinds of sample: an extent spilling out of
its tier part of the time, or a second hardware path (on the Xeon, Lat-V2P-L3-Copy reached
1.4-1.5x at 256 and 512 bits, and stayed near 1.2x at 128). The percentiles are within 0.05% of an
exact sort.

**The final summary**'s Per-Test Performance table adds Samples, P5 to P99.9 and Spread for each
latency test, averaged over the cycles.

**The results file** keeps, per cycle, each step's `latency_samples`, `latency_p5_ns` to
`latency_p99_9_ns` and `latency_spread` (in `cycles[].tests[]`), and each thread's `min_ns`,
`p50_ns`, `p99_ns` and `spread` (in the step's `threads[].latency`). `overall_stats.per_step_averages`
has their averages over the cycles. `tmr --compare-results` compares a step's time and MiB/s, not
its percentiles; a latency test's MiB/s rises as its latency falls.

**Errors and the seal.** These tests check no data, so their test errors are always 0. A step can
still fail: WHEA errors (hardware errors Windows reports) during it count against it. In a sealed
run, one with stability tests in it such as `plan=all`, the seal over the memory a latency step will
overwrite is checked before it runs, and what that finds is the step's Seal count: a fault in data
an earlier step left, not one the latency test found. `plan=perf` has no test that takes the seal,
so it runs unsealed.

## Settings that matter

- **`cpus=`, `cputype=`, `skip-cores=`**: how many threads chase at once, and whether two share a
  core, which halves the L1 and L2 extents. Compare runs only at the same ones.
- **`width=`**: the width of the Auto tests.
- **`calibrate=`**: `off` sizes the extents from CPUID; `recheck` probes the stored calibration
  first and measures it again if the cache has changed.
- **`memory=`**: how much memory each thread has, which is the DRAMFull extent.
- **`minpage=`, `maxpage=`**: the page size, which moves the DRAM and DRAMFull figures. Compare
  runs on the same page size.
- **A TMR `.json` config** can name any of these tests in its `test_sequence`. Give each its own
  extent and length, or it takes the config's defaults: a default extent of the whole memory turns
  Lat-L1-Read into a DRAMFull chase, and as a sample counts as a cycle, a `default_test_cycles` of
  1 (as in TMR's demo configs) stops it after one sample. With both `duration_secs` and
  `min_duration_secs` it runs that long whatever the defaults say:

  ```json
  { "enabled": true, "function": "Lat-L2-Read",
    "extent": { "mode": "cache", "target": "L2*0.8" }, "chunk": { "mode": "whole" },
    "duration_secs": 10, "min_duration_secs": 10 }
  ```

## Caveats

- **Like for like.** A figure compares only between runs with the same threads, page size and
  extents (the plan table shows the extents). In three runs of `plan=perf` on each of four
  machines (2026-11-03), the typical test's figures varied 0.05-0.26% from run to run, and 1.1% at
  worst.
- **Unloaded, not idle.** Every thread chases at once. For the single-thread figure other tools
  give, run `--cache-latency`, or `cpus=1`.
- **Auto across machines.** Lat-NTW-DRAM-Write-Auto runs at 512 bits on a CPU with AVX-512 and at
  256 without, which take different paths: on 2026-11-03 it ran at 512 on a Ryzen 5 8600G (5.8 ns)
  and at 256 on a Ryzen 7 5700X (21.3 ns). Compare it only between runs at the same width; its name
  in the plan says which (`-512_A`).
- **`test=` and Auto.** `test=Lat-V2-L2-Read-256` picks the -256 test and also the -Auto one
  wherever Auto runs at 256, as it does on a CPU without AVX-512.
- **Not a stability test.** An unstable overclock can still crash during them, but a clean latency
  step says nothing about stability: the stability plan is for that.

Back to [TMR's tests](README.md).
