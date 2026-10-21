# TODO 99. [TMR-APP] Hot loops entirely in registers

**Raised** 2026-10-19 by the owner, from the TODO 16 asm review: "we should be methodical and try to
keep kernels' hot loops 100% in registers, even basic loop counters". Per-chunk bookkeeping (the
seal calls, the stats, the timers) may spill: it runs once a chunk, not once a word. Later the same
day: speed decides, and a loop that can't fit in registers is flagged, not forced (the owner).

## The tools and the rules

- `tools/hot_loop_spills.py` reads the post-LTO asm and lists, per kernel, the hot loops that touch
  the stack (round trips and reloads, gpr and SIMD), with review columns `indexed`, `const`, `long`
  and `width`. Its blind spots and fixes: `tools/README.md`.
- `tools/bench_ab.py` runs binaries alternately against a reference test (`Mem-StuckBit512_A`), on
  this box or on a test host (`--host`, `--scripts`). Noise floor on the dev box: identical
  binaries land 1.0-1.6% apart (A/A, 2026-10-19); config runs without the reference ±5-10%.
- What the rounds taught is `doc/simd_codegen_rules.md` Rule 6 (6.1-6.19). The headlines: clean asm
  is necessary, not sufficient (a verify with every value in registers ran 2.1x slower than one
  with a spill, 6.12; a store loop full of spills beat every clean one on Intel, 6.17); measure on
  both vendors and let AMD decide 512-bit kernels (6.18).

## Rounds

Benchmarked on the dev box (EC2 r8i, Granite Rapids, AVX-512, 4 physical cores) against the binary
before, alternating, three runs each, `cycles=2 memory=8GiB-target` from round 3 on; from round 4
also on the US VM (m8a.4xlarge, Zen 5, 16 cores) at 16 and at 4 cores.

| Round | What | Commit |
|---|---|---|
| 1 | Mem-Stride: write and verify each a function, the verify's strides const generics (1.98x) | 1e0f938 |
| 2 | Mem-CacheBust: walks on a walked pointer; the verify explicit `u64x4` (parity, clean; indexed addressing had cost 6%) | d895c6a |
| 3 | A workflow (the owner's call, Ultracode): an agent per kernel family in its own worktree, an adversarial reviewer each, merged and benchmarked here. Native SimpleTest's grouped verify ran 0.47x for `random` until each line was checked before the next was computed (6.12) | 830c919 |
| 3b | 17 candidates for the regressions and openings, reviewed, benchmarked, the winners merged | da06138, c2644ab |
| 4 | The owner's follow-ups, a workflow chain and parallel jobs, each reviewed: the scalar variants removed; Bench-Init/Verify's numbered modes and legacy SimpleTest unified (one fill and one check per mode and width); one strided walk for native SimpleTest and the numbered modes (Mem-Sequence keeps its own: the shared one ran Parameter 16384 at 0.645x on AMD); the barrier one helper (`opaque_ptr`, `opaque_u64`); Mem-Random's stop count and an extent's worth of random reads a cycle; `width=`; seal calibration; visit-order and dispatch tests | tmr99/integrate |

The hot-loop counts with today's tool (the round-2 binary re-measured; the old tool missed loops over
150 instructions and stack arrays indexed by a register):

| | Round trips gpr / SIMD | Reloads gpr / SIMD | Indexed | Const | Clean kernels |
|---|---|---|---|---|---|
| Round 2 (d895c6a) | 56 / 30 | 472 / 93 | 1286 | 88 | 28 |
| Final | 3 / 3 | 90 / 10 | 285 | 62 | 166 |

## Speed, round 2 to final

**Intel (dev box, 4 cores, 2 GiB a thread; 2026-10-21, final build b275a8f).** Ratio = final /
round 2, three runs each alternating:

| Test | Ratio |
|---|---|
| Bench-Verify-TM5-0 / TM5-1 / TMR-13 | 2.64 / 1.82 / 2.30 |
| Mem-MovingInv | 2.23 |
| Bench-Verify-Header / Quiet | 2.19 / 1.92 |
| Mem-SimpleV2 modes 13 / 0 / 2 / 30 at 512 | 1.58 / 1.49 / 1.10 / 1.06 |
| Mem-Sequence: default / mixed orders / header-heavy | 1.26 / 1.24 / 1.55 |
| Mem-BlockMove | 1.14 |
| Mem-Stride (after round 1's 1.98) | 1.12 |
| TM5 1usmus_v3.cfg strided SimpleTests (Tests 13, 10, 11, 8, 12) | 1.12, 1.10, 1.10, 1.10, 1.09 |
| Mem-SimpleV2-512 / -128 / -256 (native) | 1.09 / 1.09 / 1.01 |
| Mem-SimpleNT-512 / 256 / 128 | 1.09 / 1.04 / 1.01 |
| Bench-Init-* | 1.03-1.11 |
| Mem-SimpleV2 mode 20 (128, 256, 512) / mode 10 (256) | 1.03-1.06 / 1.03 |
| Mem-ModuloX, seal checks, Spd copies, MirrorMove, Mem-Random, the other Bench-Verify-* | 0.99-1.12 |
| Mem-SimpleV2-128 mode 10 | 0.98 |
| Bench-Verify-TMR-10 / 11 / 30 | 0.91 / 0.91 / 0.71 |

The last row is the unification at work, not a regression in a kernel: these Bench pairs now run
SimpleTest's own 512-bit kernels, where round 2 ran Bench-only 256-bit ones, and on this Intel box
check-only work runs slower at 512 (the seal shows the same). SimpleTest itself is faster at 512 in
every mode, AMD at 4 cores runs the same Bench pairs at 1.03-1.06, and `width=256` reproduces round
2's numbers here.

**AMD Zen 5 at 16 cores (`memory=50%`, round 3b):** DRAM saturates in most tests, so most are at
parity, including Intel's biggest wins. What shows: Mem-Sequence 1.05 / 1.17 / 1.18 (default,
mixed orders, header-heavy), the TM5 1usmus strided Tests 10 and 12 +12% and +4%, Mem-Stride +2.4%,
mode 20 at 512 +2.7%. Mode 30 (0.87) and mode 10 at 128 (0.95) lost there; both kernels were
replaced in round 4.

**AMD Zen 5 at 4 cores (2 GiB a thread, as Intel; the final kernels):** Mem-Sequence 1.32 / 1.46,
legacy modes 30 / 0 / 13 at 512 1.31 / 1.17 / 1.70, Mem-Stride 1.09, Bench-Verify modes 0, 1, 13,
20, 30 1.03-1.06, Bench-Verify-Quiet / Header 1.05 / 1.02, SimpleV2-512 1.02; MovingInv, ModuloX,
Random, BlockMove, Mirror, the seal 0.99-1.01. Nothing below 0.988. So the work is generic: what
Intel gained most on (MovingInv 2.28x, Bench-Verify-Header 2.19x, TM5-0 2.9x) were Intel penalties
AMD wasn't paying.

Not measured: the desktops (Zen 4's double-pumped AVX-512 on the 8600G, Zen 3's AVX2 on the 5700X,
any Intel desktop without AVX-512); cache-resident sizes (the kernel-efficiency plan, below).

## What stays flagged, and why

- **Mem-ModuloX's aggressor pass** (emulated 64-bit multiply, `.rdata` constants, a stack reload):
  every clean form tried ran 6-8% slower on Intel, the same on AMD (Rule 6.17: not the store shape,
  the store width or the dependency).
- **The 256-bit mode 20 check** (Bench-Verify-TMR-20 and SimpleTest mode 20 at 256) reloads two
  multiplier halves from the stack: AVX2's 64-bit multiply is emulated and needs more than 16 ymm.
  It hashes four lines at once, 14% faster than the clean one-line form: 74 instructions per four
  lines against 39 a line (round 3b's asm), the stack reloads two L1 loads a step.
- **The heavy pattern families' lines** (quiet, header, switch at 12/24 bits, in native SimpleTest
  and Mem-Sequence 256): their scalar line code in the shared generators (fmix, balanced_bytes'
  masks, pin_mask) needs more values than 15 gprs alongside the walk.
- **Mem-Sequence 512's quiet12/24** forward and reverse read one constant from `.rdata` a line.
- **Mem-Stride's one round trip** is the per-stride-pass setup (cold).
- **The strided checks** keep 1 accumulator chain at 512 and 2 at 256: four measured slower on the
  TM5 config (0.83-1.01x).
- **Check-only work at 512 on Intel** (the seal, Bench-Verify) runs 5-15% slower than at 256; AMD
  doesn't care. The seal's width is now calibrated per machine (`--calibrate-seal`).

## Decisions

- 2026-10-19: the per-chunk seal calls are an `#[inline(always)]` guard around an `#[inline(never)]`
  chunk function (bf3660b).
- 2026-10-19: drop no accumulator to save registers without measuring it (the owner).
- 2026-10-19: no 8-wide variant timing for CacheBust: the clean form stays (the owner).
- 2026-10-20 (the owner):
  - Mem-MovingInv reads, checks and writes 32 B at a time.
  - No release-mode check that chunks are whole 4 KiB granules; documented on
    `test_memory::GRANULE` (a range's arithmetic, not the stitched pages under it).
  - The empty-`asm!` barrier stays, as one helper.
  - The scalar variants go (Lat-NTW-DRAM-Write-Scalar stays: a deliberate MOVNTI experiment). TM5's
    RefreshStable, legacy SimpleTest and the importer's plain names map to the Auto tests; an
    imported RefreshStable flushes before its sleep, as the scalar Mem-Refresh always did.
  - Mem-SimpleV2-128 runs at 128 bits (its numbered modes had run shared 256-bit loops).
  - Bench-Init/Verify's numbered modes are SimpleTest's stages again: one kernel per mode and
    width, the Bench pairs at the run's width.
  - Auto is the widest width for tests; `width=auto|128|256|512` overrides it, and the results
    record the width each test ran at. The seal's auto is the calibrated width (`--calibrate-seal`,
    `--calibrate-all`, one thread, the fastest, ties to the narrower), else 256 with AVX2; the seal
    calibration carries its own machine fingerprint.
  - Mem-Random: the stop count is the reads done; a cycle reads an extent's worth; its 25 s may
    run to the cycle's end (accepted).
  - The strided checks keep their chains; Mem-Sequence keeps its own strided walk.
  - AMD decides 512-bit kernels (Rule 6.18): mode 30 checks in lanes at 256 and 512.
- 2026-10-21 (the owner): no fallbacks. A test the CPU can't run (a 512-bit one without AVX-512)
  is left out of the run however it was picked, a plan, `test=` or a config
  (`runner::leave_out_what_the_cpu_lacks`); none steps down to 256 or returns empty results any
  more, and a seal width the CPU lacks is refused with the seal off too. The kernels don't check:
  the run stops with a `bug:` panic at a test that got past (`runner::assert_cpu_runs`). Before,
  only plans that list their tests left them out, so `plan=all`, `test=` and configs reached the
  41 latency tests at 512 unchecked (an illegal instruction on an AVX2 CPU). AVX-512 is F, BW,
  CD, DQ and VL, the kernels' `#[target_feature]` set (`simd::has_avx512`), not F alone.

## Follow-ups (not TODO 99)

1. TODO 100, a new kernel baseline on all hardware: every kernel at DRAM and at L1/L2-resident
   sizes, so a kernel's own speed shows on a box whose DRAM saturates (the owner's idea), on both
   VMs and the desktops (the 8600G and 8500G for Zen 4's AVX-512, the 5700X for AVX2's 256 paths).
   Mind 6.17: decide stress kernels at DRAM.
2. Mem-SimpleV2-128 mode 10 (0.98) and SimpleNT-128 (0.986): small, not chased.
3. The tm5 seal's per-page setup (gathers from a stack copy, ~40% of its instructions at 512);
   Lat-Write's offset arithmetic; the NT stores' `lea` per store (the intrinsics' register address).
