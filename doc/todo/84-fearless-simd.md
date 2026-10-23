# TODO 84. [TMR-APP] Assess fearless_simd as a replacement for TMR's SIMD code

**Raised** 2026-10-02 (the user). **Closed** 2026-10-23: no port (the owner).

## History

- 2026-10-03: first assessment in `../fearlessD/` (`FINDINGS.md`): fearless_simd 1.0 ports of 78
  kernels at about ±2% of TMR's, on Intel. The work moved to `../plumb` (published 2026-10-05):
  `plumb_lines` (cache-line operations for fearless vectors), `plumb_tiles` (AMX), and a bench of
  TMR-style twins behind an asm gate.
- 2026-10-19..21: TODO 99 rewrote TMR's hot loops, which left that evidence stale: the copied kernels
  were from before it.
- 2026-10-23: re-assessed against the current kernels (plumb TODO 12), compile-only.

## The 2026-10-23 assessment

**Method.** A workflow of 74 agents. One agent per kernel family listed its hot loops and the
fearless_simd 1.0.0 APIs that would express them, citing the source. Another catalogued fearless,
and a third checked plumb's coverage. Then 30 gap claims, each checked by two independent reviewers:
one through the API, one by compiling a probe and reading its asm (scratch crates, nightly 1.101,
x86-64-v3). Nothing was timed. The deciding results were spot-checked by hand afterwards. The full
report, every verdict with its file:line evidence, is `../fearlessD/FINDINGS-post99.md` (and
`.json`); the probe crates were deleted once it was written.

**Speed: still in the ballpark.** TODO 99's gains are loop structure: a function per loop, a pointer
walked to a bound, explicit vectors, the `asm!` barrier, a line checked before the next is computed,
four lines hashed in one vector. A fearless `#[simd]` body carries all of it unchanged. Ports from
every family compiled to TMR's hot loops instruction for instruction, or within one or two
instructions with no stack traffic: 11 of 14 SimpleTest mode pairs, 6 of 8 Mem-Sequence loops
including the 512-bit hashed steps, every CacheBust/Random/Stride loop at AVX2.

**But only in TMR's own shapes.** Fearless's idiomatic forms (slices, `as_chunks`, `from_fn`
builders, plain copies, dispatching everything at the highest level) are the ones that compile
worse, mostly silently. Raw pointers, `unsafe` and the `asm!` barrier stay. The 9 gaps that stand
(13 of the 30 claims were refuted and 8 are disputed), none of them a speed blocker:

- Fearless's Avx512 level needs the full Ice Lake set (IFMA, VBMI and VBMI2, VNNI, BITALG,
  VPOPCNTDQ, GFNI and more; `fearless_simd-1.0.0/src/lib.rs:290-322`), where TMR's 512-bit tests need
  F, BW, CD, DQ and VL (`simd::has_avx512`). Older AVX-512 CPUs would lose those tests, or TMR would
  keep a second hand-written 512 path.
- The Avx512 level reshapes code TMR keeps at 256 bits or scalar: gathers and scatters (CacheBust,
  Random), merged zmm copies, a zmm `vpmullq` in ModuloX's aggressor, `vpermb`. TMR's own 512
  kernels have no opt-out.
- Silent traps: copies with no operation between load and store go through the stack; a 512-bit
  helper can run as two ymm halves; `from_fn` builders can become a call per operation; 128-bit code
  is raised past xmm.
- Helpers neither fearless nor plumb ships (a move, a volatile 128-bit access, a 64-bit multiply, a
  bridge for permutes); no MOVNTI; `kernel!` takes only safe, non-generic functions.
- plumb's parity evidence is stale except for StuckBit and Refresh, whose loops TODO 99 didn't touch.

**The 2026-10-22 on-paper claims.** No OR-reduce: true, and free (a split tree gives TMR's
sequence). No two-input swizzle: half true (interleave, slide and select exist, and TMR's sequences
are reachable). AVX2's 64-bit multiply as four scalar ones: true in the source, but re-vectorized in
optimised builds; in a fat-LTO probe the fearless and TMR loops have the same instructions.

**Method lesson.** Asm emitted from a library crate is pre-LTO, and plumb's bench builds without LTO
while TMR ships fat LTO, so plumb's TMR-style copies don't reproduce TMR's binary exactly.

## Decision (the owner, 2026-10-23)

No port. Fearless_simd's value is one kernel for many instruction sets, with runtime dispatch and
type-checked tokens. TMR is x86-only and its widths are test parameters. A port would match TMR's
speed only by reproducing its loops line for line, so the kernels wouldn't get shorter, and it would
add the Ice Lake requirement (or a second 512 path), the reshaping, new silent traps, slower builds
and a rebaseline on Intel and AMD.

Still useful from plumb: `plumb_tiles`, which has no fearless dependency (the AMX layer and the
transform-verify test, plumb TODO 6, a TMR TODO candidate), and the upstream work (CLFLUSHOPT
stabilization, MOVDIR64B, LLVM's non-temporal store fix), which TMR's own code uses. A gap in TMR's
kernel tools found on the way is TODO 101.
