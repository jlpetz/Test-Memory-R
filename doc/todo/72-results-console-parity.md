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
  (`runner.rs:1217` → `display_and_save_results` → `save_to_file`, `runner.rs:4842`). Ctrl+C
  still saves, but a timeout kill (the test hosts' `run-tmr.ps1` calls `$p.Kill()`), a dropped
  remote session, a hang or a bugcheck loses the whole run.
- **No build identity.** `tmr_version` is hard-coded `"1.0.0"` (`results.rs:319`); no commit, no
  rustc. The nightly toolchain is unpinned, so a rebuild of the same commit is not the same binary.
- **Left-out tests aren't recorded.** `leave_out_what_the_cpu_lacks` (2026-10-22) prints and logs
  them only.
- **Per-thread data is in hand** where the step report prints it (`runner.rs` ~1678), and the
  final summary already renders from the saved struct
  (`converters::create_overall_stats_summary_report(&TestRunResult)`): one report already works
  the way the better version wants.
- **`--compare-results` matches steps by position** (`(step, id, name)`, `results.rs:684`), so a
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

**Unrounded, 2026-11-02.** The file saves its figures as the run has them, none rounded (the owner:
the file is for tools, and an exact replay needs the raw values); before, a speed was saved whole
and times, latencies and GiB to 2 dp. The size test's figures are now worked out as a run's are
(from a `Duration`, cycles over the TSC's GHz, bytes over time), where they were 2 dp already.
Rounded on save, those figures give the table's sizes to within 0.01 MB (8.64 MB pretty, not
8.63); unrounded:

| What | Rounded | Unrounded | Growth |
|---|---|---|---|
| final file, pretty | 8.64 MB (311 B a record) | 8.97 MB (322 B a record) | +0.33 MB, +3.8% |
| final file, compact | 3.64 MB | 3.98 MB | +0.34 MB, +9.3% |
| over the run | 220.5 MB | 229.1 MB | +8.6 MB, +3.9% |

The time to serialize didn't change (the two builds alternated, three runs each: `to_string_pretty`
19.1-19.4 ms on the final file, 459-468 ms over the run, either way). A cycle adds 0.18 MB at
32 x 16, not 0.17; the shapes below grow by about 4% too. The console's final summary from a saved
and loaded file is the text it showed at the run's end, figures and all (`tmr-cli`'s
`renderer::tests::the_final_summary_from_the_results_file_is_the_consoles`).

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

## 2026-11-02: `--compare-results` matches steps by test

The owner: the main use is one machine before and after an overclock, machine to machine should
work too; show what changed in the plan, then compare the matched tests. Before, steps matched by
`(step, id, name)`: the position after left-out tests and the width-resolved name. A test left out
on one machine (the 5700X) shifted every later step, a build that added a test to a plan did the
same on one machine, an `-Auto` test never matched across widths, and what didn't match was
skipped without a word.

- **The key** (`run_context::compare_plans`): the test as its plan or config names it, before an
  `-Auto` name resolves (`run_config.tests[].test`, new, required; `Mem-StuckBitAuto`), its config
  id, and which of that test's runs in the cycle the step is (a TM5 `Test 1 x2`). Not the id
  alone: ids are unique within one config only, and a TM5 import numbers its tests 0..N. With the
  id in the key, one of an import's many `Mem-SimpleV2-Auto` tests disabled shifts none of the
  others. The occurrence is what makes repeated names safe, why TODO 74 matched by position.
- **The plan is `run_config.tests`**, one entry a step (entry i is step i + 1), which gains `test`,
  `id` and `label`; a step's figures come from `per_step_averages` by its step in its own run. So a
  run that stopped before a step still has it in its plan, and the comparison says it wasn't run.
- **The output**: the run differences (`Config`, `Plan` and `Test filter` rows now, which warn);
  then the plans' changes: the steps only in the baseline, only in the current (with
  `tests_left_out`'s reason when that is why), and the settings that differ of the tests both have,
  once a test, `SIMD width` for an `-Auto` test at two widths; then the overall changes, with a
  caveat that the totals cover different work when the plans differ; then each matched step, at
  its step in each run (`Step 4 -> 3`), with both names when they differ, `⛔ settings differ`, or
  `not run in the current`. The per-test settings moved from `compare_config`'s run differences to
  the plans' changes: they no longer make the whole comparison "not like for like", only their
  tests. The window's Compare tab shows the same: the text, then the steps as a table with a note
  column. The golden is `testdata/console/comparison.txt`.
- **Files**: results files from before have no `test` and `label` in their plan and don't load
  (the owner allows it, pre-stable). The pre-TODO 72 fixture gained them, one entry a step.

## 2026-11-02: the owner's decisions

From a read-only assessment of what D and step matching need now the engine is split (four readers,
each checked by a skeptic), then the owner's answers:

- **The saves between cycles stay as they are.** They run at a cycle's end only, while the tests
  wait, so they don't touch a test's timing; at most once a minute bounds what a run of short
  cycles writes. Writing once at the end, and appending a step at a time (JSON Lines), were
  weighed and not taken.
- **The file stays a summary.** No step-by-step replay (a step report's parameter text, 8 of the
  latency table's 14 columns and its sample counts, and the stamp under it aren't saved), and no
  saved event stream (about 60 payload types would become a file format, and the log already holds
  the console's text of every event, since ea672a7). An old run in the window would show its
  summary and its steps' figures, not each step's report.
- **Saved unrounded** (0d274ef): the file is for tools; a person reads the log or the window.
  serde_json's `float_roundtrip` isn't on: about 12% of a run's figures read back 1 ulp off, which
  no printed figure shows (the parity test renders identically).
- **Each test's parameter** (1c9aad0), `run_config.tests[].parameter`, the Config line's text. It
  follows the run's `channels=` (or the config's) and `mirror=`, not the hardware: two machines on
  one plan and one command line record the same.
- **A run ID** in the results and the log header (d282a7f; `29-output-boundary.md`, option (c)).
- **Commit headroom dropped.** `ullAvailPageFile` is the commit remaining, not page-file space, and
  only an `ERROR_COMMITMENT_LIMIT` (1455) failure would need it; none has been seen. Trigger: one.
- **D**: `--show-results <file>`, on the console: a first line saying how the run ended (finished,
  stopped, its time limit, halted on errors, or never finished: killed or crashed, figures up to
  its last saved cycle), then the final summary, as the console printed it.
