# TODO 99. [TMR-APP] Hot loops entirely in registers

**Raised** 2026-10-19 by the owner, from the TODO 16 asm review: "we should be methodical and try to
keep kernels' hot loops 100% in registers, even basic loop counters". Per-chunk bookkeeping (the
seal calls, the stats, the timers) may spill: it runs once a chunk, not once a word.

## The check

`tools/hot_loop_spills.py` reads the post-LTO asm (`cargo rustc --release --bin tmr -- --emit asm`)
and lists, per kernel, the hot loops that touch the stack. A hot loop is an innermost loop in a
test module's function with no call that touches memory off the stack, at most 150 instructions;
rescans, parsers and setup are left out by name. Two kinds:
- **round trip**: a slot stored and reloaded every iteration, a value carried through memory. A
  store-to-load forwarding latency (about 5 cycles) sits on the loop's dependency chain.
- **reload**: a value the loop never changes, reloaded every iteration. An L1 load; hidden when the
  loop waits on DRAM, but it takes a load slot when the data is in cache, as in SimpleTest's verify
  passes over a 4 MiB chunk.
`--detail NAME` prints a kernel's flagged loops; `--max N` fails over N, for a check before releases
that ratchets down as kernels are fixed.

## Baseline (2026-10-19, bf3660b)

39 kernels; 28 round-trip loops, 519 reload loops. Every one is a general-purpose register: no
hot loop spills a vector register. The leaders:

| Kernel | Round trip | Reload |
|---|---|---|
| `sequence_rw_256` | 5 | 33 |
| `simple_native_512` (closures) | 4 | 69 |
| `simple_native_scalar` | 4 | 56 |
| `simple_native_256` | 3 | 49 |
| `simple_native_128` | 2 | 57 |
| `sequence_read_256`, `_512` | 2, 2 | 44, 34 |
| `sequence_write_512`, `_256` | 2, 0 | 19, 36 |
| `simple_test_v2_sequential` (legacy) | 1 | 12 |
| `bench_verify_multi` | 1 | 10 |
| `stride_access_multi` | 1 | 6 |
| `block_move_multi` | 1 | 3 |
| `sequence_rw_512` | 0 | 31 |
| `cache_busting_multi` | 0 | 8 |

The cause is more live scalars than the 15 usable general registers: line ids, pattern keys,
bounds, pointers, counters. Mem-Stride's verify (`accumulate_stride_4`) is the worked example: four
loads a stride apart into four accumulators, which the compiler keeps as four data pointers and four
expected-value offsets, with the index, bound, step and a temporary: 16 values, so two live on the
stack. CacheBust's strided write has registers free but the allocator, working across the whole
function, gave its pattern base a stack slot.

## Approach

1. Fix in order of cost: the round trips in native SimpleTest (the default stability test) and
   Mem-Sequence first, then their reloads (the most-run kernels), then the rest.
2. Per kernel, read what is live and cut it, keeping the accumulators (a settled decision, measured):
   one moving pointer with offset addressing instead of several pointers (its loads stay
   independent: every address comes from the iteration's start, and the only carried value is the
   position, as today), values derived from others, loop constants in vector registers (AVX-512 has
   32), or a loop split in two.
3. Confirm in the asm that the loop touches no stack (`--detail`), and that it does the same work.
4. Measure: the variants where they trade (four accumulators with one pointer, two pointers, two
   accumulators) as steps of one run beside the test that showed four was best, on the desktops when
   they are free; a changed binary against the old in separate runs, three each, with a reference
   test in every run.
5. Lower `--max` as each kernel lands, and run it before each release build.

## Decisions so far

- 2026-10-19: the per-chunk seal calls are an `#[inline(always)]` guard around an `#[inline(never)]`
  chunk function (bf3660b): no call per chunk after a step's first cycle, the seal's work out of the
  test functions. Inlining the work too would save one call per chunk in the first cycle (about 2 ns
  against a chunk's millisecond of seal work) for 73 copies; it made no difference to the hot-loop
  spill count (544 inlined, 547 not).
- 2026-10-19: drop no accumulator to save registers without measuring it (the owner: "validate it
  based on the test that showed 4 was better and see what's best").

**Done** when the check shows no hot loop touching the stack, or each that remains has a written
reason it can't be avoided.
