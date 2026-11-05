# Latency under load: Lat-Loaded-DRAM and Lat-Turnaround-DRAM

These two tests time a read from memory while the other cores keep the memory busy. TMR's other
latency tests, the [unloaded ones](latency.md), time a read while every thread chases its own
chain, one read at a time each: a light load, not idle, and the kind of figure other tools report.
That figure doesn't move with the DRAM's write timings. These two do. They are measurements, for
tuning: they check no data, so they find no memory errors of their own.

- **Lat-Loaded-DRAM** (about 10 s) times a read in three cases: idle, while the other cores write
  to memory as fast as they can, and while they read as fast as they can.
- **Lat-Turnaround-DRAM** (about 50 s) does the same. Then it holds the other cores to 25%, 50% and
  75% of the lower of those two speeds, writing and then reading at the same speed. Its
  **turnaround** at each level is the read's latency under the writes less its latency under the
  reads.

The [user guide](../guide.md)'s "Latency under load" has the short version.

## What they're for

A memory controller reports a write as done as soon as the write is in its queue. A read of a line
that was just written is answered from that queue. So a DRAM write timing reaches an app's read
only when writes keep the DRAM's banks busy. The probe these tests were built from showed this on a
Ryzen 7 5700X at DDR4-3600, one timing changed at a time:

| Change | What moved | What didn't |
|---|---|---|
| tWR 12 to 24 | a read's latency under a flat-out write load: +6.7 ns (see below) | everything else |
| tRDWR 8 to 12 | the turnaround: +12 ns at 50%, +24 ns at 75% | everything else, the flat-out rows too |

The tWR figure is the less certain of the two. On that machine the flat-out write row can sit in a
slower state for a while, mostly in the first run after a boot (175 ns settled, about 207 ns in the
slow state), and both tWR readings came from a first run after a reboot. The tRDWR rise is larger and
the matched-load rows don't flip. A re-test with these tests themselves, setting aside the first run
after each boot, is planned.

Neither change moved a figure taken without that load: not the probe's idle read or write
latency, and not TMR's unloaded latency tests run beside them. So Lat-Loaded-DRAM's flat-out write
row is where write recovery (tWR) shows up, and Lat-Turnaround-DRAM's turnaround is where switching
the bus between writes and reads shows up (tRDWR, tWTR, tWRRD, and the controller draining its
write queue).

## How they work

**Who does what.** The run's first thread, on the core Windows ranks highest, measures. One thread
on each other physical core in its NUMA node loads the memory. Every other thread sits out: the
measuring core's SMT sibling, the loaders' siblings, and every thread on other nodes. The measuring
core has no busy sibling, and other nodes' memory stays quiet. So:

- `cputype=` doesn't change the load: with `cputype=threads` the extra threads sit out.
- `cpus=` and `skip-cores=` do: they set how many cores are in the run, and so how many load. The
  report shows the count. Compare runs only at the same settings.
- With no other core on the measuring core's node (`cpus=1`, say), it measures idle only, and says
  so.

**The measurer** follows a chain: each read's address is the value the last read returned, so each
read waits for the one before. The chain visits 128-byte cells in random order over 32 times the
CPU's L3 (1 GiB at least; 3 GiB on an X3D), so neither the L3 nor the prefetchers can help. It
times 1,000 reads at a time with the CPU's timestamp counter.

**The loaders** each work in their own memory, the same size as the chain, 4 KiB at a time. They
write with NT (non-temporal) stores, which go to memory without first reading each line in as an
ordinary store does, or they read. They use the CPU's widest SIMD width, as the Auto tests do
(`width=` sets it). Each window, each loader starts at a new random block, so every window is a
fresh instance of the load.

**Windows.** A step is a fixed schedule of windows. Each is an untimed 0.2 s settle, while the
loaders reach their new load, then 1 s timed. A window's figures are its mean latency and the
loaders' MiB/s over exactly that second.

| Windows | Lat-Loaded-DRAM | Lat-Turnaround-DRAM |
|---|---|---|
| Idle | 1 | 1 |
| Flat out, write/read pairs | 3 pairs | 5 pairs |
| At 25%, 50% and 75%, write/read pairs | none | 5 pairs at each |
| Idle | 1 | 1 |
| In all | 8 windows, about 10 s | 42 windows, about 50 s |

In an idle window the loaders spin on their cores without touching memory, so the clocks stay where
they are under load. The pairs alternate their order (writes then reads, then reads then writes,
and so on), so a slow drift, such as the memory warming up, cancels out.

**The levels.** Lat-Turnaround-DRAM takes its levels from its own flat-out pairs: shares of the
lower of the two flat-out speeds (usually the writes'). Each loader is held to an equal share by a
clock deadline for each 4 KiB block, staggered so the load is smooth. A loader that falls more than
a block behind doesn't rush to catch up: the shortfall shows in the MiB/s reached.

Before the clock starts, the loaders fill their memory once and the measurer builds its chain
(about 0.3 s each on the 5700X).

## Which plans run them

`plan=perf` (one cycle) runs both, after the [unloaded latency tests](latency.md) and before the
[bandwidth tests](bandwidth.md); they add about a minute to it. `plan=all` runs them with every
other test. On their own:

```
tmr test=Lat-Loaded-DRAM,Lat-Turnaround-DRAM
```

A `test=` run does one cycle unless you give `cycles=` or `duration=`, and each cycle runs each
schedule once. The other plans (`stability`, `quick`, `widths`) don't run them. Neither test has
width variants. In the window, type the same settings in its box.

## Reading the report

A Lat-Turnaround-DRAM step on the 5700X at DDR4-3600 (the thread table and the rules left out):

```
📊 Test report - Cycle 1 - Step 2 Lat-Turnaround-DRAM: ✅ 50.7s, 0 errors (0 test + 0 WHEA), Cycles 🔴 1/1, Time Limit 🟢
   Test: 50.4s, 891.69 GiB @ 18115 MiB/s, 120.14B ops @ 2.38B ops/s, Turnaround +16.0 ns at 20326 MiB/s
   Intermediate: seal fill n/a; seal check n/a; pattern fill 0.3s, 7.00 GiB @ 24575 MiB/s; chain build 0.3s
   Config: parameter=-, mode=-, extent=1.00 GiB, chunk=Whole
🔍 Under load: thread 0 (CPU 2, core 1, node 0) measures; 7 threads load, one a core
   1.00 GiB chain in 128 B cells; 256-bit loaders in 4 KiB blocks; 1.0 s windows after 0.2 s settles; median of window means [lowest-highest]
   Levels 25%, 50%, 75% of 27101 MiB/s (the lower flat-out peak): 6775, 13551, 20326 MiB/s; pin them with load-levels=6775,13551,20326
Load                    Under NT writes       MiB/s             Under reads       MiB/s                Turnaround
idle                66.4 ns [66.0-66.7]
25% 6775 MiB/s      72.4 ns [72.3-72.4]        6712     69.5 ns [69.4-69.7]        6704     +2.9 ns [+2.7 - +3.0]
50% 13551 MiB/s     80.9 ns [80.7-80.9]       13369     73.4 ns [73.4-73.5]       13337     +7.5 ns [+7.3 - +7.5]
75% 20326 MiB/s     94.3 ns [93.7-94.4]       20015     78.1 ns [78.1-78.2]       19967  +16.0 ns [+15.6 - +16.2]
full             176.3 ns [174.4-206.3]       27101  309.1 ns [306.1-315.3]       45227
Turnaround: under NT writes less under reads, at the same MiB/s; negative when reads cost a read more than writes did
```

- **The step line:** time and errors. The 🔴 marks the limit that ended the step, its one cycle.
- **Test:** the loaders' data and speed over the whole step. That speed mixes idle, flat-out and
  held windows, so it isn't a bandwidth figure. Then the headline: Lat-Loaded-DRAM's flat-out row
  (`Under load: writes 174.3 ns @ 27106 MiB/s, reads 308.5 ns @ 45256 MiB/s`), or
  Lat-Turnaround-DRAM's turnaround at the highest level both kinds of load matched (`Turnaround not
  matched at any level` if none did).
- **Intermediate:** the loaders' fill and the chain's build. **Config:** `parameter=` shows the
  load settings you gave (`Levels(...)`, `Pairs(N)`, `Window(250ms)`), `-` at the defaults.
- **The thread table** lists the loaders only: how evenly they shared the load.
- **Under load:** which thread measured (CPU, core, node), how many loaded and how many sat out;
  the chain, the loaders' width, the window and settle; and Lat-Turnaround-DRAM's levels, with the
  `load-levels=` that pins them.

**The table** has a row for each load. `idle` is the two idle windows, its one figure in the first
column. A level's row starts with its share (unless it was pinned) and the MiB/s it was held to.
`full` is flat out. Each latency cell is the median of its windows' means, with the lowest and
highest window in brackets. The MiB/s beside it is the median the loaders actually reached in those
windows. A change is real when it clears the brackets.

The **Turnaround** cell is the row's writes figure less its reads figure, taken from its pairs in a
way that cancels drift, with the lowest and highest pair in brackets. It reads `not matched` when
either kind of load ended up more than 3% off its level. The line under the table explains the sign.

Don't compare the `full` row's two columns with each other: they run at different speeds. On the
5700X, reads flat out moved 45 GB/s and writes only 27 GB/s, so the reads column looks worse. The
level rows are the ones that compare writes and reads at the same speed.

**Warnings** above the table:

- `The chain is under 16 L3s (...)`: the thread's memory was too small for the whole chain (many
  threads, a small `memory=`), so some reads may hit the L3 and read low. Give each thread more.
- `Stopped before its schedule ended: the rows are its first N windows'`: a stop (Ctrl+C) came
  during the step. A level it never reached has no row.

**Errors and the seal.** These tests check no data, so their `test` errors are always 0. A WHEA
memory error during the step still counts and fails it. On their own, or in `plan=perf`, no step
takes the seal, so the run is unsealed. In a sealed run (`plan=all`), the seal checks the memory
before the step overwrites it, and what it finds is the step's `seal` errors.

**In the window**, a step's row opens the same report. The live chart follows the loaders: it rises
and falls as the windows change, every 1.2 s or so (it plots the speed over the last 3 s, which
smooths the steps), drops for the levels, and through an idle window holds its last figure, as the
parked loaders report nothing. That is by design.

## Reading the turnaround

Any traffic takes up bus time, and a read waits its turn behind it. Holding writes and reads to the
same MiB/s takes that wait out of the comparison. What's left is the cost of switching between
writes and reads, beyond the bus time the traffic itself takes.

- **Positive:** writes cost a read more than the same MiB/s of reads does. On the 5700X (Zen 3,
  DDR4, two DIMMs a channel) it was +16 ns at 75%.
- **Negative:** on that controller, reads queued ahead of a read cost it more than writes do. On a
  Ryzen 5 8600G (Zen 4, DDR5) it was -18 ns at 75%.
- Its size grows with the load, so compare levels only at the same MiB/s.

The sign and size depend on the memory controller, so compare a machine with itself, not with
another. For tuning, change one timing and watch how the turnaround moves, at the same pinned
levels.

It isn't the DRAM's own turnaround timings in ns: those are set in clocks, and ZenTimings shows
them. TMR measures what they cost an app's read, end to end.

## Figures from three systems

Three runs each, at `memory=50% cputype=cores cpus=100% skip-cores=0`:

| System | Idle | Under NT writes, flat out | Under reads, flat out | Turnaround at 25 / 50 / 75% |
|---|---|---|---|---|
| Ryzen 7 5700X, DDR4-3600, two DIMMs a channel | 66 ns | 174 ns | 308 ns | +2.8 / +7.6 / +15.8 ns |
| Ryzen 5 8600G, DDR5-6000 | 67 ns | 123 ns | 334 ns | -0.7 / -4.4 / -17.6 ns |
| Xeon 6975P-C, one node, 4 channels of DDR5-7200 | 130 ns | 1,640 ns | 427 ns | +4 / +160 / +478 ns |

The three runs' turnarounds agreed within 1 ns on the Ryzens and within 12 ns on the Xeon, where
writes flat out push a read past 1.6 µs. The Xeon's memory per thread capped its chain at 4 GiB,
and its report warned.

## Settings

| Setting | Default | What it does |
|---|---|---|
| `load-levels=` | `25%,50%,75%` | Lat-Turnaround-DRAM's levels, 1 to 8 of them. Each is a share of the lower flat-out peak (more than 0%, at most 100%) or a fixed MiB/s (1 to 1,000,000). You can mix them: `25%,50%,8058`. |
| `load-pairs=` | 3 for Lat-Loaded-DRAM, 5 for Lat-Turnaround-DRAM | Write/read pairs at each load, 1 to 50 |
| `load-window-ms=` | `1000` | Each window's timed length, 10 to 60000 ms. The settle stays 0.2 s. |

A step has 2 + 2 × pairs × (levels + 1) windows, each the window plus 0.2 s. Lat-Loaded-DRAM has
no levels and ignores `load-levels=`. In a TMR `.json` config the same three are `load_levels` (a
string, `"25%,50%,8058"`, on Lat-Turnaround-DRAM only), `load_pairs` and `load_window_ms`. An entry
for either test keeps the test's own memory size and length, and refuses an `extent`, `chunk`,
`cycles` or duration of its own.

## Comparing two settings

1. **Run each setting about three times** (three runs, or `cycles=3`) and compare the medians. A
   flat-out write row can sit in a slower controller state for a while: in one 5700X run, flat-out
   writes began at 207.8 ns, then held 174 (the next step's range, [174.4-206.3], shows the state
   ending).
2. **Pin the levels.** A share is of each run's own peak, which a faster setting raises: an
   overclock moved the 5700X's 50% from 8.4 to 14.1 GB/s. Give the second run the `load-levels=`
   the first one's report prints, so both load at the same MiB/s.
3. **Keep the rest the same:** `cpus=` and `skip-cores=` (the loader count), and the page size. On
   4 KiB pages the read load pushes the page tables out of the cache and NT writes don't, which
   skews the turnaround; such a run doesn't compare with one on large pages.

`--compare-results` doesn't compare these tests' rows yet: the speed it compares for them is the
Test: line's mixed one. Compare the step reports, or the results files' `loaded` blocks.

## Caveats

- **One node.** Only the measuring core's NUMA node loads: on a multi-node system the figures are
  that node's.
- **The fabric.** On the 5700X the flat-out write speed is the Infinity Fabric's write link's (16
  bytes a clock at FCLK), so the flat-out write row follows FCLK as well as the DRAM timings. Under
  reads, loaders on the measuring core's CCD share its link and its L3. The report can't tell
  fabric from DRAM.
- **Slow cores.** Each loader is held to an equal share. One that can't keep up, such as a hybrid
  CPU's smaller core, leaves the level short, and it reads `not matched`.

## The results file

In `results\`, each of these steps has a `loaded` block in place of the other latency tests'
`latency_*` figures, and each thread's entry has its `role` (`measures`, `loads` or `sits out`):

- `measurer` (thread, CPU, core, node), `loaders`, `sat_out`, and the setup (`chain_bytes`,
  `width`, `window_ms`, `settle_ms` and the like).
- `levels`: as given, the peak they are a share of (`base_mib_s`), and each one's MiB/s.
- `rows`: the table, each cell with its samples' P50 and P99 too (`p50_ns`, `p99_ns`), which the
  console doesn't show, and each turnaround with every pair's difference.
- `windows`: every window: its kind (`idle`, `writes`, `reads`), level and pair, the MiB/s it was
  held to, its mean latency, the MiB/s reached, its reads, and `fell_behind`, the loaders' 4 KiB
  blocks that came more than a block late.
- `partial`: a stop cut the schedule short.

Back to [TMR's tests](README.md).
