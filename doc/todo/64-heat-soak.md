# TODO 64: heat soak and saturation (is the test actually heating DRAM?)

Raised 2026-08-20. The entry in `../TODO.md` holds the status; this file holds the claim, the figures and the history.

## The claim to verify
TMR's tests stress *memory* hard enough to expose thermally-induced instability, not merely load the CPU. Marginal DDR5 often fails
only once the DIMMs are hot, which is why sustained-load testing finds errors that short runs miss.

## Work items
1. A sustained max-bandwidth preset (ties to TODO 25's plans): physical cores only, extent far past L3, NT or flush-based so the
   cache can't absorb the traffic, run for tens of minutes.
2. Report the achieved aggregate GB/s, so a user can see DRAM is saturated. **Met** (the step reports and the results file).
3. Cross-check against a reference tool once (Intel MLC, AIDA64; OCCT, which is free, in practice), to confirm TMR's figure is real
   bandwidth and not a harness artefact.
4. Watch for the inverse failure: a test that pegs the cores but leaves DRAM idle (achieved GB/s far below the DRAM ceiling).

Out of scope when raised: reading DIMM thermal sensors (SMBus/SPD access needs a driver or vendor tooling, under the no-deep-hardware
rule). The owner's point (2026-11-04): without temperatures, TMR can't tell when a soak is hot, so item 1 can't be judged on bandwidth
alone.

## History: the shuffle benchmark (TODO 65, 2026-08)
On the dev VM, memory saturated at one thread per physical core: 4 threads peaked, and 6 measured about 13% worse (NT-512 41,518 to
36,177 MiB/s), recovering only partly at 8. **Not reproduced on hardware (2026-11-03, below)**: there, SMT siblings moved bandwidth by
-4.6% to +2.8%. The conclusion stands in its weaker form: past saturation, more threads aren't more DRAM pressure, so a preset should
take physical cores.

## The 2026-11-03 campaign (build a5de854; figures in `..\baselines\2026-11-03\REPORT.md`, beside the repo, not in git)
- **The soak, the 8500G (Ryzen 5 8500G, DDR5):** plan=stability, 12 cycles, 49 min (soak2; the first soak was cut at 11 min when the
  machine was shut down at the desk). Every step held within 2% from the first cycle to the last, and stayed flat after 1 h 52 min of
  load in total. It started cold (about 9 min after a restart).
- **How hard stability loads DRAM:** 0.84 of the read ceiling (Spd-DRAMFull-Read) over the soak.
- **Item 4, the steps that leave DRAM partly idle** (fraction of the read ceiling): Random 0.10-0.13, Refresh 0.13 (DDR5), ModuloX
  0.44-0.64, Stride 0.42-0.53. **By design (the owner, 2026-11-04):** Random waits on each dependent access, Refresh waits for the
  refresh interval on purpose, and ModuloX and Stride touch only part of the memory each pass, as TM5's tests do. The soak shows no
  cooling from them.
- **Cores against threads (r4, A/B x 3):** SMT siblings -4.6% to +2.8%; one outlier, r4_b3, +6-10%, not rerun.

## Item 3: OCCT on the dev box (2026-11-04, the owner)
The dev box is an AWS r8i.2xlarge (Intel, 4 cores and 8 threads, 64 GB, one module reported). OCCT's thread count isn't known.

| Figure | OCCT | TMR, the last dev-box run (2026-10-01, an older build, `--quick-test`, 4 threads) | TMR / OCCT |
|---|---|---|---|
| L1 read | 1215.49 GiB/s | 752.6 | 0.62x |
| L1 write | 938.69 GiB/s | 492.9 | 0.53x |
| L2 read | 604.08 GiB/s | 704.5 | 1.17x |
| L2 write | 237.19 GiB/s | 234.5 | 0.99x |
| Memory read | 64.81 GiB/s | 58.0 (Spd-DRAMFull-Read) | 0.90x |
| Memory write | 97.15 GiB/s | 87.6 (Spd-DRAMFull-Write) | 0.90x |
| Latency L1 / L2 / L3 / memory | 4.13 / 24.88 / 174.76 / 192.54 ns | calibration 2026-11-04: 1.4 / 4.1 / 36-37 / 138 ns | |

- TMR falls short at L1 only, which the Spd kernels' two clock reads a pass explain (TODO 105 (2)). DRAM is within 10%, both ways.
- OCCT's latencies sit far above TMR's calibration at every tier, and its L3 latency is near its memory latency: its L3 probe likely
  missed this VM's effective L3 (calibration finds about 70-90 MB of the 480 MB the CPU reports). It isn't a like-for-like latency.
- Left: a current TMR `plan=perf` on the dev box beside OCCT, and OCCT's thread count.
