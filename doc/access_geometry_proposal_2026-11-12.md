# Proposal: explicit access geometry and useful presets

**Status:** proposal for the owner and Opus to critique before implementation. Written 2026-11-12.
This document adds no configuration fields or kernels. It leaves the importer fixes to the separate work already in progress.
The owner has declined the block-code-0 rounding change; it is outside this proposal.

## 1. Recommendation

Expose the access distance that SimpleTest and Mem-Sequence already implement, and add an optional two-line visit to their
strided walks. Give users named example configurations and explanations of when to try them. Keep the numeric controls for
experiments; choosing a useful test should not require knowing the memory controller's address mapping.

The proposed per-test fields are:

- `period_lines`: distance between the starts of successive visits within an offset pass, in 64-byte lines.
- `group_lines`: adjacent complete lines visited before taking that step; initially 1 or 2, default 1.

These describe software accesses. They do not select a channel, configure interleaving, change a DRAM burst length, or select
the SIMD width. The data pattern and its transfer profile remain independent of this geometry.

**The main reason to expose them is to run complementary shapes together.** As the owner clarified, uncertainty about the mapping
is a reason to test several shapes over the same allocation, not to leave users guessing which one to select. The recommended
user path is a combined example configuration; individual presets and numeric controls support focused comparisons.

Most of the stride machinery already exists. This is a small extension and a clearer configuration interface, not a replacement
of the test suite or a new hardware-detection subsystem.

## 2. What channel information can tell us

A user can often identify the populated channels from the motherboard documentation and DIMM placement. That information is
useful. Knowing that channels are interleaved still does not identify the physical-address bits or XOR functions that select them,
the interleave granularity, or whether every allocated region uses the same arrangement.

Consequently, documentation cannot truthfully promise:

> Set X to target exactly one channel; set Y to switch channels on every access.

It can say:

> Try Spaced to repeatedly access the same offset within successive 32 KiB address intervals. Compare it with Paired and Offset
> variation to change which address bits vary between visits. These may distribute requests differently on your controller.

Even a verified mapping would describe the explicit accesses, not necessarily all traffic: prefetching, other workers, cached
stores and controller scheduling also contribute. A preset that concentrates requests may increase conflicts and lower throughput;
it is not automatically the maximum electrical or bandwidth stress.

### A concrete reason to keep several shapes

Let `L` be a physical cache-line number. These are illustrative two-channel selectors, not claims about the user's hardware:

| First four addresses in one offset pass | `channel = L & 1` | `channel = (L XOR (L >> 9)) & 1` |
|---|---|---|
| Period 512: 0, 512, 1024, 1536 | A, A, A, A | A, B, A, B |
| Period 513: 0, 513, 1026, 1539 | A, B, A, B | A, A, A, A |

The preferred period reverses with the mapping. TMR's positions are virtual, so it cannot generally substitute them for `L`
across the whole allocation either.

For the restricted model `channel = L mod C`, a period P visits `C / gcd(P, C)` channels within a sufficiently long offset pass.
A multiple of C stays on one channel; a period coprime to C visits all C. This is useful explanatory mathematics, not a portable
hardware guarantee. An odd period only guarantees coprimality with power-of-two C, not every possible channel count.

Knowing C therefore does not make another user-facing channel-count knob necessary for native geometry. Preserve TM5's
`Channels` as an input to its chosen workload; do not automatically replace it with detected channels or DDR5 subchannel counts.

## 3. The user-facing presets

The following names would initially be documentation labels and example test entries. They do not require a new preset parser
or command-line language. Values are starting points for comparison, not experimentally established optima.

Examples show the offset-zero pass for clarity. The complete strided sweep also visits every other offset, as specified below.

| Preset | Period / group | Example visits | When to try it |
|---|---|---|---|
| Sequential | 1 / 1 | `0, 1, 2, 3, ...` | A streaming baseline with contiguous accesses; useful alongside sparse traffic |
| Close spacing | 16 / 1 | `0, 16, 32, ...` | Shorter 1 KiB spacing when comparing locality and prefetch behaviour with the 32 KiB cases |
| Spaced | 512 / 1 | `0, 512, 1024, ...` | Repeated offsets in 32 KiB intervals; a candidate for concentrating requests under some mappings |
| Paired | 512 / 2 | `(0,1), (512,513), ...` | Add adjacent-line accesses while retaining the same 32 KiB interval between group starts |
| Offset variation | 513 / 1 | `0, 513, 1026, ...` | Change low address bits as the walk advances; compare with Spaced to avoid relying on one alignment |
| Random order | Existing Mem-Sequence `order=random` | Existing reproducible shuffled traversal | Compare regular spacing with a traversal less structured by a single period |

For a user asking:

- **“I want to try concentrating traffic.”** Start with Spaced, then compare the other shapes. Do not label it “single channel.”
- **“I want accesses that may distribute traffic differently.”** Compare Paired, Offset variation and Random order.
- **“I want closer successive addresses.”** Try Close spacing or Sequential. A smaller distance does not prescribe more channel
  switches or more requests per second.
- **“I want two lines together before the next jump.”** Use Paired. This is an exact description of the software walk.
- **“I do not know which is best.”** Use a documented comparison configuration containing several shapes.

The first comparison configuration should contain Sequential, Spaced, Paired and Offset variation. Random order can be an
additional Mem-Sequence entry. A few separate entries make the work and results visible; avoid a hidden inner-loop sweep over
dozens of settings. Running more entries takes more time, which the plan must show.

In particular, **Spaced + Offset variation + Paired in one run** is the proposed access-diversity experiment. Keep their extents,
chunks and worker assignment matched so they revisit the same allocated memory. The two periods deliberately vary different
address bits; the paired shape also varies the lowest line bit within each visit. One shape may provoke a fault another misses.
This is a reason to retain complementary shapes even if their peak bandwidths are similar. It does not establish that the set
exhausts all mappings or fault conditions.

Start with an explicit comparison configuration. After correctness and performance validation, consider promoting the combined
selection into a normal native testing plan. Do not silently multiply every imported TM5 test's work or change the current default
suite as part of adding the fields.

Geometry presets must not silently change the pattern program, repetitions, extent/chunk sizes, CPU participation, SIMD width
or cache policy. Use complete P/~P pairs and make the program's family coverage across suite cycles clear. In particular, adding
four geometry steps does not imply that each step has already exercised every family of a longer pattern program.

Suggested plan text:

```text
Access: 64 B per visit, 32 KiB between visit starts; all lines checked
Access: 128 B per visit, 32 KiB between visit starts; all lines checked
```

Use “requested access order” where distinguishing it from memory-bus order matters. Avoid names such as “single-channel,”
“quad-channel stress,” or “guaranteed row misses.”

## 4. Existing capabilities to reuse

| Existing code/configuration | Relationship to this proposal |
|---|---|
| SimpleTest `parameter`, with global `channels` | Already resolves a positive parameter to period `channels * parameter` lines |
| `TestParameterContext.stride_cachelines` / `stride_elements` | Internal skip values; not direct fields on the JSON `TestConfig` |
| Mem-Sequence `order` and `sequence` | Already supplies forward, reverse, random and strided traversal, with W/R/RW operations |
| Mem-Stride `subdivisions` | Uses a fixed selection of strides in 8-byte words; preserve its distinct workload |
| CacheBust `stride_patterns` | Changes data backgrounds on its fixed page-strided walk, not the access distance |
| Mirror `jump:N` | Already operates on 128 B units; its paired swaps are different operations from grouped SimpleTest visits |
| Sequential SIMD loops' groups of vectors | Internal unrolling and accumulator scheduling; not a selectable grouped strided walk |

At global `channels=2`, the existing positive `parameter` can express only even periods. A user could change the global multiplier
to 1, but native per-test geometry should not require that workaround or affect other legacy-style entries.

For the two-channel TM5 case already reviewed, Parameter 256 gives a 32 KiB period in both interleave forms: one accesses 64 B
per visit and the other 128 B. Those correspond to period/group 512/1 and 512/2. This explains the abstraction; it is not a claim
that the two resulting workloads have equal stress.

This proposal does not add an `Interleave Type=0` importer implementation. Existing supported imports keep their current walk.
Grouped visits are a native testing option. Mirror kernels and the dedicated Stride/CacheBust tests retain their current contracts.

Source anchors, reviewed against local main `30fe263` and the working tree:

- `src/config.rs`: `TestConfig`, `TestParameterContext`, `interpret_tm5_parameter_with_channels`, `check_sequence`.
- `src/tests.rs`: `strided_walk!`, `native_differs_strided!`, `sequence_strided!`, `mirror_swap_jump!`.
- `src/sequence.rs`: `Order`, `Op`, `Sequence`.
- `../TM5/bin/mtests0.asm`: SimpleTest's `BlkSize` / `JumpStep` calculation.

## 5. Proposed configuration contract

The following are proposed test-entry fragments, not configurations supported by today's executable:

```json
{
  "function": "Mem-SimpleV2-Auto",
  "period_lines": 512,
  "group_lines": 2
}
```

```json
{
  "function": "Mem-Sequence",
  "order": "stride",
  "period_lines": 513,
  "group_lines": 1
}
```

Other required configuration fields and the existing test settings are omitted from these fragments.

Recommended rules:

1. Initially accept these fields only on SimpleTest's supported variants and Mem-Sequence.
2. `period_lines` is a positive integer. Zero is an error; 1 with group 1 is sequential access.
3. `group_lines` defaults to 1 and initially accepts only 1 or 2. An explicit `group_lines` requires `period_lines`.
4. Require `period_lines >= group_lines` and divisibility by `group_lines`. Thus 513/1 and 514/2 are valid, but 513/2 is not.
   This restriction keeps complete non-overlapping group coverage simple. It is not a power-of-two restriction on periods.
5. Reject a test entry containing both `parameter` and `period_lines`, rather than silently choosing one.
6. Without the new fields, preserve current behaviour, including `parameter=0` and the legacy sequential case.
   With the new fields, global `channels` and a `channels=` override do not multiply the explicit period.
7. Mem-Sequence must contain a strided pass if it specifies this geometry. Its other passes retain their own orders.
   Update sequence validation to inspect the resolved geometry, not require the old raw `parameter` field.
8. Group 2 initially requires native patterns. Keep the legacy numbered kernels at group 1 and reject a grouped request they cannot
   execute. Resolve native-family numeric aliases before making this check; Mem-Sequence already requires native patterns.
9. Reject unused new fields on other tests. Check integer conversion and byte/pointer arithmetic before running.

No new global period/group override is necessary in the first implementation. Per-test entries are sufficient for mixed suites.

Resolve both old and new inputs into one access description before entering the kernel. Keep its source available for the plan
and saved results: explicit geometry, legacy Channels/Parameter, or the existing default. Retain raw import inputs as provenance.
This does not require changing TM5 configs or inferring hardware topology.

## 6. Exact walk and coverage contract

Let N be the number of 64-byte lines in a chunk, P the period and G the group size. Current chunks are whole 4 KiB granules,
so N is divisible by both supported group sizes. All positions below are relative to the chunk's start.

Define one complete strided sweep as:

```python
for offset in reversed(range(0, min(P, N), G)):
    for first in range(offset, N, P):
        for line in range(first, first + G):
            visit(line)
```

This is a specification, not the intended implementation of the hot loop.

- Descending offset passes preserve the existing group-1 order. Lines inside a group are visited in ascending order.
- Each line is visited exactly once per complete sweep.
- No line is accessed outside the chunk.
- P does not need to divide N. An uneven final offset pass is handled naturally.
- P may exceed N: there are at most N/G offset passes, avoiding arbitrarily many empty passes. There is then only one group
  per offset pass, so the requested long-hop behaviour is absent. Report this degeneration once in the plan when it occurs.
- A two-line group is two ordinary cache lines, not an atomic 128 B transaction.

Coverage proof: every line i has a unique representation
`i = q*P + r + j`, where `r = G*floor((i mod P)/G)` and `0 <= j < G`.
Because G divides P and N, r names one offset pass and its group contains i without crossing the chunk end. This also handles
P greater than N. Thus changing groups does not introduce omissions or duplicate visits within a sweep.

During proposal preparation, an offline check exercised 1,035 valid N/P/G combinations, including short chunks, non-dividing
periods and periods larger than chunks. Every sweep covered each line exactly once; group 1 matched the existing offset-pass
order. The two illustrative channel selectors and conditional gcd formula were also checked. These validate the mathematics
in this document, not an implementation or hardware error sensitivity.

The whole extent still uses TMR's existing overlapping chunks. A line may therefore be visited again through a different chunk,
as it is today. Its expected value must remain derived from its stable position, epoch, family and polarity; do not seed by group
number, traversal counter or chunk origin. Geometry changes order, not identity.

Preserve the existing OR-accumulated checks, rescans, halt behaviour, seal boundaries and P/~P scheduling. Check both lines of
every group, including every vector lane and the last group. Count bytes from lines actually processed, not the number of groups.

## 7. Keep pattern profiles and traffic policy separate

`beat=` selects the data layout used by switching-related families. It does not set the access distance or the number of lines
visited together. Ordinary identity/random families remain independent of bus width.

SMBIOS memory type selects a conventional profile; it does not measure the controller's encoding or burst ordering. The current
12/24-bit profiles remain models. Future DDR/LPDDR profiles can change the pattern encoder without requiring different geometry
parameters or presets. A per-test transfer-profile override is a separate possible enhancement, not required for this work.

Likewise, W/R/RW sequencing and cache exposure remain explicit. A cached read followed by a store is not proof of a DRAM
read/write turnaround. Small cache-resident chunks are useful tests too; do not silently flush them because a geometry preset
has a memory-oriented name. Geometry comparisons should state their cache policy.

## 8. Implementation and validation after approval

1. Add configuration resolution and the direct period field, retaining the existing group-1 kernel path.
2. Add group 2 for the native SimpleTest and Mem-Sequence strided paths. Use the existing per-width macro approach and dispatch
   outside the hot loop. The group size must not become a branch per line. Do not refactor unrelated kernels.
3. Preserve group 1's current prefetch behaviour. For group 2, aim to preserve lookahead measured in visited lines and prefetches
   per visited line; treating a group as a former single line would otherwise double the lead. Validate the resulting prefetch
   sequence and measure it rather than assuming an identical instruction count or speed.
4. Add the example configurations and plan/result descriptions once the access contract is implemented.

Before hardware comparisons:

- Record actual kernel visits and compare them with the independent walk specification. Exercise periods smaller and larger
  than chunks, periods not dividing chunk lengths, overlapping chunks, and both group sizes.
- Prove that all SIMD widths write the same image for a fixed phase. Check guard regions and inject errors into both lines,
  every accumulator/vector lane, and final groups. Verify a transient still fails when its rescan is clean.
- Keep group 1's legacy order and images unchanged. Check explicit-period/legacy-input equivalence and invalid configuration errors.
- Inspect post-LTO assembly and spills under the repository's normal kernel rules. Keep four independent accumulators and
  verify that the selected SIMD width survives.

Use small offline mapping models to explain differences, not to score actual error-detection effectiveness. Include direct
low-bit, higher-bit and XOR selectors rather than optimizing a preset for one synthetic controller.

For an isolated geometry benchmark, use the same allocated buffer, phase/key, family, SIMD width and cache policy. A fixed run
seed alone does not give different application steps identical data: their epoch also includes step identity. A probe can hold
the phase fixed without changing the application's seed design.

Hardware runs should be a small follow-up when machines are available, using the existing alternating A/B tooling. Compare
throughput and, on a failing system, error detection. Neither faster throughput nor zero errors on a stable run proves improved
coverage. No broad hardware campaign or physical-address mapping is needed to review this proposal.

## 9. Questions for Opus's critique

1. Does the combined preset deliver useful access diversity without leaving users to guess a channel mapping?
2. Are two group sizes sufficient for the first experiment, and is requiring P to be divisible by G the right initial contract?
3. Can the direct period reuse the current group-1 path without changing its measured loop shape?
4. What is the cheapest group-2 implementation that preserves the image, complete checking and four independent accumulators?
5. Does the proposed prefetch policy keep the comparison meaningful without costing more than grouping can gain?
6. Does the existing plan/results schema need only resolved period/group fields, or another representation to preserve replay?

The proposed decision is to expose existing access distance clearly, add one grouped alternative, and run complementary shapes
over the same allocation through an understandable combined preset. Their value is in giving faults different opportunities to
appear while preserving complete verification.
