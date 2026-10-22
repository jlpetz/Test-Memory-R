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
  (`runner.rs:1263` → `display_and_save_results` → `save_to_file`, `runner.rs:4771`). Ctrl+C
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
- **`--compare-results` matches steps by position** (`(step, id, name)`, `results.rs:709`), so a
  test left out on one host (the 5700X) shifts every later step and they silently stop matching.
  Cross-host comparison (TODO 100) needs it to match by test.

**The cheap slice, independent of #29/#7** (additive `#[serde(default)]` fields, cold path, no
kernel touched):
- A. save the file after every cycle (write a temp file, then rename);
- B. per-thread records for each step, with the physical core;
- C. the tests left out, the build commit and rustc, ops counts, commit headroom;
- D. a `--show-results <file>` that replays the final summary through the existing converter.
