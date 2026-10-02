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
📊 Memory Allocation — plan-pagesize-pref, x.x s
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

## Fairness per dimension: open

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
