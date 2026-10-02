# TODO 75. [TMR-APP] Allocators: fix the legacy fallback, a tunable minimum block, and the stitched allocator beside it

> Full record, moved verbatim out of `TODO.md` on 2026-09-28 (open item).
> The short entry in `TODO.md` holds the current status; this file holds the reasoning.

**Priority**: High. The fallback leaves threads short on a fragmented machine, and the stitched
allocator is meant to replace it.
**Raised**: 2026-09-28, the user's bundle: "fix that allocator bug … make the minimum block size
tunable AND integrate the new allocator", keeping both for now. A and the over-commit in B came
from #71. Adapting the tests is #76.
**Status**: Closed 2026-10-02 (A, B and C, with the review follow-up below). D moved to
TODO 80. The outcome is next; the original record follows it unchanged.

## Outcome (2026-10-02)

As first delivered. The follow-up below moved both allocators onto one fill order and replaced the
cap's fallback; where the two disagree, the follow-up is current.

**A, fixed.** `execute_plan_page_type_first` no longer pools. `fill_gaps` runs each page-size tier
(1 GiB, then 2 MiB, then 4 KiB if `minpage` allows) down a power-of-two ladder from 4 GiB. 1 GiB
pages stop at 1 GiB; the other tiers stop at `largefloor`. At each size it runs rounds of one block
to every thread whose gap still holds it, largest gap first. The first refusal ends that size for
the node. Every request fits one thread's gap, so nothing is over-allocated and nothing is freed.
Phases 1b/2b and `distribute_planned_chunks` are gone from this path; plan-blocksize-pref still
uses the latter.
- Unit tests on a fake page-pool backend (`allocator.rs` tests). The 2026-09-28 shape (33 free
  pages, 4 × 12.47 GiB) ends exact at 9/8/8/8 1 GiB pages, with only the refusals that found the
  pool short. Also covered: the 4 KiB fallback per gap, short threads under `minpage=large`, the
  ladder, and the setting checks.
- Display. Per-Thread Block Allocation and the fairness table compare each thread with its target:
  `vs Target`, ⚠️ Short / Over. On the same node they flag "N fewer 1 GiB pages" when a thread
  is more than one page behind the best-served one. The footer gives the 1 GiB pages per thread as
  min-max. Rows are sorted by thread.

**B, done as tunables plus a cap.** The user chose the ladder and tunables over prediction:
granular defaults, raised on big machines. Predicting when the 1 GiB pool will run out can only be
a guess from available memory, so it is not attempted.
- `largefloor` (16 MiB) is the one minimum-block name: the bottom rung of plan-pagesize-pref and
  stitched's smallest commit. `hugechunk` and `largechunk` (1 GiB each) are stitched only.
  greedy and plan-blocksize-pref keep the fixed `LEGACY_LADDER_MB`.
- `blkroundtarget` (1 GiB) and `blkround=up|down|nearest` (up) control share rounding before
  allocation, for every allocator. The step must be a multiple of `largefloor`, and no bigger than
  the allocator's largest block (`AllocationStrategy::top_block`).
- The over-commit is fixed by "round up, but cap at available". If the rounded share × threads
  would pass the reference figure (available for `-from-available`, installed otherwise), the share
  rounds down instead, with a warning. The memory table then shows "down (up exceeds available)".
- #73: the ladder and 16 MiB literals in `allocator.rs` are now `LEGACY_LADDER_MB`,
  `LEGACY_TOP_BLOCK` and `LEGACY_FLOOR`.
- Not done: an L3-relative floor warning.

**C, landed** as `allocator=stitched` (`src/memory/stitched.rs`), per `INTEGRATION.md` steps 1-6.
Dropped: Equalize, `BlockShape::PerThread`, and the page helpers only those used. Its sizes come
from `block_sizing()`. It has 21 unit tests. Free-then-allocate is the only path used; `Replace`
is dormant and not exposed. The CLAUDE.md Settled Design Decisions are updated (stitching, never
release, free-then-allocate, alignment), and so is the `tmr-design-rationale` skill.

**Live runs**, all `Spd-DRAMFull-Read-Auto`, 0 errors. Machine: AWS r8i.2xlarge, 62.87 GiB visible,
6 threads, 1 NUMA node, 23 free 1 GiB pages. Output is in `logs/live75_*.txt`.

| # | allocator | memory | result |
|---|---|---|---|
| 1 | stitched | `8GiB-target` | 6 × 2 GiB (1.33 rounded up: 12 GiB), all on 1 GiB pages, audit clean |
| 2 | plan-pagesize-pref | `8GiB-target` | 6 × 2 GiB, all on 1 GiB pages, exact |
| 3 | plan-pagesize-pref | default | Found the over-commit: 54 GiB on 53.73 available, 19 s of 2 MiB allocation. Led to the cap |
| 3b | plan-pagesize-pref | default, capped | Exact, 1 s |
| 4 | stitched | default | 6 × 9 GiB exact, 1 GiB pages 4/4/4/4/4/3, audit clean |
| 5 | plan-pagesize-pref | `1GiB-from-available blkroundtarget=16MiB` | 55.69 of 56.60 GiB, exact, 4/4/4/4/4/3 |
| 6 | stitched | same | 56.91 of 57.86 GiB, exact, 4/4/4/4/4/3, audit clean (29,136 samples) |
| 7 | stitched | `0%-from-available` | Cap fired: up meant 60 GiB on 58.43, so 9 GiB each, exact |

Not run live: multi-NUMA, `minpage=regular`, a raised `largefloor`, `hugechunk` above 1 GiB.

Seen along the way, and where each went:
- plan-blocksize-pref dealt pooled 1 GiB pages to the first threads: removed (follow-up).
- plan-pagesize-pref's ladder started at 4 GiB, so a short pool left threads up to one 4 GiB block
  apart: it takes `hugechunk` now (follow-up).
- The Per-Thread Block Allocation `CPU` column shows the thread id, and Per-Thread Allocation
  Breakdown is a strict subset of that table, in hash order: TODO 81.
- `cpu_list` identity: a false alarm. `detect_runtime_capabilities` fills in the identity, but
  `cli.rs` replaces it with the real CPU selection before anything is allocated.
- The usage examples printed once at the end of every run (not twice, as first reported): now
  only under `--help`. A `-target` above installed memory asks for all installed: it warns now.

## Follow-up after review (2026-10-02, same day)

The user's calls on the review, committed with the rest:
- **Removed greedy and plan-blocksize-pref** with everything only they used (`chunk_allocate`,
  `discover_chunks_per_numa`, `distribute_chunks_to_threads`, `allocate_with_page_type`,
  `create_allocation_plan`, `execute_plan_block_size_first`, `distribute_planned_chunks`,
  `LEGACY_LADDER_MB`, `LEGACY_TOP_BLOCK`, `is_page_size_allowed`). `allocator=` takes
  plan-pagesize-pref or stitched; anything else is an error naming both.
- **One fill order for both allocators**, `memory/fill.rs`: `fill_ladder` (least-filled first,
  ties to the thread served longest ago, a refusal halves the node's request size),
  `fill_regular`, the thread → node shares, the minpage/maxpage limits and the page audit. Each
  allocator implements `Fill`: plan-pagesize-pref asks for a power of two no bigger than the gap,
  since each request is a block; stitched for any multiple of the floor, since its commits merge.
- **plan-pagesize-pref takes `hugechunk` and `largechunk`** instead of a fixed 4 GiB top. The
  user's run (4 × 14 GiB, 23 free 1 GiB pages) split 8/6/5/4; the same case in a unit test now
  splits 6/6/6/5. The price: its blocks top out at 1 GiB by default. The user chose fairness;
  big machines raise `hugechunk`.
- **Cap, the user's design:** a share that would pass the reference rounds at the next power of
  two below the step, and so on down to `largefloor`; if none fits, down to `largefloor`. With a
  reserve this keeps the share coarse and greedy (10 % reserve, 6 threads, 53.73 GiB: 8.5 GiB
  each at 512 MiB, where a whole step down gave 8). With no reserve it ends at the 16 MiB round
  down. The memory table shows the step used, e.g. "up (1.00 GiB step exceeds available)".
- **Refusals sorted by code** (`backend::is_exhaustion`, `AllocError`): running out steps down,
  anything else fails, for both allocators. Before, plan-pagesize-pref stepped down on any
  error. A machine with no 1 GiB pages at all now fails with a hint to set `maxpage=large`, if
  Windows answers that with a code other than the exhaustion ones.
- `-target` above installed memory warns once. The usage examples print only under `--help`
  (and `/?`, `-?`); the two filler lines at the end of a run are gone.
- **Tie-break, the user's catch.** Ties first went to the thread served longest ago. After a
  2 MiB top-up, though, the threads never served in that tier are the ones that got the extra
  1 GiB pages, so they led the next round, and a 2 MiB shortfall landed on the shorted threads a
  second time. Ties now go to the thread with less on bigger pages, then served-longest-ago. In a
  unit test (7 free 1 GiB pages, 3 GiB of 2 MiB, 4 × 4 GiB) thread 3, one 1 GiB page short, now
  ends with 1 GiB of 4 KiB pages instead of 2.
- **Trace back on** (the user wants it while the allocators settle; remove near release): a ✅
  line per request size granted and a ❌ line per refusal, for both allocators and all three page
  sizes. The audit line is skipped when nothing is on large pages.
- Report tables: TODO 81.

**Live runs, second round** (2026-10-02, just after a reboot; 4 threads via `skip-cores=0
cputype=cores cpus=100%`; 49-50 free 1 GiB pages; output in `logs/live75b_*.txt`). All exact,
audits clean, 0 errors:

| # | allocator | settings | result |
|---|---|---|---|
| 1, 2 | both | the 2026-09-28 failure: `blkroundtarget=16MiB blkround=down`, `Mem-Refresh*`, 2 cycles | 4 × 13.094 GiB, 1 GiB pages 13/13/12/12, no "Unallocated" |
| 3, 4 | both | defaults | 4 × 14 GiB, 13/13/12/12; full-memory read 50.6 vs 50.3 GiB/s; allocation 4.1 vs 2.3 s |
| 5, 6 | both | `memory=0%-from-available` | 59.38 of 59.43 and 60.19 of 60.23 GiB: up fitted no step, so down to 16 MiB |
| 7 | plan-pagesize-pref | `memory=1%-from-available` | up to 1 GiB, 512 and 256 MiB passed available; 128 MiB fitted: 4 × 15.125 GiB |
| 8 | stitched | `hugechunk=4GiB largefloor=64MiB` | 12 × 4 GiB, 2 GiB refused, retried at 1 GiB: 13/12/12/12 |
| 9, 10 | both | `minpage=regular maxpage=regular` | 4 × 14 GiB on 4 KiB pages |
| 11 | plan-pagesize-pref | `maxpage=large minpage=regular memory=0%-from-available` | 59.94 GiB all on 2 MiB pages, 228 KiB left |

**Found: Mem-Refresh throughput depends on the block count.** Runs 1 and 2 differed by 40% on
Mem-Refresh (11.7 vs 16.4 GiB/s). The bytes are right; the time isn't comparable. Its window,
2 × (L1+L2+L3) = 977 MiB, is smaller than one block, and `prepare_blocks_for_window` takes one
power-of-two piece per block until the window is spent. On plan-pagesize-pref's 1 GiB blocks
that is 512, 256, 128, 64, 16 MiB and then 0.5, 0.25 and 0.12 MiB slivers: 8 pieces. On stitched's
8/4/1 GiB blocks it is 5. Each piece is a chunk, and Mem-Refresh sleeps 64 ms per chunk (debug runs,
`logs/live75b_dbg_*.txt`). It is a harness effect for TODO 76, not memory speed. Until then,
window-limited tests don't compare across allocators.

---

## Original record (2026-09-28)

**A) Fix the legacy fallback** (bug B4 in `doc/memory_system_design.md` §1).

The bug (found 2026-09-28 from the reverted round-down, moved from #71). In
`execute_plan_page_type_first` (`allocator.rs` ~417), when planned huge-page blocks fail, Phase 1b tries extra 1 GiB huge blocks, but only if 1 GiB is
not in the plan, and it sizes them against the node's whole deficit, not against any thread's
gap. Those blocks use up the byte deficit, so Phase 2a caps the planned blocks it still needed.
Then `distribute_planned_chunks` (~870) places planned sizes first and fills gaps only with
blocks that fit, so a 1 GiB block with no 1 GiB gap is dropped ("Unallocated: 1024MB chunk").
Whole-GiB shares make this rare, because the gaps are whole GiB too, but a 12 or 14 GiB share
with fragmented huge pages can still hit it. Two display gaps with it: a short thread shows only
in the log, and in Per-Thread Block Allocation's Total Size, with nothing comparing it to the
plan. And the fairness table measures deviation from the *mean*, not the plan, so threads 0.47
GiB short still read "✅ Fair".

It is the other side of archived #11. That fix made Phase 2a cap by `min(plan_still_needed,
byte_deficit / block_size)`, and that cap is what starves the tail once Phase 1b's block has used
the deficit. Fix direction: after Phases 1 and 1b, place what was granted per thread, then have
Phase 2 fill each thread's remaining gap from the size ladder instead of capping against the node's
pooled deficit. Then nothing is acquired that no gap can take, and #11 stays fixed because each gap
is exact. Reproduce with the user's 2026-09-28 run (4 threads, share 12.469 GiB, 8 of 12 huge
4096 MiB blocks granted, one extra huge 1 GiB block): expect 12.469 GiB per thread and nothing
unallocated. For the display gaps, measure fairness against the plan and flag a short thread. #70's
page-type fairness sub-task (one thread at 0 % huge pages still read `CV=0.000 ✅ Fair`) is the
same table. Keep the fix narrow. The legacy allocator stays the default until D, and C doesn't have
this bug: each thread fills its own span, least-filled first, with no pooled deficit and no
distribution afterwards.

**B) A tunable minimum block size, and the rounding over-commit (from #71).**
- Legacy: the block ladder `[4096 … 16]` MiB appears four times (`allocator.rs` 330, 569, 634,
  1060), plus `16 * 1024 * 1024` literals at 509, 576, 766, 1099, 1365 and 1420 (#73 wants those
  named anyway). The setting is its bottom rung.
- In legacy the smallest block also limits rounding. A share that isn't a multiple of it leaves a
  plan remainder (`create_allocation_plan` warns and tries anyway). So `PER_THREAD_STEP_BYTES` has
  to follow the setting, and neither goes below 1 GiB before A lands: the 2026-09-28 round-down
  showed what happens.
- Stitched already has the setting: `largefloor` (default 16 MiB), with `hugechunk` and
  `largechunk` for the tops of its two ladders. There it is only the smallest *commit*. The blocks
  tests see are power-of-two slices of the span, at least 16 MiB (the run quantum), whatever the
  commits were. Give the setting one user-facing name for both allocators, not two.
- What tuning buys. Lower gets more large-page memory out of a fragmented machine, for more calls
  and smaller blocks. Higher gives fewer, bigger blocks and leaves the rest short (under
  `minpage=large`) or on 4 KiB pages.
- A caution. A chunk is clamped to its block (B5, until #76), so small blocks mean small chunks.
  SimpleTest re-reads each chunk (1 write + N reads, 4 times over), and a chunk that fits in L3
  (96 MiB and up on X3D parts) is re-read from cache. This already happens when fragmentation
  forces the 16-64 MiB rungs; a setting makes it a choice. So consider a lower bound, or a warning
  relative to L3.
- The over-commit. Every share is rounded up to a whole GiB, so with many threads the plan can take
  the whole reserve (60 GiB available, 10 % reserve, 16 threads → 64 GiB). Options: round down to
  whole GiB (the reserve becomes a floor; an even-GiB share has no 1 GiB block in its plan, so it
  needs A first), round to nearest, or round up but cap at available. Stitched then rounds each
  thread down to 16 MiB and has no B4, so the 16 MiB step reverted on 2026-09-28 would be safe
  there, and the step could follow the allocator. Both allocators commit what the plan asks, so
  the over-commit applies to both.

**C) The stitched allocator as `allocator=stitched`, beside the current one.** Built and tested in
`../stitch-test2/` (2026-09-24/25; not in git). Use that crate, not `../stitch-test/`. Its
`INTEGRATION.md` is the recipe, steps 1-6 in order. Copy `src/stitched.rs` to `src/memory/`
(the only file that moves), add an `AllocationStrategy::Stitched` that `chunk_allocate_planned`
hands off to, add three config strings and CLI keys (step 5a is required, or it won't compile),
then trim it to one code path (step 6). Its TMR-APP line numbers were checked 2026-09-24/25, so
check them again before editing.

What it does: one placeholder reservation, and one 1 GiB-aligned span per thread. Each span is
filled from its base with 1 GiB pages, then 2 MiB, then 4 KiB. The least-filled thread commits
next, and commit sizes step down a power-of-two ladder on refusal. It returns ordinary power-of-two
`AllocationBlock`s, so every existing test runs on it unchanged. That is what makes keeping both
cheap, and why #76 can wait.

The user's decisions, which must survive:
1. Never release committed memory to rebalance or resize. Evenness comes from getting the split
   right up front: least-filled first, and the ladders. That is why step 6 removes
   `HugeFairness::Equalize`.
2. Commit by free-then-allocate at every page size: split the slot off, release it, then
   `VirtualAlloc2` at that address. Don't use `MEM_REPLACE_PLACEHOLDER`. On 10.0.26100.33438, a
   refused 1 GiB or 2 MiB replace followed by a `VirtualFree` of that placeholder bugchecks the
   machine (0x139/0xE, `MiUnlockAndDereferenceVad`).
3. Keep `CommitMethod::Replace` in the code, unused. The user will flip the one-line default in
   `StitchRequest::new` once Microsoft fixes the bug. Don't expose it as an option while the bug is
   there: any refusal takes the machine down.
4. Don't run a live huge-page or memory-exhausting test without asking. A wrong call takes the
   whole machine down. The fake-VM `cargo test` suite is safe.
5. After a crash, read the logs and give the user WinDbg commands. They analyse the dump.

What to expect:
- 23 unit tests come across, 20 after step 6 drops the three Equalize tests.
- Under TMR's default `min_page_size = "large"`, a thread whose 2 MiB pages run out comes up short,
  with a warning, instead of taking 4 KiB pages. Every stitch-test2 run allowed 4 KiB pages
  (0.9-1.5 GiB per thread when exhausting). Use `minpage=regular` to compare numbers.
- Settings: `hugechunk` (default 1 GiB), `largechunk` (1 GiB) and `largefloor` (16 MiB). The
  command-line parser only checks that each is a size; `stitched::plan` enforces power-of-two and
  ordering. On big servers, raise `hugechunk`. Live, 4 GiB commits cost about 14 ms per GiB,
  against about 47 at 1 GiB. The price is less even 1 GiB pages, which the 2 MiB phase makes up.
- Free-then-allocate opens a brief gap in the reserved range. If anything else in the process takes
  it, the allocation fails with an error (unit-tested). It never corrupts silently.
- Not yet run live: a raised `largefloor`, and a refused 4 KiB replace (moot while free-then-allocate
  is the default). The TMR-APP entry point has run live only once, and that was before the ladders
  existed.

Checks: build, clippy at zero warnings, and the user's `cargo test`. The first live run should be
small and non-exhausting through the real path, e.g. `allocator=stitched memory=8GiB`. The log
should show one reservation, each thread filled 1 GiB → 2 MiB → 4 KiB from its base, and a clean
page audit. Exhausting runs are the user's.

When it lands, update TMR-APP `CLAUDE.md` → Settled Design Decisions:
- Add three entries: no releasing committed memory to rebalance; free-then-allocate as the
  default, and why; `Replace` kept but unused until Microsoft's fix.
- Rewrite "No VA stitching". Its revival trigger was met, but keep what is still true: stitching
  VA does not stitch PA.
- Amend the large-page alignment entry. On the stitched path the alignment comes from the
  1 GiB-aligned slot address, not from `MEM_ADDRESS_REQUIREMENTS`. Keep its rule against an
  `alignment: None` retry.
- Close #70.

**D) Make stitched the default** once it has run the suite on the user's machines: pinned and
unpinned, fragmented and not. Then decide whether the legacy allocator stays as a fallback. List
what would go before deleting anything.
