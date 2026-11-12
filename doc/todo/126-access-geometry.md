# TODO 126: Explicit access geometry (`period_lines`, `group_lines`) as per-test tunables

## Why (Astra, 2026-11-12, from the Check_absolutnew.cfg validation)

Reviewing TM5's `Channels`/`Parameter` stride formula raised the question of whether TMR should try to target specific DRAM
channels. Neither channel count nor an interleaving toggle establishes the active address mapping. Reliably targeting hardware
destinations also needs physical-address information, which this project has decided to stay out of
(`TMR-APP/CLAUDE.md`, "No physical-address awareness"). Published mappings (AMD BKDG, ZenHammer,
Intel's FSP header and public register docs — see `doc/memory_mapping_research_2026-11-12.md`) confirm the mapping is partly
documented but not a portable formula from CPU name, channel count or a BIOS toggle.

The proposal's reframing: expose SimpleTest/Mem-Sequence's existing access distance and add adjacent-line grouping, without
claiming it selects hardware. Value is access-pattern diversity over the same allocation (several
shapes in one run, sharing memory state and conditions, per the project's existing "Pattern Comparisons In One Run" practice),
not proof of hitting a specific channel.

## Decided (the owner and Opus, 2026-11-12)

- Direction endorsed: build it, after the current round (117, 119, 122).
- Preset naming must not imply hardware targeting ("Spaced", "Paired" etc. need their *displayed* text to carry the caveat,
  not just surrounding prose) — the plan-text format in the proposal's §3 ("Access: 64 B per visit, 32 KiB between visit
  starts") is the right model: no hardware-sounding noun at all.
- The approved importer work and pattern-conversion reporting are complete. The owner declined the full ABSOLUT regression
  fixture and ordinary missing-section compatibility; neither remains a prerequisite.
- The owner requested a hardware-stress rationale and assessment of larger groups before implementation. The proposal now
  includes TM5's stated intention (one DIMM before the next), conditional concentration/distribution effects, and G=2/4/8/16.
  This is an expanded assessment, not approval of every candidate as a runtime setting.

## Full proposal and research

- `doc/access_geometry_proposal_2026-11-12.md`: the proposed `period_lines`/`group_lines` fields, named presets, the exact
  walk/coverage proof, configuration contract, hardware-stress hypotheses, macro sketch, and questions for critique.
- `research/access_geometry/models.py`: reproducible additional coverage checks (364 N/P/G cases through G=16), plus six
  illustrative channel selectors over full sweeps and five aligned bases. No hardware traffic or error detection is modeled.
- `doc/memory_mapping_research_2026-11-12.md`: the AMD/Intel sourcing (spot-checked by Opus against the Intel FSP header
  directly — exact match on `ChHashInterleaveBit`/`ChHashMask`'s bit ranges and granularity claims), and the review of importer
  commit `8675f6c` that found the code-0 chunk-doubling regression (independently verified, then fixed by reverting the floor
  rather than reordering it — see commit `3517ed7`).

## Open questions before building

1. Macro cost: compare constant-offset expansion against a bounded runtime/microtile candidate at one width. Macros keep the
   source shared but may emit specialized code across family/beat/operation/group/ISA. Auto dispatches the three real widths
   (128/256/512); it is not a fourth width. A separate kernel per G is an implementation choice, not a consequence of the rule.
2. Grouped prefetch lookahead and speed must be measured (`bench_ab.py`, `hot_loop_spills.py`), not assumed free.
3. Whether `period_lines`/`group_lines` reuse the group-1 hot path unchanged for the direct-period case (should be yes; confirm
   via post-LTO asm per this repo's asm-check rule).
4. Select the shipped group set after the bounded experiment. G=2/4/8 are the primary candidates; G=16 differs under coarser
   interleave models. Keep P divisible by G. Defer odd G: different per-visit imbalance is possible, but adds tail/period cases
   without evidence of a required error-detection benefit.

## Next

Pick up after the current round (117, 119, 122). Review the expanded hardware rationale and one-width implementation sketch,
then run the bounded kernel experiment before selecting the first grouped implementation. Importer work is settled.
