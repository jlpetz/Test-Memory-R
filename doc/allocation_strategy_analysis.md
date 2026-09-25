# Is TMR's allocation strategy ideal? — design analysis

Companion to [`memory_allocation_models.md`](memory_allocation_models.md), which describes what
TM5 and TMR actually do. This doc argues about what TMR *should* do.

> **Partly superseded by [`allocation_questions_answered.md`](allocation_questions_answered.md).**
> That doc follows up with eighteen questions, four of which turned out to be bugs. It **withdraws**
> two things below: the "rotate the pattern within the line" recommendation (§5, §7 item 1 — the
> fault model doesn't justify the pattern-identity cost) and the run-to-run reproducibility
> argument (§1a — deliberately not chased). It also corrects §5's claim that `clflushopt` leaves
> the flush path unaffected: `flush_range_to_dram` under-flushes on a misaligned base. Read that
> doc's **Section B** for the current recommendation order, and **Q18** for the strongest
> justification of the tile layer argued in §2 — TMR currently allocates 1 GiB huge pages and then
> discards them at high thread counts.

Questions on the table:

1. Would **consistent-size blocks** be better than the current variable greedy decomposition?
2. Does uniform sizing give more **flexibility in crafting tests and test kernels** — specifically
   for future **AMX**?
3. Should some tests **cross alignment boundaries** on purpose, by offsetting/salting the start
   address?

Short version: **yes to uniform sizing, but not by changing the allocator.** The allocator is
solving the right problem; the mistake is that its output geometry is also the *test's* geometry.
Separate those two and you get uniform tiles, keep 1 GiB pages, and unlock three stalled features
at once. AMX is a poor fit for a memory tester and uniform blocks are not the reason to want it.
Salting is worth having, but the reason usually given for it has a cheaper answer.

---

## 1. The actual diagnosis: two jobs in one struct

TMR's allocation has to satisfy two goals that pull in opposite directions:

```
  ALLOCATOR'S JOB                          TEST KERNEL'S JOB
  "capture as many bytes as possible,      "give me a predictable, uniform
   at the largest page size available,      region geometry I can write a
   under whatever fragmentation the         tight loop against, identically
   OS happens to have today"                every run, on every machine"

  wants: opportunistic, size-flexible,     wants: fixed, power-of-two,
         page-size-greedy                         reproducible, interchangeable
```

Those are genuinely different objectives, and a single number — "block size" — is currently
serving both. That's the whole problem. It's not that variable sizes are wrong for the
allocator; they're right there. It's that the test loop sees them.

Three concrete costs of the conflation, all visible in current code:

**(a) Coverage is not reproducible run-to-run.** `create_allocation_plan`
(`src/memory/allocator.rs:480`) decomposes `per_thread_target` greedily, and the target depends
on `available` memory at launch. Close a browser between runs and the plan changes shape:

```
  Run A:  target 3.5 GiB  ->  3 x 1 GiB + 1 x 512 MiB    (4 blocks)
  Run B:  target 3.9 GiB  ->  3 x 1 GiB + 1 x 512 MiB
                              + 1 x 256 MiB + 1 x 128 MiB (6 blocks)
```

`prepare_blocks_for_window` (`src/tests.rs:1433`) then spreads the window budget across
whatever blocks exist, largest first. Different block count → different per-block spans →
**different addresses tested**. For "did BIOS change X make this stable?", where the whole
method is A/B comparison, non-reproducible coverage is a direct attack on the tool's purpose.

**(b) The partial-block path silently discards coverage.** In `prepare_blocks_for_window`:

```rust
let test_size = if block_size <= remaining { block_size } else { prev_power_of_two(remaining) };
```

`prev_power_of_two` exists to keep chunk division exact — a reasonable goal — but it means a
700 MiB remainder becomes 512 MiB, and **188 MiB of intended coverage evaporates**. Worst case
approaches 50% of the remainder. That rounding is only necessary *because* the geometry is ragged.

**(c) Every test that wants fixed geometry pays for it, forever.** Any kernel with a fixed tile
shape — AMX's 16 rows, a cross-thread page handoff, a multi-block strided walk — has to carry
ragged-size handling. That cost is paid once per test, in perpetuity, and it's the direct answer
to "would that allow more flexibility in crafting tests": yes, because raggedness is a tax on
every future kernel, not a one-time setup cost.

---

## 2. Recommendation: uniform test tiles carved out of variable allocations

The key realisation is that **block size and page size are independent**, as long as the block
is a multiple of the page size and aligned to it. So:

- keep allocating in whatever sizes get the best page size (1 GiB-aligned huge chunks first —
  the current strategy, unchanged);
- then carve each allocation into **uniform power-of-two tiles**, and hand *tiles* to the tests.

```
  ALLOCATION LAYER (unchanged — greedy, page-size-first, fragmentation-tolerant)

  ┌────────── 1 GiB, huge page ──────────┐ ┌──── 512 MiB, 2 MiB pages ────┐
  │                                      │ │                              │
  └──────────────────────────────────────┘ └──────────────────────────────┘

  TILE LAYER (new — a view, not an allocation.  Q = 128 MiB in this example)

  ┌──┬──┬──┬──┬──┬──┬──┬──┐               ┌──┬──┬──┬──┐
  │T0│T1│T2│T3│T4│T5│T6│T7│               │T8│T9│TA│TB│      12 identical tiles
  └──┴──┴──┴──┴──┴──┴──┴──┘               └──┴──┴──┴──┘
    ^ every tile: same size, Q-aligned, carries its backing page type
    ^ tiles 0-7 report page_type = Huge(1 GiB), tiles 8-B report Large(2 MiB)
    ^ NO tail, because Q is a power of two and divides every allocation size
```

The tile layer costs one extra descriptor indirection **at setup time only** — it is pointer
arithmetic over an existing mapping, zero syscalls, zero hot-path effect. `TestBlock` already
wraps the buffer (`src/test_harness.rs:192` takes `tb.block.buffer.as_mut_ptr()`), so this is a
change to what `TestBlock` points at, not to any inner loop.

**Why there is no tail.** Every size on the ladder
(`[4096, 2048, 1024, 512, 256, 128, 64, 32, 16]` MB) is a power of two, so **any power-of-two Q
no larger than the smallest block a thread owns divides all of that thread's blocks exactly.**
That is the constraint Q must satisfy — and it means Q has to be chosen *after* allocation
completes, from the actual block set, because phases 1b/2b/3 can add sizes the plan never asked
for. Uniform tiles therefore cost **zero captured bytes**; the "you'd waste up to Q/2 per thread"
objection applies to uniform *allocation*, which is not what's being proposed.

Suggested sizing rule, so tile count stays sane across a 16 GiB laptop and a 1 TiB server:

```
  Q = min( smallest_block_this_thread_owns,
           clamp( prev_pow2(per_thread_total / 16), 16 MiB, 256 MiB ) )

  blocks 3x1 GiB + 1x512 MiB   ->  min(512 MiB, 128 MiB)  =  Q = 128 MiB  -> 28 tiles
  blocks 1x512 MiB             ->  min(512 MiB,  32 MiB)  =  Q =  32 MiB  -> 16 tiles
  blocks 4x16 MiB              ->  min( 16 MiB,  16 MiB)  =  Q =  16 MiB  ->  4 tiles
```

The `min` is load-bearing: without it a 128 MiB Q cannot tile a 64 MiB block, and the tail is
back. ≥16 tiles per thread keeps window granularity fine and gives a cross-thread exchange queue
enough tokens to be interesting; the 256 MiB cap means even a lone 1 GiB block yields 4 tiles.

One escape hatch is needed. Phase 3's 4 KiB-page mop-up can produce a single small block that
would otherwise drag Q down for the whole thread. Rather than let one 16 MiB block dictate
geometry for 12 GiB of huge-page memory, exclude blocks smaller than Q: tile them at their own
size and mark them **sub-tile**, eligible for ordinary linear tests but skipped by any kernel
that assumes uniform geometry. So Q is computed from the smallest block *at or above* the rule's
value, with the remainder flagged.

Q should be overridable and **logged in the result file** — it is now part of run identity.

### What uniform tiles unlock

| Capability | Why it needs uniformity | Status today |
|---|---|---|
| Reproducible coverage across runs | Tile *identity* is stable even when tile *count* varies; test region N is the same span every run | broken (§1a) |
| Exact window budgeting | window = N tiles, no `prev_power_of_two` truncation | lossy (§1b) |
| Rotating window (TM5's sliding aperture) | "advance by K tiles per cycle" is trivial on a uniform grid | missing (§6) |
| Cross-thread page exchange (TODO #19) | Interchangeable tokens are the entire mechanism — this is exactly stressapptest's design | TODO |
| Fixed-geometry kernels (AMX, 2D walks) | Kernel can assume a tile shape and skip all ragged-tail code | blocked |
| Per-tile error attribution | "errors clustered in tiles 9 and 10" is a usable signal; "in the 512 MiB block" is not | weak |

That last row is worth dwelling on. Uniform tiles give you a **stable coordinate system** for
memory, which is the closest thing TMR can get to physical-address diagnostics without breaking
the settled no-PFN rule. It doesn't tell you which rank failed, but "the same tile index fails
across reboots" is a genuinely new and useful signal, and it's free.

### Honesty check: what uniform tiles do *not* buy

- **Not throughput.** Per-block setup cost amortises over hundreds of MiB of traffic. Anyone
  arguing uniform blocks are *faster* is wrong; expect zero measurable delta and don't sell it
  that way.
- **Not more captured memory.** Identical bytes to today, by construction.
- **Not physical uniformity.** Tiles are uniform in VA. Two tiles inside the same 1 GiB page are
  physically contiguous with each other; two tiles in different allocations are at unrelated PA.
  Tiles are a *test* coordinate system, not a DRAM one.
- **Not a fix for the biggest current gap.** The pinned window (§6) is a coverage hole today,
  independent of tiling. Tiling makes it easy to fix; it doesn't fix it.

---

## 3. Page size is a test capability, not an allocation optimisation

Reframing this changes how the strategy should be chosen. The current default
(`plan-pagesize-pref`) maximises page size first, which implicitly treats "bigger page = better"
as self-evident. It isn't. Here is what 1 GiB actually buys over 2 MiB:

```
  1. TLB pressure
     1 GiB page : 1 STLB entry covers 1 GiB
     2 MiB page : 512 entries cover 1 GiB
     Modern server STLBs hold on the order of a couple of thousand 2 MiB entries,
     i.e. multi-GiB of reach. So:
       - LINEAR streaming:  ~1 TLB miss per 2 MiB vs 32,768 line fills. Irrelevant.
       - RANDOM access over a footprint beyond STLB reach: 1 GiB pages win, and it
         is measurable. This is Mem-Random and the DRAM-tier latency tests.
     (Numbers vary by uarch - measure on your target, don't trust this table.)

  2. Physical contiguity horizon   <-- the one that matters for test design
     Within ONE large/huge page, VA adjacency IMPLIES PA adjacency.
       2 MiB page: a stride is physically meaningful for  2 MiB
       1 GiB page: a stride is physically meaningful for  1 GiB
     Beyond the page, the next VA is at an unrelated PA -> unrelated row/bank/channel.
```

Point 2 is the real content. It means **page size sets the maximum stride over which a test
knows anything about physical layout** — and TMR has large-stride tests (`Mem-CacheBust`,
`Mem-Stride`) whose whole premise is defeating row/bank locality:

```
  Mem-CacheBust with a 4 MiB stride on 2 MiB pages:
      addr, addr+4M, addr+8M, ...  -> every step lands in a DIFFERENT page
                                   -> physically RANDOM row/bank each step
                                   -> still a fine stress test, but the "stride"
                                      is decorative; you're doing random access

  Same test on 1 GiB pages:
      256 steps stay inside one page -> a genuine, repeatable physical stride
```

So the honest statement is: **large strides are only reproducible inside a huge page.** That is
a real argument for `plan-pagesize-pref` as the default — but it's an argument from *test
semantics*, not from performance, and it should be documented as such so it isn't "optimised
away" later by someone who measures linear bandwidth, sees no difference, and concludes huge
pages don't matter.

Practical consequence: `min_page_size`/`max_page_size` are currently framed as tuning knobs.
They're closer to **test preconditions**. A test whose stride exceeds its tile's backing page
size should say so — either by requiring `Huge` tiles, or by declaring itself
layout-indifferent. Tiles carrying their backing page type (§2) makes that expressible.

---

## 4. AMX

### The mental model is right

> "if I understand AMX it moves linear addressing into a 2d tile representation of memory"

Correct, and precisely so. `TILELOADD tmm, [base + index*scale]` takes a **stride in a GPR**,
and loads up to 16 rows of up to 64 bytes each:

```
  TILELOADD tmm1, [rax + rbx]      ; rax = base, rbx = stride

  memory (linear)                          TMM1 (2D, max 16 x 64 B = 1 KiB)
  base + 0*stride  ┌────64 B────┐          ┌────────────────┐ row 0
  base + 1*stride  ┌────64 B────┐   ───>   ├────────────────┤ row 1
  base + 2*stride  ┌────64 B────┐          ├────────────────┤ row 2
        ...                                        ...
  base + 15*stride ┌────64 B────┐          └────────────────┘ row 15

  A tile is a STRIDED RECTANGLE VIEW over linear memory.
  Contiguous span required: 15*stride + 64 bytes.
```

So the allocation requirement AMX imposes is exactly: **a contiguous, predictable span of at
least `16 × stride`**. With stride = 4 KiB that's a 64 KiB span (trivial). With stride = 2 MiB
it's 32 MiB, which crosses 2 MiB page boundaries — so a large-stride AMX walk on 2 MiB pages is
physically meaningless (§3 again). This is a real argument for uniform, huge-page-backed tiles,
and it's the user's instinct being correct: variable geometry *is* overhead for a fixed-shape
kernel.

### But AMX is a weak fit for memory testing

Four reasons, in descending order of how decisive they are:

**(1) It cannot verify. AMX has no bitwise or compare operations.** The tile op set is
load / store / zero / dot-product (`TDPBSSD`, `TDPBUSD`, `TDPBF16PS`, and FP16 on Granite
Rapids). There is no `TXOR`, no `TCMP`. TMR's entire verification pattern — XOR the difference,
OR-accumulate, test once per interval — has no tile-domain equivalent. You would have to either
`TILESTORED` back to memory and verify with AVX-512 (adding a full extra memory round trip to
every check, i.e. *reducing* the fraction of traffic that is actual test traffic), or use a
dot-product as a checksum, which is lossy: a dot product against a constant vector is a weighted
sum, so it can miss compensating bit errors and cannot localise a failure to a byte. For a tool
whose output is "which bit flipped, where", that's a downgrade.

**(2) A memory tester wants *low* arithmetic intensity — AMX is a high-arithmetic-intensity
unit.** AMX exists so that one memory load feeds hundreds of MACs. A memory test wants the
opposite ratio: as much DRAM traffic per unit of compute as possible. AMX's reason for existing
is the thing TMR is trying to avoid.

**(3) No bandwidth advantage.** AMX loads and stores contend for the same L1D ports and the same
fill buffers as AVX-512. DRAM-resident streaming is bounded by the memory controller and the
number of outstanding line fills, not by the width of the consuming instruction — which is
already why AVX-512 doesn't beat AVX2 on DRAM-tier tests in TMR's own measurements. AMX cannot
beat a bound that isn't about the core.

**(4) Availability and cost.** Intel server only — Sapphire Rapids, Emerald Rapids, Granite
Rapids, Xeon 6. Not on any consumer desktop, and AMD has nothing equivalent. TMR's audience is
DDR5 overclockers, who are overwhelmingly on consumer platforms. Plus 8 KiB of tile state
(XSAVE), which needs on-demand enabling via XFD before first use, and makes every context
switch on that thread more expensive.

### What AMX would actually be good for

There is one genuinely interesting thing, and it's worth writing down so the idea isn't lost:

> A single `TILELOADD` generates **16 concurrent strided address streams** from one instruction,
> with the addresses produced by hardware rather than by the loop.

Pick stride = a DRAM row size multiple and you have 16 rows being touched near-simultaneously
from one instruction — a bank/row-interleaving stressor with a very different issue pattern from
16 software pointers (different prefetcher interaction, different TLB pressure, different
scheduling). As a *distinct stressor*, that's a legitimate test idea. As a *faster way to do
what TMR already does*, no.

### Verdict

**Do not build for AMX now, and do not let AMX be the justification for uniform tiles.** The
uniform-tile case stands on reproducibility and cross-thread exchange, which pay off on every
machine today. Uniform tiles happen to also make AMX cheap to add later, and the requirement it
would impose (`16 × stride` contiguous, huge-page-backed) is already satisfied by them — so this
is a free option, not a cost to pay up front.

If it ever gets built: one `Mem-AMXStride` test, int8 tiles, `TILELOADD`/`TILESTORED` as a
strided **mover**, verification in AVX-512 afterwards, gated on CPUID `AMX-TILE` + successful
XFD enable, and honestly labelled as an access-pattern generator rather than a faster test.

---

## 5. Alignment salting

> "we currently try to align block boundaries in allocations for SIMD, but ... some stress tests
> might be better crossing alignment boundaries ... offset/salt the starting address"

Two corrections to the premise first, then the part that's right.

**Correction 1: block alignment is not for SIMD.** CLAUDE.md already settles this — 2 MiB/1 GiB
alignment exists because on x86-64 that is *the mechanism* by which you get a large page (a
1 GiB page is a PDPTE with PS=1 and cannot exist at an unaligned VA). SIMD needs only 64 bytes;
2 MiB over-satisfies it by 32,768×.

The hard consequence: **you cannot salt the allocation base.** Move it by 1 byte and you lose
the large page. Salting must happen *inside* a block — as an offset applied to the window or
chunk start pointer. That's fine, and it's strictly more flexible (per-test, no allocator
involvement), but it's a different change than "salt the allocation".

**Correction 2: "crossing alignment" is two different things with different price tags.**

```
  base + 64k  (LINE-PRESERVING salt, multiple of 64)
    - never splits a cache line   -> zero throughput cost on any AVX2-era CPU
    - satisfies MOVNTDQ/VMOVNTDQ  -> NT-store safe at every width
    - changes WHICH line each test index lands on

  base + 8, +24, +40  (LINE-CROSSING salt, not a multiple of 64)
    - every vector access spans 2 cache lines -> 2 line fills, 2 tag lookups
    - at a 4 KiB boundary: split-page -> 2 TLB lookups, possibly 2 page walks
    - VMOVNTDQ (zmm) requires 64 B alignment; MOVNTDQ requires 16 B
      -> misaligned NT stores raise #GP. Mem-SimpleNT and the NT bandwidth
         paths CANNOT take a line-crossing salt.
```

On modern x86 the cost of unaligned access is **per split line, not per misaligned
instruction** — so a 64-byte-multiple salt is genuinely free, and only the sub-line salt costs
anything.

### The usual justification has a cheaper alternative

The strongest-sounding argument for salting is **DQ/byte-lane rotation**: a DDR5 subchannel is
32 bits wide with BL16, so a 64 B access is 16 beats, and a byte's position within the line
determines which beat and which DQ nibble carries it. Rotate that mapping and you cover per-pin
marginality that a fixed mapping never touches. Real goal, worth wanting.

But address salting is the expensive way to get it, and for TMR's patterns it often gets you
nothing at all:

```
  Pattern is POSITIONAL/uniform (e.g. 0xA55AA55AA55AA55A splatted — most TMR tests):
      a 64 B-multiple salt changes NOTHING presented to any pin. Every line still
      receives the identical 64 bytes; byte j is still byte j, still on the same DQ.
      Only WHICH lines get touched changes — which matters only if the window
      doesn't cover everything anyway.

  Pattern is ADDRESS-DERIVED (TM5's ST_GeneratePattern, seed from addr>>12):
      a salt does change which value lands where. But so does changing the seed,
      at zero cost.

  What actually rotates the value->pin mapping: a SUB-LINE shift (1..63 bytes),
  which is precisely the expensive, NT-incompatible one.
```

And there's a strictly better way to get the same coverage:

> **Rotate the pattern within the line, not the address.** Byte-rotate the 64-byte fill block by
> `k` and keep the buffer 64 B aligned. Value V now appears at byte offset `j+k` — a different
> beat and a different DQ nibble — with zero misalignment cost, full NT-store compatibility, and
> a single `k` in the result file making it exactly reproducible.

For a splatted `u64` lane pattern this is `k ∈ 0..8` (rotate the lane), which is a
`u64::rotate_left(8*k)` on the constant. That's a ~5-line change to the pattern generator that
delivers the actual goal better than address salting does. It should be done *first*, and it may
close the requirement entirely.

### What only address salting can do

Two things survive, and they justify keeping it as an opt-in knob:

1. **Split-line and split-page access as a deliberate stressor.** Two line fills per access, two
   TLB lookups, and at a page boundary potentially two page walks — with two DRAM accesses in
   flight per instruction, in a fixed phase relationship. That is a genuinely different traffic
   shape that cannot be synthesised by pattern rotation. It stresses the core's load/store path
   and the memory pipeline together, and it's a well-known place where marginal
   memory-controller settings show up. Worth one test.
2. **Rotating chunk-boundary phase.** Every chunk boundary today lands on a power-of-two-aligned
   address, so "start of chunk" behaviour (pipeline restart, prefetcher retrain) always coincides
   with the same position within a page. Salting by a non-power-of-two moves that phase. Plausible
   but speculative — no strong prior that boundary phase matters.

### Recommended shape

```
  Per-test, opt-in, defaulted OFF:
      salt = None                    (default, today's behaviour)
      salt = Lines(n)                offset  n*64  bytes   - free, NT-safe
      salt = Bytes(n)                offset  n     bytes   - split-line stress
      salt = SplitEvery(n)           deliberately straddle every n-th boundary

  Hard rules:
    - applied to the WINDOW/CHUNK start pointer inside a tile, never to an allocation base
      (an allocation base must stay page-aligned or the large page is lost)
    - Bytes(n) with n % 64 != 0 is REJECTED for any NT test at config-validation time,
      with a clear error - not silently ignored, and not left to fault at runtime
    - the tile's last (salt) bytes fall outside the salted span; shrink the tested
      length rather than running past the end
    - salt value recorded in the result JSON - it is part of run identity
```

`clflushopt` has no alignment requirement (it operates on the containing line), so the
flush-to-DRAM path in `Mem-Refresh` is unaffected either way.

---

## 6. The gap that is bigger than any of this

While mapping TM5's sliding window onto TMR, one asymmetry stands out (detail in
`memory_allocation_models.md` §2.2):

```
  TM5:  window slides through the locked pool  ->  small working set AND 100% coverage
  TMR:  window is pinned to block offset 0     ->  small working set XOR full coverage
```

`test_harness.rs:192` takes `tb.block.buffer.as_mut_ptr()` — the block base, always, every
cycle. So whenever `window_size < total_allocated`, the tail of every block is **never visited
by that test, in any cycle**, and the covered part is re-tested at the same addresses forever.

For deliberately cache-resident tests that's partly intentional — you *want* a small hot working
set. But TM5 shows the two properties are separable, and it got both: a small window that walks.
TMR currently pays full coverage for cache residency, and no config option buys it back.

**With uniform tiles this is a handful of lines:** the window is a tile count, so advance the
starting tile index by the window's tile count each cycle and wrap. Cache-tier tests keep their
small hot footprint *per cycle* while sweeping the whole allocation *over* cycles.

```
  cycle 0:  [T0 T1]  T2  T3  T4  T5  T6  T7
  cycle 1:   T0  T1 [T2 T3] T4  T5  T6  T7
  cycle 2:   T0  T1  T2  T3 [T4 T5] T6  T7
                                              window stays 2 tiles; coverage -> 100%
```

I'd rank this above tiling in value, and tiling is the thing that makes it cheap — which is a
good reason to do them together.

---

## 7. Recommendations, in order

1. **Rotate the pattern within the cache line** (`k` byte/lane rotation, recorded in results).
   Smallest change, delivers the real DQ/byte-lane goal, no alignment or NT cost. Do this before
   any salting work — it may remove the need.
2. **Introduce a uniform tile layer** over the existing allocations (§2). Do not touch the
   allocator. Q as a power of two ≤ 16 MiB divisor; tiles carry their backing page type; Q logged
   in results. This is the load-bearing change.
3. **Make the window rotate over tiles** (§6). Closes a real coverage hole and restores the one
   capability lost with AWE.
4. **Drop `prev_power_of_two` from window spreading** — with uniform tiles the window is an exact
   tile count and the truncation has nothing left to do.
5. **Reframe `min/max_page_size` as test preconditions, not tuning knobs** (§3). Document that
   `plan-pagesize-pref` is the default for *stride reproducibility*, not for bandwidth, so nobody
   benchmarks linear throughput and "optimises" it away.
6. **Add opt-in salting, default off**, with the NT rejection enforced at config-validation time
   (§5). One test (`Mem-SplitLine` or a `salt=` variant of an existing test), not a global mode.
7. **Cross-thread tile exchange (TODO #19)** becomes straightforward once tiles are
   interchangeable — this is stressapptest's core mechanism and TMR would get it nearly free.
8. **AMX: record the design, build nothing** (§4). Revisit only if TMR gains a server audience,
   and then as a distinct 16-stream access-pattern generator, never as a faster verify.

### What to measure before committing

Each of these is cheap and each could invalidate part of the above. Do them rather than trusting
the reasoning:

- **Tiling has zero throughput cost.** Same test, same total bytes, tiled vs not. Expect noise-level
  difference. If tiling costs anything measurable, the descriptor indirection leaked into a hot loop
  — fix that, don't accept it.
- **Coverage reproducibility is actually broken today.** Run the same config twice with different
  amounts of free memory and diff the per-block tested spans in the result JSON. This is the
  motivating claim for the whole redesign, so verify it before acting on it.
- **Huge vs large pages on `Mem-Random` / DRAM latency.** The §3 TLB argument predicts 1 GiB pages
  win on random access over a large footprint and are irrelevant for linear streaming. If the
  random-access delta is also noise, `plan-pagesize-pref`'s value rests entirely on the
  stride-reproducibility argument — worth knowing.
- **Split-line cost, and whether it finds anything.** Measure throughput at `salt=Bytes(8)` vs
  `salt=None`, then check error rates on a known-marginal config. If a split-line test finds
  nothing a normal test doesn't, it's an expensive way to run slower.
- **Pattern rotation changes what the pins see.** Read the emitted asm to confirm the rotated
  constant is still non-byte-uniform after rotation — a rotation of a byte-uniform value is still
  byte-uniform, and `LoopIdiomRecognize` will happily turn it back into `memset` (see
  `doc/simd_codegen_rules.md`). `0xA55AA55AA55AA55A` rotated by 8 is `0x5AA55AA55AA55AA5` — still
  safe, but check, don't assume.
