# TODO 72. [TMR-APP] Results file ↔ console parity: load a run and see what the console showed

> The entry's original text, moved verbatim out of `TODO.md` on 2026-10-22, then the research of
> that day. The short entry in `TODO.md` holds the current status.

**Raised**: 2026-09-27, while settling #71's `backend` label.

## The original entry (2026-09-27)

**Goal.** Load a results file and see, almost exactly, how the run went as the console showed it.
Run-to-run comparison builds on that.

**Gap today.** `results/*.json` holds the run identity and resolved config (#67), plus per-test
*aggregates* (`TestResult`: duration, bytes, MiB/s, errors, WHEA, latency summary). The console
prints several datasets the file never receives:
- per-thread timing for each test: `create_thread_timing_report` (`runner.rs` ~1162), with
  bytes, elapsed, errors, operations and cycles per thread;
- the CPU performance summary: `print_detailed_cpu_performance_summary` (`runner.rs` ~838), with
  per-CPU throughput by core type. This is the data that shows a bimodal multi-CCD split
  (`memory/cpu-memory-domains.md`), which a per-test average hides;
- per-thread block allocation: `create_block_allocation_report_from_windows` (`runner.rs` ~419),
  with block sizes, page type and NUMA node per block. The file keeps only the totals
  (`page_mix`, `blocks_allocated`);
- the consolidated memory report (`layout.rs` ~40) and the test-configuration table (`runner.rs`
  ~667). `RunConfigSnapshot` mostly covers these, but nobody has checked field by field;
- a candidate the console doesn't show either: commit remaining at start (`ullAvailPageFile`, the
  misnamed and unread `available_virtual_bytes` that #69 E deleted). It explains an
  `ERROR_COMMITMENT_LIMIT` (1455) failure, which free physical memory cannot.

**Direction (not decided).**
- The cheap version adds the missing datasets to the JSON one by one.
- The better version has the console's own report builders render from a loaded file, so the
  console and a replay cannot drift apart. `TestRunResult::to_text_report`, deleted under #69 E,
  is the counter-example: a second text renderer with no caller, which had already drifted. Don't
  bring back that shape.
- That better version is #29/#7's engine-event question. If the engine emits structured events and
  the console is one renderer, then the results file is that stream saved, and a replay is the
  console renderer run over it. So scope this item with #29/#7, not ahead of them.

**Size check before choosing.** Per-thread data per test per cycle grows the file with threads ×
tests × cycles. Measure it on a long run (e.g. 32 threads × 16 tests × 50 cycles) before choosing
between storing every cycle and storing per-thread aggregates per test.

## 2026-10-22: what the code does now (read-only research)

The line numbers above are from 2026-09-27; these are current.

- **Written once, at the end.** The file is saved only when the suite finishes
  (`runner.rs:1252` → `display_and_save_results` → `save_to_file`, `runner.rs:4924`). Ctrl+C
  still saves, but a timeout kill (the test hosts' `run-tmr.ps1` calls `$p.Kill()`), a dropped
  remote session, a hang or a bugcheck loses the whole run.
- **No build identity.** `tmr_version` is hard-coded `"1.0.0"` (`results.rs:335`); no commit, no
  rustc. The nightly toolchain is unpinned, so a rebuild of the same commit is not the same binary.
- **Left-out tests aren't recorded.** `leave_out_what_the_cpu_lacks` (2026-10-22) prints and logs
  them only.
- **Per-thread data is in hand** where the step report prints it (`runner.rs` ~1678), and the
  final summary already renders from the saved struct
  (`converters::create_overall_stats_summary_report(&TestRunResult)`): one report already works
  the way the better version wants.
- **`--compare-results` matches steps by position** (`(step, id, name)`, `results.rs:704`), so a
  test left out on one host (the 5700X) shifts every later step and they silently stop matching.
  Cross-host comparison (TODO 100) needs it to match by test.

**The cheap slice, independent of #29/#7** (additive `#[serde(default)]` fields, cold path, no
kernel touched):
- A. save the file after every cycle (write a temp file, then rename);
- B. per-thread records for each step, with the physical core;
- C. the tests left out, the build commit and rustc, ops counts, commit headroom;
- D. a `--show-results <file>` that replays the final summary through the existing converter.

## 2026-10-22: TODO 29 S1, the cheap slice's A to C

Every new field is `#[serde(default)]`, so a file written before them still loads
(`testdata/result_files/before_todo_72.json`, `results::tests`).
- **A.** The file is saved between cycles (`runner::save_after_cycle`): after the first, after one
  that stopped early, and after any other once a minute has passed since the last save
  (`runner::RESULTS_SAVE_INTERVAL`, see the size below), so a cycle of a minute or more is each
  saved. A save goes to `<file>.tmp`, flushed to the disk, then renamed over the file
  (`TestRunResult::save_atomic`), so a killed run or a system that went down leaves the last save
  whole. A refused rename (a viewer holding the file without share-delete) writes in place with a
  warning; when that is refused too, the save stays in the `.tmp` and the warning names both.
  `overall_stats.in_progress` is true until the final save, which adds `outcome` (completed,
  time_limit, interrupted, halted) and `status` (passed, failed, not_run, interrupted). A cycle
  that stopped early is `partial`. `--compare-results` lists a "Results file" row first among the
  run differences when either file was saved mid-run.
- **B.** `cycles[].tests[].threads`: one `ThreadStepRecord` per thread, by thread: thread, cpu,
  core, node, bytes, ms, errors (alarms, not_reproduced and seal_errors when not 0), operations,
  cycles, and latency {min_ns, p50_ns, p99_ns, spread} on a latency step. A thread the pool has no
  CPU for is kept, without cpu, core and node.
- **C.** `run_config.tests_left_out` ({name, reason}); `metadata.build` {commit, dirty, rustc,
  profile} from `build.rs`, the version staying `tmr_version`; `tests[].operations` and `timed_ms`.
  Commit headroom is not in.
- **D.** Not started.

**Size, measured.** `results::tests::a_long_runs_results_file` (`#[ignore]`;
`cargo test --release -- --ignored a_long_runs_results_file --nocapture`) builds a synthetic run
and saves it after each cycle. Run on 2026-10-22, release build, on the dev box (Xeon 6975P-C,
8 vCPUs):

| What | Measured |
|---|---|
| shape | 32 threads x 16 steps (2 of them latency steps) x 50 sealed cycles: 25,600 thread records |
| final file, pretty | 8.63 MB, of which 0.67 MB is everything but the thread records (311 B a record) |
| final file, compact | 3.64 MB |
| final save | `to_string_pretty` 19.4 ms; `save_atomic` 37 ms with the flush to the disk, 24 ms before it |
| over the run | 220.4 MB written by the 50 saves, 462 ms serializing in all |

The design's estimate (about 300 B a record, 7.7 MB final, 200 MB over the run) holds for this
shape. Other shapes, worked out from the measured 311 B a thread record and about 840 B a step for
the rest (0.67 MB over 800 steps), and the measured 445 MB/s of `to_string_pretty`:

| Run | A cycle adds | Final file | Written, a save every cycle | Written, at most one a minute |
|---|---|---|---|---|
| 32 threads x 16 steps, 75 s cycles, 12 h (576 cycles) | 0.17 MB | 99 MB, 225 ms to serialize | 29 GB | the same: each cycle is over a minute |
| 96 threads x 16 steps (cputype=cores on the 192-vCPU EC2 host), 50 cycles | 0.49 MB | 25 MB | 0.6 GB | the same, for cycles of a minute or more |
| 16 threads x 1 step (a `test=` run), 3 s cycles, 8 h (9,600 cycles) | 5.8 KB | 56 MB, 125 ms to serialize | 268 GB, 4% of the run serializing at the end | 13 GB, 0.2% |

A save every cycle writes what grows with the square of the cycle count, so short cycles made it
hundreds of GB and a growing share of the run's time, which `duration=` counts. Hence the
once-a-minute limit: what a run writes is then bounded by its length and its final file's size,
however short its cycles. The file itself still grows with every cycle, by 0.17 MB at 32 x 16 and
0.49 MB at 96 x 16. Pretty JSON with every step of every cycle stays for now, as the design has it
("pretty stays unless a long run's final file is large"). What counts as large, and the fallback
if so (compact JSON, 58% smaller; or per-test aggregates instead of the thread rows), is the
owner's decision (29-split-design.md, "Open for the owner").
