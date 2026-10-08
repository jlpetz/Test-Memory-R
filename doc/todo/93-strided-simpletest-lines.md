# TODO 93. [TMR-APP] Strided SimpleTest: TM5 moves a 64 B block per jump, TMR one word

Entry: `TODO_ARCHIVE.md` 93. Found 2026-10-06 in TODO 89's live runs; implemented 2026-10-07 on
branch `todo85-74`; the prefetch settled 2026-10-08 (see "The goal").

## What TM5 does

`TM5/bin/mtests0.asm` ~336-394 (write) and ~420-470 (read), setup ~240-300:

- `BlkSize` = 64 B, `JumpStep = BlkSize x (Channels x Parameter - 1)`, so the period is
  `64 x Channels x Parameter` bytes, over `InterleaveCyclesAll = (JumpStep + BlkSize) / BlkSize`
  interleave passes.
- Each step writes (or reads) one whole block, 3 x 16 B plus 1 x 16 B, then `add edi, JumpStep`,
  while `edi < MaxAddr`. Pass k starts at `(k - 1) x BlkSize` with k counting **down**, so the
  period's last line goes first.
- `prefetchnta [edi + PrefetchDelta]`, `PrefetchDelta = 8 x (JumpStep + BlkSize)`: 8 periods on,
  in the same pass.
- Mode 2's pattern evolves per block in walk order (`ST_GeneratePattern2`, the xmm state carried
  from block to block), so its image depends on the walk.

## What TMR did before

`simple_test_v2_strided` wrote one u64 per step at a `(Channels x P - 1)`-line stride, over
`(Channels x P - 1) x 8` passes, so every line was visited 8 times a pass and the period was a
line short of TM5's. The line modes (2, 12) fell back to mode 10. The SIMD variants walked one
vector per step at `stride_elements x W` (the word stride multiplied by the width again), and
their verify counted 1 error per chunk.

## What TMR does now

- `for_each_strided_line!` (`tests.rs`): TM5's walk, one 64 B line per step, period
  `strided_period(stride)` = stride + 1 line, passes from the period's last line down, each a
  period at a time to the chunk's end. Every line once per pass over the chunk
  (`the_strided_walk_is_tm5s`). Each step prefetches (`prefetcht1`) the line the walk reaches
  `STRIDED_AHEAD` = 128 steps on, carried into the next pass at a pass's end and capped at the
  shortest pass's lines, so it stays in the chunk: TM5's own prefetch, which is 8 periods on in the
  same pass and leaves the chunk once a pass has fewer than 8 lines.
- The image is the sequential test's: modes 0, 1, 10, 11, 13 compute each word from its index;
  the scalar test's strided modes 2 and 12 write mode 12's lines (mode 2's chain runs a page in
  order, and the walk visits a page's lines a period apart). The SIMD variants keep their own
  positional and mode 12 patterns. `strided_simple_tests_write_the_sequential_image` checks every
  width and mode at two strides, sealed and unsealed.
- Verify: a line at a time into the accumulators, with a cold rescan that counts each bad word.
- A config's `"parameter"` strides `Mem-SimpleV2` at every width (it was ignored for `-128`,
  `-256`, `-512` and `-Auto`, so their strided code was unreachable from a config).
- Gone: the 1-way `accumulate_stride` and its `STRIDE_4_FROM` threshold (TODO 89's stopgap), and
  the SIMD block-strided macros (rewritten on the line walk).

## Measurements (dev box, 4 threads, memory=50%, 2026-10-07)

1usmus_v3's seven strided steps all use mode 2. Their chunks differ per test (`Test Block Size`):
Test 6 432 MiB, Tests 12 and 2 32 MiB, Test 10 8 MiB, Test 13 64 MiB, Test 8 880 MiB, Test 11
16 MiB.

**Round 6, 1usmus_v3, TM5 walk without a prefetch** (two runs each): 6:20 and 6:24 against head's
5:10 and 5:02. Per step (time head / time new): Test 12 0.48, Test 8 0.61, Test 2 0.61, Test 6
0.87, Test 13 0.97, Test 10 1.11, Test 11 1.11. Everything that got slower has a chunk too big for
the cache; Tests 10 and 11 (8 and 16 MiB) got faster.

**TM5's own prefetch** (`prefetchnta` 8 periods on, one 1usmus run): Test 12 0.55, but Test 13
0.45 and Test 11 0.44. At a 1.1 MB period the target is outside the 64 or 16 MiB chunk. Dropped.

**Rounds 7-10 are void**: a stray Check_absolutnew run (a run script that survived its stop) ran
alongside them from 12:57. Their one usable fact is a config one: the "density" sweep's SIMD runs
were never strided (the `"parameter"` bug above).

**Round 11, the 1usmus shapes in a sweep** (`sweep93_chunks.json`: Mem-SimpleV2 mode 2, 2 GiB per
thread, each shape's Parameter and chunk; two runs each; speed against head's walk):

| P, chunk | TM5 walk | + 256-bit line stores | + next-pass `t0` | + next-pass `t1` |
|---|---|---|---|---|
| 787, 32 MiB | 0.50 | 0.50 | 1.18 | 1.84 |
| 254, 32 MiB | 0.58 | 0.59 | 0.90 | 0.99 |
| 358, 880 MiB | 0.58 | 0.60 | 0.94 | 0.94 |
| 125, 432 MiB | 0.83 | 0.88 | 1.37 | 1.62 |
| 477, 8 MiB | 1.15 | 1.17 | 1.67 | 1.55 |

The scalar mode-10 strided write was 8 scalar stores per line (LLVM didn't vectorise it), but
storing the line as two 256-bit vectors changed nothing, so it isn't the store buffer.

**Round 12, lookahead in walk order** (`prefetcht0` 8, 16 or 32 steps on, carried into the next
pass; `t1` at 16): 0.65-1.14, below the next-pass prefetch on every shape (that one repeated at
1.71, 0.93, 0.94, 1.64, 1.44), and no better than none at P 358 / 880 MiB (0.66-0.69).

**Why** (inferred, not measured directly): head's walk read each line's 8 words in 8 passes in a
row, so the L1's next-line prefetcher fetched the line beside it, which its next passes needed. TM5's
descending order needs the line below instead, and nothing fetches it. Adding parallelism alone
(the walk-order lookahead) doesn't recover the speed; fetching the neighbouring line does. That
points at DRAM row activations: two lines in one row cost one activation, and a walk that opens a
new row for every line is bound by activations (tFAW, tRRD).

*Revised 2026-10-08:* wrong for the steady state. The next pass's line is requested a whole pass
later, after hundreds of other lines, so its row has closed by then: the software prefetch doesn't
pair lines, it gets more requests in flight. It pairs only in each chunk's first pass, and where a
pass is bigger than the L2 and the prefetched line is evicted before use. What may group lines is
the hardware's own prefetching, which the pass order invites or not. See "The goal".

**Round 13, TM5's order with the line below prefetched** (`prefetcht1`): the sweep 1.62, 0.84,
0.94, 1.61, 1.44 (41.5 s against 48 s). 1usmus_v3 4:49 and 4:39 against head's 5:08 and 5:02: Test 6
1.59, Test 12 1.61, Test 10 1.47, Test 11 1.13, Test 8 0.94, Test 2 0.88, but **Test 13 0.76**
(0.97 with no prefetch). Check_absolutnew 34:09 against 33:15 (an earlier clean run), its one
strided step (Test 15, P 256 over 1.5 GiB chunks) unchanged at 82-89 s against 86 s, 0 errors.
The SIMD variants, strided from a config for the first time, ran clean and faster than the scalar
test at every shape.

**Round 14, all seven 1usmus shapes** (`sweep93_all.json`, two runs each, against head's walk):

| P, chunk | no prefetch | TM5 order + line below | ascending + line above |
|---|---|---|---|
| 787, 32 MiB | 0.56 | 1.67 | 1.53 |
| 254, 32 MiB | 0.64 | 0.90 | 0.81 |
| 358, 880 MiB | 0.60 | 0.94 | 0.93 |
| 125, 432 MiB | 0.86 | 1.60 | 1.58 |
| 477, 8 MiB | 1.05 | 1.43 | 1.55 |
| 8968, 64 MiB | 1.00 | **0.74** | **1.67** |
| 8568, 16 MiB | 1.10 | 1.12 | 1.14 |

The sweep took 81-85 s with no prefetch, 59-62 s on head's walk, 55-56 s in TM5's order and 49-53 s
ascending. Likely why ascending wins at Test 13's shape: the hardware prefetchers favour ascending
streams (the L1's next-line prefetcher only goes up), so they join in.

**Round 15, Check_absolutnew Test 15's shape** (P 256, 1.5 GiB chunks, three runs each): head 1.00,
TM5 order 0.98, ascending 1.00. Test 2's shape again: TM5 order 0.95, ascending 0.89.

**Round 16, ascending with the line above** (committed 2026-10-07; two runs each): 1usmus_v3 4:22 and 4:33
against head's 5:20 and 5:14, step times 267 s against 317 s. Test 12 1.91, Test 13 1.87, Test 10
1.62, Test 6 1.59, Test 11 1.12, Test 2 1.01, Test 8 0.97; the sequential and mirror steps 0.95-1.09.
The seven shapes: 1.88, 0.94, 0.96, 1.65, 1.60, 1.83, 1.16. Every run 0 errors, 0 seal errors.
Check_absolutnew wasn't rerun on this build: its one strided shape tied in round 15, and the rest
of its steps don't use the walk.

## The goal (decided with the user, 2026-10-08)

What the strided walk is for: every access opens a new DRAM row, as many per second as the
timings allow. Neither misses nor speed is the aim in itself. A core stalled on a miss leaves the
memory controller idle; the timings (tRCD, tRP, tRAS, tRC, tRRD, tFAW) are pushed only when it
has requests queued for other rows. And a walk can get faster in two ways that timing can't tell
apart: more row openings per second, or more bytes per opening, when neighbouring lines arrive
together from one open row.

So the walk should make grouping unlikely by construction, and then the fastest setting is the
most stressful one, measurable by time alone (no counters in the VM):

- TM5's descending order: the L1's next-line prefetcher fetches the line above each one, which
  this order has already done, so it can't hand the walk neighbours.
- A prefetch to lines in other rows (the walk's own steps ahead, a period or more away), so
  requests stay queued without grouping.

Ascending passes with the line above prefetched (the build committed 2026-10-07) were faster,
1.2x over the seven shapes and 1.9x at Test 13's (57 lines a pass, so each page comes round every
57 steps, short enough for the L2's stream prefetcher to learn it and fetch neighbours ahead).
Likely in part by grouping, so that speed didn't measure the stress. Head's word-per-jump walk is
ascending too.

**Round 17, after a reboot (TM5's order, `prefetcht0`, lookahead 32 to 256 steps; two runs each,
against head's walk):**

| P, chunk | ascending + line above | 32 | 64 | 128 | 256 |
|---|---|---|---|---|---|
| 787, 32 MiB | 1.56 | 1.12 | 1.26 | 1.27 | 1.37 |
| 254, 32 MiB | 0.98 | 0.98 | 1.03 | 1.06 | 1.20 |
| 358, 880 MiB | 0.92 | 0.68 | 0.68 | 0.68 | 0.67 |
| 125, 432 MiB | 1.62 | 1.27 | 1.28 | 1.29 | 1.18 |
| 477, 8 MiB | 1.57 | 1.32 | 1.38 | 1.52 | 1.49 |
| 8968, 64 MiB | 1.64 | 0.72 | 0.93 | 0.93 | 0.88 |
| 8568, 16 MiB | 1.15 | 1.03 | 1.03 | 1.05 | 1.05 |

Level from 64 steps, so lead time isn't the limit there.

**Round 18, `prefetcht1`**: at 128 steps 1.50, 1.01, 0.71, 1.63, 1.38, 0.93, 1.03 (`t0` 1.36,
1.07, 0.69, 1.28, 1.52, 0.88, 1.03); 256 steps with `t1` the same as 128. `t1` closes the gap to
the ascending build at Test 6's shape (1.63 against 1.62) and most of it at Test 12's; likely the
L1's few miss slots, which `t0` holds until the line arrives. At Test 13's shape the lookahead is
capped at the pass's 57 lines, which in TM5's order is the line below: the same as round 14's
0.74, so the gap there is the pass order. At Test 8's (880 MiB) nothing tried reached head's walk.

**Round 19, the chosen build** (TM5's order, 128 steps, `t1`; two runs each): 1usmus_v3 5:12 and
5:07 against head's 5:22 and 5:14 (step times 309 s against 317 s): Test 6 1.63, Test 12 1.39,
Test 10 1.37, Test 11 1.13, Test 2 0.94, Test 13 0.90, Test 8 0.73; the sequential and mirror
steps 0.98-1.05. Check_absolutnew Test 15's shape (P 256, 1.5 GiB chunks): 0.84 (the ascending
build 1.05), about 2.5 min more on its 33. Every run 0 errors, 0 seal errors.

Still open: why the very large chunks (880 MiB, 1.5 GiB: over 20k lines a pass) are slower than
head's walk. A check without counters, on a physical box: run head's walk and this one with the
hardware prefetchers off in the BIOS. If head's walk falls back to this one's speed, its lead there
came from the hardware prefetchers.

## On physical AMD machines (2026-10-08)

The same four builds (head's walk, TM5's order without a prefetch, ascending with the line above,
the final one) on the seven 1usmus shapes (`sweep93_all.json`), two runs each, memory=50%, all
physical cores. Step-time totals for the sweep:

| Machine | head | TM5 order, no prefetch | ascending + above | final |
|---|---|---|---|---|
| Intel VM (Xeon, DDR5; rounds 14 and 17-18) | 55 s | 81-85 s | 45 s | 53-54 s |
| petz007: Ryzen 5 8600G (Zen 4), 2 x 16 GiB DDR5-6000, 1 GiB pages | 54.5 s | 49 s | 50.5 s | 44.5 s |
| daddy-petz: Ryzen 7 5700X (Zen 3), 4 x 16 GiB DDR4-3600, 2 MiB pages | 93.5 s | 84 s | 96 s | 82 s |

On both AMD machines the final walk is the fastest, and TM5's order is faster than head's walk even
without a prefetch. The ascending walk's lead is Intel's alone (on the 5700X it is 0.74x head at the
880 MiB shape), consistent with it coming from Intel's prefetchers. AMD uProf (5.3) gave no memory
counters on the 8600G: only per-core metrics, `-m memory` "unsupported", and no row-activation
counter at all, so the walks' row openings stay unmeasured.

**Errors found, petz007** (DDR5-6000 on a CPU rated for DDR5-5200; P 358 over 880 MiB chunks, three
tests a run; all transient, the reread clean):

| Walk | Tests | Errors | Per 100 tests | Per hour |
|---|---|---|---|---|
| head | 41 | 0 | 0 | 0 |
| TM5 order, no prefetch | 41 | 4 | 9.8 | 42 |
| ascending + above | 32 | 2 | 6.2 | 19 |
| final | 41 | 2 | 4.9 | 23 |

The line walks found 8 in 114 tests and head's walk none in 41; at the line walks' rate it would have
found about 3, and none happens by chance about 3-5% of the time. So the line walk catches errors
the word-per-jump walk missed on this machine. The counts can't rank the three line walks.
daddy-petz ran clean (8 runs).

## Not done

- Mode 2's strided image is mode 12's, not TM5's walk-order chain: a walk-order image isn't
  position-pure, and chunks overlap (TODO 76).
- `Mem-SimpleNT` doesn't take a stride (the parameter docs said it did; corrected).
