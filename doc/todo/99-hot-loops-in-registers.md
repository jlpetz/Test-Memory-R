# TODO 99. [TMR-APP] Hot loops entirely in registers

**Raised** 2026-10-19 by the owner, from the TODO 16 asm review: "we should be methodical and try to
keep kernels' hot loops 100% in registers, even basic loop counters". Per-chunk bookkeeping (the
seal calls, the stats, the timers) may spill: it runs once a chunk, not once a word. Later the same
day: speed decides, and a loop that can't fit in registers is flagged, not forced (the owner).

## The tools and the rules

- `tools/hot_loop_spills.py` reads the post-LTO asm and lists, per kernel, the hot loops that touch
  the stack (round trips and reloads, gpr and SIMD), with review columns `indexed`, `const`, `long`
  and `width`. Its blind spots and fixes: `tools/README.md`.
- `tools/bench_ab.py` runs binaries alternately against a reference test (`Mem-StuckBit512_A`).
  Noise floor on the dev box: identical binaries land 1.0-1.6% apart (A/A, 2026-10-19).
- What the rounds taught is `doc/simd_codegen_rules.md` Rule 6 (6.1-6.18). The headline: clean asm
  is necessary, not sufficient. A verify with every value in registers ran 2.1x slower than one
  with a spill (6.12), and a store loop full of spills beat every clean one (6.17). Measure.

## Rounds

Every change was benchmarked on the dev box (Granite Rapids, AVX-512, 4 physical cores) against the
binary before it, alternating, three runs each, `cycles=2 memory=8GiB-target` from round 3 on.

| Round | What | Result | Commit |
|---|---|---|---|
| 1 | Mem-Stride: write and verify each a function, the verify's strides const generics | 1.98x | 1e0f938 |
| 2 | Mem-CacheBust: walks on a walked pointer; the verify explicit `u64x4`. Indexed addressing had cost 6% | parity, clean | d895c6a |
| 3 | A workflow (the owner's call, Ultracode): an agent per kernel family in its own worktree, an adversarial reviewer each, then merged and benchmarked here. Native SimpleTest's grouped verify, clean, ran 0.47x for `random` at 512; checking each line before computing the next fixed it (6.12) | see below | 830c919 |
| 3b | Agents prepared up to three candidates per regression or opportunity (17 in all), checked by a reviewer; benchmarked here, the winners merged. ModuloX kept round 2's aggressor loop; Bench-Verify mode 30 round 2's check | see below | da06138, c2644ab |

The hot-loop counts with today's tool (the round-2 binary re-measured; the old tool missed loops over
150 instructions and stack arrays indexed by a register):

| | Round trips gpr / SIMD | Reloads gpr / SIMD | Indexed | Const | Clean kernels |
|---|---|---|---|---|---|
| Round 2 (d895c6a) | 56 / 30 | 472 / 93 | 1286 | 88 | 28 |
| Now (c2644ab) | 1 / 5 | 102 / 17 | 295 | 62 | 164 |

## Speed, round 2 to now (2026-10-20)

Round 2's binary against round 3b's, alternating, three runs each, with re-runs of the rows that
looked off (rule: three idle re-runs before a theory). Ratio = round 3b / round 2.

| Test | Ratio |
|---|---|
| Bench-Verify-TM5-0 / TM5-1 | 2.91 / 2.14 |
| Mem-MovingInv | 2.28 |
| Bench-Verify-Header / Quiet | 2.19 / 1.88 |
| Mem-Sequence: default / W,R@reverse,RW@random,R@random / pattern-mode=33 | 1.26 / 1.24 / 1.55 |
| Mem-SimpleV2 mode 0 | 1.32 |
| Mem-BlockMove | 1.16 |
| Mem-Stride (after round 1's 1.98) | 1.13 |
| Mem-SimpleV2 scalar / 512 / Auto | 1.12 / 1.09 / 1.08 |
| Mem-SimpleNT-512 / 256 | 1.075 / 1.025 |
| TM5 1usmus_v3.cfg strided SimpleTests (Tests 13, 10, 8, 12, 11) | 1.17, 1.11, 1.10, 1.09, 1.07 |
| Seal checks (Bench-Verify-Seal-*) | 0.98-1.14 |
| Mem-SimpleV2 modes 2, 10, 13, 20 | 1.03-1.06 |
| Bench-Verify-TMR-13 / 20 | 1.07 / 1.05-1.10 |
| Mem-ModuloX | 1.03 |
| Mem-Random, MirrorMove, StuckBit, Spd copies, Bench-Init-* | 0.99-1.03 |
| Bench-Verify-TMR-30 (after c2644ab) | 0.99 |
| Mem-SimpleV2 mode 30 | 0.99 |
| Mem-SimpleV2-128 mode 10 / Mem-SimpleNT-128 | 0.98 / 0.986 |

Not measured: the 128- and 256-bit Spd copies (not selectable by name; Spd-Copy-128 had been
running at 256 bits and is 128 now, so its numbers will drop, rightly); anything at cache-resident
sizes (see the follow-ups).

## What stays flagged, and why

- **Mem-ModuloX's aggressor pass** (emulated 64-bit multiply, `.rdata` constants, a stack reload):
  every clean walk tried (four) ran 7-10% slower at DRAM (Rule 6.17).
- **Bench-Verify mode 20's check** reloads two multiplier halves from the stack: AVX2's 64-bit
  multiply is emulated and needs more than the 16 ymm registers. 14% faster than the clean version.
- **Bench-Verify mode 30's check** is round 2's walk again (2 SIMD round trips): round 3's clean
  checks ran 0.91-0.95x.
- **The heavy pattern families' lines** (quiet, header, switch at 12/24 bits, in native SimpleTest
  and Mem-Sequence 256): their scalar line code in the shared generators (fmix, balanced_bytes'
  masks, pin_mask) needs more values than 15 gprs alongside the walk. Mem-Sequence 256's random
  walks hold 16 values (13-14 reloads); the scalar strided walks 8-9.
- **Mem-Sequence 512's quiet12/24** forward and reverse read one constant from `.rdata` a line
  (round 3b's rotate form, 1.6% faster than the alternative on mixed orders).
- **Mem-Stride's one round trip** is the per-stride-pass setup (cold).
- **The strided checks** keep 1 accumulator chain at 512 and 2 at 256 (unchanged): four chains
  measured slower on the TM5 config (0.83-1.01x), so per the owner they stay.

## Decisions

- 2026-10-19: the per-chunk seal calls are an `#[inline(always)]` guard around an `#[inline(never)]`
  chunk function (bf3660b).
- 2026-10-19: drop no accumulator to save registers without measuring it (the owner).
- 2026-10-19: no 8-wide variant timing for CacheBust: the clean form stays (the owner).
- 2026-10-20: Mem-MovingInv reads, checks and writes 32 B at a time (the owner: fine, and closer to
  whole-line writes).
- 2026-10-20: no release-mode check that chunks are whole 4 KiB granules; documented instead on
  `test_memory::GRANULE` (the owner).
- 2026-10-20: the strided checks keep their chains (four measured slower).

## Open, for the owner

1. The empty-`asm!` barrier (native's pointer step and header line id, Mem-Sequence's strided line
   id): no instruction, stops loop strength reduction merging or splitting counters; behind the
   header and quiet gains. Recommended: accept, as one shared documented helper.
2. Drop the scalar variants (Mem-SimpleV2, Mem-StuckBit, Mem-Refresh, Mem-MirrorV2): the AVX2 gate
   makes them no fallback, they were never scalar-shaped, and the DRAM can't tell. The owner leans
   yes; the list of what goes needs approval.
3. Merge the duplicate kernels: Bench-Init/Verify's numbered modes and legacy SimpleTest's were
   rewritten separately by two agents, so Bench no longer measures what SimpleTest runs; and three
   strided walks (native, Mem-Sequence, legacy) replace the one. Keep the faster per mode, measured.
4. A kernel-efficiency plan: every kernel at L1/L2-resident sizes, to see its CPU limit (the
   owner's idea, 2026-10-20). Mind 6.17: decide stress kernels at DRAM.
5. Mem-SimpleV2-128 mode 10 (0.98) and SimpleNT-128 (0.986): small, not chased.
6. Mem-Random: an early stop reports `cycle x the last batch's length` operations (a pre-TODO-99
   bug); and its random phase reads at most 50,000 words a cycle whatever the extent (intended?).
7. The agents' worktrees (`.claude/worktrees/`, about 23) hold many GB of build output; their
   branches stay in git.
