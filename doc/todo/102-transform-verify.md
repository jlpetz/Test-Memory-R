# TODO 102. [TMR-APP] Transform-verify in TMR: a memory test under AMX load, ready for ACE

**Raised** 2026-10-23 by the owner: land the AMX test in TMR, and have it ready for when ACE hardware
and compilers arrive.

## What exists (plumb)

- **Design**: `../fearlessD/design/amx-transform-verify.md` (the owner's idea, 2026-10-21, with the
  dated decisions).
- **Prototype**: `../plumb/bench/src/xform.rs` with `xform_builtin.rs`, 15 tests in
  `tests_xform.rs`, bench groups `xform`, `xformc`, `xformw`, `xformrw` and `xformr`, results in
  `../plumb/bench/RESULTS.md` (2026-10-23), plumb TODO 6 and 13.
- **The test**: per chunk, W stores a pattern tile into each 1 KiB tile; RW multiplies each tile in
  place by a fixed matrix (`TDPBUUD`); R adds each tile's product with its pattern's own matrix into
  a running group checksum `1 + r` times (`r` is the CPU-load setting, up to 65534) and compares it
  with a constant. A group holds each of the set's X patterns once (X = 16, 32 or 64), in a rotation
  keyed per cycle by the run's seed. Any one or two flipped bits in the stored result are caught by
  construction; flips before the transform are caught exhaustively for the built-in set (tested).
  Tier 2 is TMR's rescan (volatile rescan, rereads, flush and reread, a reread through the tiles,
  "not reproduced"), capped as TMR's. The built-in set's checksum is stored and checked at startup.
- **Speed** (dev VM): in DRAM it moves 9-24% more traffic than StuckBit with a large chunk at r = 0,
  and about as much at r = 8. W stores at fill speed; R reads at 77-82% of a plain read at r = 0. RW
  always reads ahead, and R from r = 6.
- **Tile layer**: `plumb_tiles`, with no fearless_simd dependency. It provides tokens (CPUID and the
  OS's tile state), sessions (`LDTILECFG`/`TILERELEASE`, nesting refused), typed tile registers,
  loads, stores and `TDP*`. It has a stable `asm!` path and stdarch's intrinsics behind `nightly`;
  TMR is on nightly, so it would take the intrinsics and plumb_tiles' two calling practices.

## Plan

1. **TMR's own tile layer, no dependency** (the owner, 2026-10-23). About 100-150 lines in TMR's
   style: detection and a session guard taken from plumb_tiles' `detect.rs` and `session.rs`, and
   stdarch's `_tile_*` intrinsics in `#[target_feature(enable = "amx-tile,amx-int8")]` functions.
   plumb_tiles' safe slice API, nesting checks and stable `asm!` backend aren't needed: TMR walks
   raw pointers (Rule 6), owns its threads, and builds on nightly.
2. **Test definition**: a name, config keys (X, r, the layout seed), the plan it goes in (opt-in,
   since AMX is Xeon-only), TMR's chunk-size and flush settings (a large chunk to reach DRAM), and
   the results fields (X, r, alarms, not reproduced).
3. **CPU gating**: `TestDefinition::cpu_lacks` checks a width (`Width::check`). AMX needs a feature
   requirement (AMX-INT8 plus the OS's tile state), so `leave_out_what_the_cpu_lacks` and
   `assert_cpu_runs` learn features as well as widths.
4. **Kernels in TMR's runner**: per chunk W, RW and R, with `add_bytes`, the halt and shutdown
   checks and the seal's `reseal`, as `stuck_bit_impl!` does. Tier 2 goes through TMR's `Rescan`
   (alarms, not reproduced, rereads, failing bits) rather than plumb's own counts. The built-in
   self-test runs once per run and fails the test cleanly if the matrix unit is wrong.
5. **Tests**: port `tests_xform.rs`. The hardware tests skip without AMX-INT8, as in plumb.
6. **Codegen**: TMR's tools on the post-LTO asm. TODO 101's call check matters here: a tile
   intrinsic that fails to inline is a call per tile. Keep the sessions in `amx-tile` functions
   (plumb_tiles' calling practice 2).
7. **Timing**: `tools/bench_ab.py` against StuckBit with a large chunk, at DRAM and cache sizes, on
   the dev VM. No AMD host can run AMX.
8. **ACE seam**: ACE has no tile loads or stores; data moves between memory and tiles through zmm
   (plumb `PLAN.md`, "ACE vs AMX"; plumb TODO 7). Keep W, RW, R and the checks over a small backend
   (tile from memory, tile to memory, the int8 multiply-accumulate): AMX implements it with
   `TILELOADD`/`TILESTORED`, ACE with zmm moves. Check ACE's int8 dot product against `TDPBUUD`'s
   unsigned-by-unsigned semantics, which the guarantees rely on.

## The port (2026-10-23)

Steps 1-6 done, step 6 against Rule 6.
- **Files**: `src/amx.rs` is the tile layer. It detects AMX-TILE and AMX-INT8 with the OS's tile
  state through `is_x86_feature_detected!`, plus leaf 1Dh's palette and 1Eh's TMUL limits, and it
  holds the LDTILECFG descriptor and a `Tiles` guard that releases on drop. `src/xform.rs`, with
  `xform_builtin.rs` and `xform/tests.rs` (16 tests, 3 ignored), is the test.
- **Name and settings**:
  - The test is `Mem-TileTransform`.
  - The keys are `tile_patterns` (X: 16, the default, 32 or 64) and `tile_load` (r: 0, the default,
    up to 65534). Other tests refuse both.
  - In the suite its chunks are twice the L3, as Mem-MovingInv's, and it runs for 30 s. It is in
    plan `all` only, not `stability` or `quick`. Results label it `Tiles(X,rN)`.
- **Gating**: `simd::Need` (`Width` or `AmxInt8`) is what `cpu_lacks`,
  `leave_out_what_the_cpu_lacks` and `assert_cpu_runs` check. On a CPU without AMX-INT8 the test
  is left out with its reason, and the console and window group left-out tests by reason. The runner
  tests' `suite()` and latency filter use the same check.
- **Kernels**: W is `write_tiles`, RW is `transform_tiles`, and R is `verify_groups`, with a chunk's
  partial groups compared word by word (`verify_part`). Each kernel is one tile session.
  - Per chunk: `check_seal`, `add_bytes`, `reseal`, and the shutdown check every 2^20 multiplies.
  - Tier 2 goes through `Rescan` (it gains `word_read`), then rechecks through the tiles.
  - At start-up the matrix unit is checked against the stored checksum.
- **Asm** (post-LTO; the working files are in `../tmr102-asm/`, outside the repo):
  - It matches plumb's build block for block. R runs two chains, reads ahead from r = 6 and goes in
    pairs below that. RW keeps tile 7 resident with four tiles in flight. W goes in pairs.
  - Every kernel has one LDTILECFG and one TILERELEASE, and no call or div.
  - W kept two values on the stack, a reload of each per group, until two changes:
    - The tail is written before the whole groups. Written after them, it kept `base` and `end`
      live through the loop.
    - The group's rotation goes through `opaque_u64`. LLVM had kept its two terms apart and
      re-added them for every tile.
    - The result: 172 → 157 instructions a group, 2 → 0 reloads. Every loop is now off the stack.
  - `indexed` on every tile loop is TILELOADD's stride register, which can't be avoided.
  - R at X = 16 keeps six copies of its r loop, unrolled by 8, on its r ≤ 5 path. They never run;
    the cost is code size only.
  - Existing kernels are unchanged (asm fingerprint and spills table).
- **Tests**: the workspace passes, and clippy is clean. Both exhaustive sweeps pass in release on
  the dev box: every stored bit, and every bit before the transform, 131,072 cases each.

Left: the ACE seam (step 8). The kernels call `_tile_loadd`/`_tile_stored` directly, and those are
the ops ACE replaces. The owner left the timing (step 7) out (2026-10-24).

## Open questions for the owner

- Settled 2026-10-24: the owner kept the name, plan `all` only, and X = 16 and r = 0 by default,
  to adjust later.
- Whether r should load W and RW too (plumb TODO 13).
