# TODO 117: write latency, latency under load and write turnaround

The owner's question (2026-11-04): does anything TMR measures track DRAM's write timings? TMR measures
what an application sees, end to end; ZenTimings shows the DRAM's side. The question is which
end-to-end figure moves with a DRAM write timing.

## The probe (`../write-latency-test/`, not in git)

One thread pinned on CPU 2 over a 2 GiB buffer on large pages, random line order, each measure a
chain (each step waits on the last), the median of 7 samples, each sample at its own place in the
order. Versions, each a fix of the one before:

| Version | sha256 prefix | What changed |
|---|---|---|
| b648 | b64849a706999fc8 | samples in their own lines (the first reused 400,000, which sat in L3), MFENCE in the read-backs (SFENCE doesn't order loads), helper bandwidth inside the window only |
| v2 | f5b7553e02ab3c0c | helpers fill their buffers and count every MiB (256 MiB passes read 110 GB/s of writes on DDR5-6000, 96 at most); a helpers-alone row |
| v3 | 35ce21234f95f429 | loaded rows with 2M-read windows (0.3 s ones read 390 and 448 ns back to back); helpers held to 25/50/75% of their lesser peak, writing and reading at the same GB/s |

The rows: `R` (a read), `W-nt+sf` (an NT write + SFENCE), `W-fl` (a write + CLFLUSHOPT + SFENCE),
`W-clwb`, `RAW-nt` (NT-write the next pointer, MFENCE, read it back), `RAW-fl`, and reads with the
other cores (one thread a core) streaming NT writes or reads, flat out or held to a rate.

## daddy-petz, three settings (Ryzen 7 5700X, 4 x 16 GB DR DDR4, 2 DIMMs a channel)

A BIOS reset left it at DDR4-2133 CL15 2T, FCLK 1067 (snapshot 13:31). The owner's overclock is
3600 CL14-14-14-28 1T GDM, FCLK 1800 (every timing 45-55% shorter in ns, except tWRRD: 0.9 to 2.2
ns). Then the same with tWR alone 12 to 24 (6.7 to 13.3 ns; ZenTimings: nothing else changed).
Medians, [lowest-highest] of 2 probe runs (each a median of 7) or 3 TMR runs:

| Measure | 2133 | 3600 | 3600, tWR 24 | tWR 24 against 12 |
|---|---|---|---|---|
| R | 101.7 | 65.4 | 65.5 | +0.1 |
| W-nt+sf | 123.5 | 91.8 | 91.8 | 0 |
| W-fl | 195.1 | 134.4 | 134.4 | +0.1 |
| RAW-nt | 236.1 | 166.2 | 166.1 | -0.1 |
| v3 50%, under writes / reads | 124.9 / 120.8 | 74.8 [69-80] / 72.5 | 75.0 / 72.8 | +0.2 / +0.4 |
| v3 75%, under writes / reads | 149.2 [123-176] / 132.4 | 90.8 [86-96] / 79.2 | 87.3 [78-96] / 77.2 | noise |
| **v3 flat out, under writes** | 451.5 [451-452] | 213.9 [214-214] | **220.6 [217-224]** | **+6.7 (+3.1%)** |
| v3 flat out, under reads | 264.6 | 153.8 | 153.2 | -0.6 |
| TMR Lat-DRAM-Read | 102.7 | 65.8 | 65.8 | 0 |
| TMR Lat-DRAM-Write | 125.8 | 78.3 | 78.6 | +0.3 |
| TMR Lat-V2P-DRAM-Write-256 | 266.4 | 157.5 | 157.8 | +0.3 |
| TMR Lat-NTW-DRAM-Write-256 | 33.6 | 20.6 | 20.6 | 0 |

(TMR: `test=Lat-NTW-DRAM-Write-256,Lat-V2P-DRAM-Write-256,Lat-V2P-DRAM-Read-256,Lat-DRAM-Read,Lat-DRAM-Write
cycles=1 calibrate=off memory=50% cputype=cores cpus=100% skip-cores=0`, 0a8467d.)

What it shows:
- tWR moved one figure, a read's latency under a flat-out write load, by its own 6.7 ns: with the
  banks kept busy by writes, each row a write closed waits the longer recovery, and a read behind it
  pays. Every idle write figure, TMR's three included, stayed flat: the controller acknowledges a
  write when it is queued.
- The three write figures (W-nt+sf, RAW-nt less R, W-fl less R) moved alike from 2133 to 3600
  (-25 to -26%); W-nt+sf and RAW-nt less R stayed about 9 ns apart at both, a check on each other.
  CLWB differs by CPU (Zen 4 evicts the line anyway; Zen 3 is cheaper than a flush).
- At daddy-petz's write peak the limit is the Infinity Fabric's write link, 16 B a clock x FCLK:
  17.1 and 28.8 GB/s against 16.9 and 28.3 measured. So from 2133 to 3600 the flat-out-under-writes
  figure (-53%) moved mostly with FCLK; the tWR step kept FCLK fixed.
- Matched turnaround is steady within a run but not between runs at 50% (at 3600, -3.7 and +8.4
  ns, each run's windows within 1 ns): the controller settles into a state that lasts a whole load.
  It didn't move with tWR, which it shouldn't: turnaround is the bus switching (tRDWR, tWTR,
  tWRRD).

## tRDWR alone (8 to 12 at 3600, 4.4 to 6.7 ns; tWR back at 12)

| Measure | 3600 | tRDWR 12 | |
|---|---|---|---|
| R, W-nt+sf, W-fl, RAW-nt, TMR's five Lat-* | | | 0 (within 0.4%) |
| v3 under reads, 25/50/75%/flat out | 68.9 / 72.5 / 79.2 / 153.8 | 69.3 / 73.0 / 78.2 / 151.5 | noise |
| **v3 50%, under writes** | 74.8 [69-80] | **87.6 [85-90]** | **+12.8** |
| **v3 50% turnaround** | +2.3 [-4..8] | **+14.6 [12..17]** | **+12.3** |
| **v3 75%, under writes** | 90.8 [86-96] | **113.4 [112-115]** | **+22.6** |
| **v3 75% turnaround** | +11.6 [5..18] | **+35.2 [32..39]** | **+23.6** |
| v3 flat out, under writes | 213.9 | 215.7 | +1.8, noise |

Each of the two measures sees a different timing, and no test TMR has sees either: tWR shows in
latency under a flat-out write load only, tRDWR in turnaround at matched load only, 5-10 times its
own 2.2 ns (a read waits behind many read-to-write switches). Flat out the bus is nearly all writes
and seldom switches, so tRDWR vanishes there.

## Other hosts (v3, 2 runs each)

| Host | Idle R | 50% under writes / reads | Flat out under writes / reads |
|---|---|---|---|
| petz007 (Ryzen 5 8600G, DDR5-6000) | 66 | 76 / 84-88 (turnaround -7 to -11) | 90-91 / 250-254 |
| daddy-petz at 2133 | 103 | 123-127 / 121 | 451 / 264 |
| m8i.metal-48xl (Xeon 6975P-C, node 0: 4 channels of DDR5-7200) | 132 | 284-308 / 167-169 | 1,755 / 398 |

Writes barely touch reads on Zen 4; Zen 3 at 2133 pays near saturation; the Xeon's controller lets
writes push a read past 1.7 us flat out (+500 ns at 75%), within 1 ns run to run. A figure that
separates memory-controller generations, as the owner expected.

## Decided (the owner, 2026-11-05)

- Both measures go into TMR: latency under load and write turnaround ("both are important to know
  for overclockers trying to move the needle"). On main, in large commits.
- Which write figure stays: the owner decides on the overclock result. Proposed: they moved alike,
  so W-nt+sf, with RAW-nt less R as its check; CLWB dropped. None of them saw tWR.
- The m8i metal box stays up to validate the build.

## The build

A design workflow (three designs, a judge, 2026-11-05) gave two tests on one function:
`Lat-Loaded-DRAM` (latency idle and under flat-out NT writes and reads) and `Lat-Turnaround-DRAM`
(write/read pairs at matched levels, ABBA order, each window a fresh instance of the load). One
thread measures (thread 0, the top-ranked core), one loader a physical core on its node, the rest
sit out; roles chosen once. Its plan: `117-loaded-latency-design.md`. Built on 2026-11-05 (b4ad1ad,
264b1e1, fd055aa, then the review's fixes), both run by name and are in no plan yet; next, their
host runs (the design's step 8), then the plan, default and headline decisions with the owner.
