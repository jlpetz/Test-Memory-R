# TODO 120: the allocator's progress on the console

## Where the time goes (measured 2026-11-09, from the kept logs)

The allocation table prints its own time (`📊 Memory Allocation (stitched, N s)`). Across 577 logs on the dev box since 2026-10-20 (the
2026-11-03 campaign's and the owner's local runs):

| Run | 1 GiB pages | 2 MiB requests (128 MiB each) | Allocation | Each 2 MiB request |
|---|---|---|---|---|
| Dev box (Xeon 6975P-C VM), 48 GiB, 2026-11-06 | refused (1450) | 384 | 29.4 s | 77 ms |
| Dev box, 28 GiB, 2026-11-04 | 1, then refused | 216 | 15.6 s | 72 ms |
| Dev box, 28 GiB, 2026-11-05 | refused | 224 | 13.0 s | 58 ms |
| desktop-v6v7goc (8500G), 30 GiB, 2026-11-03 | 14, then refused | 128 | 6.5 s | 42 ms |
| Dev box, 28 GiB, 2026-11-09 | 23, then refused | 40 | 2.0 s | 49 ms |
| daddy-petz, 32 GiB, 2026-11-03 | 24, then refused | 64 | 0.00 s | under 0.1 ms |
| The metal box, 384 GiB over 192 threads, 2026-11-03 | (most) | | under 1 s | |

- **1 GiB pages, when Windows has them, are instant** (23 in under a millisecond).
- **2 MiB pages are slow once memory is fragmented**, the state that refuses 1 GiB pages after a machine has been up a while: 42-77 ms
  per 128 MiB, 1.6-3 GiB/s, Windows assembling contiguous runs and zeroing them. Fresh from a boot they were instant too (daddy-petz,
  the campaign's hosts).
- **The thread split is instant** (the stitched arena's slices, one line).
- **What the console shows meanwhile**: "Stage 1: Pre-allocating memory blocks...", the page sizes, the 1 GiB refusal (❌), then
  nothing until the pass ends: the ✅ lines summarise each request size at a pass's end or a refusal (`fill_ladder`'s tally). They are
  the run's own lines (`SHOWN_TARGET`), shown at every log level; release builds hiding info (TODO 105) didn't hide them. The silence
  is the pass, up to 29 s here.

## Proposed (for the owner, 2026-11-09)

A progress line while the fill runs, from shared counters the frontends read on their own timer, as the tests' figures are read: the
fill stores the bytes granted and the page size after each request (a relaxed store every 1-80 ms, in no test loop), and
`Run::AllocationStarting` carries the counters as `SuiteStarted` carries its `ProgressTracker`.

- **A console** (the ticker): one line redrawn in place 2-4 times a second, gone when the fill ends, before the allocation tables:
  `⏳ Allocating: 18.4 of 48.0 GiB (38%) on 2 MiB pages, 12 s`.
- **Redirected output** (a script, WinRM, a file): no redraw; a plain line every 5 s while the fill lasts, so a fill under 5 s adds
  nothing.
- **The window**: the same text in its header while it allocates, where it says "Setting up".
- **By time, not by count**: a 1 GiB request takes under a millisecond and a 2 MiB one up to 80, so a count would flood or stall.
- **Not shown**: each request, and the thread split.
- **Maybe**: the 1 GiB refusal's line saying why the rest is slow and what helps ("memory is fragmented: 2 MiB pages are slower to
  take; a restart usually brings 1 GiB pages back").

## Checks

- Tests: the counters through a fake `Fill`; the console's line and the redirected lines against goldens.
- Live: a slow fill shows on the dev box (fragmented memory, 2 MiB pages). This session's sign-in lacks the large-page right, so the
  live look is the owner's run there.
