# TODO 69. [TMR-APP] Dispatch-layer dead code + harness hygiene

> Full record, moved verbatim out of `TODO.md` on 2026-09-28 (open item; closed 2026-10-01).
> The short entry, now in `TODO_ARCHIVE.md`, holds the current status; this file holds the reasoning.

**Priority**: Low-Medium (A/B are pure deletion and remove a live footgun; C is a one-line fix;
D is a real refactor and may be declined outright) — **but E is the one with leverage**: it is the
*reason* A/B/E-class dead code can exist at all in a crate that reports zero clippy warnings.
**Status**: A, B, C, E **done 2026-09-26** (see the block at the end of this item). F is kept under
`#[expect]`, as decided. **D is still open.** Raised 2026-09-14, from writing `doc/test_harness_tiers.md`.
**E added 2026-09-21** (found while verifying #68A's `whea_monitored` flag).
**F added 2026-09-23** (found while adding the `Pass` column to the report tables).

Six findings from auditing the test-dispatch layer while documenting the execution tiers. A-C are
cleanup with no behaviour change. D is a judgement call, kept here so it stops being invisible.
E is a lint-visibility fix that makes A/B/E-class findings compiler-detected instead of found by
hand months later. F is one more instance of E, in the reporting layer.

**A) `TestFunction::Simple` / `WithConfig` are dead — delete them and their dispatcher.**

Both variants and their type aliases (`TestFunctionSimple`, `TestFunctionWithConfig`) exist in
`runner.rs`, and both are dispatched in `run_test_with_memory_stages` — but **nothing constructs
either variant anywhere in the crate**. They are the pre-MultiBlock single-pointer signature
(`fn(*mut u8, size, …)`), left behind by that migration.

`run_test_with_memory_stages` is dead along with them: its only two reachable arms (`MultiBlock`,
`Latency`) both `return Err("… must be called with all blocks at once")`, and the live path is
`ThreadPool::execute_test`. `thread_pool.rs` already asserts this with
`unreachable!("Only MultiBlock and Latency tests are registered")`.

Delete: both variants, both type aliases, `run_test_with_memory_stages`, and the now-total match
in `thread_pool.rs` loses its `_ =>` arm — which is the actual win, since the compiler then
enforces exhaustiveness on `TestFunction` instead of a runtime `unreachable!`.

**B) `ThreadPool::execute_latency_test` / `WorkItem::RunLatencyTest` are dead, and carry a live
footgun.** No caller outside their own definitions; the live path is `execute_test` →
`WorkItem::RunTest`, whose match already handles `TestFunction::Latency` itself.

The footgun: the dead worker arm **hardcodes `crate::latency_tests::read_latency_multi` regardless
of `test_name`**. If anyone ever wires `execute_latency_test` up (the name invites it), every
`Lat-*-Write` and `Lat-*-Copy` silently runs the *read* test and reports plausible numbers under
the wrong name. That is the same class of failure as the SIMD codegen traps — correct-looking
output, wrong measurement — so prefer deleting it over "fixing" it. Delete the method, the
`WorkItem` variant, and its worker arm.

**C) `block_move_multi` reads `config.parameter_context` inside the chunk loop** (`tests.rs`, the
`copy_dirs` binding). An `Option` chain + `.expect()` per chunk. It is *between* chunks so it is
not a hot-path violation, and `Mem-BlockMove` will not measurably change — but every other test
hoists its `parameter_context` fields to locals before the loop (`cache_busting_multi`,
`stride_access_multi`), and CLAUDE.md states the rule ("never push a struct field-read into a hot
loop; destructure into locals up front"). Move it above `loop {}` for consistency.

**D) Bandwidth + latency tests duplicate the scaffolding — decide whether to care.**
`bandwidth_tests.rs` (3 macros) and `latency_tests.rs`/`latency_tests_v2.rs` (~12 macros)
hand-roll window sizing, the cycle timer, the 250 ms progress throttle and the `TestStats`
literal, instead of using `TestRunner`. So the "one source of truth" that #19 Part A established
for Tiers 1 and 2 stops at these files.

Reasons not to bother, which are real: no error checking means `should_halt` and the error-mode
dispatch — roughly half of `TestRunner` — is moot; `spd_*` deliberately does **not** chunk (it
walks a whole block per cycle, because a chunk boundary inside a pure-bandwidth measurement is a
loop boundary in the thing being measured); and the latency tests return `LatencyTestStats`, not
`TestStats`, so `finish_completed`/`finish_aborted` do not fit without a second constructor.

A partial adoption (`TestRunner::new` for window/block prep + timers + `update_progress`, keeping
bespoke stats) would remove most of the duplication and is the only version worth doing. Any
touch of these loops needs the asm check per CLAUDE.md, which is most of the cost.

**Not part of this item — two things it is easy to conflate with D:**
- **Latency tests already run in the main pipeline on the plan's memory.** They take
  `&[AllocationBlock]`, go through `ThreadPool::execute_test` as `TestFunction::Latency`, and call
  `prepare_blocks_for_window` on the plan's own blocks. There is no separate allocation and no
  "move them into the main harness" work outstanding. D is purely about internal bookkeeping
  duplication.
- **`calibration.rs` is the one that allocates its own memory** — that is the deferred question,
  and it is recorded as Open Design Question 1 in TMR-APP/CLAUDE.md, not here. It already has a
  `run_with_buffer()` / `run_extended_with_buffer()` path that borrows an existing `MemoryBuffer`;
  the open part is whether the standalone `run()` / `run_extended()` paths should go through the
  main allocator (page-type preference, NUMA placement, retry policy) rather than their own.

**E) Narrow the lib's public surface so `dead_code` can actually see dead code.**

**The mechanism — why clippy never flagged A, B, or the function deleted below.** TMR is a
**lib+bin crate**: `src/lib.rs` (lib target, crate name `tmr`) plus `src/main.rs` (bin), and
`main.rs` consumes the lib as a genuinely *external* crate (`use tmr::{…}`, 56 `tmr::` references).
rustc's `dead_code` lint asks "is this item reachable from the crate's public API?" — and for a
**library** target, every `pub` item behind an unbroken chain of `pub mod` **is** the public API.
The compiler cannot know whether a downstream crate calls it, so it must assume one might.
**Consequence: a `pub fn` in this crate can never be reported as dead, no matter how unreachable
it is.** This is not a clippy configuration gap — clippy just inherits rustc's `dead_code`, so no
lint setting fixes it. `unreachable_pub` does not help either: it flags `pub` items *not* reachable
outside the crate, and these genuinely are reachable.

So **"zero clippy warnings" (#20) means "no dead *private* code"** and says nothing about the
**352 `pub fn`s**, which are structurally exempt from the analysis.

**The controlled experiment that proves it** (both unused, same file, same edit):

| Item | Visibility | What the compiler said |
|------|-----------|------------------------|
| `create_final_test_summary_report` (142 LOC) | `pub fn` | **silence**, for as long as it sat there |
| `format_duration` (`converters.rs:798`, 14 LOC) | plain `fn` | `never used` on the *first* `cargo check` after its caller went |

Identical dead status; the only difference is the `pub` keyword.

**DONE 2026-09-21 — the instance that exposed this is deleted.**
`reporting/converters.rs` 953 → 795 LOC: `create_final_test_summary_report` (142 LOC, **zero
callers repo-wide**, verified three ways — one textual occurrence in the whole repo, no sibling
probe crate has any path dependency on `tmr`, no `pub use` re-export chain), plus the private
`format_duration` it exclusively kept alive, plus the then-unused `CycleStats` import. It also
**hardcoded `whea_monitored: true`** with the comment "this path has no handle on the monitor's
state" — the exact false-clean-bill-of-health that #68A check 1 exists to catch, so it was a
latent footgun of the same class as B, not merely unused. The live path
(`create_overall_stats_summary_report` ← `runner.rs:4319`) correctly reads
`overall_stats.whea_monitored` from `progress.whea.is_active()`, so **#68A check 1 needs no code
fix.** `clippy --all-targets` clean, `cargo build --release` clean. `cargo test` / `--quick-test`
still owed (user runs those).

**The proposal.** Blanket `pub(crate)` is not available — the bin really does import from the lib —
but that set is **bounded and small**: 7 `use tmr::…` lines in `main.rs`. So:

1. Downgrade lib items to `pub(crate)` by default.
2. Build; promote back to `pub` only what the bin actually demands.
3. `dead_code` is then re-armed permanently over the ~90% of the surface the bin never touches.

This converts a standing audit task into a compiler-enforced invariant — the preferred shape per
the TODO-closure convention (an enforceable lint beats a recurring manual sweep).

**Scale of what is currently invisible.** A crude scan (`pub fn` names with exactly **one**
repo-wide textual occurrence, i.e. only the definition): **62 of 352**. Evidence it is signal, not
noise — **`execute_latency_test` is on the list**, which is item **B**, found by hand weeks
earlier. E would have surfaced B automatically. Also on it: `create_cycle_report`,
`create_operation_breakdown`, `create_memory_allocation_report`,
`create_test_configuration_report` — the *same* reporting-converter family as the function deleted
above, so that family is the first place to look.

**Caveat — the 62 is a candidate list to eyeball, not a delete list.** Textual matching produces
false positives: trait-impl methods reached by generic dispatch, macro-generated call sites, and
generically-named items (`get_default`, `from_parameter`, `backend_name`) that may be trait
members. Deliberately **out of scope**: the `Backend` trait / `BackendType` / `MemoryType`
vocabulary, which CLAUDE.md keeps *on purpose* as the ring-0 revival seam — a visibility sweep must
not "clean up" those (they are `pub` for a reason, and a future ring-0 backend is the consumer).

Sequencing note: do **E step 1** before hand-auditing anything, so the compiler produces the list
instead of grep. A and B then likely fall out of it for free.

**F) The whole per-cycle report path is dead — three layers of it.** Found 2026-09-23 while
reworking the report tables for the WHEA `Pass` column; **deliberately left in place** (the user's
call: don't mix a deletion into a reporting change). It is exactly the family E predicted —
`create_cycle_report` is already named above as one of the 62 candidates.

The chain, top to bottom, with no external caller at any level:
- `reporting/formatters.rs:1176` `prepare_cycle_report_table` (to ~1227)
- `reporting/mod.rs:151` `report_cycle` — **zero callers repo-wide**
- `reporting/models.rs:405` `CycleReport`, built at `reporting/converters.rs:267` and `282`

Two reasons it matters beyond LOC:
1. **It is drifting.** The live tables were just converted to a `Pass` column with ✅/❌ and plain
   numbers in the error cells. `prepare_cycle_report_table:1199/1217` still carries the **old
   ✅-on-zero convention** (`if errors > 0 { number } else { "✅" }`) that every live table has now
   dropped — so reviving it as-is would print a table that disagrees with the rest of the output.
   Same shape as B's footgun: not broken, just silently wrong if trusted.
2. **It is another `pub`-in-lib+bin blind spot** (item E). All three levels are `pub`, so
   `dead_code` is structurally incapable of reporting them, and `clippy --all-targets` is clean
   with them present. E step 1 (`pub(crate)` by default) surfaces this whole chain for free —
   **don't hand-delete it ahead of E**, let the compiler produce it.

**Decided 2026-09-26: revive, don't delete** (the user's call). A between-cycle report, shaped like
the final summary but covering only the cycle that just finished, is wanted later. So F is
not cleanup. It is a table that will be kept in step with the live ones and wired up again later.
- **How it went dead**: no one switched it off to cut console noise. The pre-2025-08-28 runner
  (`recovered_runner.rs` in `3807cce`) called a `print_cycle_report` after every cycle, and again on
  interrupt as `=== Interrupted Cycle N Report ===`. The runner was rebuilt after it was lost in that
  commit. The new table-based `report_cycle` landed in the same commit, but nothing ever called it.
  `git log -S "report_cycle("` shows no commit that removes a call.
- **Conventions**: brought in line with the live tables in the same pass as the WHEA-column
  rework (#63), so reviving it later means adding the call site and nothing else.
- **E interaction**: E step 1 (`pub(crate)`) *will* flag this chain. Keep it anyway: add an
  `#[expect(dead_code, reason = "…")]` pointing here, don't delete it. `expect` rather than `allow`
  so the attribute becomes a warning the day the chain is wired back in.
- **Where it gets wired**: the call belongs in whatever #29 decides the engine emits per cycle. Until
  then, the old call point is the end of each cycle in `run_tests_with_layout_and_timing_filtered`,
  plus the interrupt path.

**Verification for A-C, E and F**: `cargo clippy --all-targets` clean, `cargo build --release`, and
`--quick-test` unchanged (A, B and F are unreachable code, C is between-chunk, E is visibility-only
and cannot change codegen). No asm check needed — none of A-C, E or F touches a kernel.

**DONE 2026-09-26 — A, B, C, E (F kept, D open).** `check`/`clippy --all-targets` at 0 warnings,
`build --release` clean; `cargo test` / `--quick-test` owed (the user runs those — calibration's
tests changed). 36 files, ~3,370 net LOC deleted.
- **E went further than proposed.** Rather than promoting what the bin imports, the old `main.rs`
  body moved into the lib as `src/cli.rs`, and `main.rs` is a 4-line shim calling `tmr::cli::main()`.
  `pub mod cli` is the lib's only public module; every other module is private, items `pub(crate)`.
  The bin needs nothing else, so `dead_code` now covers the whole crate. First check: **169
  warnings**. It also re-armed two clippy lints the `avoid-breaking-exported-api` default had been
  suppressing on the public surface, both fixed rather than allowed: `upper_case_acronyms`
  (`CacheTarget::DRAM` → `Dram`) and `large_enum_variant` (`WorkItem::RunTest` boxes its
  `TestMemoryConfig`, one allocation per dispatch, off the hot path).
- **A, B** as specified. The pool's match is exhaustive, with no `unreachable!`. B also took
  `LatencyWorkResult`, `TestStatsTuple` and the driver-era `execute_memory_type_change`.
- **C** hoisted above `TestRunner::new`, with the other tests' `parameter_context` reads.
- **F**: `create_cycle_report`, `prepare_cycle_report_table`, `report_cycle` and
  `TestPerformanceEntry.throughput_gib_s` carry `#[expect(dead_code, reason = "TODO #69 F: …")]`.
- **Deleted, by family** (the ones E found, plus their exclusive callees):
  - the rest of the reporting chain: 21 report models, 5 `report_*`, 13 `prepare_*`/`format_*`,
    6 `Renderer` trait methods (`render_info`/`render_error`/`render_success` came back on
    2026-09-27 under `#[expect]`, as severity levels for #29's event stream), and the second
    progress line (`report_test_progress`, which #71 named);
  - `formatting.rs`'s unused helper set;
  - calibration's `TierCalibrator` (a second calibrator the sweep superseded), its results-file
    cache (`load_from_file`/`save_to_file`/`matches`, `app_config::clear_stale_calibration`), and
    every `ProbeStatistics`/`ProbeResult`/`CalibrationConfig` field only those read;
  - `tests.rs` operation-metadata scaffolding (`DetailedOperationCount`, `SIMDType`,
    `AccessPattern`, `get_operation_metadata`, `expand_operations`, …);
  - `runner.rs` `setup_signal_handler` (an empty stub, though the live suite entry called it) and
    `run_tests_with_layout{,_and_timing}`, and `results.rs` `save_test_result` and
    `TestRunResult::to_text_report` (not `TestComparison`'s, which `--compare-results` prints). None
    of this is lost wiring (see #71's old-runner calls);
  - **not dead, simplified, then restored:** `PerformanceConfig`/`ThreadPriority`. The pool applied
    `PerformanceConfig::default()`, which was always `High`, so the enum's `Normal`/`Realtime` arms
    could never run. #69 collapsed it into one function. On 2026-09-27 the enum came back without
    the one-field `PerformanceConfig` wrapper; see #71;
  - 14 unused `constants.rs` consts (consolidating the rest is #73), the unused `AllocationConfig` builder, `cpu_topology`
    `NumaNodeInfo`/`GroupMask`, and `params.rs` `generate_help`/`get_default`/…;
  - config fields that were parsed but never acted on. No lint can see these, so they are
    listed under #71.
- **Also kept under `#[expect]`, each with its reason:** `calibration::run_with_buffer` /
  `run_extended_with_buffer` (CLAUDE.md Open Design Question 1: the path for calibrating on the
  plan's own memory); `config::get_test_configs_with_sequence` + `LegacyTest.id` (#74, TM5
  `Test Sequence`);and the revival seam (`Backend::name`, `AllocationConfig.memory_type`,
  `MemoryType`, the `PageType::Mixed` count) per TODO #4/5.
- **CLAUDE.md's Source File Map is stale** (`main.rs` → `cli.rs`, and most LOC figures). Not updated here.
