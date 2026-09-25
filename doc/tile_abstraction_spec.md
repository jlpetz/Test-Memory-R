# Tile abstraction — specification for debate

Status: **proposal, not implemented.** Nothing in `src/` works this way yet.

> **Read sections 14 and 15 first — together they are the actual proposal.** Section 14 states it;
section 15 fixes 14's one real error (segments must be 64-byte quantised, not byte-granular) and
sets out honestly what the block seam does and does not give you versus TM5. Sections 2–6 describe an earlier,
> over-engineered version built on a fixed "tile granule" `Q` with chunks confined inside one
> block. Section 14 replaces it with a single logical address space, which is both simpler and
> strictly more capable: chunks of any size including cross-block, no granule, no remainder list,
> no power-of-two requirement, and no exiled memory. Sections 7 (pattern invariance), 8 (page
> geometry), 11 (overhead) and 13 (why block seams are not special) still stand as written.

Companion to [`allocation_questions_answered.md`](allocation_questions_answered.md) (Q19–Q22, which
established *why*) and [`memory_allocation_models.md`](memory_allocation_models.md) (which describes
TM5's and TMR's current behaviour). This doc specifies *how*, with the arithmetic worked all the
way through, so the design can be argued before any code changes.

Worked throughout against one concrete allocation:

```
  per-thread allocation:  1 x 4 GB HUGE  +  1 x 1 GB HUGE  +  1 x 512 MB LARGE  =  5632 MB
```

and one concrete config — `Check_absolutnew.cfg`'s locality sweep:

```
  Test  BlockSize   Time(%)   loops   BlockSize x (1+loops)
  ──────────────────────────────────────────────────────────
   1      4 MB        240      150          604 MB
   8      8 MB        120       75          608 MB
   9     16 MB         60       37          608 MB
  10     32 MB         30       18          608 MB
  11     64 MB         16       10          704 MB
  12    128 MB          8        5          768 MB
  13    256 MB          8        5         1536 MB  <- loops floors at 5,
  14    512 MB          8        5         3072 MB     so the constant-work
  15    window          8        5         9216 MB     invariant breaks here
```

**Read that config's intent first, because it is the acceptance test for this whole design.**
Columns 2 and 5 together say: *hold the work per chunk constant (~604 MB touched) and sweep the
locality from L2-resident to DRAM-bound.* Test 1 hammers 4 MB 151 times; test 12 sweeps 128 MB 6
times. Same bytes moved, completely different part of the memory hierarchy under test. Any
abstraction that cannot reproduce that sweep is wrong.

(Test 15's `BlockSize = window` with `1536 = 9216/6` tells us this config's window is **1536 MB** —
derived from the table, not assumed.)

---

## 1. The problem, in one diagram

Today the unit handed to a kernel inherits the *allocator's* shape, so it is not uniform:

```
  TODAY  (B5 in the Q&A)

  window = 1536 MB byte budget, spread greedily largest-block-first
  config asks for chunk = 512 MB

  ┌──────────── block 0: 4096 MB ────────────┐ ┌── blk 1: 1024 ──┐ ┌─ blk 2: 512 ─┐
  │ test_size = 1536 (window-limited)        │ │ test_size = 0    │ │ test_size=0  │
  │ chunk = min(512, 1536) = 512 MB          │ │  NEVER TESTED    │ │ NEVER TESTED │
  └──────────────────────────────────────────┘ └──────────────────┘ └──────────────┘

  ...and with a smaller/odder block set, chunk is computed per block from that
  block's own test_size, so one test runs 512 MB chunks on one block and 32 MB
  chunks on another -- 16x apart, different cache tiers, same named test.
```

The tile layer inserts one level so the kernel's unit stops inheriting the block's size:

```
  PROPOSED

  Stage 1  ALLOCATION   blocks, heterogeneous, whatever the OS gave us   (unchanged)
  Stage 2  TILE GRID    uniform granule Q laid over every block          (NEW, free)
  Stage 3  COVERAGE     which tiles this cycle visits  (ring: start,count)
  Stage 4  CHUNK        k adjacent tiles = the unit handed to the kernel
  Stage 5  DWELL        reps within the chunk  (write_read_cycles / test_reps / verify_reps)

  Stages 3-5 are all integer counts. Only stage 1 touches the OS.
```

---

## 2. The tile grid over the example allocation

Pick `Q = 1 MB` (justified in section 3). Lay it over all three blocks and number the tiles
**continuously across blocks**:

```
   Q = 1 MB                                              N = 5632 tiles

   block 0  (4096 MB, HUGE 1 GiB pages)
   tile#  0        1024       2048       3072       4095
          ├────────┼──────────┼──────────┼──────────┤
          │▓▓▓▓▓▓▓▓│▓▓▓▓▓▓▓▓▓▓│▓▓▓▓▓▓▓▓▓▓│▓▓▓▓▓▓▓▓▓▓│   4096 tiles
          └────────┴──────────┴──────────┴──────────┘
           ^page0    ^page1     ^page2     ^page3        4 x 1 GiB pages

   block 1  (1024 MB, HUGE 1 GiB pages)
   tile#  4096                                  5119
          ├──────────┤
          │▓▓▓▓▓▓▓▓▓▓│                                   1024 tiles
          └──────────┘
           ^one 1 GiB page

   block 2  (512 MB, LARGE 2 MiB pages)
   tile#  5120                     5631
          ├─────┤
          │░░░░░│                                        512 tiles
          └─────┘
           ^256 separate 2 MiB pages  <-- different substrate, see section 8

   flat index space:  [0 ................................................ 5631]
                      block 0          block 1        block 2
```

The index space is the abstraction. Everything downstream — coverage, chunking, advance,
reporting — is arithmetic on these integers.

---

## 3. Choosing Q — and a correction to my earlier claim

In the Q&A I wrote `tile_floor = chunk_floor = plan_floor`. **That was wrong, and conflating them
is what made this hard to visualise.** They are three unrelated quantities:

| Quantity | What sets it | Cache-derived? |
|---|---|---|
| `plan_floor` | smallest block worth a syscall (currently 16 MB) | yes — a block below cache size can't host a DRAM-bound test |
| `chunk_bytes` | the *test's* locality target — 4 MB for test 1, 512 MB for test 14 | yes, **and deliberately sweeps across tiers** |
| **`Q`** | the addressing granule | **no. None at all.** |

`Q` has no cache relationship because it is never the working set — it's just the unit the index
space counts in. So the only constraints are:

```
  (1)  Q must be a power of two                     -> exact division, see section 5
  (2)  Q must divide every ring-eligible block      -> guaranteed by (1) since all
                                                       allocator sizes are powers of two
                                                       (allocator.rs:489,734,801,1235)
  (3)  Q <= the finest chunk any test needs         -> 4 MB for Check_absolutnew
```

And critically — **small Q costs nothing**, because tiles are never materialised (section 4). So there's
no pressure to make Q large:

```
  Q = 1 MB   ->  N = 5632   tile "array" size: 0 bytes
  Q = 64 KB  ->  N = 90112  tile "array" size: 0 bytes
```

**Recommend `Q = 1 MB` as the default**, dropping to 64 KB only if a test wants sub-MB chunks.
1 MB expresses every chunk in the sweep above and every chunk currently hardcoded in `runner.rs`.

> **Q must be global across threads, not per-thread.** Threads own different block sets, so a
> per-thread Q gives per-thread chunk geometry — B5 again, just moved from between-blocks to
> between-threads, and it would make per-thread results non-comparable. Derive Q (and
> `chunk_tiles`, section 5) once from the *global* minimum ring block, and every thread runs identical
> geometry. This is a design commitment, not an implementation detail.

---

## 4. The data structure — a block table, not a tile array

The thing that makes this cheap: **do not build an array of 5632 tile descriptors.** Store one row
per *block* and compute tiles on demand.

```
  BlockTable  (a thread owns 2-5 blocks; the ladder has 9 possible sizes)

  idx │ base_va        │ size_tiles │ first_tile │ page_type
  ────┼────────────────┼────────────┼────────────┼───────────
   0  │ 0x0000_2A00... │    4096    │      0     │ Huge(1GiB)
   1  │ 0x0000_7C00... │    1024    │   4096     │ Huge(1GiB)
   2  │ 0x0000_9E00... │     512    │   5120     │ Large(2MiB)
                                       ^^^^^^
                                    prefix sum of size_tiles

  3 rows. Not 5632. Memory cost: ~120 bytes per thread.
```

Tile resolution is then a scan of that tiny table:

```
  fn tile(i) -> Tile:
      b = last row where first_tile[b] <= i          // <= 5 compares, branch-predicted
      off = (i - first_tile[b]) * Q                  // 1 sub, 1 shl
      Tile {
          block_base: base_va[b],     // pattern coordinate origin  (section 7)
          offset:     off,            // pattern index origin       (section 7)
          len:        Q,
          page_type:  page_type[b],   // NOT uniform                (section 8)
      }
```

Consequence: **re-tiling with a different Q is changing one integer.** No allocation, no rebuild,
no syscall. That is the whole answer to "TM5 could reshape memory and we can't" — we reshape the
*description*, and the description is three rows and a divisor.

---

## 5. Chunk = k tiles — and why nothing ever straddles a block

A chunk is `chunk_tiles` adjacent tiles. The one hard constraint is that **a chunk must not cross a
block boundary** (no VA stitching — adjacent VA is unrelated PA, settled decision). Naively that
needs a per-chunk boundary check and produces ragged tails. It doesn't, because of this:

```
  THEOREM
    If every ring block size is a multiple of chunk_bytes, then every block's
    first_tile is a multiple of chunk_tiles, so every chunk lies wholly inside
    one block. No straddles, no ragged tails, no runtime check.

  WHY IT HOLDS HERE
    first_tile is a prefix sum of block sizes.
    A sum of multiples of C is a multiple of C.
    All allocator sizes are powers of two (verified: allocator.rs:489 etc).
    So: require chunk_tiles to be a power of two, and chunk_bytes <= min ring block.
```

Check it on the example with `chunk_bytes = 128 MB`, i.e. `chunk_tiles = 128`:

```
  first_tile:     0        4096      5120
  / 128:          0   ok    32  ok    40  ok      <- all exact, no chunk spans a seam

  block 0: 4096/128 = 32 chunks
  block 1: 1024/128 =  8 chunks
  block 2:  512/128 =  4 chunks
                     ── total 44 chunks, every one exactly 128 MB
```

So the three rules that buy total uniformity:

```
  R1  every ring block size is a power of two          (allocator already guarantees this)
  R2  Q is a power of two                              (section 3)
  R3  chunk_tiles is a power of two,
      and chunk_bytes <= smallest ring block           (clamp; see section 6)
```

`prev_power_of_two` (`tests.rs:1459`) was groping toward R3 but did it in *byte* space, where
rounding down **discards memory** (512 MB block vs 300 MB remaining → 44 MB dropped). In tile
space the same rounding is exact and loses nothing. **The function gets deleted, not fixed.**

---

## 6. The one honest limit: `chunk_bytes <= smallest ring block`

R3 clamps. Where does that bite? Only test 15:

```
  smallest ring block = 512 MB   ->  max chunk = 512 tiles

  test 14:  512 MB requested  ->  512 tiles   exact, no clamp
  test 15: 1536 MB requested  ->  512 tiles   CLAMPED (3 chunks instead of 1)
```

**Argument that this is harmless:** chunk size controls one thing — whether the working set is
evicted before you come back to it. That effect *saturates*. Both 512 MB and 1536 MB are ~5×–16×
the entire 96 MB L3 of a 9800X3D, so both are fully DRAM-bound, and the only difference is how
often a chunk boundary is crossed for bookkeeping. Tests 14 and 15 become the same effective
stressor.

**And they arguably already were.** Look at the user's own table: the constant-work invariant
holds cleanly for tests 1–12 and then *breaks* at 13/14/15 because `loops` floors at 5. By test 13
the config has already lost its design intent — the "constant 604 MB of work" becomes 1536, 3072,
9216 MB. So the clamp lands precisely where the sweep had already stopped meaning anything. That's
a much better defence than "close enough".

**The rule that follows** — and this is the fix for B5's *harmful* direction:

```
  clamp chunk to the smallest ring block, uniformly, for all threads.
  Then report it, and escalate to a WARNING only when the clamp crosses
  a cache tier -- i.e. when:

        requested_chunk  >  2 x total_cache      (test wanted DRAM)
    AND clamped_chunk    <= 2 x total_cache      (but got cache residency)

  Clamping 1536 -> 512 MB: both above the threshold. Silent, harmless. Log at debug.
  Clamping  512 ->  32 MB: crosses into L3 on an X3D part. LOUD. The test now
                           passes for the wrong reason -- same failure family as B3.
```

That distinction is the whole content of B5. Today every clamp is `log::debug!`, so the harmful
case is indistinguishable from the harmless one.

---

## 7. Pattern gen and verify — the invariance argument

This is the part that must be airtight, because if re-tiling forces pattern regen the design is
dead on cost.

**TMR is already safe.** The pattern is a function of `(block_base, offset_within_block)` —
verified at `test_harness.rs:25-31` (`ChunkCtx.ptr` is documented "start of this block's memory",
`chunk_start` is an index *within the block*) and `tests.rs:3067-3069`. Neither coordinate is a
property of the tile or the chunk. So:

```
  value at a byte  =  P( block_base , offset_in_block )
                         ^^^^^^^^^^   ^^^^^^^^^^^^^^^
                         from the block table          not from the tile,
                                                       not from the chunk,
                                                       not from visit order

  => changing Q, chunk_tiles, start, or advance policy CANNOT change any
     expected value. Zero regen. Ever.
```

Shown on one 4 MB span of block 0, under three different tilings:

```
  byte offset in block 0:   16 MB ─────────────────────────────► 20 MB
  values written:           P(b0, 16M) P(b0,16M+8) ... P(b0, 20M-8)

  tiling A  Q=1 MB, chunk=4 tiles     [ t16 t17 t18 t19 ]  one chunk
  tiling B  Q=4 MB, chunk=1 tile      [      t4        ]  one chunk
  tiling C  Q=1 MB, chunk=1 tile      [t16][t17][t18][t19] four chunks
  tiling D  Q=1 MB, chunk=4, visited in reverse ring order

  ALL FOUR write and verify the identical byte values, because the value never
  depended on which bracket the byte sat inside.
```

### What this costs the descriptor — corrects my earlier Q12 advice

I previously said kernels should keep `(ptr, len)`. **That is wrong here.** A kernel handed a bare
pointer into mid-block would compute `block_seed(ptr)` from the *tile's* start, making the pattern
tile-relative — and then every one of the guarantees above evaporates. The descriptor must carry
the origin separately:

```rust
struct Tile {
    block_base: *mut u8,   // pattern seed origin — NOT the tile's own start address
    offset:     usize,     // byte offset within the block — pattern index origin
    len:        usize,     // chunk_tiles * Q  (or the remainder, section 9)
    page_type:  PageType,  // section 8
}
// kernel:  ptr  = block_base.add(offset)
//          seed = block_seed(block_base, cycle)
//          idx  = offset/8 .. (offset+len)/8
```

Two extra words per chunk handoff, computed at setup. Nothing enters the hot loop.

### `thread_id` must come out of the seed

`block_seed(addr, thread_id, cycle)` (`pattern_gen.rs:50`) mixes in `thread_id`. That is fine today
but it **hard-blocks cross-thread tile exchange** (TODO #19, Q17's last row): thread B verifying a
tile thread A wrote computes a different seed and reports every element as an error.
`(block_base, offset)` is already globally unique, so `thread_id` contributes no distinctness —
**drop it**. `cycle` stays (pattern varies per cycle by design; init/verify share a cycle, and the
dependent-test path already pins `cycle = 0` at `tests.rs:4872`).

---

## 8. What tiles do *not* make uniform

Honest limits, so the abstraction doesn't quietly overclaim:

**Page geometry is not uniform even when size is.** At any Q, tiles from block 0/1 sit inside 1 GiB
pages; tiles from block 2 span many 2 MiB pages:

```
  a 4 MB chunk in block 0 (HUGE):   entirely inside one 1 GiB page
                                    -> VA-contiguous IMPLIES PA-contiguous
                                    -> a 1 MB stride stays in one physical page

  a 4 MB chunk in block 2 (LARGE):  spans 2 separate 2 MiB pages
                                    -> a 1 MB stride is fine, a 4 MB stride is not
                                    -> PA relationship across the seam: arbitrary
```

So `page_type` is load-bearing, and **any kernel whose stride exceeds its tile's page size must
filter on it.** Otherwise "uniform tiles" lies about what was tested — the same silently-lossy
failure mode as the memset trap. Tiles buy uniform *geometry arithmetic*; they cannot buy uniform
*physical contiguity*.

**Chunks cannot exceed the smallest ring block** (section 6) — a consequence of the settled no-VA-stitching
decision, not something the tile layer can fix.

**Non-power-of-two blocks would break exactness.** The theorem in section 5 rests on R1. The plan ladder is
all powers of two, but off-plan paths (Phase 1b, gap filling) must be held to that too, or blocks
that violate it must be routed to the remainder list (section 9). **This becomes an invariant to assert,
not an assumption.**

---

## 9. Coverage — the ring

Coverage is `(start, count)` over the index space, wrapping:

```
  window = { tile[(start + k) mod N]  :  k in 0..count }
```

Test 12 (`chunk = 128 MB = 128 tiles`, window 1536 MB = 1536 tiles, N = 5632):

```
  N = 5632 tiles                                          ring wraps here ─┐
  ┌─────────────────────────────────────────────────────────────────────┐  │
  0                                                                  5631  │
  ├──────────────┬──────────────┬──────────────┬────────────┬─────────────┤ │
  │  cycle 1     │  cycle 2     │  cycle 3     │  cycle 4 ...            │ │
  │  0..1536     │  1536..3072  │  3072..4608  │  4608..6144 ─────────────┼─┘
  │  12 chunks   │  12 chunks   │  12 chunks   │  wraps: 4608..5631      │
  └──────────────┴──────────────┴──────────────┴──── then 0..512 ────────┘

  advance:  start = (start + count) mod N          <- TM5's slide, one line
```

Three properties worth having:

- **No dropped tail.** TM5 discarded a short final window slice (Q3); linear indexing would too.
  `mod N` wraps into tile 0 instead — a strict improvement over TM5, not just parity.
- **Coverage becomes reportable.** "Tiles visited" is a set of small integers, so TMR can finally
  state *which* fraction of memory a short run actually touched. It cannot today.
- **Other advance policies are free**: `start += stride`, `start = perm[cycle]` (logged seed),
  `start += count/2` (overlapping windows). All the same cost.

**Fragments stay out of the ring.** A block too small for one chunk, or violating R1, goes to a
separate list:

```
  ring[0..N)      uniform chunk-sized units  <- coverage/advance/dwell operate here
  remainder[..]   leftovers                  <- swept by a linear kernel, reported separately
```

If fragments were in the ring, "a window of 12 chunks" would intermittently contain a short one and
coverage per cycle would stop being constant. Keeping them out preserves ring uniformity **and
nothing is left untested** — which is the rule from Q18: a floor may never discard memory.

---

## 10. The acceptance test: `Check_absolutnew.cfg` through this model

`Q = 1 MB`, `N = 5632`, window = 1536 tiles, smallest ring block = 512 MB:

| Test | TM5 chunk | `chunk_tiles` | chunks/window | dwell `(1+loops)` | bytes/chunk | clamp |
|---|---|---|---|---|---|---|
| 1, 7 | 4 MB | 4 | 384 | 151 | 604 MB | — |
| 8 | 8 MB | 8 | 192 | 76 | 608 MB | — |
| 9 | 16 MB | 16 | 96 | 38 | 608 MB | — |
| 10 | 32 MB | 32 | 48 | 19 | 608 MB | — |
| 11 | 64 MB | 64 | 24 | 11 | 704 MB | — |
| 12 | 128 MB | 128 | 12 | 6 | 768 MB | — |
| 13 | 256 MB | 256 | 6 | 6 | 1536 MB | — |
| 14 | 512 MB | 512 | 3 | 6 | 3072 MB | — |
| 15 | 1536 MB | **512** | 3 | 6 | 3072 MB | yes, section 6 |

**The sweep survives intact for tests 1–14** — every chunk size expressible, every division exact,
uniform across all blocks and all threads, and the constant-work invariant of columns 2/5
reproduced exactly. Test 15 collapses onto test 14, which section 6 argues is behaviourally identical and
lands where the config had already saturated.

The dwell column is what B2 currently gets wrong (`verify_reps` vs `cycles`); this model doesn't fix
B2, it just makes the column meaningful once B2 is fixed.

---

## 11. Overhead accounting

Per **chunk handoff** (not per element):

```
  tile -> block resolve      ~5 compares            (section 4, table has 2-5 rows)
  offset arithmetic           1 sub, 1 shl
  pointer add                 1 add
  block_seed (murmur)        ~9 ops
  ────────────────────────────────────────
  ~20 ops per chunk
```

Against the work in a chunk:

```
  chunk = 4 MB  = 524,288 u64 elements  ->  20 / 524288  =  0.004%
  chunk = 1 MB  = 131,072 elements      ->  20 / 131072  =  0.015%
  chunk = 64 KB =   8,192 elements      ->  20 /   8192  =  0.24%   <- worst plausible case
```

**Nothing is added to the hot loop.** The kernel still receives a base pointer and a length and
runs the same tight loop it runs today; every tile computation happens between chunks. The ring's
`mod N` is one compare-and-subtract per chunk — do *not* force `N` to a power of two to make it an
`AND`, that would waste memory to optimise an operation that runs a few thousand times per run.

---

## 12. Open questions — the actual debate

1. **Is a global `Q` and global `chunk_tiles` acceptable?** section 3 argues yes (comparability across
   threads), but it means one thread's unlucky small block constrains chunk size for everyone. The
   alternative — exclude small blocks from the global ring and let them ride the remainder list —
   costs those blocks their DRAM-bound coverage. **Recommend: global, with the smallest ring block
   floored at `plan_floor`, and anything below routed to remainder.**
2. **Does `advance` default to `sequential` (TM5 parity) or stay pinned at 0 (today's behaviour)?**
   Sequential is strictly better coverage but changes every existing result baseline. Recommend
   sequential, and treat the baseline change as expected.
3. **Should the remainder list get dwell/coverage knobs, or just one linear sweep per cycle?** A
   sweep is simpler and it's a small fraction of memory; knobs are more uniform conceptually.
4. **Does `Tile` carry `owner` now or later?** Not needed until TODO #19 cross-thread exchange, but
   dropping `thread_id` from `block_seed` (section 7) is worth doing at the same time as this change rather
   than as a second migration.
5. **Do we assert R1 (power-of-two blocks) or handle violations?** Asserting is cleaner and the
   allocator already complies; handling means the remainder path must be robust. Recommend assert
   + route violators to remainder, so a future allocator change degrades instead of corrupting.
6. **Migration shape.** Tier 1 (`run_phased_test`, 28 call sites) can adopt this behind
   `ChunkCtx` with no kernel edits, since `ChunkCtx` already carries block base + block-relative
   indices — the change is in who computes `chunk_start`/`chunk_end`. Tier 2 (`TestRunner`, 8 call
   sites) owns its own loop and needs per-test work. **That asymmetry means Tier 1 could land
   first and prove the model on 28 tests before touching Tier 2.**

---

## 13. Could a chunk span blocks, as TM5's window does?

Yes — three mechanisms exist, and none is impossible. But the answer to *"does it matter"* is
mostly **no**, for a reason that isn't obvious and that reframes the whole question: **the ceiling
on meaningful contiguity is the page size, not the block size.** Blocks are already ≥ page size,
so a block seam is never the binding constraint on anything physical.

### 13.1 Why a block seam is not a special boundary

The instinct is that a block seam breaks something a within-block walk preserves. It doesn't,
because within-block walks are *already* full of arbitrary physical jumps:

```
  A 1536 MB walk inside ONE 4 GB block (HUGE, four 1 GiB pages):

    ├──── 1 GiB page 0 ────┼──── 1 GiB page 1 ────┼── page 2 ──
    │ PA base 0x1_4000_0000│ PA base 0x7_8000_0000│ 0x3_C000_0000
    └──────────────────────┴──────────────────────┴────────────
    walk ─────────────────►│─────────────────────►│
                           ^                      ^
                    arbitrary PA jump      arbitrary PA jump

  The SAME 1536 MB assembled from three blocks:

    ├─ blk2 512 MB ─┤   ├─ 512 of blk1 ─┤   ├─ 512 of blk0 ─┤
    walk ──────────►│   │──────────────►│   │──────────────►
                    ^                   ^
             arbitrary PA jump    arbitrary PA jump
             + a VA jump (one pointer reload; the MMU never cared)

  Identical count of arbitrary PHYSICAL discontinuities.
  The seam adds a VA discontinuity, which costs a pointer reload and nothing else.
```

And the scale of it: **inside block 2 alone** (512 MB of 2 MiB LARGE pages) a linear walk crosses
**255** arbitrary physical page boundaries. Two block seams in a 1536 MB span are ~0.8% of the
discontinuities that one LARGE block already contains. Crossing a block boundary is the *least*
significant discontinuity in the system.

So: crossing blocks buys **no additional memory-subsystem stress**, because the thing it would
preserve — physical contiguity — was already lost at the page boundary. The largest span with
*guaranteed* PA contiguity is one page (1 GiB at best), and that is unaffected by chunk size,
block size, or tiling.

### 13.2 What a large chunk actually controls, and why it saturates

Chunk size has exactly one functional job: make the working set large enough that the start is
evicted before you return to it. That effect **saturates**:

```
   chunk vs 96 MB total cache (9800X3D)

     4 MB  ├▓▓░░░░░░░░░░░░░░░░░░┤  L2/L3-resident      <- test 1's intent
    32 MB  ├▓▓▓▓▓▓▓▓░░░░░░░░░░░░┤  partly resident
   128 MB  ├▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓┤  spills
   512 MB  ├══════════════════════════════┤  fully DRAM-bound
  1536 MB  ├══════════════════════════════════════════════════┤  fully DRAM-bound
                                  ^
                        beyond here, nothing changes
```

Everything else a chunk could plausibly influence is independent of it:

| Property | Does a cross-block chunk help? |
|---|---|
| Cache defeat | No — saturated by 512 MB (section 6) |
| Physical contiguity for strides | No — capped at page size (13.1) |
| Coverage | No — coverage is which *tiles* get visited; 3×512 MB covers what 1×1536 MB covers |
| AMX tile loads | No — needs `15×stride + 64` bytes contiguous, i.e. kilobytes |
| TM5 fidelity | Only test 15, which collapses onto test 14 where the config's own invariant had already broken (section 6) |

### 13.3 The one benefit that is real: no exiled memory

There *is* a genuine argument, and it isn't about chunk size for its own sake. It's this
constraint chain:

```
  chunk_bytes <= smallest ring block          (R3, the no-straddle rule)
  => ring floor must be >= largest chunk any test wants
  => blocks smaller than that get exiled to the remainder list
```

For `Check_absolutnew` (largest chunk 512 MB) the ring floor is 512 MB, so **every block below
512 MB leaves the uniform ring.** On a fragmented system with blocks like
`[512, 256, 128, 64, 32]` MB, that's 480 of 992 MB — nearly half the memory — swept as remainder
instead of participating in uniform coverage.

Cross-block chunks would break that chain: ring floor could sit at the granule, all memory stays in
one uniform ring, and any chunk size is expressible. **That is a coverage-uniformity argument, not
a stress argument** — and it is the only real one on the table.

### 13.4 The three mechanisms, costed

**(a) Segmented chunk — a chunk is a list of `(ptr, len)` spans.** This is what you described:
resolve the segment list from the block table at chunk setup, then the kernel loops segments.

```rust
// harness, ONCE per chunk — never per element
let segs: SmallVec<[Seg; 4]> = resolve(tile_range);   // ~5 compares per block crossed

// kernel
for s in &segs {
    let seed = block_seed(s.block_base, cycle);       // pattern still correct (section 7)
    hot_loop(s.ptr, s.len, seed);                     // unchanged inner loop
}
```

**Critically this must not become a per-access lookup.** If a kernel consulted the block table per
element, the indirection would dominate the memory traffic and destroy the measurement — that
implementation is the one thing that must never happen. Segment resolution happens once per chunk;
the inner loop is byte-identical to today's.

Cost is not in the inner loop, it's in the **contract**: 36 kernels gain an outer loop, and so do
`flush_range_to_dram` (per segment), the XOR/OR accumulator reset boundaries, `ErrorCheckInterval`
masking, and error-offset reporting (an offset is now segment-relative). That's real surface area
against the explicit goal of keeping kernels simple and tight.

**(b) VA stitching** (`MEM_RESERVE_PLACEHOLDER` + `MapViewOfFile3`/`MEM_REPLACE_PLACEHOLDER`) —
reserve one range and place blocks adjacently so they genuinely *are* VA-contiguous, needing no
kernel change at all. **Settled: no.** Worth noting the settled decision's revival trigger is *"a
kernel needing one contiguous logical span larger than the biggest single allocatable block"* —
that is **not** met here. We want a span larger than the *smallest* block (512 MB), while the
biggest is 4 GB. Different condition; the decision stands. And 13.1 is exactly why its stated
rationale ("stitching VA does not stitch PA") is correct.

**(c) AWE** — TM5's actual mechanism, and the only one that gives true contiguity with remappable
backing. Assessed in Q16: works from user mode on Win64 without a driver, but is **4 KB-page only**,
so it trades away every 2 MiB/1 GiB page. Self-defeating.

### 13.5 Recommendation

**Don't do cross-block chunks.** The stress and fidelity case is empty (13.1, 13.2), and the one
real case (13.3) is a coverage-uniformity concern that has a cheaper fix:

```
  1. Ring floor = plan_floor (or the cache-defeat floor), NOT the largest requested chunk.
  2. chunk_bytes clamps to the smallest ring block, uniformly, globally  (R3 unchanged).
  3. Blocks below the ring floor -> remainder list, swept linearly, coverage reported separately.
  4. A block that cannot hold a full chunk gets ONE partial chunk, flagged and counted --
     not silently resized like today (B5).
```

That keeps every kernel at one `(ptr, len)`, keeps the no-straddle theorem, and makes the
deviation *visible and bounded* instead of silent — which was the actual defect in B5, not the
non-uniformity itself.

**What would change this recommendation:** measure the remainder fraction on real allocations. If
routine consumer runs put more than ~10–15% of memory outside the uniform ring, uniform coverage is
materially compromised and segmented chunks (option a) start earning their complexity. That is a
concrete, measurable trigger — and it should be measured *before* this spec is implemented, since it
changes whether `Tile` needs to be a segment list from day one or can stay a single span.

> **Superseded by section 14.** The exile problem in 13.3 turned out to be avoidable outright, and
> segmented chunks turned out to cost far less than estimated in 13.4 — they *replace* a loop level
> rather than adding one. 13.1 and 13.2 still stand: crossing a seam buys no extra stress. The
> reason to do it is that it removes the size coupling, not that it stresses anything harder.

---

# 14. THE PROPOSAL — one logical address space

Everything above about granules, ring floors and remainder lists was over-engineered. This is the
actual design, in full.

## 14.1 The idea

Number every byte of a thread's memory `0 .. T` continuously, across all its blocks, and treat that
numbering as **circular**. That is our own address space. A test unit — "chunk", TM5's
`Test Block Size` — is simply a *range* in it. Resolving that range against a small table yields
1–3 real pointer spans.

```
  OUR LOGICAL SPACE  (contiguous, circular, T = 5632 MB)

  0                            4096            5120        5632 ─┐
  ├────────── block 0 ─────────┼─── block 1 ────┼── block 2 ─────┤ │
  │          4096 MB           │    1024 MB     │    512 MB      │ │
  └────────────────────────────┴────────────────┴────────────────┘ │
   ^                                                              │
   └──────────────────── wraps ───────────────────────────────────┘

  REAL VIRTUAL ADDRESSES  (three unrelated regions — test logic never sees these)

  block 0 @ 0x2A00_0000_0000    block 1 @ 0x7C00_0000_0000    block 2 @ 0x9E00_0000_0000
```

Config asks for a 1536 MB chunk. It gets exactly 1536 MB, every time, wherever it lands:

```
  C = 1536 MB

  chunk 0  logical [   0 .. 1536)  ->  1 segment   (block0 + 0,    1536 MB)
  chunk 1  logical [1536 .. 3072)  ->  1 segment   (block0 + 1536, 1536 MB)
  chunk 2  logical [3072 .. 4608)  ->  2 segments  (block0 + 3072, 1024 MB)
                                                   (block1 + 0,     512 MB)
  chunk 3  logical [4608 .. 6144)  ->  3 segments  (block1 + 512,   512 MB)
           wraps past 5632                         (block2 + 0,     512 MB)
                                                   (block0 + 0,     512 MB)  <- wrapped

  Every chunk is EXACTLY 1536 MB. No clamping, no partial tail, no memory excluded.
```

That is the whole answer to *"how do we get a 1536 MB chunk out of 512 MB blocks"*.

## 14.2 What this deletes

Because the space is circular and we *walk ranges* instead of dividing anything, most of the
machinery in sections 2–6 becomes unnecessary:

| Earlier proposal | Why it's gone |
|---|---|
| tile granule `Q` | uniformity comes from the range width `C`, not from a divisor |
| power-of-two blocks (R1) | we never divide a block, so odd sizes are fine |
| `chunk <= smallest block` (R3) | a chunk just spans more segments |
| ring floor / `plan_floor` coupling | nothing is excluded, so there is no floor to choose |
| remainder list, sub-tile flag | a 16 MB block is simply 16 MB of the space |
| the no-straddle theorem | straddling is the supported case now, not the error case |
| `prev_power_of_two` | still deleted, now for a simpler reason: nothing rounds |

**100% of allocated memory is in the space.** That is the "use as much memory as possible" goal met
exactly, and it fixes **B4** (stranded 1 GiB huge pages) and **B5** (per-block chunk divergence) as
side effects — a huge-page block is just 1 GiB of the space, reachable by any chunk.

## 14.3 The two data structures

```rust
/// Built once per thread at setup. 2-5 rows in practice.
struct LogicalSpace {
    rows:  SmallVec<[Row; 8]>,
    total: usize,               // T
}
struct Row {
    logical_start: usize,
    len:           usize,
    base:          *mut u8,     // the block's real VA
    page_type:     PageType,    // still per-segment — see section 8
}

/// One contiguous pointer span. This is EXACTLY what today's ChunkCtx carries.
struct Seg { base: *mut u8, offset: usize, len: usize, page_type: PageType }
```

```
  LogicalSpace for the example — three rows, ~150 bytes

  logical_start │  len  │ base              │ page_type
  ──────────────┼───────┼───────────────────┼────────────
       0        │ 4096  │ 0x2A00_0000_0000  │ Huge(1GiB)
    4096        │ 1024  │ 0x7C00_0000_0000  │ Huge(1GiB)
    5120        │  512  │ 0x9E00_0000_0000  │ Large(2MiB)
                                    total T = 5632 MB
```

## 14.4 Resolution — the entire mechanism, ~15 lines

```rust
fn resolve(ls: &LogicalSpace, start: usize, len: usize, out: &mut SmallVec<[Seg; 8]>) {
    out.clear();
    let mut pos  = start % ls.total;
    let mut left = len;
    while left > 0 {
        let r    = ls.row_containing(pos);        // linear scan, <= 8 compares
        let off  = pos - r.logical_start;
        let take = left.min(r.len - off);         // stop at this block's end
        out.push(Seg { base: r.base, offset: off, len: take, page_type: r.page_type });
        pos   = (pos + take) % ls.total;          // wrap
        left -= take;
    }
}
```

Called **once per chunk**. Never per element — a per-access lookup would dominate the memory traffic
and destroy the measurement; that is the one implementation that must never happen.

## 14.5 How a test loop uses it — and why it costs nothing

The critical point: **this does not add a loop level, it swaps one.** Today the harness loops
blocks, then chunks within a block. Now it loops chunks, then segments within a chunk. Same depth.

```
  TODAY  (test_harness.rs:239-266)        PROPOSED

  for block in blocks:                    for chunk in cycle:
    for chunk in block:                     segs = resolve(chunk)   <-- once per chunk
      for wrc:                              for wrc:
        for test_reps:                        for test_reps:
          test_fn(ctx)                          for s in segs: test_fn(ctx(s))
        flush(chunk)                          for s in segs: flush(s)
        for verify_reps:                      for verify_reps:
          verify_fn(ctx)                        for s in segs: verify_fn(ctx(s))
```

**Kernels do not change at all.** A `Seg` is exactly what `ChunkCtx` already carries: `base` is the
block base (`ChunkCtx.ptr`), and `offset`/`len` become `chunk_start`/`chunk_end`. So pattern
generation stays correct for free by section 7's argument — seed from `base`, index from `offset`,
and neither depends on how the range was carved.

Two details that matter for correctness:

- **Reps must wrap the segment loop, not sit inside it.** `for rep { for seg { … } }` keeps the
  working set equal to the whole chunk, which is the entire point of chunk size. Inverting them
  makes the working set one segment and silently changes the locality under test.
- **Stride-carrying kernels must carry their phase across a seam.** `Mem-Stride` / `Mem-CacheBust`
  walk with a stride that may not divide the segment length; if the stride position restarts at each
  segment boundary the access pattern subtly changes at seams. Pass the residual in rather than
  restarting it. **This is the one genuine kernel-side care point in the whole design.**

## 14.6 Overhead, counted

Per chunk, for a 2-segment chunk:

```
  resolve()        ~12 ops   (2 iterations, ~6 ops each)
  per segment      ~10 ops   (pointer add + block_seed murmur)
  ─────────────────────────
  ~32 ops per chunk
```

Against the work inside it:

```
  C = 1536 MB = 201,326,592 u64 elements  ->  32 / 201M    = 0.000016%
  C =    4 MB =     524,288 u64 elements  ->  32 / 524288  = 0.006%
  C =   64 KB =       8,192 elements      ->  32 / 8192    = 0.39%    <- worst case
```

The inner loop's instruction stream is byte-identical to today: same SIMD width, same unrolling,
same accumulator pattern. Nothing was added to it.

## 14.7 `Check_absolutnew.cfg` through this model

`T = 5632 MB`, window 1536 MB (so 1536 MB of logical space visited per cycle):

| Test | chunk C | chunks/window | dwell | segments/chunk | clamped? |
|---|---|---|---|---|---|
| 1, 7 | 4 MB | 384 | 151 | 1 | no |
| 8 | 8 MB | 192 | 76 | 1 | no |
| 9 | 16 MB | 96 | 38 | 1 | no |
| 10 | 32 MB | 48 | 19 | 1 | no |
| 11 | 64 MB | 24 | 11 | 1 | no |
| 12 | 128 MB | 12 | 6 | 1 | no |
| 13 | 256 MB | 6 | 6 | 1–2 | no |
| 14 | 512 MB | 3 | 6 | 1–2 | no |
| 15 | **1536 MB** | 1 | 6 | **2–3** | **no** |

**The entire sweep reproduces exactly, including test 15.** Nothing clamps, because chunk size is no
longer bounded by block size. Small chunks almost never split — a 4 MB chunk splits only if it lands
on a block boundary, 2 positions out of 1408.

## 14.8 Coverage advance

`start` moves through the logical space per cycle, `mod T` handles the wrap:

```
  cycle N start = (N * window_bytes) % T        <- sequential, TM5's slide
```

With window 1536 MB and `T` 5632 MB, starts land on multiples of `gcd(1536, 5632) = 512 MB`, giving
11 distinct positions before repeating — and `11 × 1536 = 3 × 5632`, i.e. **exactly 3 complete
passes over all memory in 11 cycles, no gaps and no favoured region.** TM5's dropped tail (Q3)
cannot occur because there is no tail.

## 14.9 What remains honest

- **Page geometry is still not uniform** (section 8). A chunk's segments can have different page
  types, so a stride-heavy kernel must respect `Seg.page_type` or be handed single-page-type chunks.
- **Crossing a seam still buys no extra stress** (section 13.1). It is not a feature — it is simply
  no longer a *limitation*. The reason to allow it is that it removes the size coupling and exiles
  no memory.
- **Segment count grows with fragmentation.** 32 × 16 MB blocks with a 512 MB chunk gives 32
  segments — 32 outer iterations against 67M inner ones, so irrelevant to throughput, but
  `SmallVec<[Seg; 8]>` will spill to the heap. Accept the rare setup-time spill.
- **Error offsets must be reported in logical space**, not segment-relative, or a reported offset
  isn't reproducible. Harness tracks `chunk_logical_start + bytes_consumed`.
- **This is per-thread.** Each thread has its own logical space over its own blocks; no sharing
  until TODO #19.

## 14.10 Revised open questions

1. **Per-thread `T`, or one global space?** Per-thread is simpler and matches today's ownership.
   Global is what cross-thread exchange (TODO #19) eventually wants, but adds coordination.
   Recommend per-thread now, with `Row` carrying enough to widen later.
2. **Does `advance` default to sequential?** Strictly better coverage, but it moves every existing
   result baseline.
3. **Any cap on `C`?** Only `C <= T`. Beyond that a chunk revisits bytes within one pass — probably
   worth rejecting as a config error rather than silently allowing.
4. **Migration order.** Tier 1 (28 `run_phased_test` sites) needs **no kernel edits** — only the
   harness's block/chunk loops are replaced. Tier 2 (8 `TestRunner` sites) owns its own loop and
   needs per-test work, and the stride-carrying tests (14.5) are the only ones needing real thought.
   Land Tier 1 first and prove the model on 28 tests.

---

# 15. Granularity — bytes are the wrong unit for a segment

Section 14 said "a chunk is a range of bytes" and left it there. That is wrong, and the codebase
already says so.

## 15.1 The invariant is already in the source

Every SIMD kernel macro opens with this (`tests.rs:3578-3579`, and the same shape at 4059, 4138):

```rust
let _len = $ctx.chunk_end - $ctx.chunk_start;
debug_assert!(_len % $simd_w == 0, "chunk not aligned to SIMD width");
for i in ($ctx.chunk_start..$ctx.chunk_end).step_by($simd_w) {
    *($ctx.ptr.add(i) as *mut $simd_type) = expected;   // full-width, unconditional
}
```

There is **no tail path**. If the span length is not a multiple of the vector width, the final
iteration writes a whole vector starting at the last step position and runs past `chunk_end` — up to
7 u64 (56 bytes) over. In debug the assert fires; in release it is silent, and it lands on the next
chunk's memory, corrupting that chunk's pattern and producing a **false error report** (or a genuine
out-of-bounds write on the very last chunk of a block).

Today that invariant is supplied at chunk level by `align_to_boundary(size, cache_line_size)`
(`tests.rs:1128/1136/1144/1158`, helper at `1309`). 64 bytes = 8 u64 = exactly `u64x8`'s lane count,
so a 64-B-aligned length is automatically a multiple of `u64x2`, `u64x4` and `u64x8` alike.

**What section 14 got wrong:** it moved the unit of work from *chunk* to *segment* without moving
the invariant with it. A segment ending at an arbitrary byte breaks every kernel above.

## 15.2 The fix — one quantum, stated once

```
  Q = 64 bytes    (hard floor: widest vector = widest cache line = u64x8 lane count)

  Every length in the logical space is a multiple of Q:
      row length        (block size)
      chunk length      (C)
      start position    (window advance)

  => every SEGMENT length is automatically a multiple of Q, because a segment
     ends at exactly one of two places:
        (a) a row boundary  -> multiple of Q by rule 1
        (b) the chunk end   -> multiple of Q by rule 2
```

That is the whole enforcement. It is a property of the space, not per-segment arithmetic — nothing is
checked in the hot path, and the kernels' existing `debug_assert` becomes the free proof that it
holds.

## 15.3 Why this costs essentially nothing

| Rule | Already true? | Source |
|---|---|---|
| row length % 64 == 0 | **yes** — block sizes are whole MB | `allocator.rs:489` `[4096,2048,…,16]` MB |
| chunk length % 64 == 0 | **yes** — unless `allow_misaligned` | `align_to_boundary(…, cache_line_size)` |
| start % 64 == 0 | **yes** — window rounds down to 64 B | `calculate_window_size` |

**Recommended addition, free:** round the *start advance* to **4 KB** rather than 64 B. Rows are
already MB multiples, so this makes every segment **page-aligned as well as vector-aligned**, which
(a) makes page geometry trivially uniform inside a segment and (b) removes the misaligned-base/tail
case that is bug **B3** in `flush_range_to_dram`. Do *not* push 4 KB onto the chunk length — a
cache-relative chunk like `L3/3` is a 64-B multiple but not always a 4 KB one, so quantising it would
move existing result baselines for no gain.

## 15.4 The second kind of requirement — relational kernels

Q=64 covers "my loop is 512 bits wide." It does **not** cover "my pattern has structure across the
span." Those are different problems and only one of them is an alignment problem.

```
  POSITION-LOCAL kernels          touch element i, derive everything from i
  ────────────────────────────    StuckBit, Refresh, SimpleV2, SimpleNT, Bench-Init/Verify
  Need: length % 64 == 0.         Split across segments freely. Nothing to do.

  POSITION-RELATIONAL kernels     touch element i AND element f(i) in the SAME span
  ────────────────────────────    Mem-Mirror (mirror across span), Mem-BlockMove (half -> half),
                                  Mem-Stride / Mem-CacheBust (stride cycle over the span)
  Need: both positions live at once, e.g. mirror_swap_subblocks (tests.rs:3593) takes
  chunk_len, divides by N subblocks and walks each pair in lockstep. N=3 is not even a
  power of two, so no choice of Q helps.
```

For relational kernels the answer is **not** a bigger Q — it is that they get **single-segment
chunks**. The harness clamps their chunk size so it fits inside one row, which is exactly today's
behaviour, preserved for the tests that need it. The difference from today is that it becomes a
**declared per-test property** rather than a global limitation on everyone: 6 of the 9 correctness
tests get unbounded cross-block chunks, 3 keep the old constraint and say why.

`TestMemoryConfig` already carries per-test flags of this shape (`requires_locality`,
`config.rs:409/757`), so this is one more field, not new machinery.

*Could* a relational kernel work across segments? Yes — express both positions in logical space,
resolve twice, and walk the two segment lists in lockstep, merging their boundary sets. That is a
real interval-intersection walk in setup code. It is possible, it is not free, and it should not be
in the first version.

## 15.5 A pre-existing bug this surfaces

`allow_misaligned` bypasses `align_to_boundary` at five sites (`tests.rs:1125/1133/1141/1155/1280`),
so setting it produces a chunk length that trips the `debug_assert` above and silently overruns in
release. **It is not set `true` anywhere in the tree** (`config.rs` derives it from JSON; no literal
`true` exists), so the path is dormant — but it is reachable from a hand-written config. This
upgrades the "mislabelled flag" row in the bug table: the flag does not merely have a misleading
name, it disables an invariant the kernels rely on for memory safety. Recommend it be deleted rather
than fixed — nothing uses it, and "let the vector loop overrun" is not a feature.

## 15.6 What the seam is, and what it is not

Direct answer to the fair objection: **no, this does not let a test shift over a block boundary the
way TM5 does.** Being precise about the gap:

```
  TM5                              THIS PROPOSAL
  ───────────────────────────      ────────────────────────────────────────
  ONE virtual span, 1536 MB.       1536 MB in 2-3 virtual runs.
  Physical pages rotate            The pointer JUMPS between runs.
  underneath. The test's
  address stream is perfectly
  continuous.

  WHAT IS THE SAME
    coverage            every byte visited, chunk size honoured, no dropped tail
    working set         1536 MB resident regardless of carving -> same cache pressure
    logical continuity  if the kernel carries its stride phase across the seam (14.5),
                        the LOGICAL access stream is continuous

  WHAT IS NOT THE SAME
    virtual continuity  TM5: none of the test's addresses jump.  Here: 1-2 jumps per chunk.
```

Does the virtual jump cost stress? The honest accounting, which is section 13's argument applied to
this specific question:

- **DRAM side: no.** In TM5, `X+1023MB` and `X+1024MB` are at unrelated physical addresses too — AWE
  rotates 4 KB pages, so the PA stream is arbitrary at every page boundary. A 512 MB walk in a 2 MiB
  page block already contains 255 arbitrary PA transitions; adding one more at a block seam is noise.
- **Prefetcher / TLB: no meaningful change.** Hardware prefetchers do not cross page boundaries, so
  they are already reset far more often than once per seam. A seam adds a page-walk into a different
  top-level subtree — the same *kind* of event, less frequent than what already happens.
- **What is genuinely lost:** a test that wants to prove *virtual* contiguity over a span. TMR has no
  such test, and the settled decision against VA stitching says it does not want one.

And the connection worth recording: **"a kernel needing one contiguous logical span larger than the
biggest single allocatable block" is the stated revival trigger for VA stitching.** In the 5632 MB
example that trigger is *not* met — 1536 MB would fit inside the 4096 MB block; the chunk only spans
blocks because coverage marched it across a seam. The trigger fires only if someone wants a chunk
larger than the largest block (>4 GB here). If it ever does fire, stitching **composes** with this
design instead of competing with it: stitching merges rows, so the row table shrinks toward one row
and nothing else changes.

So the correct claim for section 14 is narrower than "like TM5": **this closes the coverage gap from
the discarded tail and removes the size coupling between chunk and block. It does not reproduce TM5's
seamless virtual addressing, and nothing measured by TMR's tests depends on that.**
