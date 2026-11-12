# Proposal: explicit access geometry and useful presets

**Status:** proposal for the owner and Opus to critique before implementation. Written 2026-11-12.
This document adds no configuration fields or kernels. The approved importer fixes and pattern-conversion reporting are complete.
Block-code-0 rounding is outside this proposal. The change in importer commit `8675f6c` is reverted at the owner's request because
it could nearly double traversal work. The source review is in [the follow-up research note](memory_mapping_research_2026-11-12.md#review-of-importer-commit-8675f6c).

## 1. Recommendation

Expose the access distance that SimpleTest and Mem-Sequence already implement, and add grouped visits to their strided walks.
Give users named example configurations and explanations of when to try them. Keep the numeric controls for
experiments; choosing a useful test should not require knowing the memory controller's address mapping.

The proposed per-test fields are:

- `period_lines`: distance between the starts of successive visits within an offset pass, in 64-byte lines.
- `group_lines`: adjacent complete lines visited before taking that step; default 1. Evaluate 2, 4 and 8 for the initial
  implementation, with 16 as a further experimental candidate. Two is not a hardware limit.

These describe software accesses. They do not select a channel, configure interleaving, change a DRAM burst length, or select
the SIMD width. The data pattern and its transfer profile remain independent of this geometry.

**The main reason to expose them is to run complementary shapes together.** As the owner clarified, uncertainty about the mapping
is a reason to test several shapes over the same allocation, not to leave users guessing which one to select. The recommended
user path is a combined example configuration; individual presets and numeric controls support focused comparisons.

Most of the group-1 stride machinery already exists. Exposing its period is straightforward; adding fast grouped kernels needs
its own code-size and performance check. The implementation options are in section 8.

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

### Public mappings and BIOS controls

Follow-up research covers AMD BGS/BGS Alt, published Ryzen DDR5 mappings, and Intel's documented channel and bank-group hash
controls: [AMD and Intel memory mapping research](memory_mapping_research_2026-11-12.md).
The mapping is partly documented and configurable, not wholly opaque. Use these sources to evaluate candidate access shapes
offline; a CPU name, channel count or BGS setting alone does not establish the active mapping.

Page translation preserves the low 12 address bits in an ordinary 4 KiB mapping. This gives short strides and adjacent-line
groups useful structure even without physical addresses. Larger pages preserve more offset bits within each page, but stitching
pages does not make their physical placement contiguous. These facts refine the rationale; they do not change the proposed fields
or prove a particular preset's error sensitivity.

### TM5's names, intended target and the current translation

`Interleave Type` selects TM5's model of the access unit. It does not mean interleaving off/on and does not change a BIOS setting.
Interleaving normally distributes address regions between independently usable resources; it does not require one software
access to hit every channel simultaneously. Separate outstanding requests can nevertheless keep several channels busy.

The local [TM5 author's notes](../../TM5/TM5_Author_website_notes.txt) state:

> The idea is that one DIMM is checked first, then the second (and third, for variants like Core i7).

This is the author's explanation of positive SimpleTest `Parameter`. His intended effect was to concentrate an offset pass on
one DIMM under his assumed mapping, then cover the others in later passes. In the separate MirrorMove128 discussion, he says
that under the newer 64-byte interleaving model, “when accessing 128 bytes, both DIMMs are read.” That explains an intended
benefit of adjacent-line groups. These are historical design intentions, not verified descriptions of every modern controller.
His “paired” 128-byte unit must not be read as a claim that Core 2 has 128-byte architectural cache lines.

For the usual `Channels=2`, positive `Parameter=J`, the assembly and TMR resolve as follows:

| Input/path | Access unit per visit | Period between visit starts | Interpretation |
|---|---:|---:|---|
| TM5 `Interleave Type=1` | 64 B / one line | `2 * J` lines | Newer block-interleave model; individual-line offset passes |
| TM5 `Interleave Type=0` | 128 B / two lines | `2 * J` lines | Older paired-access model; adjacent-line offset passes |
| TMR import today | 64 B / one line | `2 * J` lines | Uses the first geometry; does not consume `Interleave Type` |
| Proposed explicit geometry | `64 * G` B | `P` lines | Describes exactly the intended software walk |

At J=256 these are P/G=512/1 and 512/2. At J=1, under the illustrative alternating-line channel map, the one-line walk accesses
`1,3,5,...` on B, then `0,2,4,...` on A. The two-line walk accesses `(0,1),(2,3),...`, involving A and B within each visit.
Neither walk leaves any line untested. This is a concrete concentration-versus-distribution example behind the terminology.

For TMR's current general positive-parameter case, `skip_lines = Channels * Parameter - 1`; after processing the current line,
the next line begins `period_lines = Channels * Parameter` away. Parameter zero selects sequential access. Channels is thus a
workload multiplier, not a measured or enforced channel count. The old TM5 type-0 formula is different: it omits that multiplier
and doubles its access unit when Channels is greater than one. Do not generalize type 0 as “group size equals channel count.”

Source: `../TM5/bin/mtests0.asm`, SimpleTest's `BlkSize`/`JumpStep` and `GetDIMM2DIMMstep`; `../TM5/mt_ini.inc`, the fixed
64-bit DIMM width and 64-byte block defaults; `src/config.rs`, `interpret_tm5_parameter_with_channels`; `src/tests.rs`,
`strided_period` and `strided_walk!`. Neither historical label is a portable synonym for ganged/unganged operation.

### Hypothesized stress effects

These are reasons to compare workloads, not established rankings of error detection. A “visit” below is a sequence of ordinary
loads/stores, without an extra fence or atomic multi-line transaction.

| Geometry/change | Possible hardware effect and fault opportunity | What it does not establish |
|---|---|---|
| Sparse, repeated-offset visits, e.g. 512/1 | May concentrate requests in one channel, bank group or bank. If those requests select different rows of the same bank, repeated precharge/activate operations can exercise row-cycle timing margins. | A large stride need not cause a row conflict; it can also spread requests or retain row locality. Concentration can reduce traffic. |
| Nearby period, e.g. 513/1 | Changes low address bits between visits. May disturb the destination concentration of a power-of-two stride and vary bank/channel queue pressure. | Does not guarantee channel alternation, defeat hashing, or beat the original period. |
| Larger adjacent groups at a fixed period | Can cross low-bit channel/bank-group boundaries within a visit, making independent destinations available close together. Alternatively, several lines may share an open row, creating short runs of efficient transfers between long jumps. | “More channels per visit” and “more row hits” are alternative mapping-dependent effects, not simultaneous guarantees. |
| Longer contiguous runs, including sequential access | Can improve prefetch efficiency, row reuse and sustained bus occupancy; useful for bandwidth, thermal load and repeated data-pattern transfers. | Higher bandwidth need not impose the most difficult row-cycle or controller-scheduling conditions. |
| Groups combined with explicit W/R/RW operations | Changes the locality and ordering of read and write demand, which may alter write draining and bus-direction changes. | Channel switching is not read/write turnaround. CPU order, cache policy and the controller determine actual DRAM direction changes. |
| Different spacing at the same group size | Changes cache-set reuse, TLB pressure, prefetch training and available concurrent misses as well as possible DRAM destinations. | A timing difference alone does not locate the bottleneck in DRAM or prove increased memory stress. |

One sweep covers the same addresses once for every valid geometry. With a fixed mapping, it therefore has the same total visits
to each destination. What changes is their order, the run lengths and the set of destinations encountered close together.
Prefetching and caching can also change the actual number of DRAM transactions. Measure complete sweeps and short request
sequences; comparing only aggregate channel counts would miss the purpose of this experiment.

Geometry can also change which lines' data follow one another on a bus, affecting signal history between bursts if the
controller preserves that order. It does not change the switching pattern's within-burst bit layout. Pairing complementary
lines in software is not enough to prove they will appear consecutively on the same physical data pins.

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

During the group-size experiment, add explicit entries labelled “256 B per visit, 32 KiB between starts” (512/4) and
“512 B per visit, 32 KiB between starts” (512/8). These extend the adjacent region sampled before each jump. The numeric access
description belongs in the displayed name/plan, not only in a disclaimer elsewhere. They are candidates, not extra default
steps yet; the implications and the 1024 B candidate (512/16) are assessed below.

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

This proposal does not add an `Interleave Type=0` importer implementation. Existing supported imports keep their current walk.
Grouped visits are a native testing option. Mirror kernels and the dedicated Stride/CacheBust tests retain their current contracts.

Source anchors, rechecked against local main `c4b2ae0`:

- `src/config.rs`: `TestConfig`, `TestParameterContext`, `interpret_tm5_parameter_with_channels`, `check_sequence`.
- `src/tests.rs`: `strided_walk!`, `native_differs_strided!`, `sequence_strided!`, `mirror_swap_jump!`.
- `src/sequence.rs`: `Order`, `Op`, `Sequence`.
- `../TM5/bin/mtests0.asm`: SimpleTest's `BlkSize` / `JumpStep` calculation.

### Why groups larger than two are worth assessing

A software group is a run of 64-byte lines, independent of the number of populated channels and the pattern's DQ width.
Groups of 1, 2, 4, 8 and 16 mean 64, 128, 256, 512 and 1024 bytes per visit. All divide the 64 lines in a 4 KiB granule, so
they require no partial-group handling at the chunk end under the proposed divisibility contract.

At fixed P and N, larger groups do not increase the bytes checked in a sweep: there are N/G visits instead. The number of
period-sized advances within offset passes is `(N - min(P,N)) / G`. For a sparse period such as 512, group 8 reduces those long
jumps by seven eighths compared with group 1 and adds short contiguous runs. Retaining sparse groups matters: this changes the balance of
local transfers and long jumps, not an upgrade that makes smaller groups obsolete.

This table gives distinct destinations **within one aligned visit**, for deliberately simple two-channel models. It does not
count how many channels the complete sweep tests, which is two in every case.

| Illustrative channel selection | G=1 | G=2 | G=4 | G=8 | G=16 |
|---|---:|---:|---:|---:|---:|
| Alternates each line | 1 | 2 | 2 | 2 | 2 |
| Alternates each two-line region | 1 | 1 | 2 | 2 | 2 |
| Alternates each four-line region | 1 | 1 | 1 | 2 | 2 |
| Alternates each eight-line region | 1 | 1 | 1 | 1 | 2 |

Intel's documented two-, four- and eight-line interleave granularities make these useful explanatory cases. They do not imply
that every controller implements a pure low-bit selector. With four channels alternating by line, G=4 instead visits four
destinations and G=2 visits two. Higher-bit XOR contributions can change the destination sequence between visits.

The bounded offline check in [research/access_geometry/models.py](../research/access_geometry/models.py) validates 364 complete
walk cases for G=1/2/4/8/16, including periods that do not divide the chunk and periods larger than it. It also evaluates six
illustrative selectors over complete 4096-line sweeps at five 4 KiB-aligned physical bases. All destination totals remain
identical between geometries. The table above is reproduced by running that script. The mapping examples assume contiguous
physical lines; they model neither stitching nor cache, prefetch or controller timing and do not predict error detection.

**Recommendation:** retain G=1 as the compatibility path; assess G=2/4/8 together before deciding the first shipped grouped
set. Keep G=16 in the offline and implementation-size experiment because it differs under the eight-line example. Beyond
that, a larger group increasingly resembles a contiguous burst of software requests; it may still help, but more values need
a specific hypothesis or evidence to justify code and configuration growth. No proposed finite set covers every mapping.

### What about groups of three or five?

They are possible and can create a different workload. For example, three consecutive lines under an alternating two-channel
map give a 2:1 destination split within the visit, and odd-spaced group starts can reverse which destination receives the extra
request. Their boundary cadence differs from power-of-two groups. They do not break SIMD alignment: each line remains aligned.

The complications are bounded, not mathematical obstacles:

- A 4 KiB chunk has 64 lines, leaving one line after complete groups of 3, or four lines after groups of 5.
  Larger chunk lengths have other remainders.
  A partial-group path can sit outside the main full-group loop; a conditional on every vector is unnecessary.
- Under the proposed P-divisible-by-G rule, P=512 cannot be paired with G=3 or G=5. Supporting arbitrary P/G needs a revised
  partition that handles period boundaries as well as the chunk tail. Otherwise groups can duplicate or overrun lines.
- Additional remainder cases, prefetch wrap cases and potentially specialized code must be validated. Compare group sizes at
  a common divisible period, such as 480, when isolating G; comparing 512/4 against 513/3 also changes P.

Defer odd groups from the first implementation. Their load is not identical to even groups, but no available observation
establishes a missing error class that requires them. Direct periods already provide inexpensive non-power-of-two variation
(513/1, 514/2, 516/4), though those are not equivalent substitutes for odd G. Revisit odd groups if a specific fault responds
to the different run length or an efficient general walker makes their extra cost small.

DDR6/LPDDR6's proposed 12/24-bit transfer profiles do not supply a reason for G=3. Those describe bits on DQ pins; group size
describes separate CPU cache-line addresses. Their relationships are handled by different layers of the test.

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
3. `group_lines` defaults to 1. Candidate values are 1/2/4/8, with 16 subject to the same implementation review; the first
   shipped set remains to be measured and agreed. Reject other values initially. An explicit `group_lines` requires `period_lines`.
4. Require `period_lines >= group_lines` and divisibility by `group_lines`. Thus 513/1 and 514/2 are valid, but 513/2 is not.
   This restriction keeps complete non-overlapping group coverage simple. It is not a power-of-two restriction on periods.
5. Reject a test entry containing both `parameter` and `period_lines`, rather than silently choosing one.
6. Without the new fields, preserve current behaviour, including `parameter=0` and the legacy sequential case.
   With the new fields, global `channels` and a `channels=` override do not multiply the explicit period.
7. Mem-Sequence must contain a strided pass if it specifies this geometry. Its other passes retain their own orders.
   Update sequence validation to inspect the resolved geometry, not require the old raw `parameter` field.
8. Groups greater than 1 initially require native patterns. Keep the legacy numbered kernels at group 1 and reject a request they cannot
   execute. Resolve native-family numeric aliases before making this check; Mem-Sequence already requires native patterns.
9. Reject unused new fields on other tests. Check integer conversion and byte/pointer arithmetic before running.

No new global period/group override is necessary in the first implementation. Per-test entries are sufficient for mixed suites.

Resolve both old and new inputs into one access description before entering the kernel. Keep its source available for the plan
and saved results: explicit geometry, legacy Channels/Parameter, or the existing default. Retain raw import inputs as provenance.
This does not require changing TM5 configs or inferring hardware topology.

## 6. Exact walk and coverage contract

Let N be the number of 64-byte lines in a chunk, P the period and G the group size. Current chunks are whole 4 KiB granules,
so N is divisible by every proposed power-of-two group size through 16. All positions below are relative to the chunk's start.

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
- A group is G ordinary cache lines, not an atomic G*64 B transaction or a demand for simultaneous service.

Coverage proof: every line i has a unique representation
`i = q*P + r + j`, where `r = G*floor((i mod P)/G)` and `0 <= j < G`.
Because G divides P and N, r names one offset pass and its group contains i without crossing the chunk end. This also handles
P greater than N. Thus changing groups does not introduce omissions or duplicate visits within a sweep.

The original proposal's offline check exercised 1,035 valid G=1/2 cases. The retained script linked in section 4 adds 364 cases
across G=1/2/4/8/16, including short chunks, non-dividing periods and periods larger than chunks. Every sweep covers each line
exactly once; group 1 matches an independent expression of the existing descending-offset order. These validate the specification,
not an implementation or hardware error sensitivity.

The whole extent still uses TMR's existing overlapping chunks. A line may therefore be visited again through a different chunk,
as it is today. Its expected value must remain derived from its stable position, epoch, family and polarity; do not seed by group
number, traversal counter or chunk origin. Geometry changes order, not identity.

Preserve the existing OR-accumulated checks, rescans, halt behaviour, seal boundaries and P/~P scheduling. Check every line of
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

### A shared source definition does not require one machine-code loop

Opus is right to ask about code growth, but the macro rule does not logically require a separate body for every possible G.
It requires keeping width-specific operations inside their `#[target_feature]` boundary. Auto selects 128/256/512; it is not a
fourth SIMD kernel width. There are three approaches worth distinguishing:

| Approach | Advantage | Cost/risk |
|---|---|---|
| One macro definition, specialized constant line offsets for G=2/4/8 | Dispatch once outside the hot loop; direct aligned accesses and a period jump after each group | More emitted code for each family/beat/operation/ISA combination; large unrolls can increase register pressure |
| One runtime-sized contiguous inner loop inside each strided visit | More group sizes with less emitted code | Adds a group-end test and live loop state; short G can make this overhead material and complicate prefetching |
| Fixed small microtiles repeated to make larger groups | Reuses a compact access body | Still needs control for the group boundary; compiler duplication, extra arithmetic and register pressure may erase the benefit |

The macro approach avoids maintaining a separate handwritten algorithm for each group, even where the resulting machine code
is specialized. Count emitted code across family, beat, operation, group and ISA; the cost can exceed “three extra functions.”

The runtime-loop options are candidates for a bounded experiment, not a proposal to put per-line dispatch into production.
Likewise, replacing a branch with masking arithmetic is not automatically cheaper: it can add a dependent pointer-update chain.
Do not introduce trait/generic dispatch across target-feature boundaries or a callback per visited line.

The natural first sketch for a 256-bit constant-offset verify is:

```text
match G outside the walk:
    2 -> grouped_walk!([0, 1],       line_body_256)
    4 -> grouped_walk!([0, 1, 2, 3], line_body_256)
    8 -> grouped_walk!([0, ..., 7], line_body_256)

for each group start p:                 // implemented by the walk macro
    expand each constant line offset j:
        expected = pattern(line_id + j)
        acc[(2*j)   % 4] |= load256(p + 64*j)      XOR expected[0]
        acc[(2*j+1) % 4] |= load256(p + 64*j + 32) XOR expected[1]
    advance p by period_bytes
```

This is macro expansion pseudocode, not a callable function abstraction or a new kernel. Generate and verify one line before
the next; do not keep all G generated lines live. There are still four accumulators, not four per line. At 512 bits, using all
four independent chains may require unrolling multiple visits when G<4; verification unrolling and user-visible group size are
separate choices. Check the actual dependency chains in assembly, not merely the size of an accumulator array.

There is already evidence against assuming a universal shared loop is free: `sequence_strided!` records a previous shared-walk
attempt at Parameter 16384 running at 0.645x on Zen 5. Short offset passes made extra pass setup expensive. Keep SimpleTest and
Mem-Sequence's existing optimized group-1 walks, and share the specification and macro access body where it actually helps.

Before expanding across all patterns and widths, compare constant groups with one bounded runtime/microtile candidate at one
width. Record instruction count, emitted code size, active accumulators, spills and the exact visit/prefetch trace. Speed still
needs the existing hardware A/B tools: the seed-mask investigation already showed that fewer instructions can run slower.

### Implementation order

1. Add configuration resolution and the direct period field, retaining the existing group-1 kernel path.
2. Use the one-width experiment to select the grouped implementation and supported set. Extend native SimpleTest and Mem-Sequence
   with the existing per-width macro approach. Keep dispatch outside the hot loop and do not refactor unrelated kernels.
3. Preserve group 1's current prefetch behaviour. For larger groups, aim to preserve lookahead measured in visited lines and prefetches
   per visited line; treating a group as a former single line would multiply the lead by G. Validate the resulting prefetch
   sequence and measure it rather than assuming an identical instruction count or speed.
4. Add the example configurations and plan/result descriptions once the access contract is implemented.

Before hardware comparisons:

- Record actual kernel visits and compare them with the independent walk specification. Exercise periods smaller and larger
  than chunks, periods not dividing chunk lengths, overlapping chunks, and every supported group size.
- Prove that all SIMD widths write the same image for a fixed phase. Check guard regions and inject errors into every group position,
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
2. Which of G=2/4/8/16 justify their emitted code and measured cost, with P divisible by G and odd groups deferred?
3. Can the direct period reuse the current group-1 path without changing its measured loop shape?
4. Does specialized expansion beat the bounded runtime/microtile alternative while preserving complete checking and four accumulators?
5. Does the proposed prefetch policy keep the comparison meaningful without costing more than grouping can gain?
6. Does the existing plan/results schema need only resolved period/group fields, or another representation to preserve replay?

The proposed decision is to expose existing access distance clearly, assess a small set of grouped alternatives, and run complementary shapes
over the same allocation through an understandable combined preset. Their value is in giving faults different opportunities to
appear while preserving complete verification.
