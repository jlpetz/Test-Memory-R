# TODO 22: calibration under load, as a run's first step

## The problem

The cache calibration measures one thread. An L3 test then divides what that thread reached by
the threads sharing it. That misses what N threads do to each other in the L3: replacement
pressure, prefetchers, and on a VM the neighbours. On the dev box's EC2 VM, CPUID reports the
socket's 480 MB L3; one thread reaches 64-71 MB of it.

Until 18c17ff (2026-11-04), the divisor was every thread of the run, but a single thread reaches
one L3 group's L3. On the m8a.48xlarge (24 groups of 32 MB) that gave 32 MB / 192 per thread,
smaller than L2. It is now the most threads the run puts in one group
(`cpu_selection::Choice::threads_per_l3`, `tests::l3_sharers`), calibrated or by CPUID.

## Standard against extended, on the dev box (2026-11-04)

Three of each, alternated, on the release exe of 65fabbe (r8i.2xlarge, 4 cores and 8 threads,
Xeon 6975P-C, 2 MB pages):

| Run | L1 | L2 | L3 | Time |
|---|---|---|---|---|
| standard 1 | 49 KB | 1.6 MB | 34.1 MB | 97 s |
| standard 2 | 49 KB | 1.6 MB | 45.1 MB | 67 s |
| standard 3 | 49 KB | 1.6 MB | 39.2 MB | 71 s |
| extended 1 | 48 KB | 1.8 MB | 70.8 MB | 107 s |
| extended 2 | 48 KB | 1.8 MB | 64.2 MB | 97 s |
| extended 3 | 48 KB | 1.8 MB | 64.2 MB | 102 s |

- The L3's latency climbs from 35 to 70 ns before DRAM's ~115 ns, with no clean step.
- The standard calibration takes the largest probe of the coarse (15%) sweep whose window is
  steady, so it stops early, at a size that varies by a third.
- The extended calibration sweeps 5% steps, three repeats each, in [edge / 2, edge]. It agrees
  with itself within 10%, for 10-30 s more.
- L1 and L2 match between the two.
- DRAM and FullDRAM both came out 1.7 GB. The DRAM tier's range starts at CPUID's total cache
  (about 490 MB here), which is above twice the measured edge, so it falls back to the largest
  stable probe.

## Decided (the owner, 2026-11-04)

- The calibration becomes a setup step of a run, on the run's own allocation, threads, pinning
  and pages, before the plan's sizes are fixed. `calibrate=auto|always|off`; `auto` uses a stored
  one, else it measures. `--calibrate-cache` stays, as a run that only calibrates. This answers
  CLAUDE.md's open question 1.
- TODO 110 (the window) builds on it.

## Built (the owner's go, 2026-11-04)

- **One calibration**, the extended one (6ba08e8): `--calibrate-cache-ext` and `calibration_extended`
  are gone.
- **Anchors B and C** (ff518fb), after A, each thread pinned and chasing its own block, all
  starting together at a barrier. Their chains have one node a cache line with u32 indices, a
  sixteenth of a block in heap. B sweeps a core's SMT pair over L1 and L2. C sweeps every logical
  CPU from just past a thread's L2 to twice CPUID's L3 share.
- **C's edge** is the first 1.4x step past the L3's own latency. With every CPU running, the step
  into L3 comes first (4 to 34 ns at 1.4 MB here). Its size is the largest steady one still within
  1.25x of A's L3 latency: the finer sweep's top was steady at 82 ns, half way to DRAM.
- **Sizing.** A run's L3 reach is the curve through A and C, reach(k) = own + split / k. An L3 test
  takes `scale` of it, never inside the thread's L2 share + 64 KiB. L1 and L2 tests with two
  threads a core take B's.
- **A run's first step** (0e8574f): `calibrate=auto` measures on the run's largest span after
  allocation when none with B and C is stored, then saves it. `always` measures every run, `off`
  takes none (`--no-calibration` is gone).

### Measured on the dev box

| Run | A's L3 | B: L1, L2 a thread | C: L3 a thread, 8 sharing | Time |
|---|---|---|---|---|
| 3 | 67.8 MB | 25 KB, 887 KB | 16.9 MB at 43 ns | 116 s |
| 4 | 55.8 MB | 19 KB, 887 KB | 14.0 MB at 39 ns | 129 s |
| in a run | 59.0 MB | 25 KB, 887 KB | 15.4 MB | 147 s |

The L3 doesn't split evenly. 8 threads hold about 15 MB each, 120-135 MB in all, about twice what
one thread reaches. So one thread's reach divided by the threads undersizes a run's share. The
curve's own part (about 8-12 MB here) takes this up. A 4-thread `cputype=cores` run sized its L3
tests at 10.82 MiB a thread (`L3/4/2 [cal]`), and Lat-L3 read about 28 ns there: about 17% L2
hits, the rest L3.

## The curve checked (2026-11-05, `--calibrate-curve`, 13e7d2c)

The L3 a thread reaches with k threads in each L3 group running, measured by C's method, against the
curve through A and C. "Off" is the curve against the measurement: negative, the curve gives a test
less than fits, the safe side. The outputs are in `baselines/2026-11-05-curve/` (outside git).

| k | dev box (VM), one a core / both | petz007 (8600G) | daddy-petz (5700X) |
|---|---|---|---|
| 1 | -8% | +16% | +5% |
| 2 | -3% / -1% | +9% / +4% | -14% / -4% |
| 3 | -19% | -7% | -9% |
| 4 | -19% / -7% | -2% / -19% | -2% / -6% |
| 5-8 | / -5%, -13% | -9%, -8% / -21%, +0% | -16%, -35%*, -8%, -8% / -7%, -7% |
| 10-16 | | / +5%, +4% | / -4%, -4%, -29%*, +0% |

\* Each larger than the next count down: a single point scatters by 15-30% (C itself read 16.1 MB
then 18.5 MB on the dev box in one session).

- Every CPU together holds the L3 and the L2s, as Zen's victim L3 should. The 8600G: 12 x 1.8 MB =
  22 MB (16 MB L3, 6 MB L2). The 5700X: 16 x 1.9 MB = 30 MB (32 MB L3, 4 MB L2). The VM holds
  148 MB with 8, where one thread reaches 68 MB.
- The curve stays within about 20% at every count on the three hosts, mostly low. Its errors are
  the measurement's own scatter, so more anchors wouldn't buy accuracy.
- The largest overestimates are +16% and +9% (the 8600G at 1 and 2 threads), where an L3 test takes
  half the reach.
- One thread a core (`cputype=cores`) is the one place it errs low consistently (-19% on the VM),
  because C runs both threads of each core. An anchor D, one thread a core on every core, would
  fix it for about 13 s more. Left out: it is the safe side, and within the scatter on the
  desktops.
- Decided (2026-11-05): the two-point curve stays. Next, a recheck (TODO 116), and a final run on
  the metal box (several L3 groups and NUMA nodes).

## C confirms its size, and the recheck (2026-11-05)

On daddy-petz a run's C took 2.6 MB a thread with 16 sharing: 42 MB, more than the chip's 32 MB
L3 and 4 MB of L2s hold. Its finer sweep's three probes at that size had all read steady. The next
run's recheck (`calibrate=recheck`, TODO 116) read it at 31.6 ns, past the L3, and measured again:
1.9 MB.

Since 06dbdae, C probes the size it would take three times more before taking it. The probes must
read within 1.5x A's latency, as a recheck reads it; otherwise C tries the next smaller size. Three
calibrations on each desktop after that:

| Host | A, one thread | C, every CPU, a thread | Note |
|---|---|---|---|
| petz007 | 11.4, 11.4, 11.4 MB | 1.9, 1.8, 1.7 MB | |
| daddy-petz | 21.0, 21.0, 21.0 MB | 2.4, 1.9, 1.9 MB | run 1 rejected 2.8 MB (31.0 ns) |

The recheck took 5.3 s on petz007, 6.8 s on daddy-petz and 13.9 s on the dev box. Most of that is
A's three probes at each size, up to 2 s each.

## One NUMA node: the Intel metal box (2026-11-05, 0a8467d)

No m8a.metal-48xl capacity all day, so the metal box ran as an m8i.metal-48xl: the dev box's Xeon
6975P-C, 96 cores / 192 threads, SNC3 (3 NUMA nodes, 3 L3 groups of 64 CPUs, 160 MB of L3 each;
CPUID says 480 MB, the package's). 99f4337's in-run calibration took the run's largest span, but
every thread's span was 3.6 GB, so the pick fell to a hash map's order: any thread's, on any node.
A stays on the top-ranked core, on node 0. With SNC a node's memory is cached in that node's L3
slices alone, so:

| Run | A's span | A's L3 | Saved |
|---|---|---|---|
| cal_measure | local | 156.9 MB at 36.8 ns, DRAM 125 ns | right (160 MB a node) |
| cal_recheck_1 | remote | flat at 54-57 ns to 138 MB, then a gradual climb to 130 ns | 475 MB at 110 ns |

On remote memory no step from L3 to DRAM reached the edge's 1.4x over the three before it (at most
1.34x), so the sweep fell back to CPUID's 480 MB, and the finer sweep there took its largest steady
size, deep in DRAM. The next recheck held it: it compares against the stored latency. C ran all
192 CPUs on one node's memory: one node's L3 shared by 192, not 64.

The x105 runs that followed sized their L3 tests from that 475 MB: 7.42 MiB a thread, 237 MB a
group. With one thread a core each also has its core's 2 MiB L2, so a group holds 160 MB of L3 and
64 MB of L2s, and that mostly fit: Spd-L3 read 1,248-1,263 GB/s (DRAM reads 574 at most), the
fixed calibration's 3.11-3.20 MiB 1,193-1,213, and a5de854's 2.5 MiB (CPUID's 480 MB over all 96
threads, not a group's 32) 1,555. Half a thread's reach (`scale`) is mostly L2 on a part like this:
2 of the 3.2 MiB.

Since 0a8467d the calibration keeps to A's node: B and C take its CPUs alone (as the owner first
put C: "full node load"), the in-run calibration and the recheck the largest span wholly on it, and
`--calibrate-cache` asks for its buffer there, strictly for large pages. On one node nothing
changes. The review after it: a span counts as the node's when its thread is, and the calibration
takes the one with the most from its start on the node, those bytes alone (wholly on it fell to any
node's span when the node was short of large pages); with no span on the node (`cpus=`) the run
neither measures nor rechecks, and says so; and the buffer's large pages come from any node before
4 KiB when the node has none (a 2990WX in NUMA mode, a vNUMA VM).

Left: A's edge test misses a gradual climb. On local memory the step was sharp enough, but a
fallback to CPUID's L3 is the package's on an SNC part; a cap on the latency the finer sweep may
take (as C's 1.25x of A's) would stop it taking DRAM sizes.

## Notes (TODO 116 has them)

- The coarse sweep runs to 4x CPUID's L3, 1.9 GB here, and most of its time goes on the DRAM
  probes. A flushed chase would give DRAM's latency in milliseconds, so the sweep could stop a
  few probes past the L3's edge.
- A recheck mode: a few probes either side of each stored edge.
- `auto` doesn't key the stored calibration by page size.
- On a VM the neighbours change what the L3 gives, so `auto` might re-measure there each run.
