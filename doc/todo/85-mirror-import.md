# TODO 85. [TMR-APP] TM5 MirrorMove / MirrorMove128 import: run what TM5 runs

Entry: `TODO.md` 85. Raised 2026-10-03 from the power-of-two / fast-math investigation (a
four-agent sweep with a skeptic per agent; the TMR-side mapping re-read by hand).

## What TM5 does

The two tests read `Parameter` differently (`TM5/bin/mtests0.asm`, `mtests0.inc`, cross-checked
against `TM5_Source/MT0.cxx` ~914-1135):

| Test | `Parameter` means | Block rounding | Moves |
|---|---|---|---|
| MirrorMove | subblock count: 2, 3, 4; anything else (0, 1, 16384...) is one whole block (`mtests0.inc:49`, `.asm` ~1152-1157) | down to 128 B (~1120-1124) | whole: 64 B per step, lanes reversed; 2: halves, 32 B per step; 4: quarters, 16 B per step; 3: `div 3`, each subblock rounded down to 128 B, tail left unmirrored (~1307-1318) |
| MirrorMove128 | a jump in 128 B units, capped at a quarter of the block (~1527-1543) | down to 256 B (~1509-1513) | 128 B swaps, step = jump + 128 B, (jump + 128) / 128 interleave passes from one setup `div` (~1546-1586) |

## What TMR does today

- `map_legacy_function` renames both to `Mem-MirrorV2-128` (`config.rs` ~1414) before
  `interpret_tm5_parameter_with_channels` reads the parameter, so it lands in the
  `Mem-MirrorV2-128` arm (~519): any non-zero P sets `page_stride_bytes = (P + 1) * 128`. The
  subblock arm (~506) matches `"MirrorMove"` but has never been reachable from a `.cfg`: it came in
  with ec2bb44 (2026-03-16), and the rename (to a 128-bit mirror test) existed from the first
  commit, 6e5dc07.
- `SwapMode::from_config` (`tests.rs` ~3112) checks `page_stride_bytes` first and then uses
  `raw_parameter`, not the byte value: `PageStride(P)` swaps one SIMD vector every (P + 1)
  vectors, so 16 B every (P + 1) x 16 B on -128, with no cap. `page_stride_bytes` is only tested
  for presence.
- `raw_parameter` is shared: SimpleTest derives its stride from it too.

What the shipped configs run:

| Config, test | TM5 | TMR |
|---|---|---|
| 1usmus_v3 Test3, MirrorMove P=1 | whole-block mirror | PageStride(1) |
| 1usmus_v3 Test5, MirrorMove P=4 | 4 subblocks | PageStride(4) |
| 1usmus_v3 Test14, MirrorMove P=16384 | whole-block mirror | PageStride(16384) |
| 1usmus_v3 Test4/Test15, MirrorMove128 P=510/2 | 128 B swaps, (P+1) x 128 B step | 16 B swaps, (P+1) x 16 B step |
| Check_absolutnew Test4, MirrorMove P=4 | 4 subblocks | PageStride(4) |
| Check_absolutnew Test3/Test5, MirrorMove128 P=2/1 | as above | as above |

## Subblock rounding (do first)

`mirror_swap_subblocks!` computes `sub_size = chunk_len / n` with no rounding. With n = 3 on a
power-of-two chunk the subblock bases are not vector-aligned, and the swaps compile to aligned
moves (`vmovdqa` ymm, `vmovdqa64` zmm; checked in the release asm, two `imul` and four `vmovdqa`
per loop body): every 256/512-bit swap faults, and 128-bit faults on 2^odd-byte chunks. Reachable
today only through JSON `Mem-MirrorV2-Auto` with parameter 3 or CLI `parameter=subblocks:3`
(`params.rs` accepts 2-4). The scalar `Mem-MirrorV2` ignores the subblock count. The fix is TM5's:
round each subblock down to 128 B and leave the tail. It must land before the import fix, which
would route TM5 P=3 configs here.

The same rule applies to every test that divides a chunk into parts (the user, 2026-10-03):
MirrorMove halves, subblocks, BlockMove halves and quarters, Spd-Copy halves. Each part must be a
multiple of the vector width at least. TM5 rounds the whole block to twice what it moves from each
end (128 B for MirrorMove, 256 B for MirrorMove128), so its halves and quarters stay whole; only
the 3-way split rounds each part.

## CLI override

`parameter=subblocks:N`, `stride:N` and `none` (`runner.rs` ~443, the parameter-override block)
replace each test's whole `parameter_context` (`none` sets it to `None`). That wipes
`rng_sequences`, `stride_patterns`, `subdivisions` and `copy_directions` (Mem-Random, CacheBust,
Mem-Stride and Mem-BlockMove panic on their `expect`) and SimpleTest's stride, and turns every
mirror test into the given mode. The parser itself validates (`params.rs`: subblocks
2-4, stride > 0, non-numbers rejected).

The user's direction: an upsert. Set the one field, leave the rest; a test that doesn't read it
ignores it. Subblocks and stride are two modes of one mirror setting (stride wins when both are
set), so the upsert sets one and clears the other, and `none` clears only the mirror fields.

## Decided

- TMR's mirror variants give the parameter one meaning; the `.cfg` importer translates each TM5
  test's meaning into it (the user, 2026-10-03: TM5's same-named tests disagree on what
  `Parameter` means, and TMR shouldn't copy that).

## Open

- The TMR-side shape: separate named fields (e.g. `subblocks`, `jump`) rather than one raw
  parameter, and their names.
- Whether TMR keeps its vector-granular stride as a native mode beside TM5's 128 B-swap jump.
- The jump cap (a quarter of the block) and the 128 B / 256 B block rounding on TMR's chunks.

## Implemented (2026-10-06, branch `todo85-74`, 07e65a5)

Option A of the 2026-10-06 discussion (the user: "A"; three subblocks "double agree" a load error):

- **One mirror setting, three modes** (`config::MirrorMode`, JSON `"mirror"`, CLI `mirror=`, one
  parser): `whole`, `subblocks:2|4`, `jump:N`. PageStride, `subblock_count` and `page_stride_bytes`
  are gone. Every mode is one round trip per test op, as one TM5 pass is: the two ends walk the
  whole range and cross the middle, so each pair is swapped twice. `jump:N` is MirrorMove128's
  traversal exactly: 128 B units, step (N + 1) x 128 B with the jump capped at a quarter of the
  chunk, interleave passes from the last to the first. A pass pairs unit x with n-1-x, so it
  either swaps its class with another pass's, which swaps them back, or with itself, twice.
- **Importer**: MirrorMove P 2/4 is `subblocks:P`, P = 3 a load error saying why (a third isn't
  vector-aligned; TM5 rounds each to 128 B and leaves a tail; no shipped config uses it), anything
  else `whole`; MirrorMove128 P is `jump:P`. Both run `Mem-MirrorV2-Auto`. The made-up
  `MirrorMove256/512` names are gone. A JSON mirror test with `"parameter"` is a load error.
- **Dwell**: `test_reps = N` (was ceil(N/2)).
- **CLI**: `mirror=` sets the one field on every test and leaves the rest (it replaced the whole
  context and crashed four tests).
- **Tests**: a recording pointer shows each mode at each width visits every vector in exactly two
  swaps with its mirror image; every mirror test runs clean in every mode; every shipped Parameter
  maps as above. Asm: plain zmm/ymm/xmm load/store swaps, no calls.

Since TODO 74 (551b5f1) the mirror's data is the seal and its verify the seal check, as TM5's.
