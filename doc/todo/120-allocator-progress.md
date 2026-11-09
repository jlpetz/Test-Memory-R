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
- **2 MiB requests took 42-77 ms each** (128 MiB, 1.6-3 GiB/s) on the dev box (a VM) and the 8500G, and under 0.1 ms on daddy-petz
  and the campaign's other hosts. Why isn't established. Running out of 1 GiB pages is itself normal for a run that takes most of the
  memory, fragmented or not (the owner).
- **The thread split is instant** (the stitched arena's slices, one line).
- **What the console shows meanwhile**: "Stage 1: Pre-allocating memory blocks...", the page sizes, the 1 GiB refusal (❌), then
  nothing until the pass ends: the ✅ lines summarise each request size at a pass's end or a refusal (`fill_ladder`'s tally). They are
  the run's own lines (`SHOWN_TARGET`), shown at every log level; release builds hiding info (TODO 105) didn't hide them. The silence
  is the pass, up to 29 s here.

## Built (92cded0, 2026-11-09)

The owner chose lines over a ticker: no wiring, every frontend has them (the console, the window's Log tab, the log file), and they
leave a record, as the timestamps above did. The run's own lines (`SHOWN_TARGET`), by time, not by count:

- **A start line**, naming the allocator: `⏳ Allocating 28.00 GiB for 4 threads with the stitched allocator`.
- **A line as each pass begins**, when a thread can take that page size: `⏳ stitched: 2MB large pages, 8.00 GiB to go`, a remote pass
  `from other nodes`.
- **While a pass runs past 5 s, a line every 5 s**: `⏳ stitched: 18.40 of 28.00 GiB so far (12 s)` (`fill::Progress`).
- **A finished line**, the time and every call to Windows by kind (the owner): `✅ stitched: 28.00 GiB for 4 threads in 4.17 s, 255
  calls to Windows: 85 commits (1 refused), 83 placeholder splits, 85 releases, 1 VA reservation, 1 re-reservation`. Counted by
  `stitched::Counted`, a wrapper around the arena's VM, so the fake's tests count the same calls (`every_os_call_is_counted`).
- **No hint** on the 1 GiB refusal: the line stays factual (the owner).

Seen live on the dev box (28 GiB: 20 x 1 GiB in 1 s, 64 x 128 MiB in 4 s), the 5 s line not reached. Then the owner's slow one
(`--quick-test maxpage=large memory=5%`: 52 GiB, all 2 MiB pages, 416 x 128 MiB in 18.95 s), its 5 s lines at 5, 10 and 15 s, and
what they show: the fill slows as it nears the memory limit, 7.1 GiB/s in its first 5 s, then 1.7, 1.2 and 0.5. Likely Windows taking
memory back from other uses (its file cache) and zeroing it, once what it had free is gone (not measured).

## Checks

- Tests: the pass and progress lines' text and their 5 s timing (`fill.rs`); every call counted through the fake VM, and the finished
  line's text (`stitched.rs`).
- Live: the dev box, 2026-11-09 (above). (An earlier note here said this session lacked the large-page right: Git Bash's `whoami` is
  coreutils', not Windows'; `whoami.exe /priv` shows it enabled.)
