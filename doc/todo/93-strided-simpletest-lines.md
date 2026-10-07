# TODO 93. [TMR-APP] Strided SimpleTest: TM5 moves a 64 B block per jump, TMR one word

Entry: `TODO_ARCHIVE.md` 93. Found 2026-10-06 in TODO 89's live runs; implemented 2026-10-07 on
branch `todo85-74`.

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
  `strided_period(stride)` = stride + 1 line, each pass a period at a time to the chunk's end.
  Every line once per pass over the chunk (`the_strided_walk_is_tm5s`). Two changes from TM5,
  for speed (measurements below): the passes go **up** from the period's first line (TM5's count
  down from its last), and each step prefetches the line above (`prefetcht1`), the one the next
  pass takes.
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

**Round 16, the final build** (ascending, line above; two runs each): 1usmus_v3 4:22 and 4:33
against head's 5:20 and 5:14, step times 267 s against 317 s. Test 12 1.91, Test 13 1.87, Test 10
1.62, Test 6 1.59, Test 11 1.12, Test 2 1.01, Test 8 0.97; the sequential and mirror steps 0.95-1.09.
The seven shapes: 1.88, 0.94, 0.96, 1.65, 1.60, 1.83, 1.16. Every run 0 errors, 0 seal errors.
Check_absolutnew wasn't rerun on this build: its one strided shape tied in round 15, and the rest
of its steps don't use the walk.

## Decision for the user

Two departures from TM5's walk, both measured, each one edit to undo in `for_each_strided_line!`:

- **The prefetch** changes the DRAM command pattern: every pass's lines come in pairs from one
  row, as head's word-per-jump walk's did (by the hardware prefetcher). TM5's walk on the same
  CPU pairs fewer: at most half, and only if the CPU's adjacent-line prefetcher is on (not checked
  on the dev box). Without it the walk opens a row per line, TM5's pattern, at round 6's speed.
- **The pass order** is reversed. Each pass is TM5's; only the order of the passes differs.

## Not done

- Mode 2's strided image is mode 12's, not TM5's walk-order chain: a walk-order image isn't
  position-pure, and chunks overlap (TODO 76).
- `Mem-SimpleNT` doesn't take a stride (the parameter docs said it did; corrected).
