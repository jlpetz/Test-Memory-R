# 24-bit switching: SIMD encoding and repetition — 2026-10-11

Update, 2026-10-12: Opus retained this encoder after reporting 24-bit verify throughput
of 58,026 → 59,712 MiB/s on the 8600G (+2.9%) and a −0.4% change on the 5700X.
These hardware numbers are supplied results, not measurements independently rerun
in this review. The separate seed-mask optimization was reverted after a repeatable
approximately 2.1% regression on the 5700X's 16/32-bit paths; see the
[corrected recommendation](pattern_followup_review_2026-10-11.md).
The encoder now uses an ordinary module declaration in `src/lib.rs`.
The assembly counts, test totals and patch below remain records of the October 11 snapshot.

The native 24-bit switch encoder now expands the balanced bytes and arranges the
payload in SIMD registers. It writes exactly the same pattern. The change is in
the working tree, uncommitted, on top of the previously reviewed native patterns.

**Result**

In the complete release build, the sequential writer's instructions per 64-byte
line changed as follows:

| CPU SIMD width | Previous encoder | New encoder |
|---|---:|---:|
| 128 bits | 53 | 31 |
| 256 bits | 48 | 28 |
| 512 bits | 45 | 25 |

These counts include loop control and moves; they are not cycles. The SIMD store
widths remain four XMM stores, two YMM stores or one ZMM store. At that snapshot,
the other transfer profiles retained their previous writer instruction counts.
Their later seed-mask revert is outside this comparison.

The bounded CPU probe measured the following median nanoseconds per line for the
256-bit kernels. Each entry has nine samples with rotating candidate order. The
fill occurs outside the verification timer.

| Buffer | Operation | Previous encoder | New encoder |
|---|---|---:|---:|
| 8 KiB | Fill | 3.479 | 1.693 |
| 8 KiB | Verify | 3.773 | 1.936 |
| 256 KiB | Fill | 3.377 | 1.646 |
| 256 KiB | Verify | 3.701 | 1.856 |

That is approximately half the CPU time in this probe. It is not a measurement
of TMR's aggregate DRAM throughput, a DDR6 machine, or improved hardware error
detection. The recorded [samples and ranges](../research/pattern_review/switch24/cpu-summary.json)
also include the 128- and 512-bit results and the rejected candidates.

**What changed**

The old path expanded hash nibbles into balanced bytes using scalar shifts and
masks, repeated the 24-bit seed using a 128-bit intermediate, extracted different
64-bit words and assembled vectors from them.

The replacement:

1. Keeps the same scalar hash, once per line.
2. Moves its result into an XMM register. A shift and byte interleave put the
   relevant nibbles in separate byte positions.
3. Uses `PSHUFB` with a 16-byte register table to turn each nibble into the
   corresponding balanced byte. This is a register shuffle, not a data-dependent
   memory lookup.
4. Places the three seed bytes with fixed shuffle masks and complements every
   other three-byte transfer. The existing phase supplies the final polarity.

All selector and inversion tables are compile-time constants. The complete
release loops contain inline SIMD operations, with no helper calls or
`memset`/`memcpy` replacement. Four independent verification accumulators remain.
The 128-bit verifier folds two constant-mask stack reads into XOR instructions;
the rewrite does not promise that every constant stays in a register.

The scalar hash still uses the existing two 64-bit multiplications. AVX2 has no
native packed 64-bit multiply, so moving that hash wholesale into SIMD is a
separate tradeoff. Most of this improvement comes from the byte expansion and
packing, where SIMD has suitable instructions.

Two pure 64-bit arithmetic alternatives generated more instructions than the
existing packer and were rejected. A simpler byte-shuffle-only candidate helped,
but expanding the seed in SIMD helped further.

The patch changes the native SIMD path, including its Bench use. Scalar expected
data and the legacy mode-14 implementation remain useful references. The same
hash, line ID and phase still produce identical bytes in every implementation.
No pattern version, program, CLI setting or result format changes are needed.

**Does repeating a half-line weaken detection?**

Yes for misplaced data, while maximum in-burst switching is preserved in the
current payload models. Copying one identical 32-byte half over the other is
invisible because the expected bytes are equal. A changed bit still differs
from the expected image, and the P/~P pair still provides both polarities.

| Transfer profile | Why the data repeats | Are its 32-byte halves equal? |
|---|---|---|
| 12 bits | The model restarts the same seed for each separate 32-byte payload | Always |
| 16 bits | Four-byte seed/complement motif divides both words and half-lines | Always |
| 24 bits | Six-byte motif continues through the modeled 64-byte payload | Usually different |
| 32 bits | Eight-byte motif divides a half-line | Always |
| 64 bits | Sixteen-byte motif divides a half-line | Always |

The 24-bit layout is not globally unique: it normally has only three different
64-bit word values within a line. Among its 4,096 possible balanced seeds, 16
produce a shorter motif with identical halves. All of these still meet the
same modeled switching objective. Different-looking words caused by packing
alignment do not establish general address-fault coverage.

This is a reason to run `ident`/`random` and `switch`, as the program already does.
The switch pattern supplies one particular signal history; the identity and
random patterns cover misplaced data and other histories.

**What should be consistent?**

Keep the rule consistent: follow the transfer profile's payload mapping and
complement successive transfers within each modeled burst. The 12-bit model
uses two 32-byte payloads, whereas the 24-bit model assumes one 64-byte payload.
Making their CPU words look alike would disregard that distinction.

An experiment that complements the second 32-byte half illustrates the tradeoff:

| Profile | Switching on adjacent known payload bits after that change |
|---|---:|
| 12 bits, separate 32-byte payloads | 100% within each payload |
| 16 bits, separate 32-byte payloads | 100% within each payload |
| 24 bits, one 64-byte payload | 95.08% |
| 32 bits, one 64-byte payload | 93.33% |
| 64 bits, one 64-byte payload | 85.71% |

If the two halves are actually separate bursts, different seeds or opposite
polarities could add diversity while retaining the within-burst metric. The table's
16-bit row assumes that split; it does not establish it for every LPDDR4/5 transfer.
A whole-line burst would instead lose a transition per pin at the artificial
half-line boundary. SMBIOS memory type alone does not resolve the active burst
mapping. Separate seeds also change inter-burst signal history. At 24/32/64 bits
the half-line boundary is inside the modeled burst and breaks one transition per pin.

Recommendation: retain the existing switch behavior and its identity/random
companions. Treat per-burst seed diversity as a separate, named experiment if
we want to study that signal history. There is no need for separate user-visible
patterns just because one CPU encoder uses different instructions.

These figures count adjacent **known payload-to-payload** edges. They exclude
metadata, active encoding and controller reordering. The 12/24-bit profiles
retain their model labels; the optimization establishes nothing new about the
final physical DDR6 mapping.

**Validation and artifacts**

- All **179 product tests** passed after applying the change; all-target Clippy
  passed with warnings denied.
- The isolated full candidate passed **192 tests**, including the 13 existing
  independent review probes and their **32,076 injected-corruption checks**.
- The new product test covers every one of the **4,096 possible seeds** at all
  three available SIMD widths, with both clear and nonzero unused hash bits,
  against an independent byte reference.
- The standalone comparison passed **122,880 single-bit fault injections** over
  the five candidates, three SIMD widths and both polarities, plus paired equal
  faults to check that errors do not cancel.
- Complete release assembly was inspected for writing and verifying, including
  tails and strided paths. No TMR application or live large-page test was run.

Implementation: [pattern_switch24.rs](../src/pattern_switch24.rs), dispatched by
the existing native phase macros. The [patch](../research/pattern_review/switch24/switch24-simd.patch)
archives the original four source-file changes, separately from the user's
earlier uncommitted work. The encoder is installed, but its module declaration has
since moved. Do not reapply this historical patch to the current tree.

The [research directory](../research/pattern_review/switch24/README.md) contains the
independent repetition simulation, candidate probes, raw measurements, input
hashes, assembly extracts and test logs.
