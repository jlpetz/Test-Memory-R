# TODO 25. [TMR-APP] Built-in test plans

**Raised** before 2026-10-01: `--quick-test` only injected memory, cycles and CPU defaults, then ran
the whole suite (274 tests on 2026-10-16: 33 stability tests at every width, 46 Bench, 180 latency,
15 bandwidth), and so did a plain run. **Built** 2026-10-16/17 (`src/plans.rs`).

## Design (agreed 2026-10-16)

A plan is an ordered list of the built-in suite's test names. It only chooses: each test keeps its
built-in settings, so `plan=stability` and `test=Mem-MovingInv` run the same Mem-MovingInv.

The TODO's first sketch, a full config file per plan embedded in the binary, was dropped:
- A config entry doesn't inherit a test's built-in extent, chunk and timing; it falls back to the
  file's defaults. Every plan would repeat each test's settings, and drift when the suite is tuned.
- A config file also sets memory, CPUs, pinning and the error mode, which the command line can
  only partly override; `--quick-test`'s own memory handling would have fought it.
What name lists give up: per-test tweaks inside a plan, and a file to copy. Add an override, or an
"export this plan as a config" command, if either is ever wanted.

## The plans

Times are this dev VM's (28 GiB, 4 threads, the 2026-10-16 TODO 76 run). Ten of the eleven
stability tests stop on time, so other machines come out close; StuckBit and Stride grow with memory.

**`stability`** (the default for a plain run; 3 cycles, the usual default), the widest width (Auto):

| # | Test | Why | Time |
|---|---|---|---|
| 1 | Mem-StuckBitAuto | Every bit held 0 and 1 over all memory; first, so a hard fault shows in seconds | 4 s |
| 2 | Mem-SimpleV2-Auto | TM5's SimpleTest with the native program; found the 8600G's fault | 32 s |
| 3 | Mem-SimpleNT-Auto | The same with non-temporal stores: writes go straight to DRAM at full rate | 31 s |
| 4 | Mem-MovingInv | Up then down through memory: neighbouring-cell and address-decoding faults | 35 s |
| 5 | Mem-ModuloX | Every Nth word held while its neighbours toggle: cell-to-cell interference | 32 s |
| 6 | Mem-Sequence | Every line in scrambled order (defeats prefetch), with read-write passes | 35 s |
| 7 | Mem-Random | Random-address reads over all memory, then a sweep | 26 s |
| 8 | Mem-RefreshAuto | Write, 64 ms idle, check: the only retention and refresh test | 15 s |
| 9 | Mem-MirrorV2-Auto | TM5's MirrorMove: in-place swaps, reads and writes mixed | 10 s |
| 10 | Mem-Stride | Distances from 8 B to 32 KiB, so bank and row switching vary | 4 s |
| 11 | Mem-BlockMove | Large block copies: read and write streams at once | 20 s |

About 244 s a cycle. Left out: Mem-CacheBust (it works mostly inside the caches; the owner: "I
don't think it was useful beyond what the others do"), the flush variants (at these chunk sizes
eviction already reaches DRAM, `doc/cache_management.md`), the fixed widths, Bench, latency and
bandwidth.

**`quick`**: the same eleven, one cycle (about 4 min). `--quick-test` is `plan=quick` on 60% of
available memory reserved, as before. A shorter subset would need data on which tests find faults
first; TODO 97 has that for patterns only.

**`widths`** (3 cycles): the families with width variants, every width side by side, so each tests
the same memory in the same seconds of every cycle (separate runs drift; petz007 went 38, 24, 0, 0
errors over four 10-minute blocks on 2026-10-11):
StuckBit, Refresh, SimpleV2, MirrorV2 at scalar, 128, 256, 512 and Auto; SimpleNT at 128, 256, 512
and Auto. 24 tests, about 7 min a cycle. A CPU without AVX-512 drops the four fixed 512-bit ones
(they'd return empty results), logged; Auto is then 256.

**`perf`** (1 cycle, about 5 min): the original latency tests (pointer chasing, one dependent
access at a time: the figure other tools report) at L1, L2, L3, DRAM, DRAMFull × Read, Write, Copy;
the 15 bandwidth tests at the same levels (Auto); Lat-NTW-DRAM-Write-Auto. No V2P (the owner,
2026-10-16): its effects are on the cache side and converge at DRAM, measured on Intel only.

**`all`**: every test in the suite, in its order, as a plain run did before.

The latency families, for the record (2026-10-16, nothing removed): Lat (15) is the base; Lat-V2
(60) was V2P's one-access baseline and tracks Lat; Lat-V2P (60, six accesses in flight) found the
L2 write, L1 copy and L3 copy effects (`doc/store_buffer_pressure.md`); WriteFull/CopyFull (40)
answered that the L3 split follows instruction width; Lat-NTW (5): only full-line NT writes take
the fast path (`doc/nt_stores.md`).

## How a run picks its tests

- `plan=<name>` picks a plan; `test=` narrows it. `config=` brings its own tests; with `plan=` too
  it is an error.
- No plan, no config, no filter: `stability`. A filter alone (`test=`, `--ram-latency`,
  `--cache-latency`) picks from every test, as before.
- A plan's cycles apply when the command line gives none; `test=` still makes it 1 first.
- The startup line says which plan ran (`📋 Plan: stability (the default): ...`), and the results
  file records it (`execution.plan`; none for a config run).

## Calls made while the owner was away (2026-10-17), to review

1. `quick` and `perf` default to 1 cycle; `stability`, `widths` and `all` keep the default 3.
   `widths` at 3 is about 22 min here; its cycles interleave the widths, which is what cancels drift.
2. `test=` with no plan keeps picking from every test, so `test=Mem-SimpleV2-*` still finds every
   width. Making it narrow `stability` would have broken that habit and `--ram-latency`.
3. `--ram-latency` and `--cache-latency` stay filters; not turned into plans.
4. `plan=` with `config=` is refused rather than one winning silently.
5. A `test=` that matches nothing inside a plan lists that plan's tests and points at `plan=all`.
6. The plan goes into the results file.
7. No `soak` plan (`stability` with `duration=`, until TODO 97's long run says otherwise), no width
   suffix (`quick-256`), no per-test overrides.
