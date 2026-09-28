# TODO changelog, up to 2026-09-28

> The "Last Updated" log from the top of `TODO.md` and the "Last archived" log from the top
> of `TODO_ARCHIVE.md`, moved here verbatim on 2026-09-28. Entries now carry their own dates.

## From `TODO.md`

# TMR Project TODO

Central TODO list for TMR-APP development. (TMR-MD, the kernel driver, is shelved — its client
code was purged from TMR-APP on 2026-09-14; see archived #4/5.)

Completed/DONE tasks live in **`TODO_ARCHIVE.md`** — this document tracks only active work.

**Last Updated**: 2026-09-14 — **#4/5 CLOSED and archived**: the TMR-MD driver client is purged
from TMR-APP. `src/driver/` (1,395 LOC) plus every IOCTL, config key (`use_driver`,
`driver_chunking`, `remap_mode`, `default_memory_type`), CLI flag (`--driver-chunking`,
`--batch-remap`), KMDF version check and driver report is gone; `DmaConfig`, `to_dma_config` and
`allocator_BACKUP.rs` went with it as already-dead. **Kept on purpose**: the `Backend` trait,
`BackendType` enum, `MemoryType`, `PageType`, `PageSizeLevel` — a ring-0 backend would return as
one more `Backend` impl, so the seam survives. `TMR-MD/` is left untouched on disk (it is not in
git, so a revival means resurrecting that tree, not reverting a commit). **Two live bugs fixed on
the way out**: `auto_detect_backend` preferred a stale installed driver over VirtualAlloc2, and
`huge_pages_available` was reported as "is the driver handle open?" — i.e. "no 1GB pages" on every
machine. Decision recorded in both `CLAUDE.md`s + `AGENTS.md`; clippy clean, release build OK.

**2026-09-09** (#19 Part A FULLY COMPLETE — SIMD `_impl` variants macro-ized +
migrated to TestRunner + memset-defeated + N=4 MLP, commit d818d44. Also DONE and archived:
#30 two-stage CPU selection (`skip-cores=` filter + `cpu-stride=` spacing, new
`src/cpu_selection.rs`), #31 per-thread P-Core label fix, #32 clippy `as_chunks` — see
TODO_ARCHIVE.md. clflushopt now uses the `_mm_clflushopt` intrinsic. Zero clippy warnings.
#61 CLOSED — `doc/simd_codegen_rules.md` + CLAUDE.md rules (commit 74a8b65).
**#59 CLOSED + VALIDATED (commits 69ceca9, then follow-up)** — CLFLUSHOPT-verify is a reusable
phase (`flush_before_verify`), now also settable per-test from JSON configs. Chunk-size sweep
(64 KiB→256 MiB, Intel 4T/8T) proved the point: **flush=off varies 3.1× with chunk size while
flush=on is flat within 1.6%** — the flush makes the test chunk-size-*independent*, it isn't
merely "slower = more thorough". `Mem-StuckBit-Flush*` accordingly repointed to
`ChunkMode::Cache{L2, 0.5}` (was a ~6% fraction, i.e. the 1.19× no-op zone). Full table in
`doc/cache_management.md`. Two gaps found and fixed in the process: flush names were missing
from `get_test_function_by_name` (JSON configs silently fell back), and `TestConfig` had no
`flush_before_verify` field (so the flag was unreachable from config).
**2026-08-20**: #19 Part B re-scoped. **REP MOVSB CUT** (step 5) — no coverage gain over
`Mem-BlockMove`, and benchmarked slowest of six primitives (#65). Remaining order, easiest-first:
**address-in-address → random writes → moving inversions → cross-thread page exchange → extended
bit fade**. Three new items: **#63 WHEA error monitoring** (DDR5 on-die ECC silently corrects
single-bit errors, so a marginal overclock can pass every pattern test — WHEA-Logger is our only
user-mode visibility), **#64 heat-soak/saturation validation**, **#65 instruction survey**
(reference; records negative results incl. `-Z build-std` measured as pure noise so they are not
re-proposed).
**2026-09-08**: **#67 raised (High)** — results files record no run identity and no resolved
config, so regression comparison is unreliable. A 2026-07-31 vs 2026-09-08 compare reported
StuckBit "+10-20%" and Refresh "-14%" that were **entirely** a 56 GiB → 24 GiB allocation
difference, reconstructable only from `logs/`. `SystemInfoSnapshot` turns out to be dead
placeholder data (never written since `results.rs:219`), while `machine_id` +
`memory_fingerprint` are already computed on every run and simply never stored. Covers three
things: (1) system info + fingerprint in results with mismatch warnings on compare, (2) test
config + resolved allocation/thread/page-size-mix at run level, (3) revisit the results-vs-log
boundary — couples to **#29**.

**2026-09-09**: **#67 CLOSED and archived** — items 1+2 landed in commit b58db84
(`src/run_context.rs` stamps `RunIdentity` + `RunConfigSnapshot` into every result file;
`--compare-results` emits `run_differences` and flags comparisons it cannot attribute; no backwards
compatibility, so **delete pre-#67 baselines in `results/`**; JSON presentation trimmed). Then the
last unblocked piece was fixed: a run's log and result files now share one filename stem, built from
a single captured instant (`run_context::run_start`) — they previously differed by both timezone
*and* ~12 s, because logging is initialised before allocation and results after `ThreadPool::new`.
**Console + log are local and stamped as such** (the console formatter was printing `Local::now()`
with a literal `Z`, i.e. claiming UTC); **results carry both** local-with-offset and UTC.
**Item 3 folded into #29** rather than left open — the results/log boundary can't be settled without
#29 *and* #7, since a GUI forces an engine/presentation split that makes the real question "what
structured events does the engine emit". #7 and #29 now cross-reference each other.
**#66 CLOSED** — `SAFETY:` pass done on all 26 FFI sites and made enforceable with
`#![warn(clippy::undocumented_unsafe_blocks)]` scoped to the 5 FFI files (which immediately caught
4 gaps the manual pass missed). Archived. **#68 raised** — #63 WHEA and #66's alignment fixes are
both shipped but have **never run on real hardware**; that needs a run, not code.

**2026-08-24**: **#63 WHEA monitoring IMPLEMENTED** — new `src/whea.rs` (`EvtSubscribe`
signal-event form, drained by the existing 2s reporter thread), per-test **WHEA / WHEA Corr**
report fields threaded through `TestSummary` → `TestResult` → `TestAverage` → `OverallStats` →
the console tables (conditional columns, so healthy output is unchanged), and any WHEA event now
fails the run. Also fixed a latent bug it exposed: the outer `success` flag was **shadowed** by a
local inside `execute_test_cycle`, so memory errors never reached `final_success` — the verdict now
derives from `progress.total_errors`. See #63 for the full write-up.

**2026-08-20 — BUG FIXED in `memory/backend.rs`**: `allocate_with_virtualalloc2` stored a raw
pointer to a **block-scoped** `MEM_ADDRESS_REQUIREMENTS` in `extended_params`, then called
`VirtualAlloc2` (where the kernel dereferences it) after that scope had ended — dangling stack
pointer on **every** allocation path, in **two** duplicated blocks. Hoisted to function scope and
the two blocks collapsed into one. Invisible to both the borrow checker (the raw cast erases the
borrow) and clippy. Note the "flaky huge pages" theory is **unproven** — huge-page failures have an
ordinary cause (physical fragmentation), so don't assume that symptom went away. Follow-up audit
tracked as **#66**.

**2026-09-11**: **#33 CLOSED and archived** without doing the proposed upstream-issue sweep — that
sweep is a point-in-time snapshot of a live tracker and would never truly close, while everything it
protected is already banked: both traps documented, the intrinsic-surface inventory done (all
single-instruction, no emulation path), and the `--emit asm` check now promoted into
`TMR-APP/CLAUDE.md`. `_mm_clflushopt` — the one intrinsic TMR needed — landed upstream
(stdarch#2141); `MOVDIR64B`/`MOVDIRI`/`CLWB` were benchmarked in #65 and beat none of TMR's existing
primitives, so contributing them is optional open-source work, not TMR work.

**Next: #68** hardware verification (no coding — just runs), or **#27** two-tier verifier (now
load-bearing: StuckBit/Refresh dropped intermediate error localization), or **#19 Part B step 5**
(address-in-address), or **#25** curated plans.
**14 active items**: 68, 27, 28, 29, 25, 3, 7, 13, 19, 63, 64, 65, 16, 22.
New hardware/perf knowledge: memory/simd-loop-optimization.md, memory/cpu-memory-domains.md,
refresh-test/) |
2026-06-12 (#19 Part A scaffolding complete; archive split) | 2026-06-01 (clflushopt target-
feature path) | 2026-05-28 (#27 two-tier verifier + #28 worker model added)

---


## From `TODO_ARCHIVE.md`

# TMR Project TODO — Archive (Completed Tasks)

Completed/DONE tasks moved out of the live `TODO.md` to keep that document focused on
active work. Nothing here is pending — this is a historical record. For active tasks see
`TODO.md`.

**Last archived**: 2026-09-28 (added #71 lost-wiring scan — closed; its allocator items moved to
#75) | 2026-09-14 (added #4/5 TMR-MD driver assessment — closed by purging the driver
client from TMR-APP; `TMR-MD/` itself left on disk, see its entry) |
2026-09-11 (added #33 SIMD codegen-trap audit — closed without the upstream sweep,
see its entry for why) | 2026-09-09 (added #67 run identity + config in results, #66 FFI `unsafe`
audit) |
2026-07-29 (added #30 two-stage CPU selection, #31 per-thread P-Core label
fix, #32 clippy `as_chunks` — all from the multi-CCD memory-domain investigation) |
2026-06-05 (moved #26, #20, and the full Completed Tasks section out of the live TODO during
the #19 Part A completion compaction).

---


## The Notes section from the end of `TODO.md`

## Notes

### File Locations
- **TMR-APP**: `./TMR-APP/` - Userspace memory testing application
- **TMR-MD**: `./TMR-MD/` - Windows kernel driver — SHELVED, not referenced by TMR-APP, not in git
- **Completed tasks**: `./TODO_ARCHIVE.md` - DONE/finished tasks moved out of this doc
- **Archived Analysis**: `./Old/TMR-APP-Analysis/` - Historical planning documents

### Quick Commands
```bash
cd TMR-APP && cargo build --release
cd TMR-APP && cargo test
./TMR-APP/target/release/tmr.exe --quick-test
```
