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

## Open questions for the owner

- The test's name, and whether it joins any default plan.
- Defaults for X and r.
- Whether r should load W and RW too (plumb TODO 13).
