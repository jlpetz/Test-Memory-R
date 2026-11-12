# AMD and Intel memory mapping: implications for TMR

Research and source review, 2026-11-12. Companion to the
[access-geometry proposal](access_geometry_proposal_2026-11-12.md).
No runtime fields, kernels or BIOS settings changed.

## Finding

Memory address mapping is partly public and configurable. There is enough information to choose meaningful contrasting
workloads and to evaluate them against example mappings. There is not one portable formula obtainable from DDR generation,
populated channels and an interleaving switch.

Keep `period_lines` and `group_lines` as descriptions of requested accesses. Use published mappings as evidence for the
choice of example configurations, without turning them into a hardware-mapping dependency in TMR.

## What the sources establish

1. **AMD documents an actual bank-group swap in an older controller.**
   Family 15h Models 70h-7Fh BKDG, publication 55072 Rev 3.09, page 375, describes `BankGroupSwap` as exchanging normalized
   address bits 7:6 with bank-group bits. The adjacent `BankSwizzleMode` description specifies XOR-based bank selection.
   This is a concrete historical example, not the bit formula for every Ryzen generation.
   [AMD BKDG](https://www.amd.com/content/dam/amd/en/documents/archived-tech-docs/programmer-references/55072_AMD_Family_15h_Models_70h-7Fh_BKDG.pdf).

2. **BGS Alt is not enough information to recover a modern map.**
   ASUS's ESC N4A-E11 manual, page 80, exposes `Address Hash Bank 2 ColXor` when BGS Alt is enabled, plus separate bank,
   chip-select and RM hash controls. This demonstrates additional configuration behind the BIOS label. The sources checked
   do not establish a universal BGS Alt formula across Zen generations.
   [ASUS manual page](https://www.manualowl.com/m/Asus/ESC-N4A-E11/Manual/653779?page=80).

3. **Modern AMD DDR5 mappings have been published.**
   ZenHammer's Table 11 gives Ryzen 7 7700X mappings for three DIMM geometries on a single-channel, single-DIMM setup.
   Subchannel selection combines a low address bit with higher bits; the masks vary with geometry. Its address-remapping
   treatment also shows why a formula may need a physical-range correction.
   [ZenHammer, USENIX Security 2024](https://comsec.ethz.ch/wp-content/files/zenhammer_sec24.pdf), Table 11 and section 4.

4. **Intel exposes useful configuration details directly.**
   The Raptor Lake-S FSP header documents a channel-interleave bit selection from address bits 6 through 13 and a channel
   XOR mask covering bits 19:6. It describes bit 7, 8 and 9 selections as two-, four- and eight-cache-line granularity.
   Defaults in this firmware interface are not evidence of a particular motherboard's active settings.
   [Intel FSP header](https://raw.githubusercontent.com/intel/FSP/master/RaptorLakeFspBinPkg/Client/RaptorLakeS/Include/FspmUpd.h),
   `ChHashEnable`, `ChHashOverride`, `ChHashInterleaveBit`, `ChHashMask`.

5. **Intel's current public register documentation separates the controls.**
   Core Series 3 document 917382 describes channel XOR selection, selectable low-bit contributions to bank-group hashing,
   extended bank hashing, and enhanced interleaving. They are distinct settings.
   [Channel hash register](https://edc.intel.com/content/www/us/en/design/publications/core-processors-series-3-cfg-and-mem-registers/001/mad-channel-hash-0-0-0-mchbar-mad-channel-hash-0-0-0-mchbar-offset-d824/);
   [DRAM hash register](https://edc.intel.com/content/www/us/en/design/publications/core-processors-series-3-cfg-and-mem-registers/001/mad-dram-hash-0-0-0-mchbar-mad-dram-hash-0-0-0-mchbar-0-offset-d80c/).

6. **Cross-vendor measurements supply further examples.**
   Sudoku's Tables 1-2 report mappings for a Ryzen 9 7950X and Intel i9-12900K/i9-14900K systems under multiple DIMM
   populations. Its timing analysis distinguishes same/different bank-group accesses. These are configuration-specific
   results, not universal maps selected by CPUID.
   [Sudoku, 2025 preprint](https://arxiv.org/html/2506.15918v1).

Bank groups are groups of banks inside DRAM, within a rank on a channel/subchannel. Their scheduling constraints matter:
concentrating requests can exercise different timing paths from distributing them. A busy bank can also stall traffic, so
concentration is not synonymous with maximum bandwidth or maximum electrical stress. BGS does not change the number of DQ pins
or make data values alternate on them.

## Useful consistency without mapping the machine

Ordinary 4 KiB page translation preserves the low 12 address bits. A larger page preserves more offset bits within that page.
Thus TMR can deliberately change low address bits without learning a page's physical frame. Across stitched pages, higher
physical-address bits remain unknown.

For an XOR selector `f(a) = parity(mask & a)`, the relative selector change is:

```text
f(a) XOR f(b) = parity(mask & (a XOR b))
```

Within one physical page, its fixed high bits cancel. This permits conditional predictions from a known selector, although it
does not reveal the mask or the absolute channel/bank number.

For example, Intel's documented channel-register example uses address bits 8, 16 and 17. Within one 4 KiB page the latter two
are fixed. Consecutive 64-byte lines then select channels in groups of four, up to an A/B label swap. A selector using bit 6
instead would alternate on every line. This is why two-line groups cannot be assumed to cross a channel boundary.

More generally, a power-of-two period preserves a set of low line-address bits within an offset pass; a nearby odd period
changes them. An odd period visits every residue modulo a power of two over a sufficiently long sequence. This is arithmetic
about requested addresses, not a guarantee about hashed DRAM destinations or the order in which the controller serves requests.

## Recommendation for presets

Retain Sequential, Spaced 512/1, Offset variation 513/1 and Paired 512/2 as the initial comparison. Keep Close spacing 16/1
available. Their purpose is complementary access structure over the same allocation, not a claim that 512 is optimal.

Before choosing permanent defaults, evaluate short periods such as 2, 4, 8 and 16 and their nearby odd alternatives against
the published examples. Also evaluate groups of 4 and 8 offline; these vary more low bits than group 2. This does not require
adding those group sizes to the first implementation. Prefer a small selection with distinct behaviours over dozens of presets.

For each model, distinguish channel/subchannel distribution, bank-group distribution and repeated-bank accesses. Use several
base addresses and page layouts; count complete sweeps, not just their offset-zero pass. Such analysis can remove redundant
candidates and identify differences, but cannot rank real error detection.

Do not select one workload from a BGS toggle or automatically double a stride for DDR5. Test the BIOS configuration the user
intends to run; changing BGS is a separate system comparison that also changes the test environment.

`beat=` remains the data-pattern transfer profile. It is independent of this access geometry, including for future DDR/LPDDR
generations. Knowing the DQ width does not determine bank or channel address selection.

## Review of importer commit 8675f6c

The original review compared the commit with its parent and `../TM5/`, without a build or live TMR test. The missing-Test0 fix
and code-0 rounding revert passed `python tools/ci_local.py --quick`: formatting, build-tool checks, all three Clippy
configurations and 572 Rust tests (six skipped). No live memory test was run.

Confirmed:

- Every import sets `skip_spec="0"`. CLI `skip-cores=` can still override it.
- Nonzero `Cores=N` reaches selection as an absolute logical-CPU count. The existing selector caps it to availability; zero
  requests 100%. CLI `cpus=` still wins.
- Section/key case is normalized while values retain their case.
- All 96 entries in the six per-slot default arrays match the assembly tables. An independent text comparison checked this;
  `DefTstBlkSizeMb` is 16. The per-test time default is 100 (`DefTestLength`).
- The code-0 floor matched the assembly's divide/floor/clamp sequence and kept chunks bounded by the extent. It is now reverted
  for the traversal-work reason below; codes 1-3 retain their existing flooring.

Remaining qualifications:

1. **Missing Test0 fixed in the follow-up; ordinary missing slots remain open.** Omitting `[Test0]` previously left TMR's seal
   off. The importer now supplies that slot's enabled RefreshStable defaults, independent of `Test Sequence`, preserving explicit
   `Enable=0` and CLI `seal=` overrides. All 11 configs in `../TM5_configs/bin` already declare enabled RefreshStable Test0, so none
   had this defect. Four omit 0 from their sequences; those seals already worked. Entirely missing nonzero slots still cause
   rejection when referenced, whereas TM5 loads defaults governed by `Tests=`. That broader compatibility gap remains open.
2. **The complete ABSOLUT regression fixture is still absent.** The new focused tests do not substitute for importing the real
   file and pinning its complete execution plan.
3. **Documentation corrected in the follow-up.** `doc/test_parameters.md` now distinguishes ordinary JSON's reserve from imports'
   `skip-cores=0`. The validation report's incorrect external-northbridge explanation of ganged mode has been replaced.
   AMD Family 10h integrated controllers supported it
   ([AMD BKDG 31116](https://www.amd.com/content/dam/amd/en/documents/archived-tech-docs/programmer-references/31116.pdf),
   section 2.8); modern CPUs can execute a two-line access group regardless of their channel organization. The decision to defer
   the legacy importer option stands without that explanation.
4. **The rounding change is reverted.** Its effect was not limited to Ollie: it also changed code-0 chunks when the available
   extent capped an otherwise divisible window to a nonmultiple of lock granularity. At a 500 MiB extent it changed a 500 MiB
   chunk into 496 MiB. `ChunkSpread` then produced two chunks at offsets 0 and 4 MiB: 992 MiB traversed per sweep, with 492 MiB
   overlap, rather than 500 MiB with no overlap. Coverage was complete in both cases; traversal work increased 98.4%.
   The earlier description as a minor alignment adjustment understated this consequence. At the owner's request the follow-up
   restores the previous code-0 rule and tests that the 500 MiB case stays one chunk, traverses 500 MiB and has no overlap.

The CPU-selection, key-matching and present-field default fixes should be retained. No new functional blocker was found in
their implementation. The remaining default-section gap and missing full-file fixture should stay explicit rather than being
reported as completed.
