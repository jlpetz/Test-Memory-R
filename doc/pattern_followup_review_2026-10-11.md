# Native patterns: follow-up validation — 2026-10-11

Update, 2026-10-12: **the seed-mask recommendation is withdrawn**. Opus reports a
repeatable approximately 2.1% switch-verify regression on the 5700X at 16 and 32 bits,
and has reverted that change. The separate
[24-bit SIMD encoder](switch24_simd_research_2026-10-11.md) remains installed.
The measurements below describe the earlier review snapshot, not the current build.

The corrected hardware counts are six masked and seven unmasked runs per width,
not eight per side. Reported throughput ranges in MiB/s were:

| Width | Masked | Unmasked |
|---|---:|---:|
| 16 | 45,414–45,572 | 46,421–46,550 |
| 32 | 45,529–45,584 | 46,464–46,579 |

These are the user's supplied Opus results; I have not independently recomputed
them from raw logs. Their separation and the reported recovery after reverting
support leaving the mask out of these builds. The specific Zen 3 cause remains
unresolved: instruction count alone does not capture dependency latency, execution
resources or code layout. This percentage describes the affected verification
kernels, not a measured whole-suite slowdown.

The current native reference and legacy mode 14 still share `balanced_bytes` and
`bus_flip_line`. Reverting the mask therefore does not require duplicate packers
or CPU-specific dispatch. There is little maintainability benefit to restoring it.
Identical output preserves logical pattern coverage, but changed timing and load
can change hardware fault excitation; byte equivalence does not prove equal
hardware error detection.

The fixes pass independent validation. I found no new high- or medium-priority correctness
defect in the changed paths. The earlier halt crash, incomplete polarity pairs, lost error
context, short-test timing and native Bench dependency problems are addressed.

The original review recommended keeping the correctness implementation, tightening wording
about future transfer profiles, and adopting a small shared-generator optimization.
The last recommendation is superseded by the hardware update above. At this review's
original snapshot, that candidate was a separate patch and production sources were unchanged.

Reviewed the working tree above `468f23e35446760e1316930629a0b6efcf410cc4`, including untracked
`src/pattern.rs`. The index was empty: the production changes are uncommitted and unstaged.
The [input hashes](../research/pattern_review/followup/review-inputs.json) identify this round.
The [previous review](pattern_implementation_review_2026-10-11.md) describes an earlier snapshot.

**Fixes checked**

| Change | Independent result |
|---|---|
| Halt before a step first runs | Finalization and JSON round-trip succeed, retaining seal errors. Two skipped same-name steps also produce finite console-summary values. A completed run followed by a skipped run retains the correct averaging denominator. |
| Odd native write-round counts | All 10,000 accepted config counts normalize to complete pairs without exceeding the existing bound. Legacy modes and verify-only steps retain their separate semantics. |
| Polarity and cycle rotation | Each completed write pair shares its key and has opposite polarity. Verify-only cycles pair correctly across dispatches, including the unknown-width 64/32 alternation. |
| Built-in suite | SimpleV2 registrations explicitly use two rounds. Config/import defaults remain four. This is a workload change, as reported. |
| Failed-check evidence | A simulated initial difference in bits 0 and 47, with only bit 0 left for the rescan, now logs family, polarity, key, round, original mask and the vanished bit-47 mask. A wholly vanished failure still fails; `rescan=off` remains an unlocalized alarm. |
| Halt preservation | The rescan and halt leave four injected corruptions intact in the owned test buffer. This verifies that TMR does not repair them; it does not guarantee that an intermittent hardware fault remains observable. |
| Short-test throughput | The test clock is an optional `Duration`, rather than milliseconds with a one-second cutoff. A 250 µs per-thread interval produces the correct throughput. The runner now uses the available test clock regardless of length. |
| Native dependent Bench pairs | All five matching pairs are accepted; all twenty mismatched family pairs are rejected. |
| 12/24-bit packing | All 4,352 distinct balanced first-transfer seeds match an independent bit-by-bit oracle. Both polarities toggle on every adjacent payload-to-payload edge in the stated payload-first model. |
| Legacy 16/32/64 images | 12,288 matched hash inputs retain the original closed-form images in both native switch and legacy mode 14. The expected values do not call the new packer. |
| Verification coverage | 32,076 injected-corruption checks passed across scalar/128/256/512, all families and all five switch widths, three walks, nonzero offsets and a 37-line tail. Two equal corruptions do not cancel. |

The accepted `linepair` limitation remains a low-priority coverage tradeoff, not a new blocker.
The internal interval probe still confirms that native kernels check at chunk boundaries.
Only `PER_CHUNK` is exposed today, and this restriction is now documented.

**The extra 16-bit instruction is avoidable**

It is real CPU work: the shared expression leaves an extra scalar XOR in the generation
dependency chain. A statement that every affected loop is memory-bound is not sufficient to
dismiss it, particularly for cache-sized tests.

An isolated, optimized, four-accumulator verification probe on this local environment took
roughly 4–5% longer with the current expression than with the original expression. These are
CPU-cost samples on 8 KiB and 256 KiB buffers, not measurements of end-to-end TMR throughput
on an LPDDR machine.

The optimization investigated here was to expand only the hash nibbles
that can reach the output. Each balanced byte depends on four hash bits. The existing
12/16/24/32/64 profiles therefore consume only 8/8/12/16/32 hash bits, respectively.
Masking the unused bits **after hashing and before byte expansion** preserves every output
bit, while exposing redundant work to the optimizer.

The [now-rejected patch](../research/pattern_review/followup/switch-seed-mask-candidate.patch)
adds one shared helper and routes both native switch and mode 14 through it. It retains
the existing packer, profiles, keys, polarities and output images.

In the archived complete post-LTO binary, the sequential 256-bit writer changed as follows:

| Transfer profile | Current instructions per 64 B line | Candidate |
|---|---:|---:|
| 12 | 39 | 39 |
| 16 | 45 | 37 |
| 24 | 52 | 48 |
| 32 | 41 | 37 |
| 64 | 39 | 39 |

These are assembly instruction counts including moves and loop control, not clock cycles
or a prediction of the throughput improvement. The 128- and 512-bit writers also shed eight
instructions in their 16-bit paths. All inspected profile writers retain four XMM, two YMM
or one ZMM store per line, according to their selected SIMD width.

The final isolated verification sample, median nanoseconds per line over nine rotating-order
blocks per variant, was:

| Buffer | Original expression | Current shared expression | Seed-mask candidate |
|---|---:|---:|---:|
| 8 KiB | 2.928 | 3.084 | 2.312 |
| 256 KiB | 2.895 | 3.048 | 2.278 |

The candidate used about 25% less time in this local CPU probe. That percentage must not be
quoted as a TMR or DRAM speedup. These results established local benefit and byte equivalence;
they did not establish a general performance improvement. Fewer instructions in the actual
release loops were also insufficient evidence for adoption, as the subsequent 5700X results show.

A first experiment using narrower integer casts improved the isolated probe but optimized
back to the current loop in the full build. It is retained as an experiment, not recommended.
The seed-mask candidate above was checked in the full build.

**What the 12/24-bit results establish**

The implementation is correct for its explicit models. Re-running Opus's simulation gives
83.4% and 85.0% average switching on the illustrative 12- and 24-DQ layouts. Their definitions
and limits matter:

- The 100% figure counts adjacent payload bits in the assumed payload-first layout.
- The 83–85% figures use illustrative layouts, including synthetic payload numbering and
  a duplicated lane layout for 24 DQ. They are not measured LPDDR6 or DDR6 behavior.
- Neither measure includes a verified model of all metadata transitions, active encoding,
  controller ordering or boundaries between unrelated bursts.

The model labels in the log and saved `transfer_source` are appropriate. Three descriptions
should be tightened:

1. Say **“TMR has not verified the applicable packet map and encoding”**, rather than that
   no placement information is public. JEDEC has published LPDDR6, and public vendor material
   includes interspersed-overhead examples. This does not establish the controller's mapping
   of CPU bytes onto those slots.
2. Limit the unknown-width “full switching half the time” explanation to the nominal 32/64-DQ
   case. Alternating those profiles makes no such guarantee on an unknown 12-, 16- or 24-DQ
   interface.
3. Keep the future general-profile work explicit. Today's `Packing` is a repeated seed,
   a shift per word and an XOR mask per word. It implements the contiguous-payload rule.
   Arbitrary interspersed packet mappings may require a richer bit map; they are not all
   expressible by changing a width or those shifts alone.

For example, in the illustrative 12-DQ beat-major payload numbering, a metadata gap changes
which pin carries successive payload bits partway through a 64-bit word. A single rotation
of a repeated 12-bit seed cannot represent every such change for all seeds. The
[earlier transfer-profile reference](transfer_profile_research_2026-10-11.md) supports that
more general mapping offline; it is not yet the production packer.

Likewise, “scrambling makes the wire random” is too broad. A fixed XOR mask on a pin preserves
all its transition counts; other transformations can change them. The model should state
which transformation it assumes.

Useful public references rechecked in this round:

- [JEDEC's LPDDR6 publication announcement](https://www.businesswire.com/news/home/20250709315796/en/JEDEC-Releases-New-LPDDR6-Standard-to-Enhance-Mobile-and-AI-Memory-Performance):
  two 12-DQ subchannels, 32-byte granularity, flexible 32/64-byte access.
- [Keysight's LPDDR6 explanation](https://www.keysight.com/gb/en/assets/7126-1042/white-papers/Scale-AI-Engineering-the-Next-Leap-in-LPDDR6-Low-Power-Memory.pdf):
  256 payload bits within a 288-bit BL24 transfer and host-programmable Enhanced Write DBI.
- [Public reproduction of SK hynix's ISSCC 2026 material](https://www.allaboutcircuits.com/news/sk-hynix-details-low-power-ddr6-sdram-isscc-2026/),
  including its [metadata-in-packet figure](https://www.allaboutcircuits.com/uploads/articles/Metadata_included_within_data_packet.jpeg).
  This is a published implementation example, not a substitute for the normative tables and
  active host mapping.

**Validation and scope**

- Submitted tree: **177 tests passed**, `cargo clippy --all-targets --offline -- -D warnings`
  passed, and `cargo rustc --offline --release --bin tmr -- --emit asm` succeeded.
- **13 separate review probes passed**, including the counts above. Source copies and
  separate Cargo outputs prevented the probes from changing production files or its build cache.
- The subsequently rejected performance candidate, applied to that source copy: **190 tests passed**
  (177 product tests plus 13 review probes), all-target Clippy passed, and a complete release
  build plus post-LTO inspection succeeded.
- Toolchain: Rust `1.101.0-nightly (282215592 2026-10-04)`, LLVM `23.1.3`, `x86-64-v3`,
  Windows MSVC target.
- Inspected native loop listings contain no `memset`/`memcpy` replacement. The new packing
  remains arithmetic with constants, not a runtime data table.
- The candidate patch passes `git apply --check` against the submitted tree. It has **not**
  been applied to production or committed.

The old fill-inclusive cost table does not establish isolated verification cost; the corrected
timing path now makes that measurement possible. The unresolved difference between native
switch and legacy mode 14 on the intermittently failing machine remains a hardware question.
This review does not rank their error-finding rates or establish a hardware fault's origin.

The [follow-up artifacts and commands](../research/pattern_review/followup/README.md) preserve
the input hashes, test output, assembly comparisons and candidate patch. No production
source, Opus document, Git index, remote machine or existing build directory was cleaned up.
TMR itself was not run; the executable CPU probe was an isolated bounded test harness.
