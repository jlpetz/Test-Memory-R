# TODO 71. [TMR-APP] Scan for functionality lost in the 2025-08/09 runner rebuild ✓ CLOSED (2026-09-28)

> Full record, moved verbatim out of `TODO_ARCHIVE.md` on 2026-09-28 (closed item).
> The short entry in `TODO_ARCHIVE.md` holds the current status; this file holds the reasoning.

**Priority**: High (one finding already makes `duration=` do nothing; there may be more like it)
**Raised**: 2026-09-26, while diagnosing the progress line (`Cycle: 1/2` in cycle 2, `Progress: 12/6`).
**Status**: CLOSED 2026-09-28. Everything found is fixed, decided, or moved (#74, #75).

**Why this class of bug is silent.** Two commits rebuilt the runner: `3807cce` (2025-08-28, "Claude
deleted runners.rs ffs", restored it as `recovered_runner.rs`) and `b8dc5b7` (2025-09-01, "Fixing
Claud mix up"), which replaced the old `loop { … }` suite loop with `for cycle in 1..=cycles`. Callees
the rewrite stopped calling still compile and clippy stays clean, because every one is `pub` in a
lib+bin crate and `dead_code` cannot see an unreachable `pub fn` (#69 E). Config fields that are
parsed and displayed but never *acted on* are invisible to any lint.

**Found so far — all from `b8dc5b7`'s loop rewrite:**
1. **Suite `duration=` is ignored.** `should_continue_suite` was deleted, and nothing in `runner.rs`
   reads `global_duration_secs`. `duration=3600` runs exactly one cycle (`unwrap_or(1)`), while the
   banner prints "3600s duration (unlimited cycles)". `cycles=N duration=D` ignores `D`, and a
   config with both null runs one cycle, not the "Unlimited" it displays. The old loop checked both
   limits *between cycles* and printed "Test suite timing limits reached - stopping".
2. **`progress.start_new_cycle()` never called**, so the progress line's cycle number stays at 1 and
   its per-cycle test counter never resets (the `.min(100)` clamp hid it).
3. **`progress.complete_cycle()` never called**, so `ProgressTracker::cycle_stats` is always empty.
   The only builder that read it was `converters::create_final_test_summary_report`, itself never
   called in any commit and deleted in `2faeb74`; the live final summary comes from
   `results::OverallStats` via `create_overall_stats_summary_report`, which does carry latency.
4. **Per-cycle report and interrupted-cycle report** — already #69 F (decided: revive).
5. **Live per-test progress is published but never read.** Each worker makes a `TestProgress` on its
   own stack (`thread_pool.rs` ~285/337) and the tests store bytes/errors/cycles into it every
   250 ms (`TestRunner::update_progress`, `test_harness.rs`, `bandwidth_tests.rs`; latency tests
   take `_progress` and ignore it). Nothing outside the worker can see it, so the progress line
   has no live speed and its `Errors` figure only moves when a test ends. Found 2026-09-26.

**Fixed 2026-09-26 (with the progress-line rework; tested, commit 4665fc8):**
- **1** — the suite loop checks `global_cycles` and `global_duration_secs` between cycles again;
  with neither set it runs until Ctrl+C or a halt. The progress line shows the remaining time.
- **2** — `start_new_cycle()` is called per cycle, and the ticker shows the cycle and the test
  within it (form under 5 below).
- **3** — deleted, not rewired. `cycle_stats` was a second copy of `TestRunResult.cycles`, which
  `add_cycle` fills every cycle and which already feeds the final summary's per-test averages and
  the JSON. `complete_cycle`, `get_cycle_stats`, `CycleStats`, `cycle_start_time`,
  `total_bytes_processed`, `total_test_time_ms`, `current_throughput` (Speed, dropped from the
  line as stale) and `current_phase` went with it.
- **Found while fixing: an `ErrorMode::Halt` was reported as Ctrl+C.** The halt path set
  `SHUTDOWN_REQUESTED`, so the verdict printed "interrupted by user (CTRL+C)" and never the failure
  line. `execute_test_cycle` now returns the outcome (`RunOutcome::Halted`), and the dead outer
  `success` flag is gone.
- **5** — wired. The pool owns one `TestProgress` per worker (`Arc<[TestProgress]>`, 64-byte aligned
  so publishes don't share a line), zeroed by `start_test` before dispatch. The ticker is now two
  lines: `Runtime | Cycle N/M (HH:MM:SS, P%) | N errors + M WHEA[ (xC/yUC)]` (always shown, ⚠️ only
  when non-zero, live errors included), and `Test i/n Name (HH:MM:SS[, P%]) | 285.00 GiB @ 19108.7
  MiB/s (18.66 GiB/s)`, or `pending` before the first publish, or `no live data` for latency tests.
  The per-test topline gained the same `(GiB/s)`. Speed is the sum of each worker's own average
  since the test began. A blank line sets the ticker off from the output above. Each test report
  now ends with a stamp line, `Runtime | Cycle | errors | Test i/n Name (HH:MM:SS)`, so a record
  of progress stays on screen. The final ticker line is gone, because the final summary carries
  the same figures.
- **New `console.rs`** owns the ticker's screen rows. The logger (now a `Target::Pipe` into
  `console::LogSink`), the cycle header, WHEA events and the Ctrl+C handler print through
  `print_above`; the per-test report uses `hold`/`release`. It also enables VT mode explicitly:
  that used to be a side effect of env_logger's stdout writer.

**Found while wiring 5 (not fixed, outside the ticker):**
- **Each WHEA event reaches the console twice**: `whea.rs` `record()` logs it (`log::warn!`/`error!`,
  and the logger tees to the console, not just the file) and also queues it for the ticker, which
  prints it again. The comment there assumes the log goes to the file only.
  **Fixed 2026-09-28**: `record()` logs to `console::FILE_ONLY_TARGET`, which the logger writes to
  the file only. The ticker's queue is now the one console path, and it also waits while a report
  holds the screen, which the log line didn't.
- **A second progress-line implementation with no callers**: `reporting::report_test_progress` →
  `formatters::format_progress_line` (`TestProgressReport`) → `ConsoleRenderer::render_progress`.
  Delete under #69. **Deleted 2026-09-26.**
- **The Ctrl+C message promises "Press CTRL+C again to force immediate exit"**, but the handler
  only sets `SHUTDOWN_REQUESTED`. A second Ctrl+C just prints the message again. This was never
  implemented, not lost: the promise arrived with the handler itself (`dda7f3e`, 2025-11).
  **Implemented 2026-09-27.** A second Ctrl+C prints one line to stderr, flushes the log and exits
  with `STATUS_CONTROL_C_EXIT`, skipping the final report and the results file; the first press's
  message now says so. The handler keeps its own flag rather than reading `SHUTDOWN_REQUESTED`,
  because a halt sets that too, and the first Ctrl+C after a halt must still be the graceful one.

**The scan (one-shot, then close):**
- Every call in `b8dc5b7^:src/runner.rs`'s suite loop and `run_single_test_cycle_with_pool`, and in
  `3807cce:src/recovered_runner.rs`: does the callee still have a caller, or an equivalent?
  Driver/remap calls are expected to be gone (purged 2026-09-14) and are not findings.
- Every field of `TestSuiteTiming`, `TimingConfig`, `RuntimeConfig` and the CLI params in
  `params.rs`: is there a reader that *acts* on it, not just one that prints it?
- Every `pub fn` on `ProgressTracker`, `TestRunResult` and the `reporting` converters: any caller?
- Each finding is fixed, deleted, or recorded as a decision (as #69 F was). Close when done.

**Scan done 2026-09-26, alongside #69 E** (committed 2026-09-28). **Closed 2026-09-28**: the WHEA
double print and `print_summary` were fixed that day, and the per-thread round-up's over-commit and
the allocator fallback it waits on moved to #75. Decision 1 moved to #74. Decisions 2 and 3 were
settled on 2026-09-27, and the Ctrl+C message is implemented. Decision 4 and the pinning bug were
fixed on 2026-09-28.
- **Old-runner calls: no lost wiring.** Each of the old suite loop's four calls still has a live
  equivalent:
  - thread priority → `runner::ThreadPriority` (`Normal`/`High`/`Realtime`, default `High`, which
    is all HEAD's `PerformanceConfig::default()` ever applied). #69 first collapsed it into a
    `raise_thread_priority` function; the enum came back on 2026-09-27. Each worker applies it
    (`thread_pool.rs` ~115). Pool creation logs it to the console and the log file, and the results
    file records it as `threads.thread_priority` (⚠️ on compare). `Realtime` is
    `THREAD_PRIORITY_TIME_CRITICAL`: base priority 15, the top of the normal range, not the realtime
    priority class. No key selects `Normal`/`Realtime` yet (see the pinning bug below);
  - Ctrl+C → the handler in `cli.rs`. HEAD's `setup_signal_handler` was an empty stub;
  - `save_test_result` → `display_and_save_results`;
  - `print_current_memory_status` → still there, called from `cli.rs`. It only lost its return value.
  Driver/remap calls are gone, as expected.
- **Parsed but never acted on: deleted.** JSON configs that still carry these keys parsed as
  before at the time; since 2026-10-03 `deny_unknown_fields` makes them errors.
  - `memory_allocation`: `zero_memory`, `require_contiguous`, `allocation_timeout_ms`,
    `retry_interval_ms`, `max_retries` and `strict_numa`, orphaned by the driver purge. They are also
    gone from `test_configs/flush_chunk_sweep.json` and the `memory_allocation_models.md` §2.4
    table. The untracked demo JSONs lose them on the next `--create-demo-configs`.
  - `TestConfig.use_v2_tests`: no reader in any commit.
  - `min_start_address` (removed 2026-09-27): computed from the `memory=` spec's start part, then
    printed ("Min Start Address" row, the layout log's "Minimum start address"), saved to the JSON,
    and checked against physical memory size, but never applied. The allocator's `base_address`
    is always `None`. It only ever bounded *virtual* addresses, and user VA is 128 TiB, so it could
    not protect a physical range either. `SystemMemoryInfo::calculate_start_address` existed only
    to compute it. The rest of the start-address surface is decision 4.
  - `LegacyMainSection.tests` (TM5's `Tests=` count): the parsed `[TestN]` sections drive the plan.
  - Calibration: `max_total_time_secs` (never enforced), and `initial_size_factor`,
    `spread_ratio_threshold`, `cv_threshold`, `size_reduction_factor` and `min/max_probes_per_tier`,
    which only the deleted `TierCalibrator`/`is_stable` read.
- **Open, needs a decision:**
  1. **TM5 `Test Sequence` is ignored. This is the largest TM5-compat gap the scan found.**
     `to_modern_config` (`config.rs` ~1181) pushes the enabled tests in file order, once each.
     `Test Sequence` goes into `LegacyMetadata.tm5_test_sequence`, and only
     `get_test_configs_with_sequence` reads it, which has had no caller in any commit (so this
     predates the runner rebuild). Every `.cfg` on disk has all 16 tests enabled, so the sequence
     alone shapes TM5's plan, and TMR runs a different one:
     - `1usmus_v3`: TM5 runs 17 steps, starting 6,12,2,10, with test 1 twice.
     - `Check_absolutnew`: TM5 runs 31 steps, with test 15 ×8 and test 2 ×7, and never runs 0 or 13.
       TMR runs all 16 once, 0 and 13 included.
     - `MT.cfg`: TM5 runs 32 steps.
     Order matters beyond coverage: MirrorMove checks whatever data the *previous* test left behind.
     **Don't wire the latent implementation as-is.** It indexes `test_sequence` by TM5 id, but that
     list holds only the enabled tests, so every index shifts once any test is disabled. Its
     per-test builder also omits `flush_before_verify`, which the live builder sets. The fix: carry
     the TM5 id on `TestConfig`, and have both paths share one per-test builder. It also has to
     settle how Time(%) → cycles applies to a test that appears N times.
     **Decided 2026-09-27: wire it, TMR-native first. Moved to #74**, with the design, so this
     cleanup can commit without it.
  2. **The results JSON's `backend` always says `NativeLargePages`** (`runner.rs:3857`), even on
     the 4KB fallback. That is deliberate, so `WindowsBackend` keeps its soft fallback, but it makes
     the recorded label meaningless. Either derive it from the allocation outcome or drop the field.
     **Done 2026-09-27.** The variant is now `MemoryBackend::VirtualAlloc2`, so `backend` names the
     API and nothing else. The page sizes obtained were already recorded (`page_mix`), and so was
     the privilege (`large_pages_available`). `allocation_strategy`, the last of the allocator's
     three keys, is now recorded beside `min/max_page_size`, in canonical spelling, and compared as
     ⚠️. It is a required field, per #67's no-defaults rule, so result files written before this
     change no longer load. The wider results-file gap went to #72.
  3. **`AppConfig` `last_config`, `ui_mode` and `auto_start` have no reader.** They date from the
     GUI plans; decide with #7. **Moved to #7** (2026-09-27), left in place until then.
  4. **The rest of the `memory=` start-address surface does nothing useful either** (found
     2026-09-27, removing `min_start_address`). `StartAddressMode` (`allocation_strategy.rs`) parses
     `:start=+NGiB` / `:start=split:X%:Y%`, defaulting to `split:5%:95%`. It is a physical-layout
     idea ("end of used memory, plus a pre-buffer") that VirtualAlloc2 cannot act on. What is left:
     - `Offset` mode (`:start=+NGiB`) now has no effect at all;
     - split mode labels one number, the reserve, as two: the "Pre-Buffer (Frag Prevention)" and
       "Post-Reserve (Ceil/frag Protection)" rows, the `(5%:95%)` in the strategy text, the
       percentages in the "Strategy Type" row, and `reserve_pre/post_gib` in the JSON;
     - split mode rounds the notional start address up to 1 GiB and **subtracts the rounding from
       the allocation**, so 0-1 GiB less is tested (before the per-thread round-up). How much
       depends on how much memory is in use, so it varies run to run;
     - `AllocationConfig.base_address` → `LowestStartingAddress` in `backend.rs` is always `None`.
     **Removed 2026-09-28, all four** (the user's OK). Pre-1.0 means no back-compat, so there is
     no `:start=` load error either. The split path's arithmetic is now the only sizing path, minus
     the start-address subtraction, so the pre-rounding allocation is 0-1 GiB larger than before.
     `reserve_bytes` is now reference − allocation. It used to *add* the per-thread rounding
     overage, although the overage comes out of the reserve. The memory table lost its "Strategy
     Type" row, which only repeated the footer once the percentages went. A leftover `:start=…`
     is not handled specially: after `-from-available`/`-from-total`/`-target` the parser ignores
     trailing text, and on a bare amount (`2048MB:start=…`) it fails to parse.
- **Recorded, no change:**
  - `per_test_cycle_multiplier` in the results JSON is a float copy of `default_test_cycles`
    (`config.rs:560`). The value itself is applied through `TestTiming.cycles`, so nothing is lost,
    but the name suggests a multiplier that doesn't exist.
  - The calibration sweep hard-codes its stability cuts (`spread < 1.5` at `calibration.rs:972`,
    `CV < 0.15` at `:1098`). The deleted config fields were a second, unused copy, and their values
    differed (spread 2.0).
  - `ProbeResult.converged` was never read, so callers never told convergence apart from timeout.
    `probe()` now returns just the samples.
  - **Behaviour change:** the system-info table is no longer gated on `GlobalMemoryStatusEx`
    succeeding. Its memory/topology fields were never read, so the gate withheld the whole table
    over data it never showed. That call practically never fails.

**Found 2026-09-27:**
- **Fixed 2026-09-28: turning pinning off was ignored by the worker pool.** The switch is the
  `--disable-pinning` flag, or `system.cpu_pinning.enable_pinning: false` in JSON; there is no
  `pinning=` key. `cli.rs` honoured it (no CPU selection), but the runner built its own
  `CpuPinningConfig::default()` (`runner.rs` ~690, unchanged since before the rebuild), so the pool
  pinned every worker anyway, to CPUs `0..N`, and the results file recorded `pinned: true`. Now
  `RuntimeConfig.pin_threads` carries the setting to the pool, and `pinned` records it. Pinned
  stays the default. A thread-priority key would take the same path. When it is added, warn on
  `realtime` with a worker on every logical CPU, since that can starve the console and input. With
  cores left free it is a real option (the user's view, 2026-09-27).
- **Unpinned runs still label every thread with a CPU** (found fixing the above). With pinning off,
  `cpu_list` is just `0..N`, and everything downstream treats it as where the thread runs: the
  per-thread tables' CPU column (`formatters.rs` ~378, ~518, ~1195, ~1336), the physical-core and
  NUMA lookups (`converters.rs` ~464), and the JSON's `cpu_assignments`. The JSON's
  `pinned: false` qualifies its copy; the console tables don't. The allocator also places each
  thread's memory on the NUMA node of "its" CPU (`allocator.rs` ~163), which an unpinned thread may
  never run on. That is inherent to letting the OS schedule, but it should be said.
  **Fixed 2026-09-28: CPU selection now runs when unpinned.** It used to be skipped
  (`cpu_list` = `0..N`), so `cputype=cores`, `cpu-skip` and `cpu-stride` were ignored, and on a
  multi-node machine running fewer threads than CPUs all the memory landed on node 0. Now memory is
  placed exactly as in a pinned run, and the pool skips only the affinity call, so a
  pinned/unpinned A/B differs in scheduling alone. Dropping the NUMA request instead would have been
  worse: allocation runs on the main thread before the workers exist, so Windows would put
  everything on the main thread's node.
  **Display done 2026-09-28, the user's design.** The CPUs stay, because they are what decides
  each thread's NUMA node, and a user has to see them to know what to change (`cputype`,
  `cpu-skip`, `cpu-stride`). `*` would hide how the node came about. When unpinned: the start-up
  topology table is shown (it was skipped), with `Thread ID ⚠️`, and "Performance by Thread", the
  first end-of-run table, gets `L CPU ⚠️` and `P Core ⚠️`. Both carry the same footer:
  "⚠️ Pinning is off: the OS moves threads between CPUs. A thread's CPU here only chooses the NUMA
  node its memory comes from." The start-up `CPU Assignment:` line says "NUMA placement only". Left
  unmarked on purpose (the user: warn and move on): Performance by CPU and by Physical Core,
  Per-Thread Block Allocation, the per-test thread tables. The results file already has
  `pinned: false`. Don't sample `GetCurrentProcessorNumberEx`, which gives one moment while the OS
  keeps moving threads.
- **Every thread's share is rounded up to a whole GiB, which can exceed available memory**
  (found 2026-09-28, removing the split path; a round-down fix was tried and reverted the same day,
  see below).`round_up_to_chunk_combination` is meant to round
  to a mix of 1 GiB/512/256/128 MiB. But its first step (1 GiB, `div_ceil`) always covers the
  remainder, so it returns there and the smaller steps never run. The overage comes out of the
  reserve, and with many threads it can be more than the reserve. Example: 60 GiB available, 10%
  reserve, 16 threads → 54 GiB → 3.375 GiB/thread → 4 GiB → 64 GiB planned, 4 GiB more than is
  free. `reserve_bytes` now shows 0 in that case instead of hiding it. Decide the rule: 128 MiB
  granularity as intended, round *down* to whole GiB (a whole number of 1 GiB huge pages per
  thread may be the real intent), or round up but cap at reference − reserve.
  Not new: the same rounding is in the oldest commit that has this code (`3807cce`, 2025-08-28),
  and the old reserve figure hid it by *adding* the overage. The user asked whether this should be
  a tunable. Proposed: no. Every thread already gets the same size, and the rounding only decides
  whether that size is a whole GiB. The allocator splits each share into power-of-two blocks
  (`allocator.rs` ~330, 4 GiB down to 16 MiB). Blocks of 1 GiB and up try 1 GiB pages, so the only
  cost of not rounding is a tail under 1 GiB per thread on 2 MiB pages. A percentage reserve can't
  reach a whole GiB per thread, since free memory drifts between runs, but `memory=48GiB-target`
  with 16 threads can. So: round each share *down* to 16 MiB, the planner's smallest block, and the
  reserve becomes a floor. Anyone who wants whole GiB per thread uses a target.
  The user's rule (2026-09-28): no tunable, and don't change the rounding unless it is worth it.
  **Tried 2026-09-28 and reverted the same day.** The user's run (4 threads, 55.5 GiB available,
  10%) came out worse. The share became 12.469 GiB, so the plan became 3 × 4096 MiB plus a
  256/128/64/32 MiB tail, and with no 1 GiB block in the plan the allocator's fallback misfired
  (next item): 1 GiB orphaned, threads at 12.00-12.38 GiB of 12.47, where whole GiB had given a
  clean 13 GiB (3 × 4 GiB + 1 GiB) to every thread. So the round-up is back, as
  `PER_THREAD_STEP_BYTES` = 1 GiB with `div_ceil` in place of `round_up_to_chunk_combination`: the
  same sizes as before. **The over-commit moved to #75 B** (2026-09-28), with its options, because
  the choice depends on the allocator fix in #75 A.
  **Where rounding happens** (checked 2026-09-28, for the user's "show rounding wherever it
  happens"): only here, per thread. The memory table shows it at thread level (Per Thread Raw,
  Round To "up", Rounded) and its total effect (Testing Allocation, and Reserve Rounding Diff,
  labelled "Threads × per-thread rounding"). Nothing later rounds: a whole-GiB share splits into
  plan blocks exactly, and every block is a whole number of its pages (`backend.rs` ~198). The one
  thing that changes a thread's size after this is the fallback under fragmentation, which is a
  shortfall, not rounding.
- **The allocator's huge-page fallback can orphan a block and leave threads short** (found
  2026-09-28 from the reverted round-down). **Moved to #75 A**, bundled with the stitched allocator.
- **Recorded, no change: `-target` and `-from-total` print a failure-mode warning** ("🧪 FAILURE
  MODE TESTING DETECTED …", `converters.rs` ~52). The user (2026-09-28): these forms are both
  normal ways to size a run and failure-mode tools. From-available is the recommended form, because
  it is least likely to exhaust the system, and the others, set too high, can exhaust physical
  memory. So the warning stays.
- **Done 2026-09-28: the memory plan table folds in the strategy** (the user's layout). After
  the separator: Reserve Requested (Target Requested for `-target`), Threads, Per Thread Raw,
  Per Thread Round To, Per Thread Rounded, Testing Allocation, Reserve Left, Reserve Rounding Diff.
  Percentages are of the spec's reference, and the row says which (available or total installed).
  The footer and the `Memory Strategy:` block printed before it (`cli.rs`) are gone.
  `allocation_type` still goes to the results file (#72).
- **`AllocationResult::print_summary` prints nothing.** It is the fallback for when the
  consolidated memory report fails to render (`layout.rs` ~46), and it builds a list of warnings
  and then drops it. The same is true at HEAD. Either print the list or delete the fallback. It is
  minor, because that render practically never fails.
  **Fixed 2026-09-28**: it prints one line (allocation, threads × share, reserve left, mode) and
  then the warnings.

**Keeping it closed:** #69 E (`pub(crate)` by default) is what makes an uncalled function loud again.
That is the enforceable half. The config-field half has no lint; the scan is the check.
