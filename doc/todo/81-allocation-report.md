# TODO 81. [TMR-APP] Allocation report: two tables and a fairness verdict per dimension

**Raised**: 2026-10-02, from the TODO 75 review. The user: "relook at ALL of these tables to see
what we should do for a clear representation to users and you for debugging", covering overall
allocations (allocator performance), per-thread breakdowns (block sizes, page sizes, NUMA local vs
remote) and fairness on each of those. To be done after the TODO 75 work is committed.

## Today: six tables

Block Size Distribution, Page Type Summary, Per-Thread Block Allocation, NUMA Node Distribution,
Allocation Fairness Analysis, Per-Thread Allocation Breakdown. Overlaps:
- Per-Thread Allocation Breakdown is a strict subset of Per-Thread Block Allocation (every number
  in it is there too; Block Allocation adds the block sizes). It also prints in hash order.
- Fairness's Allocated column is Block Allocation's Total Size.
- On one node, NUMA Node Distribution is the totals of Block Allocation; Page Type Summary's totals
  are NUMA Node Distribution summed.
- Block Size Distribution's page type column says "Mixed" for any size that both page sizes
  produced, and Blocks/Thread (e.g. 1.8) says little.

Bugs to fix along the way:
- Page Type Summary's block counts: per-thread block sizes are grouped by size only and keep the
  first block's page type (`converters.rs` ~286), so a 4096 MB group with one huge and two large
  blocks counts as 3 huge (~377). The user's run showed 15 huge + 2 large for 7 + 10.
- The CPU column shows the thread id (`converters.rs` ~216 uses `block_info.thread_id`).

## Proposal (mock-up, the user's 2026-10-02 run: 4 threads × 14 GiB, 23 free 1 GiB pages)

```
📊 Memory Allocation — stitched, x.x s
Node  Threads     Target  Allocated      1 GiB      2 MiB  4 KiB  Remote  Requests
   0        4  56.00 GiB  56.00 GiB  23.00 GiB  33.00 GiB      -      0%  17 (3 refused)

Thread  Node  Allocated      1 GiB      2 MiB  4 KiB  Remote  Blocks             Status
     0     0  14.00 GiB   8.00 GiB   6.00 GiB      -      0%  4096MB×3, 2048MB   ✅
     1     0  14.00 GiB   6.00 GiB   8.00 GiB      -      0%  4096MB×3, 2048MB   🟡 2 fewer 1 GiB pages
     2     0  14.00 GiB   5.00 GiB   9.00 GiB      -      0%  4096MB×3, 1024MB×2 🟡 3 fewer 1 GiB pages
     3     0  14.00 GiB   4.00 GiB  10.00 GiB      -      0%  4096MB×3, 2048MB   🟡 4 fewer 1 GiB pages
Spread (node 0): 14.00-14.00 GiB, 4-8 × 1 GiB pages, smallest block 1-2 GiB
```

(That split is the old 4 GiB-first ladder. Since the TODO 75 follow-up the same pages split
6/6/6/5.)
- The node table covers allocator performance; the thread table covers what each thread got.
- Drop the CPU column (the CPU topology table already maps threads to CPUs), page counts for 2 MiB
  and 4 KiB (bytes say it), CV (the spread says it), and Node and Remote on single-node machines.
- Status replaces the `vs Target` column ("⚠️ 0.47 GiB short").
- Remote: the share of a thread's pages on another node, from each block's recorded node. Exact
  since 2026-10-02: large-page requests name their node strictly, and the page audit is gone.
  4 KiB blocks only prefer their home node, so mark them as such.
- Requests, refusals and time: counters in `fill::fill_ladder` and the allocators.
- The per-thread log lines both allocators print repeat the thread table: move them to debug.

## Design (agreed 2026-10-16, built the same day)

**Commits, not blocks.** The owner, 2026-10-16: the stitcher commits blocks in each thread's VA
range, and the report must say which commits make up each thread's range. Each request is now
recorded against its thread (`stitched::Commit`: size, page size, node, refused;
`ThreadSpan::commits`), so the report shows what was asked of Windows. Startup only: one small
entry a request, moved into the report and dropped once it prints; a refusal's error code is in the
log line where it happened. The memory tests still see only each thread's start and length. The 4096/2048 MB "blocks"
of the old tables were `power_of_two_split` cuts of the merged runs, made afterwards.

Six tables, each with one job; the old six go once these are in (their code listed for the owner
before it is deleted).

1. **Memory Allocation, by node** (title: allocator and time): Node, Threads, Target, Allocated,
   1 GiB, 2 MiB, 4 KiB, Local, Remote, Requests, Refused; a Total row when there is more than one
   node. A node's Remote is what its own threads got from other nodes. On a one-node machine Local
   and Remote are hidden and the title says "1 NUMA node, all memory local".
2. **Commits:** a row per (request size, page size, node asked, home node of the threads asking):
   Granted, Refused, Threads, Total. A remote row's Node reads `0 for 1`. Node hidden on one node.
3. **Threads:** Thread, Node, Allocated, 1 GiB, 2 MiB, 4 KiB, Local, Remote, Status. Status names
   only what is flagged (`🟡 1 GiB -5.00 GiB, remote 512.00 MiB (6.2%)`). Node, Local and Remote
   hidden on one node.
4. **Commits by thread:** each page size's granted requests in MB in the order asked, a repeat
   written `128×44`, one from another node tagged `@N` (`128×44+128@0×4`); Granted; Refused, with
   their sizes (`1 (1 GiB: 1024)`).
5. **Fairness by thread:** each dimension's deviation from the median of the other threads
   (Allocated, 1 GiB, 2 MiB, 4 KiB, Remote, Requests), `=` when equal, flagged in the cell. Kept for
   debugging; can be switched off later.
6. **Fairness summary:** a row per dimension: Min, Median, Max, Worst (threads and deviation, `±`
   when tied on both sides), Verdict.
5 and 6 print only with more than one thread.

Rules to start (each thread against the median of the others):
- Allocated: below target, or 1% below the others, ⚠️.
- 1 GiB: more than one page fewer than the others, 🟡. Only the side that lost out (the owner,
  2026-10-16): two-sided, a short node flagged every thread.
- 2 MiB: shown, never flagged. More is the 1 GiB shortfall again; fewer is more 1 GiB or 4 KiB.
- 4 KiB: any, 🟡 (slower, and tested differently).
- Remote: any, 🟡; over 10%, ⚠️.
- Requests (granted and refused): more than twice the others', 🟡 (fed in small pieces).

Units: every byte quantity in GiB/MiB, the 1 GiB deviation too (one form per quantity, where the
mock-up had "pages"). Requests is the node table's name for the count, so the fairness dimension
takes it too.

The owner kept the request record visible (2026-10-16: "if in debug logs only, issues may be
missed"), wanted the per-thread fairness as well as the summary, Refused as its own column, and the
per-thread commits as their own table (in the thread table they pushed Status off to the side). The
per-thread allocator log lines are at debug now.

## Fairness per dimension: the options weighed (2026-10-02)

The user's options:
- A) A third table, one fairness column per dimension (bytes, 1 GiB pages, smallest block,
  remote). Costs a second set of per-thread rows.
- B) A status column per dimension plus an overall one on the thread table: four or five more
  columns, mostly ✅.
- Claude's suggestion (C): one Status column naming only the dimensions that fail, and the spread
  line giving each dimension its verdict ("1 GiB pages 4-8 🟡, smallest block 1-2 GiB ✅, remote 0% ✅").
  Every dimension gets a verdict without new columns or repeated rows.
Mock up A, B and C on a two-node example before choosing.

The user, 2026-10-02: "a spread or deviation metric per dimension might work better than an
overall score which we can pass/fail as a final status". That is C's spread line, with the
per-thread Status as the pass/fail.
