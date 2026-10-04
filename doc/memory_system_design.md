# Memory system design — acquisition, address space, and delivery to tests

Status: **proposal.** Synthesis of `allocation_strategy_analysis.md`,
`allocation_questions_answered.md` (Q1–Q22, five bugs) and `tile_abstraction_spec.md` (sections 14–15).
Supersedes the three-stage model as *described*; most of the code survives, but the layer boundaries
move.

Goal, restated: be as faithful to TM5 as a non-AWE design can be, and support test algorithms that
stride, bounce, dwell and revisit — without any of that leaking into how memory was acquired.

---

## 1. The single mistake the current design makes

```
  TODAY — the shape of the allocation leaks into the shape of the test unit

    Allocation ──► Extent ──────────► Chunk
    blocks of      per-thread BYTE     derived PER BLOCK from that
    unequal size   budget, spread      block's share
                   greedily over
                   blocks

    Consequences, all observed:
      B5  one test runs several different chunk sizes (up to 16x apart)   [fixed for stitched, TODO 76]
      B4  a 1 GiB huge block gets dropped because it doesn't fit a plan gap   (fixed: TODO 75 A)
      -   chunk silently clamped to the smallest block -> config ignored
      -   allocator forced toward power-of-two blocks to keep the division tidy
```

Everything downstream asks "which block am I in?", so the allocator is under pressure to produce
tidy, uniform blocks — and tidiness costs page size, which is the only property that actually
matters. **Break that coupling and both ends get simpler.**

```
  PROPOSED — four layers, each with one job, no upward dependencies

    ┌─ L0 ACQUISITION ────────────────────────────────────────────────┐
    │  Get the most memory at the biggest page size. Shape is free.   │
    │  Output: rows = [(ptr, len, page_type)]                         │
    └────────────────────────────┬────────────────────────────────────┘
                                 │
    ┌─ L1 ADDRESS SPACE ─────────▼────────────────────────────────────┐
    │  One circular byte numbering 0..T over the rows. Q=64 quantum.  │
    │  Output: resolve(start, len) -> 1..N segments                   │
    └────────────────────────────┬────────────────────────────────────┘
                                 │
    ┌─ L2 ITINERARY ─────────────▼────────────────────────────────────┐
    │  What to visit, in what order, how long to stay.                │
    │  Four knobs: extent / unit / order / dwell                      │
    └────────────────────────────┬────────────────────────────────────┘
                                 │
    ┌─ L3 DELIVERY ──────────────▼────────────────────────────────────┐
    │  Satisfy the test's declared SpanRequest, or REJECT at plan      │
    │  time. Hand segments to an unchanged kernel.                    │
    └─────────────────────────────────────────────────────────────────┘
```

---

## 2. L0 — Acquisition

**Policy: unchanged, and now positively justified.** Greedy, page-size-first, descending sizes. The
original worry — "would consistent-sized blocks be better?" — is answered *no*, and the reason is now
structural rather than a judgement call: **nothing above L0 asks how big a block is.** Uniform blocks
would buy a property no consumer reads, and pay for it in page size, which every consumer feels.

Four concrete changes:

1. **Stop rounding to powers of two.** `block_sizes_mb = [4096, 2048, …, 16]` exists to keep the
   downstream division tidy. That division is gone. A 700 MB remainder can be requested as *one*
   700 MB block instead of 512+128+32+16 = 688 MB, so this both reduces row count and stops
   discarding the 12 MB tail. Constraint: size must be a multiple of the page size, so this freedom
   is real for LARGE (2 MiB granularity) and 4 KB, and **not** for HUGE (1 GiB granularity).
2. **Plan globally, then distribute** — fixes **B4**. Today the plan divides per-thread first, so
   Phase 1b's opportunistically-acquired 1 GiB blocks have no gap to land in and get dropped
   (`allocator.rs:1141` rejects a 1 GiB block against an 800 MB gap, `:1180` warns and frees it).
   Acquire against the total, assign to threads afterwards.
   *Status (2026-10-02):* B4 was fixed the other way round (TODO 75 A). plan-pagesize-pref now
   sizes every request to one thread's remaining gap, largest gap first, so nothing is acquired
   that no thread can take and nothing is freed. `allocator=stitched` never had the bug.
3. **Never merge different page types into one row.** Already true; make it a documented invariant,
   because L1 carries `page_type` per row and a stride kernel reads it.
4. **Keep the `Backend` seam.** Nothing here needs ring 0; the settled decision stands.

What L0 must *not* do: know about windows, chunks, tests, or SIMD widths.

---

## 3. L1 — The address space

Full derivation in `tile_abstraction_spec.md` sections 14–15. Summary:

```
  Number every byte a thread owns 0..T, continuously across rows, circularly.

  0                            4096            5120        5632 ─┐
  ├────────── row 0 ───────────┼──── row 1 ─────┼─── row 2 ──────┤ │
  │       4096 MB HUGE         │  1024 MB HUGE  │  512 MB LARGE  │ │
  └────────────────────────────┴────────────────┴────────────────┘ │
   ^                                                               │
   └──────────────────────── wraps ────────────────────────────────┘

  A chunk is a RANGE. resolve() splits it at row boundaries:

    C = 1536 MB
    [   0 .. 1536)  -> 1 seg
    [1536 .. 3072)  -> 1 seg
    [3072 .. 4608)  -> 2 segs   (row0+3072 1024 MB, row1+0 512 MB)
    [4608 .. 6144)  -> 3 segs   wraps: row1+512, row2+0, row0+0

  Every chunk is exactly 1536 MB. No clamping. 100% of memory reachable.
```

The table is 2–5 rows, ~150 bytes. `resolve()` is ~15 lines, called **once per chunk**, never per
element.

**Quantum.** Every length in the space is a multiple of **Q = 64 bytes** — row length, chunk length,
start position. Segment lengths then are too, automatically, because a segment ends only at a row
boundary or the chunk end. This is not a new constraint: `tests.rs:3579` already asserts
`_len % $simd_w == 0` and the kernels have no tail path. Round the *start advance* to 4 KB as well
(free — rows are MB multiples) so segments are page-aligned, which also removes bug **B3**'s
misaligned-base case in `flush_range_to_dram`.

---

## 4. L2 — Itinerary: the four knobs

Today there are two knobs that both mean "size" (window, chunk) and dwell is tangled across three
places (bug **B2**). There are actually **four independent questions**, and naming them separately is
what makes the system versatile:

| Knob | Question | TM5 equivalent | Units |
|---|---|---|---|
| **extent** | how much of the space per cycle | `Testing window size (Mb)` | chunks |
| **unit** | how big is one work item | `Test Block Size (Mb)` | bytes (mult. of Q) |
| **order** | which unit next | (implicit: sequential) | enum |
| **dwell** | how long on one unit | `ST_WriteReadCycles` × `dLoopCounter` | counts |

### 4.1 extent — and what the Window concept is actually for

Redefine window from "a per-thread byte budget spread over blocks" (the thing that created **B5**) to
**a count of chunks per cycle**: `chunks_per_cycle = extent_bytes / C`. It cannot diverge per block
because there are no per-block decisions left.

Its only remaining job is the **dwell-vs-coverage dial**: a small extent revisits less memory more
often. For `CacheLevel` / `CacheRelative` modes that is exactly the point. For full-memory tests
extent = T and the concept is inert — which is fine; an inert knob is better than a lying one.

### 4.2 unit

`C` in bytes, any multiple of Q, up to T. **Not bounded by block size** — that coupling is the whole
point of L1. Reject `C > T` at plan time as a config error rather than silently allowing a chunk to
revisit bytes within one pass.

### 4.3 order — the new capability, and it is ~10 lines

This is where "bounce around" comes from, and it costs nothing because it only changes *which logical
start you pick next*:

```rust
enum Order {
    Sequential,                 // pos += C          (TM5's slide)
    Strided { chunks: usize },  // pos += chunks * C
    Permuted { g: usize },      // idx_k = (k * g) mod n,  gcd(g, n) == 1
    Random   { seed: u64 },     // sampling with replacement
}
```

`Permuted` is the valuable one: with `gcd(g, n) = 1` it visits **every chunk exactly once in n steps,
in scrambled order**, with no state beyond a counter and no bookkeeping array. Random order *and*
provable full coverage, one multiply per chunk.

`Sequential` is just `Permuted { g: 1 }`.

### 4.5 Chunks never wrap — the circular framing was wrong

> **Built (TODO 76, 2026-10-05):** the even spread below, as `test_memory::ChunkSpread`, in every
> chunked test: every chunk is exactly `C`, start `k` is `k*(T-C)/(n-1)` floored to 4 KiB, each
> start on its own (a single floored step leaves a hole before the last chunk). Per-cycle rotation
> is not built.

Earlier drafts made the space circular and let a chunk wrap past `T`, splitting it a third time.
**That is avoidable and should be avoided.** The reasoning:

```
  n = ceil(T / C) chunks of size C cover n*C >= T bytes.
  Overlap = n*C - T bytes, and it is UNAVOIDABLE whenever C does not divide T.

  So the only real choice is WHERE the overlap lands:
     wrap        -> overlap at position 0, and the last chunk SPLITS at the seam
     even spread -> overlap spread across the pass, and NO chunk ever splits

  Even spread strictly dominates. Same coverage, same overlap volume, one fewer seam.
```

The start of chunk `k` in a pass:

```
  n = ceil(T / C)
  start_k = k * (T - C) / (n - 1)      quantised down to Q      (n == 1 -> start = 0)
```

Worked on `T = 5632 MB`, `C = 1536 MB`, `n = 4`, step `= 4096/3 = 1365.33 MB`:

```
  chunk 0  [   0 ..  1536)      chunk 2  [2731 .. 4267)
  chunk 1  [1365 ..  2901)      chunk 3  [4096 .. 5632)

  union = [0 .. 5632)   complete, no gap, no partial chunk
  every chunk EXACTLY 1536 MB, none wraps
  overlap = 4*1536 - 5632 = 512 MB, spread as ~171 MB x 3 instead of 512 MB x 1
```

Two properties worth noting:

- **When `C` divides `T` the formula degenerates to `k*C` with zero overlap.** `T=4096, C=1024, n=4`
  gives step `3072/3 = 1024` → starts 0, 1024, 2048, 3072. Exactly TM5.
- **The space stays circular for choosing a pass *origin*** (so coverage rotates across cycles and no
  region is permanently favoured), but individual chunks are always `[start, start+C)` with
  `start + C <= T`. Circular itinerary, linear chunks.

Consequence for `resolve()`: **the only remaining cause of a split is a real block boundary.** With
stitching (Q26/Q27) that goes to zero and every chunk is one contiguous span of exactly `C` — i.e.
TM5-identical delivery. The two fixes compose; neither alone gets there.

Why this matters for realism: TM5 walks memory in one direction forever. A permuted order changes
which rows/banks are adjacent *in time* without any physical-address knowledge — it is the cheapest
honest way to vary access history, and it is compatible with the no-physical-address decision.

### 4.4 dwell — restore TM5 fidelity, three named knobs

Bug **B2** is that `Time (%)` was wired to the outermost `cycles` with divisor 10000, instead of the
innermost verify count with divisor 2000, and `write_read_cycles` was left at 1 instead of 4 —
roughly a 12× dwell deficit. Fix by naming the three levels for what they are:

```
  for cycle in 0..cycles                      <- plan repeats (TMR's ctx.cycle)
    for chunk in itinerary
      for pass in 0..passes_per_chunk         <- TM5 ST_WriteReadCycles = 4
        write(chunk)
        flush(chunk)
        for _ in 0..reads_per_write           <- TM5 dLoopCounter
          verify(chunk)

  TM5 mapping:  passes_per_chunk = 4
                reads_per_write  = max(1, (testTime% * globalTime%) / 2000)
```

Verified nest: `mtests0.asm:312/411/520-522` — reps are innermost, per chunk. At TM5 defaults that is
24 passes over one chunk before moving on. **Dwell and coverage oppose each other at fixed runtime**;
making them separate knobs is what lets a config choose.

### 4.6 Chunk sizes are multiples of 4 KiB — reopened 2026-10-03 (TODO 76)

**Decision (TODO 76, awaiting the user's approval): `C` is any multiple of 4 KiB**, from the test's
minimum (64 KiB, more for variant counts) up to the extent. TM5's `1536 MB` and `window/3` are kept
as TM5 sizes them: codes 0-3 are floored to the `.cfg`'s Lock Memory Granularity, against the
smaller of the window and the thread's memory (TODO 76).

This replaces "C is always a power of two" (DECIDED until 2026-10-03). Why it changed:

- **No kernel needs a power of two** (`doc/todo/76-span-tests.md`, the audit). The kernels are
  `while i < end` sweeps, and their divisions run once per chunk. The one size mask, `Mem-Random`'s
  `rng & (len - 1)`, is now a multiply-high (`rng * len >> 64`), off the RNG's dependency chain.
- **What the kernels do need is a multiple of the vector width**, more where a test splits a chunk
  into parts (mirror halves and subblocks, BlockMove halves and quarters, Stride subdivisions).
  4 KiB covers all of them for every width, except MirrorMove's 3 subblocks (TODO 85), and keeps
  chunk starts page-aligned.
- **The structural payoffs** argued for powers of two don't need them. Chunks spread evenly over a
  piece (4.5), each exactly `C`. Tile mode (4.7) takes `floor(T/C)*C` for any `C`. Comparability
  comes from configs naming the same sizes, not from a ladder.
- **The power-of-two pieces came from the old allocator's blocks**, not from the tests. With
  `allocator=stitched` a thread's memory is one span, and the extent is one piece of it.

What it costs: an extent that isn't a multiple of the chunk is walked with overlaps, under one
chunk in total per pass (4.5); n = 2 nearly doubles a pass. TM5 drops such a tail instead.

### 4.7 Two coverage modes — overlap or exclusive, never both

Section 4.5 accepts a small overlap to keep every chunk exactly `C`. That is right for single-threaded
correctness tests and **wrong** for anything that hands memory between threads, where two threads
writing the same bytes is a correctness bug in the *test*, not a finding about the DRAM.

```
  COVERAGE MODE  (default — single-threaded correctness tests)
  ───────────────────────────────────────────────────────────
  uses all of T.  n = ceil(T/C).  start_k = k*(T-C)/(n-1).
  overlap = n*C - T, spread evenly. Nothing excluded, nothing wraps.

  TILE MODE  (required for cross-thread handover — TODO #19 / GSAT-style)
  ───────────────────────────────────────────────────────────
  usable = floor(T/C) * C          R = T - usable        (R < C)
  tiles  = [o_c + k*C, o_c + (k+1)*C)   for k in 0..floor(T/C)
  EXCLUSIVE, equal, non-overlapping. R bytes unused per cycle.

  Rotate the grid origin per cycle so the sacrificed bytes are not always the same:
      o_c = (c * Q_rot) mod (R + 1)         0 <= o_c <= R, so no tile can wrap
  Over enough cycles every byte is covered; within one cycle R bytes are idle.
```

This is the earlier "sacrifice coverage for equal tiles" idea, kept for exactly the case that needs it
rather than applied globally. When `C` divides `T`, `R` is 0: `T = 5632 MB` with `C = 512 MB` gives
`R = 0` (11 exact tiles); with `C = 1024 MB`, `R = 512 MB` (9% idle, rotating).

**One extra requirement for cross-thread tiles.** The pattern seed is
`block_seed(ptr, thread_id, cycle)`. If thread A writes a tile and thread B verifies it, B must seed
with **A's** `thread_id`, so the *originating* thread id has to travel in the tile descriptor. Either
that, or `thread_id` leaves the seed entirely. Decide this when #19 is built; note it now so it is not
discovered late.

---

## 5. L3 — Delivery: resolve or reject, never silently adjust

**This is the principle the five bugs all violate.** B1 misreads fraction codes as MB (440× too
small) and proceeds. B4 drops a huge page with a warning. B5 clamps chunk per block. In every case
the system quietly produced something other than what was asked for, and the run still reported
success.

> **Rule: the plan layer either satisfies the test's declared requirements or fails config
> validation with the reason. Nothing below the plan layer adjusts a size.**

Each test declares what it needs of a span:

```rust
struct SpanRequest {
    len_multiple: usize,     // 64 default; 192 for a 3-subblock mirror; rows*stride for AMX
    contiguity:   Contiguity,// AnySegments | SingleSegment
    page_type:    PageReq,   // Any | UniformWithinSpan | Exactly(PageType)
    positions:    usize,     // 1 = position-local; 2 = mirror / block-move
    shape:        Shape,     // Linear | Rows2D { rows, stride }
}
```

The plan resolves this by **rounding the chunk size** (visible, logged, reported) or by **rejecting**
— never by handing over a span that violates it.

### 5.1 Two classes of kernel

```
  POSITION-LOCAL      touch element i, derive everything from i
  ───────────────     StuckBit, Refresh, SimpleV2, SimpleNT, Bench-Init/Verify
                      contiguity: AnySegments.  Cross-block chunks, no changes.

  POSITION-RELATIONAL touch i AND f(i) in the SAME span
  ───────────────     Mirror, BlockMove, Stride, CacheBust
                      contiguity: SingleSegment (chunk clamped to fit one row)
```

6 of 9 correctness tests get unbounded chunks; 3 keep today's constraint as a **declared property
with a stated reason** rather than a global limit imposed on everyone. `mirror_swap_subblocks`
(`tests.rs:3593`) divides the span by N and walks pairs in lockstep — N=3 is not a power of two, so
no choice of Q rescues it; it genuinely needs one span.

One further care point for the relational strided tests: if they are ever allowed to span segments,
**stride phase must be carried across the seam**, not reset per segment, or the access pattern
silently changes at boundaries.

### 5.2 The kernel contract does not change

A `Seg { base, offset, len, page_type }` is exactly what `ChunkCtx` already carries (`base` = `ptr`,
`offset`/`len` = `chunk_start`/`chunk_end`). So:

- **no kernel edits** for Tier 1's 28 `run_phased_test` sites,
- **pattern generation stays correct for free** — seed from `base`, index from `offset`; neither
  depends on how the range was carved (this is why the container-absolute
  `P(block_base, offset_in_block)` coordinate class was the right one),
- it **swaps a loop level rather than adding one**: `for block { for chunk }` becomes
  `for chunk { for seg }`.

Overhead ~32 ops per chunk: 0.000016% at C = 1536 MB, 0.006% at 4 MB, 0.39% at a 64 KB worst case.
Inner loop instruction stream byte-identical.

**Reps must wrap the segment loop** (`for rep { for seg }`), not sit inside it — otherwise the working
set collapses to one segment and chunk size stops meaning what it means.

---

## 6. How the versatility asks land

| Ask | Where it lives | Cost |
|---|---|---|
| chunk not bound by allocation size | L1 | none |
| strides that bounce across memory | L2 `Order::Strided` + carried stride phase | none |
| dwell / revisit | L2 `passes_per_chunk`, `reads_per_write` | none |
| random-but-complete coverage | L2 `Order::Permuted` | one multiply per chunk |
| locality-bounded tests | L2 extent (`CacheLevel`) | none |
| relational tests (mirror, copy) | L3 `SingleSegment` | keeps today's behaviour |
| **AMX 2D tiles** | L3 `Shape::Rows2D` | never splits — see below |
| alignment-crossing stress | a dedicated test, not a layout knob — see below | one new test |
| cross-thread page exchange (#19) | L1 rows may reference another thread's blocks | future |

### 6.1 AMX — the original worry, resolved

`TILELOADD tmm, [base+stride]` needs `15 × stride + 64` contiguous bytes for a 16-row tile:
**kilobytes**. Rows are MB multiples. So an AMX tile is four orders of magnitude smaller than any
segment and **can never straddle a seam**. Declare `SingleSegment` + `len_multiple = rows × stride`
and the requirement is satisfied trivially.

So variable block size was never an AMX problem. The real AMX limitation is elsewhere and unchanged
by any of this: the tile ops have no XOR and no compare, so AMX cannot run TMR's accumulator pattern.
It is a data-movement engine here, not a verification engine.

### 6.2 Alignment-crossing stress — a test, not a parameter

The original idea was to salt the starting address so accesses cross alignment boundaries. Two
distinct things hide in that:

- **Span salt** (offset by k × 64 B): free, kernels stay valid, changes which cache sets and page
  offsets the walk lands on. Worth having as an itinerary option.
- **Sub-64 misalignment** (offset by 1–63 B): the kernels store through `*mut u64x8`, which requires
  alignment. This needs `write_unaligned`/`read_unaligned` variants — i.e. **different kernels**.

So sub-line misalignment should be **one dedicated test** (`Mem-Unaligned`), not a global knob that
would silently invalidate every other test's asserted invariant. Cheap to add, honest about its cost.

---

## 7. What this does not do

- **It is not TM5-seamless.** TM5 walks one virtual span with physical pages rotating underneath; here
  the pointer jumps 1–2 times per chunk. Coverage, working set and logical continuity match; *virtual*
  continuity does not. Nothing TMR measures depends on it — DRAM already sees arbitrary PA transitions
  at every page boundary (255 of them in a 512 MB walk over 2 MiB pages), and prefetchers never cross
  page boundaries anyway. See `tile_abstraction_spec.md` section 15.6.
- **It does not make page geometry uniform.** A chunk's segments can have different page types;
  stride-sensitive tests must read `Seg.page_type` or request `UniformWithinSpan`.
- **It does not revive VA stitching.** The recorded trigger is "one contiguous span larger than the
  biggest allocatable block" — not met by a 1536 MB chunk over a 4096 MB block. If it ever fires,
  stitching *composes*: it merges rows, shrinking the table toward one row, and nothing else changes.
- **It does not add physical-address awareness.** `Order::Permuted` varies access history without it.

---

## 7a. Worked examples — two real tests, before and after

Pseudocode. `T = 5632 MB` (rows 4096 HUGE / 1024 HUGE / 512 LARGE), config asks `C = 1536 MB`,
extent = full allocation.

### 7a.1 `Mem-StuckBit` — position-local, 3 phases (Tier 2, owns its loop)

```
  TODAY  (tests.rs:1645-1660)
  ───────────────────────────
  for test_block in test_blocks:               # 3 blocks
      len   = test_block.test_size
      chunk = chunk_size_bytes(test_block.test_size)    # <-- DERIVED PER BLOCK  (bug B5; fixed in TODO 76)
      processed = 0
      while processed < len:
          end = min(processed + chunk, len)             # <-- PARTIAL final chunk
          write_verify(base, processed, end, P1, phase 1)
          write_verify(base, processed, end, P2, phase 2)
          write_verify(base, processed, end, P1, phase 3)
          processed = end
```

What actually runs, for the config above:

| block | its chunk size | chunks produced |
|---|---|---|
| 0 — 4096 MB | 1536 | 1536, 1536, **1024 (partial)** |
| 1 — 1024 MB | **1024** (clamped to block) | 1024 |
| 2 — 512 MB | **512** (clamped to block) | 512 |

**5 chunks, 3 different sizes, 2 of them partial. The configured 1536 MB is honoured twice out of
five.**

```
  PROPOSED
  ────────
  space = LogicalSpace(rows)                   # 3 rows, ~150 bytes, built once
  n     = ceil(T / C)                          # 4
  for k in 0..n:
      start = k * (T - C) / (n - 1)            # 0, 1365, 2731, 4096   (section 4.5)
      segs  = resolve(space, start, C)          # 1-2 Segs, ALWAYS totalling exactly C
      for phase in [P1, P2, P1]:
          for s in segs:  write(s, phase)       # <-- kernel body UNCHANGED
          for s in segs:  verify(s, phase)
```

| chunk | logical range | segments | total |
|---|---|---|---|
| 0 | 0 .. 1536 | row0+0, 1536 | **1536** |
| 1 | 1365 .. 2901 | row0+1365, 1536 | **1536** |
| 2 | 2731 .. 4267 | row0+2731 (1365) + row1+0 (171) | **1536** |
| 3 | 4096 .. 5632 | row1+0 (1024) + row2+0 (512) | **1536** |

**4 chunks, all exactly 1536 MB, none partial, 100% covered.** The `write_verify` macro body does not
change: it receives `(base, start_off, end_off, pattern, phase)` exactly as now — a `Seg` supplies
those three fields directly.

Note the ordering: **phase outside, segments inside.** Phase 2 must not begin on segment 0 until
phase 1 has finished the whole chunk, or the 3-phase transition-flip coverage is measured over one
segment instead of the chunk.

### 7a.2 `Mem-Stride` — the jumping case, where chunk size genuinely matters

TM5 stride, `Channels = 2`, `Parameter = 8`: `JumpStep = 64 * (2*8 - 1) = 960 bytes`.

```
  TODAY
  ─────
  for block, for chunk in block:
      for pass in 0..Parameter:                # Parameter offset passes cover every line
          i = pass * 64
          while i < chunk_len:
              touch(base + chunk_start + i)
              i += 960

  PROPOSED
  ────────
  segs = resolve(space, start, C)
  for pass in 0..Parameter:
      phase = pass * 64                        # carried ACROSS segments, not reset
      for s in segs:
          i = phase
          while i < s.len:
              touch(s.base + s.offset + i)
              i += 960
          phase = i - s.len                    # <-- the whole "carry" mechanism
```

The numbers that make this a non-issue:

```
  stride                 960 bytes
  smallest segment    ~171 MB      = 186,000 strides inside one segment
  ratio               1 : 186,000

  A 1536 MB chunk contains 1,677,721 strides and 1 seam.
  Fraction of strides that land on a seam: 0.00006%.
```

Carry arithmetic for the real numbers: `171 MB / 960 = 186,864.53`, so `phase` enters the next segment
at `171 MB - 186864*960 = 512` bytes rather than 0. Without the carry every address after the seam
shifts by 512 bytes — coverage is still complete (it is a sweep), but the address sequence stops
matching a contiguous walk. **The carry is one subtraction per segment**, i.e. one per chunk.

**The general point for strided/jumping tests: real stride values are KB to a few MB; segments are
hundreds of MB. A stride never straddles a seam in any realistic configuration.** These tests are the
*least* affected by segmentation, not the most — they are already discontinuous by construction, so
one extra address discontinuity per 1536 MB is invisible. What they need is phase continuity, which is
arithmetic, not infrastructure.

### 7a.3 Does segmentation weaken cache defeat? No — reuse distance is what matters

The worry is that walking `64 MB + 64 MB` gives cache a 64 MB working set instead of 128 MB. What
actually governs eviction-before-return is the **reuse distance**: how much other traffic passes
between two touches of the same byte.

```
  touch byte b in seg0
    ... rest of seg0   (up to len(seg0))
    ... all of seg1    (len(seg1))
  touch byte b again           <- intervening traffic = C, regardless of segmentation
```

Reuse distance is `~C` either way, because a rep walks the *whole chunk* before returning. This is why
**reps must wrap the segment loop**: invert them and reuse distance collapses to `len(seg)`, which is
the one arrangement that genuinely does weaken the test.

### 7a.4 Honest scorecard

| test class | seam effect | disposition |
|---|---|---|
| position-local sequential (StuckBit, Refresh, SimpleV2/NT, Bench-*) | one prefetch-stream break per segment (~1 per 100s of MB) vs a page break every 2 MB | split freely |
| strided / jumping (Stride, CacheBust) | 0.00006% of strides; needs phase carry | split once carry is added; `SingleSegment` until then |
| relational (Mirror, BlockMove) | needs two positions in one span — a real blocker | `SingleSegment`, documented |

## 8. The stitching plan — and a correction to the obvious ordering

*Tracked as **TODO #70**, and it is the top active item.*

The intuitive plan is: *allocate exactly as today but with reserve, let the balancer distribute blocks
to threads, then stitch each thread's blocks into one VA before handing control back to the app.*
The intent is right; **that ordering cannot be implemented**, and the fix makes the design better.

### 8.1 Why "stitch after distribution" does not work

`MEM_REPLACE_PLACEHOLDER` is a *commit-time* argument. The sequence is:

```
  reserve placeholder  ->  split it  ->  commit INTO the split slot
```

There is no operation that takes an already-committed region and relocates it into a placeholder.
Committed memory's VA is where its PTEs live; large pages make that doubly true, since the whole point
of a 1 GiB page is a single PDPTE covering one specific aligned VA range. So "allocate first, place
second" would require **section objects** (`CreateFileMapping2` + `MapViewOfFile3`), where the backing
store is a nameable object that can be mapped at a chosen address — a different acquisition path,
needing `SEC_LARGE_PAGES`, and a bigger change than what follows.

### 8.2 What to do instead — one global placeholder, descending-size packing

Keep the user's intent (distribution stays late, threads get one flat VA) by inverting only the
*reservation*, which is free:

```
  STEP 1  RESERVE one placeholder. THE ONLY call that names address policy.

      base = VirtualAlloc2(NULL, total_rounded_up,
                 MEM_RESERVE | MEM_RESERVE_PLACEHOLDER, PAGE_NOACCESS,
                 MEM_ADDRESS_REQUIREMENTS {
                     LowestStartingAddress = <today's floor, e.g. 0x1C0000000>,
                     Alignment             = 1 GiB })

      BaseAddress is NULL here, so the requirements are legal.
      No physical memory touched. VA is not scarce (128 TiB) — over-reserve freely.

  STEP 2  ACQUIRE exactly as today: page-size-first, greedy, DESCENDING page size then size.
          For each planned block:

      2a  VirtualFree(cursor, size, MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER)
              splits [cursor, cursor+size) out as its own placeholder
      2b  VirtualAlloc2(cursor, size,
              MEM_RESERVE | MEM_COMMIT | MEM_LARGE_PAGES | MEM_REPLACE_PLACEHOLDER,
              PAGE_READWRITE,
              /* NO MEM_ADDRESS_REQUIREMENTS — cursor is already aligned */)
      2c  cursor += size

  STEP 3  DISTRIBUTE to threads — by slicing OFFSET RANGES, not by handing out blocks.
  STEP 4  Hand back one base pointer + per-thread (offset, len) per page type.
```

Worked against a real run (8 threads × 7 GiB = 56 GiB, 2026-09-18 log; huge pages exhaust after 7 of
the 8 planned 4 GiB blocks with error `1450`, exactly as they do today):

```
  huge phase — 1 GiB pages need 1 GiB-aligned offsets
    +0 GiB / +4 / +8 / +12 / +16 / +20 / +24    4 GiB huge   ✓ (7 succeed)
    +28 GiB                                      4 GiB huge   ✗ 1450
    huge region ends at +28 GiB, itself a 1 GiB multiple

  large phase — 2 MiB pages; every 1 GiB offset already qualifies
    +28 GiB   4 GiB large   (the retried slot)
    +32 GiB   2 GiB large × 8   -> +32 … +48 GiB
    +48 GiB   1 GiB large × 8   -> +48 … +56 GiB
```

Note what exhaustion does here: **a failed commit leaves the placeholder slot intact**, so the slot is
simply retried with a different page type and the layout does not shift. Today a failed huge
allocation leaves an unpredictable hole in VA that later allocations may or may not land in.

Five things fall out of this, and they are the argument for it:

1. **Each thread gets one row PER PAGE TYPE** — two in the run above, not one (see 8.2a). 24 blocks
   become 16 rows; every chunk up to ~3 GiB resolves to a single `Seg`. Segmentation becomes the
   uncommon path rather than the norm, but it does **not** disappear, and L1's multi-row machinery
   stays load-bearing.
2. **Distribution stays after acquisition**, which is exactly what **B4** needs. The stranded-huge-page
   bug and the stitching change are the *same* change.
3. **Alignment comes for free, and this is the answer to the huge-page objection.** The concern — "the
   way we call it today to FORCE huge pages stops stitching" — is correct as stated: today alignment
   comes from `MEM_ADDRESS_REQUIREMENTS { Alignment = 1 GiB }` with `BaseAddress = NULL`, and you cannot
   pass address requirements when you are naming an explicit placeholder address. **So move the
   alignment to the placeholder reservation**, where `BaseAddress` *is* NULL and the request is legal.
   Then every 1 GiB-multiple offset inside the placeholder is 1 GiB-aligned by construction. Descending
   size order does the rest: each offset is a multiple of every page size still to come, so a 2 MiB
   block can never be asked to start at a 1 GiB-hostile offset. Alignment is preserved *structurally*,
   not by luck.
4. **Cross-thread handover (TODO #19) becomes trivial** — one address space, so a tile descriptor is
   just `(offset, len)` and needs no block identity at all.
5. **Nothing above L0 changes.** L1/L2/L3 are written against a row list; this makes the list shorter.

### 8.2a Page-type fairness — today's system does not have it, and this is how to get it

The stated reason to distribute after acquisition is fairness: don't lump one thread with all the huge
pages. **Byte fairness is achieved today; page-type fairness is not.** From the 2026-09-18 run:

```
  Thread   Huge #   Huge GiB    Large #   Large GiB
     6          -          -       3584        7.00     <- ZERO huge pages
     5          4       4.00       1536        3.00
     0,1,2,3,4,7  … same as thread 5
```

`CV=0.000 ✅ Fair` is measuring bytes. The distributor groups by **block size class**
(`Distributing 8 × 4096MB blocks`), and inside the 4096 MB class 7 blocks are 1 GiB-huge while one is
2 MiB-large — so one thread draws the short straw and runs at **0 % huge pages while the others run at
57 %**. Two threads executing the same test on differently-paged memory are not comparable, and the
report does not say so.

The root cause is granularity: ownership is a whole block, and a 4 GiB block cannot be shared. Under
the placeholder design ownership is an **offset range**, so the cut can go anywhere:

```
  huge region = [+0, +28 GiB)          large region = [+28, +56 GiB)

    thread:      0    1    2    3    4    5    6    7
    huge  GiB:   4    4    4    4    3    3    3    3      (1 GiB-aligned cuts)
    large GiB:   3    3    3    3    4    4    4    4      (2 MiB-aligned cuts)
    total    :   7    7    7    7    7    7    7    7
```

Worst huge-page deviation from the 3.5 GiB mean: **0.5 GiB, versus 3.5 GiB today**. Cuts stay aligned
to their own page size so no 1 GiB page is split across two threads. This is a strict improvement, and
it costs nothing — the cut is an offset, not an allocation.

**Report it either way.** Per-thread huge/large split already appears in the allocation summary; the
fairness table should measure it too, not just bytes. That is worth doing *before* the spike, since it
makes today's imbalance visible and gives the after-state something to be compared against.

**And the constraint that forces two rows.** A 1 GiB page can only be committed at a 1 GiB-aligned
offset, so huge and large blocks **cannot be interleaved** — placing a 3.5 GiB large slice ahead of a
huge block leaves that block at a non-1-GiB-aligned offset and the commit fails. Page types therefore
stay segregated within the space, which means "one contiguous slice per thread" is only reachable by
making some threads all-huge and others all-large — *worse* than today. Two rows per thread (one per
page type) is the correct outcome, and it aligns with the existing L0 invariant that a row never mixes
page types. Consequence to state at plan time: a chunk cannot span the huge/large divide, so on this
machine `C = 4 GiB` is **rejected** with the reason, not silently truncated.

### 8.3 The unknown, and the trap

**Unknown:** whether `MEM_LARGE_PAGES` is accepted together with `MEM_COMMIT | MEM_REPLACE_PLACEHOLDER`
at all. Documented behaviour does not say, and the combination is unusual enough that it may simply
fail with `87`. That single question is what the spike answers, and it is worth answering first because
the answer changes how much multi-row machinery L1 needs in its first version.

**The trap: if it appears to succeed, do not believe it.** Real page size is not observable after the
fact (see the memory note of the same name). The plausible failure is not an error return, it is a
*silent downgrade to 4 KB pages* while the call reports success — which would be far worse than not
stitching, because it would quietly convert every test into a TLB-thrash benchmark. So a success must
be validated behaviourally: a TLB-sensitive random-access proxy over the stitched region, compared
against a plain `MEM_LARGE_PAGES` control of the same size on the same machine. Comparable latency =
real large pages. A large regression = 4 KB, and the path is dead.

**L1 ships regardless.** Stitching removes the *block seam* as a split cause; it does not remove the
logical space, which is also what tile mode, the even-spaced start formula, and cross-thread
descriptors are expressed in. If the spike fails, nothing in sections 3–7 changes — there are just more
rows.

### 8.4 This reopens a settled decision, deliberately — and only half of it

TMR-APP `CLAUDE.md` lists **"No VA stitching (`MEM_RESERVE_PLACEHOLDER`)"** as settled, with the
revival trigger *"a kernel needing one contiguous logical span larger than the biggest single
allocatable block."* Chunk sizes up to 4 GiB over a 5–6 GiB space is that case, so the trigger has
fired rather than the decision being ignored. If the spike succeeds, that entry must be rewritten, not
quietly contradicted.

But the reasoning in that entry stays true, and it caps what to claim: **stitching VA does not stitch
PA.** A seam is still a physical discontinuity, exactly as a block boundary is today — adjacent VA
either side of a seam lands at unrelated physical addresses, so a strided walk crossing one still jumps
to an arbitrary row/bank. Nothing here improves physical locality. What stitching buys is
(a) one base pointer per thread, (b) chunks that are genuinely `C` bytes instead of `min(C, block
remainder)`, and (c) relational kernels (Mirror, BlockMove) able to span what is today two blocks. The
old entry was right that (a) alone does not justify the complexity; (b) and (c) are the new part, and
they are test-fidelity arguments, not ergonomics.

---

## 9. Build order

0. **Spike the stitching question** (§8.3). Standalone, a few hours, decisive, and its answer sets how
   much of L1's multi-row path gets exercised in practice. Must include the TLB-proxy validation.
1. **L1 alone, behind the existing API.** Build `LogicalSpace` + `resolve()`, keep chunk sizes and
   order exactly as today. Nothing observable changes; this is pure de-risking.
2. **Switch Tier 1's loop** to `for chunk { for seg }`. 28 tests, no kernel edits. Prove parity
   against current results.
3. **Delete the clamps** — `prev_power_of_two` on *chunks*, per-block chunk derivation,
   `allow_misaligned` (dormant: the power-of-two round-up always undid it; deleted 2026-10-03). This is where **B5** dies
   and where `Check_absolutnew.cfg`'s test 15 starts running at one honest size for every block
   instead of five sizes across three values. Done in TODO 76 (2026-10-03, uncommitted), with the
   importer's power-of-two cap: test 15 runs at its own 1536 MiB, as one chunk under
   `allocator=stitched` (§4.6 now keeps 1536 rather than rounding it to 2048).
4. **Fix dwell (B2) and the fraction codes (B1).** Independent of the above and the largest fidelity
   gain per line changed — B1 alone is a 440× size error on real community configs. Done 2026-10-03
   (TODO 79, commit 79b9de0).
5. **L0 changes**: the global placeholder + global-then-distribute (**B4**), drop power-of-two *block*
   sizes. Chunks are multiples of 4 KiB (§4.6), so neither chunks nor blocks need powers of two;
   only the bandwidth and latency tests still take power-of-two blocks.
6. **`Order` + `SpanRequest`.** The versatility payload, once the plumbing is proven.
7. **Then** the new tests that were the point: permuted-order sweeps, `Mem-Unaligned`, AMX movement,
   and tile mode for cross-thread handover (#19).

Steps 1–4 are strictly bug-fixing and simplification. The design only starts *adding* capability at
step 6, and by then every silent-adjustment path it could hide behind is gone.
