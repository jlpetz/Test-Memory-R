# TODO 105: what adds up at 256 threads (proposals from the struct audit)

Raised 2026-10-24 by the owner: "on a big machine with 256 or more threads things start to add up;
make sure the passing and copying of structs is not needlessly large and growing out of control".
Three tracers read the dispatch path, a chunk's set-up, and the settings structs' growth and
sharing, against rustc's `-Zprint-type-sizes` and the post-LTO asm of 2026-10-24. The owner's
rule for the answers: a small fix goes in; a significant one is proposed here first.

## The answer to the question asked: struct copies are fine

- Each worker gets **one work item a step, never one a chunk**: the test runs every chunk and cycle
  inside the worker (`thread_pool.rs`, `worker_thread_loop`). Out, an 80 B `WorkItem` and a boxed
  344 B `TestMemoryConfig`; back, a 576 B `WorkResult`. At 256 threads about 1 MB a step, about
  0.1 ms against a 10 s step (under 0.1%), outside the tests' own clocks.
- **Nothing is copied or allocated a clean chunk**, on either tier. `TestRunner` (272 B) is built
  once a test call and used through a pointer; `Rescan` (1152 B) only after a failed check (every
  construction behind `#[cold]` or the check's branch, in the asm); kernels read
  `TestMemoryConfig` through a pointer, so its size costs nothing. Per chunk: about 100
  instructions and a clock read, about 5e-6 of an 8 MiB chunk's work.
- **Growth**: seven single-test fields came in between 2026-10-06 and 10-24, and the parameter
  context went 48 → 112 B, but it is copied once a step: today's +16 B is 8 KB a step at 256
  threads, microseconds. Where growth did cost was build time (the registry's configs by value;
  d6c71c5).
- **Sharing**: no two workers write one cache line, in a hot loop or a chunk. Progress slots are
  64 B aligned, one a worker; the stop flag is read-only until Ctrl+C; error counts, the seal's
  state and the stats are each worker's own.

Fixed the same night (954b997): the run's per-thread totals kept every thread's 64 B row of every
step, about 120 MB a day at 256 threads, for three figures a thread; now a running sum.

## Proposals (the owner decides; none built)

### 1. The latency tests keep every sample (the one real problem: fix first)

A latency sample is 1000 dependent loads; each is pushed into a `Vec<f64>` without a limit
(`latency_tests.rs:162/205`, `latency_tests_v2.rs:187/223`) and the whole Vec comes back in the
`WorkResult`. Measured on this box: 7.0 M samples a thread in a 10 s Lat-L1 step. The coordinator
then clones every thread's Vec, sorts it, clones and sorts again (`runner.rs` around 1514-1518 and
4541-4561), on one thread, while every worker waits. Measured gaps between Lat-L1 steps: 0.19 s
at 1 thread (7 M samples), 1.8 s at 4 (28 M). At 256 threads: about 1.8 G samples, 14 GB a copy,
some 20 GB of worker Vecs and 40-70 GB of coordinator heap at the peak, and minutes of sorting a
10 s step (plan=perf and plan=all, and Lat-* filters; not the stability plan).

- **Proposed: a fixed histogram a thread** (log-linear buckets, e.g. 64 an octave from 0.25 ns to
  1 ms, about 1.4 k counters, 11 KB), with min, max and count. The coordinator adds 256 of them
  and reads the percentiles from the sum: no sort, memory bounded whatever the duration or thread
  count, and each worker's own sort of its 7 M samples (about 0.17 s, inside its timed figure)
  goes. Percentiles come out exact to the bucket width (about 1%), finer than the 0.1 ns printed.
- Exact alternative, smaller: the per-thread Vecs arrive sorted, so the coordinator can read the
  pooled percentiles with a k-th-element search across them (milliseconds) and each thread's
  minimum as `[0]`: no copies, no sorts, but the workers' 20 GB stays.

### 2. Bandwidth tests read the clock twice a pass (an estimate: measure first)

A pass of Spd-L1 is a cache-sized extent (24 KiB here): about 100-200 clocks of work, against an
estimated 150-250 of bookkeeping (two clock reads and the throttle). If so, the L1 bandwidth
figures read low (L2 about 3-5%; L3 and DRAM nothing). Proposed: read the clock once every N
passes, N fixed from the extent so N passes are at least about 1 MiB; at the least, hand the
progress throttle's `now` to the time gate (one read a pass, not two). Needs an A/B with
`tools/bench_ab.py` first: the estimate isn't a measurement.

### 3. A per-thread "Running" log line through two global locks every step

`TestRunner::new` (`test_scaffolding.rs:118`) logs one info line a thread a step through the log
file's lock and the console's, after the test's clock has started (for the loop-owning tests and
every sealed step). Measured: 32 threads take 2-3 ms; at 256, about 20 ms a step, 0.2% of a 10 s
step and 2% of a 1 s one, plus 256 lines a step in the log. Proposed: the per-thread line at debug
(`RUST_LOG=debug` gives per-thread logs, as CLAUDE.md says) and one info line a step from the
coordinator; or keep the line but log it before the clock starts. Changes the log's content.

### 4. No cap on error lines a test

A failed check logs its rescan's lines in full through the same two locks. One bad cell touches a
few chunks and costs nothing; a grossly unstable machine, all 256 threads failing every chunk,
would run at the log's pace and grow the log without limit. Proposed: the first N rescans a thread
a test in full (as `REREAD_RESCANS` already caps rereads), the rest counted, with one line at the
test's end. Changes the log's content on a failing run only.

### 5. The run's record grows with its length (already an open question in TODO 29)

Every step keeps each thread's `ThreadStepRecord` (152 B, plus its strings) for the results file,
in memory and in the JSON: this one is needed. The largest results file so far is 497 KB (4
threads, 239 steps); at 256 threads and 1000 cycles of about 30 steps it would reach gigabytes.
`doc/todo/29-split-design.md`, "Open for the owner", already asks: per-thread rows for every step
of every cycle in pretty JSON, compact JSON, or per-test averages only?
