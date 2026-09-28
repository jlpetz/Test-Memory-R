# TODO 75. [TMR-APP] Allocators: fix the legacy fallback, a tunable minimum block, and the stitched allocator beside it

> Full record, moved verbatim out of `TODO.md` on 2026-09-28 (open item).
> The short entry in `TODO.md` holds the current status; this file holds the reasoning.

**Priority**: High. The fallback leaves threads short on a fragmented machine, and the stitched
allocator is meant to replace it.
**Raised**: 2026-09-28, the user's bundle: "fix that allocator bug … make the minimum block size
tunable AND integrate the new allocator", keeping both for now. A and the over-commit in B came
from #71. Adapting the tests is #76.
**Status**: Not started. Suggested order: A (small, and it fixes the default), then C (the recipe is
ready), then B (its setting spans both allocators), then D.

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
