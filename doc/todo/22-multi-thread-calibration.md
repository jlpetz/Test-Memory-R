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

## Proposed, waiting on the owner

- **One calibration:** today's extended one (the sweep, then the fine sweeps), so there is no
  separate `--calibrate-cache-ext` and no `calibration_extended`.
- **Three anchors, the owner's**, stored per machine and page size:
  - **A**, one thread: L1, L2, the L3 reach and the latencies.
  - **B**, two SMT siblings on one core, only with SMT: L1 and L2 a thread.
  - **C**, every logical CPU at once, each chasing its own block: the L3 a thread under full load,
    in every group, which is the VM's or the node's soak.
- **A run's L3 share** comes from A and C, as reach(k) = private + shared / k, with k the threads
  per group. Two points fix both terms; the private part is the thread's own L2, which it keeps
  however many threads share the L3.

## Notes

- The coarse sweep runs to 4x CPUID's L3, 1.9 GB here; most of its time is the DRAM probes. It
  could stop a few probes past the L3 edge.
- On a VM the neighbours change what the L3 gives, so `auto` might re-measure there each run.
