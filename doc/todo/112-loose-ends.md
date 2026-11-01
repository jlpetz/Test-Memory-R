# TODO 112: loose ends, three speed costs the code audit estimated

From the 2026-11-01 code audit of what each test's clock holds (TODO 109's flat-start investigation: a reader per group of tests, each
claim checked by a skeptic). None is the seal's, and none was measured: these are estimates from reading the code, with the lines as
they stood on 2026-11-01 (commit 1391654). The owner, 2026-11-01: not now.

## Mem-Refresh: its wait in its time

`refresh_wait` (`src/tests.rs`, about line 1417; called at about 1512) sleeps 64 ms (or `wait_ms`) between the write and the verify,
inside the test's clock, with no bytes for it. Refresh has one chunk a cycle, so it's every cycle: in the quick plan about 64 of each
cycle's 215 ms, about 30% of its timed clock, so its MiB/s (36.3K there) reads about that much under its transfer rate. It is the test's
design (WriteWaitVerify): the wait is what it tests. So most likely the fix is a word where its speed is reported, not a change.

## Spd-* at L1: the clock reads a pass

Each pass of the bandwidth tests' loops (`src/bandwidth_tests.rs`, about lines 221, 283 and 360) calls `update_progress` and
`should_continue`, two clock reads and two out-of-line calls, about 66 ns a pass on this VM. At L1 a pass is tiny (Spd-L1-Copy: 48 KiB
of traffic, about 100 ns of copying), so it could be up to about 40% of the measured time: about 1.12M MiB/s reported against the
kernel's about 1.87M (4 threads), by the estimate. L2 and beyond: small. A fix would read the clock once every N passes, N sized so a
check lands every few ms.

## Lat-L1: the bookkeeping per sample

The latency tests (`src/latency_tests.rs`, about lines 170-211, 287-332, 409-460; each `latency_tests_v2.rs` macro's loop tail) take each
sample inside an rdtscp window, then do their bookkeeping inside the test's clock: fences, f64 maths, a `Vec` push, and the same two
clock reads. About 100-145 ns a sample against a 1.25-1.35 us window, so about 7-10% of Lat-L1's MiB/s, 2-4% of L2's, under 0.2%
beyond. Their latencies are unaffected (the window excludes it). Separately, the percentile sort runs before the final time is taken:
0.5-3% of an L1 step's timed, in its final figure only.

## Next

Measure each with the cost cut (an A/B, `tools/bench_ab.py`), then fix what the numbers say is real, or document it.
