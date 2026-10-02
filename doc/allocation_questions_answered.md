# Q&A: tiles, chunks, windows, and reps

Answers to follow-up questions on
[`allocation_strategy_analysis.md`](allocation_strategy_analysis.md). Numbered in the order asked.

> **Q19–Q22 are specified in full in [`tile_abstraction_spec.md`](tile_abstraction_spec.md)** —
> the tile grid, the block table, the no-straddle theorem, pattern invariance, and
> `Check_absolutnew.cfg`'s locality sweep worked through end to end. That doc also **corrects**
> two things stated below: `tile_floor = chunk_floor = plan_floor` (Q1) is wrong — the addressing
> granule has no cache relationship — and Q12's `(ptr, len)` descriptor must carry the block base
> and offset instead.

**Five of these turned out to be bugs, not design questions** — Q4, Q5, Q10, Q18 and Q19. They're
marked `BUG` and listed together in Section A at the end. Three of my earlier claims were wrong and
are corrected in place: pattern rotation is withdrawn (Q7), the reproducibility argument is
withdrawn (Q6), and Q12's `(ptr, len)` tile descriptor is corrected to carry the block base and
offset (Q21).

---

## Q1. What is "chunk" in TM5, where is it configured, and does it set a minimum tile size?

**TM5's `Test Block Size (Mb)` *is* the chunk.** There is no separate concept. The full hierarchy:

```
  locked physical pool  (per process, AllocateUserPhysicalPages)   ~= TMR allocation
        │
  window / AWE aperture (Testing Window Size (Mb), global)         ~= TMR window
        │
  test block            (Test Block Size (Mb), PER TEST)           ~= TMR chunk   <-- the kernel's unit
        │
  operation block       (Operation Block, byts = 64, global)       ~= SIMD access granule
```

So it is **per-test, from config, not static.** The kernel is handed exactly
`(pBaseAddr, dRealBlockSize)` and does every repetition inside that span before the orchestrator
advances `pBaseAddr += dRealBlockSize` (`MainThread.asm:627-741`). Your reading is right: the
chunk is what reaches the test algorithm, and it is the cache-defeating unit.

### The minimum-size question — and you're right that it should lift the floor

Your arithmetic is correct, and it's the calculation that matters. The chunk must exceed a
thread's *share* of cache, not total cache, because L3 is shared:

```
  chunk_min  ~=  2 x ( L3_total / threads_sharing_L3  +  L2_per_thread  +  L1_per_thread )

  9800X3D, 96 MB L3 / 8 cores, 1 MB L2/core, 48 KB L1d:
     8 threads (no SMT):  2 x (12 + 1 + 0.05)   ~= 26 MB  -> round to 32 MB
    16 threads (SMT):     2 x ( 6 + 0.5 + 0.05) ~= 13 MB  -> round to 16 MB
```

Two conclusions:

1. **You do not need GB-scale allocations per thread to exhaust cache.** 32 MB does it on the
   worst case (3D V-Cache, no SMT). Large allocations buy *coverage* and huge pages, not
   cache defeat. Those are separate justifications and should be argued separately.
2. **TMR's current 16 MiB plan floor is too small on large-L3 parts.** A 16 MiB chunk on a
   9800X3D running 8 threads fits inside that thread's 12 MB L3 share plus L2 — so the test
   never reliably reaches DRAM and you're back to needing `CLFLUSHOPT` to force the round trip.
   That is precisely the situation you want to avoid by flooding instead.

So: **minimum tile = minimum chunk**, and derive it from cache topology rather than hardcoding:

```
  chunk_floor = next_pow2( 2 * (l3/threads_sharing_l3 + l2_per_thread + l1_per_thread) )
  tile_floor  = chunk_floor            // a tile must hold at least one chunk
  plan_floor  = tile_floor             // replaces the hardcoded 16 MiB
```

with a soft fallback: if `per_thread_total < tile_floor`, use `per_thread_total` and **warn that
natural cache defeat is not guaranteed** — so the user knows to enable `flush_before_verify`.
Don't silently produce a chunk that can't reach DRAM; that's a test that passes for the wrong
reason.

---

## Q2. What is the progression order in TM5? Where does the repetition sit?

**Verified from source.** `mtests0.asm:312` `LoopCheckCycle:` → `:411` `LoopRead:` → `:520`
`jg LoopRead` → `:521-522` `dec dWriteReadCycleCounter / jg LoopCheckCycle`. Both loops are
*inside a single test invocation*, and a single invocation operates on **one chunk**. So:

```
  Cycles                              (config, whole-run repeat)
   └─ Test Sequence                   (config, list of test indices)
       └─ Test                        one entry in the sequence
           └─ Window slice            wCurrentPage = 0,1,2,...  (SetPageNumb(0) then SetNextPage)
               └─ Chunk               pBaseAddr += dRealBlockSize
                   └─ WriteReadCycles  ST_WriteReadCycles = 4     <-- INNERMOST TWO
                       ├─ 1 write pass over the chunk
                       └─ LoopRead     dLoopCounter reads over the chunk
```

**Reps are innermost. Locality is maximal.** At TM5 defaults each chunk receives
`4 × (1 write + 5 reads) = 24 passes` *before the pointer moves at all*. Then chunks advance
through the slice; then the slice advances; then the whole sequence repeats.

Answering your two sub-questions directly:

- *Does it rep the chunk before others in the window?* **Yes.** All dwell is spent on one chunk,
  then it moves on. It never completes the window and comes back for a second chunk-rep pass.
- *Does it rep the window before sliding?* **No.** One pass of chunks per slice, then slide.
  Window-level repetition only happens via the outer `Cycles`, which restarts from slice 0.

And yes — **the locality is the point**, not an accident. It's what makes
`Check_absolutnew.cfg`'s block-size sweep meaningful (§1.9 of the models doc): holding
`chunk_size × (1+loops)` constant at ~600 MB while chunk size sweeps 4→512 MB only tests
"cache residency at constant dwell" *because* the dwell is concentrated on one chunk. Spread the
same traffic across the whole window and every test in that sweep becomes identical. Which is
exactly what TMR currently does to it — see Q4.

---

## Q3. Does the AWE window's start offset change each iteration, and what does that buy?

Partly right, with one important correction and one thing that's more interesting than you'd
expect.

```
  The window's VIRTUAL base (pMemTestWindow) is FIXED for the entire run.
  What rotates is which PHYSICAL pages are mapped into it:

      MapUserPhysicalPages(windowBase, n, &PFN[wCurrentPage * windowPages])
                                            ^^^^^^^^^^^^^^ this is the offset that moves
```

**(A) Coverage — yes, and it's the main payoff.** Every locked physical page passes through the
aperture, so every test covers 100% of the pool.

**But not in the way you suggested.** You wrote "ensuring any tail excluded in one run is
eventually included" — that isn't what happens. The slice boundaries are deterministic
(`wCurrentPage` always resets to 0 at test start and steps by exactly one window), so the same
bytes land in the same slice every time. And TM5's *chunk* tail drop (`MainThread.asm:741`,
remainder smaller than one chunk is skipped) is therefore **permanently dropped, identically
every pass** — sliding does not rescue it. TM5's coverage is 100% of *slices* but not 100% of
*bytes*; it leaks up to one chunk per slice, forever.

**(B) Randomness / extra load — no.** It's a deterministic sequential sweep. No added entropy.
The remap costs ~35 ms per GB, which is pure overhead, not stress.

**The interesting part, which is the opposite of what one would guess:** because the VA base is
constant, TM5's kernels see the *identical virtual addresses* on every slice — same alignment,
same page offsets, same TLB behaviour — while the *physical* memory underneath changes
completely. TM5 gets physical coverage variation at zero VA variation.

TMR has exactly the inverse: VA varies (different blocks, different offsets) while physical
coverage is pinned to block offset 0. If we add rotation (Q11), we can have TM5's property
*and* full byte coverage, because rotating a tile index changes both.

---

## Q4. `BUG` — Do we need per-chunk dwell control in TMR?

**The knobs already exist and are correctly nested. The TM5 loader wires `Time (%)` to the wrong
one.**

`test_harness.rs:239-266` — TMR's Tier-1 harness is structurally identical to TM5:

```rust
for chunk in chunks {
    for _ in 0..write_read_cycles {          // TM5 ST_WriteReadCycles
        for _ in 0..test_reps { test_fn(&ctx); }
        if flush_before_verify { flush_range_to_dram(...); }
        for _ in 0..verify_reps { verify_fn(&ctx); }   // TM5 dLoopCounter
    }
}
```

So `write_read_cycles` ≡ `ST_WriteReadCycles` and `verify_reps` ≡ `dLoopCounter`, both
per-chunk, both innermost. Good design, already there.

The problem is the translation. `config.rs:1439-1447`:

```rust
let base_cycles = test.time_percent as f64 / 100.0;
let effective_cycles = ((base_cycles * global_time_multiplier).ceil() as u32).max(1);
...
cycles: Some(effective_cycles),      // <-- OUTERMOST loop
```

TM5's `Time (%)` drives `dLoopCounter = (test% × global%) / 2000` — the **innermost** loop.
TMR maps it to `cycles`, the **outermost**. Two independent errors compound:

| | TM5 | TMR today |
|---|---|---|
| Divisor | `2000` (`ST_Div_Down`) | `10000` (two ÷100s) |
| Loop level | innermost (per chunk) | outermost (per plan cycle) |
| `write_read_cycles` | 4 | **1** — defaults never set from the .cfg (`tests.rs:784`) |

Consequences, at TM5 defaults (`Time (%)` 100 / 100):

```
  TM5:  dLoopCounter = 100*100/2000 = 5,  WRC = 4
        -> 4 x (1 write + 5 reads) = 24 passes per chunk, concentrated
  TMR:  effective_cycles = 1, wrc/test_reps/verify_reps all 1
        ->  1 write + 1 read      =  2 passes per chunk
        => 12x less work, and no dwell concentration at all
```

And on `Check_absolutnew.cfg` Test1 (block 4 MB, `Time (%)` 240, global 1250):

```
  TM5:  dLoopCounter = 240*1250/2000 = 150  ->  4 x (1+150) = 604 passes on ONE 4 MB chunk
  TMR:  effective_cycles = ceil(2.4 x 12.5) = 30 -> 30 full-window passes, 2 each
        => ~10x less traffic AND the cache-residency sweep is destroyed, because every
           test in the sweep now streams the whole window instead of dwelling on a chunk
```

**Fix (the whole answer to your question):**

```rust
// TM5: dLoopCounter = (test_time% * global_time%) / ST_Div_Down(2000), min 1
let loops = ((test.time_percent as u64 * global_time_percent as u64) / 2000).max(1) as u32;
verify_reps:        Some(loops),
write_read_cycles:  Some(4),      // ST_WriteReadCycles
cycles:             Some(self.main_section.cycles),   // TM5 [Main Section] Cycles
```

`Cycles` is a separate TM5 key that already means "repeat the sequence" — that's what should map
to `cycles`. Right now it's parsed (`config.rs:1378`) and stashed in `legacy_metadata` but
**never used for execution**.

So you don't need new tunables for TM5 fidelity. What you *should* add is what you asked for —
a **global dwell multiplier** over the per-chunk knobs, so a user can say "same config, 3× the
dwell" without editing every test:

```
  dwell=2.5            scales verify_reps (and optionally test_reps) for every test
  verify-reps=N        global default, per-test overridable
  write-read-cycles=N  global default, per-test overridable
```

One caveat to check before trusting this everywhere: the nesting above is **Tier 1**
(`run_phased_test`). Tier-2 `TestRunner` tests own their own loops, and whether each honours
`write_read_cycles`/`verify_reps` is per-test. Worth an audit — an unhonoured dwell knob is the
same class of silent-lossiness as the memset trap.

---

## Q5. `BUG` — Does the compat layer handle the `Test Block Size (Mb)` discontinuity?

**No.** `config.rs:1455-1459`:

```rust
chunk: Some(if test.test_chunk_size_mb == 0 {
    ChunkSpec::fraction(1.0)                                   // correct for 0
} else {
    ChunkSpec::absolute(&format!("{}MB", test.test_chunk_size_mb))   // WRONG for 1,2,3
}),
```

`0` happens to be right (TM5's `window/(0+1)` = whole window = fraction 1.0). `1`, `2` and `3`
are read as megabytes when TM5 reads them as fraction codes:

| cfg value | TM5 | TMR today | error |
|---|---|---|---|
| `0` | whole window | fraction 1.0 | ✅ |
| `1` | window / 2 = 440 MB @880 MB | 1 MB | **440×** |
| `2` | window / 3 = 293 MB | 2 MB | **147×** |
| `3` | window / 4 = 220 MB | 3 MB | **73×** |
| `≥4` | N MB | N MB | ✅ |

`1usmus_v3.cfg` uses `1` (Test6) and `2` (Test7), so this fires on one of the two most popular
community configs in existence. Fix:

```rust
chunk: Some(match test.test_chunk_size_mb {
    // TM5 mt_ini.asm:287-303 — values <= 3 stay raw and the runtime treats
    // anything <= 15 as a fraction code: size = window / (N+1).
    n @ 0..=3 => ChunkSpec::fraction(1.0 / (n + 1) as f64),
    n         => ChunkSpec::absolute(&format!("{}MB", n)),
}),
```

Note this interacts with Q1: TM5's fraction path also floors to `Lock Memory Granularity`, and
its absolute path floors to 4 KB with no granularity rounding. If you want bit-fidelity, mirror
both; if you want sane behaviour, floor everything to the tile/chunk floor from Q1 and log the
adjustment.

---

## Q6. Run-to-run reproducibility — agreed, don't chase it

Accepted, and it's the right call. Fighting the OS's free-memory variance would mean fixing the
allocation to a size that always succeeds, which costs coverage — the opposite trade to the one
you want.

What this does to my §1 argument: **argument (a) is withdrawn.** The tile case now rests on (b)
and (c), which are the two you said you cared about:

- **(b) `prev_power_of_two` silently discards up to ~50% of a partial block's coverage**
  (`tests.rs:1433`). This is a pure loss, unrelated to OS variance, and it exists *only* because
  the geometry is ragged. Uniform tiles delete the rounding entirely.
- **(c) Every fixed-geometry kernel pays a raggedness tax forever.** This is your "minimise test
  overhead, keep tests simple" requirement, stated as a cost.

Worth keeping even without run-to-run reproducibility: **within-run** tile identity still gives
you a stable coordinate system, so "errors cluster in tiles 9 and 10" becomes a usable signal
where "errors in the 512 MB block" is not. That's free once tiles exist.

---

## Q7. WITHDRAWN — "rotate the pattern within the line"

Your objection is correct and I'm dropping the recommendation. Let me first explain what I meant,
since you asked, then say why you're right.

**What it meant.** A DDR5 subchannel is 32 bits wide with BL16, so one 64 B access is 16 beats
of 4 bytes. A byte at offset `j` within the line goes out on beat `j/4`, on the DQ pins covering
`(j%4)*8 .. +8`. TMR writes a repeating 8-byte constant, so byte offset `j` always carries
pattern byte `j%8`:

```
  pattern P = [p0 p1 p2 p3 p4 p5 p6 p7] repeated 8x across the 64 B line

  byte offset  0  1  2  3 | 4  5  6  7 | 8  9 10 11 | ...
  pattern byte p0 p1 p2 p3| p4 p5 p6 p7| p0 p1 p2 p3| ...
  beat          0         |     1      |     2      | ...
  DQ nibble    A  B  C  D | A  B  C  D | A  B  C  D | ...
                ^ p0 ALWAYS goes out on DQ group A. Fixed map, forever.

  Rotating P by k bytes moves every pattern byte to a different DQ group.
```

So the idea was: cover pin-specific marginality that a fixed value→pin map never touches.

**Why you're right to reject it.** Three reasons, and the third is decisive:

1. It becomes part of **pattern identity**. Pattern-aware dependent tests (`Bench-Verify`, and
   anything reading a prior test's data) need the exact generator state, so `k` has to enter
   `ActivePattern`/`PatternState` — the TODO #13 work. Real complexity for the whole dependent-test
   architecture, exactly as you said.
2. If `k` isn't restored, a following pattern-agnostic test (`MirrorMove`) is fine, but any
   verify against a regenerated pattern mismatches — a false error, which is the worst failure
   mode a memory tester can have.
3. **The fault model barely justifies it.** `Mem-StuckBit` already writes a pattern and its exact
   complement, so **every pin already sees both a 0 and a 1**. What rotation adds is only
   variation in the *transition sequence* on a given pin — a much weaker and more speculative
   fault model than stuck-at. Paying (1) and risking (2) for that is a bad trade.

Verdict: **don't do it.** If per-pin coverage ever becomes a real target, the cheap version is to
add one or two extra whole-line constants that are rotations of each other and run them as
ordinary independent patterns — no state, no dependency implications.

---

## Q8. Split-line stress — agreed, and no, striding is not the same thing

You already have stride tests, but **striding and splitting are different phenomena**, and TMR
currently has zero of the second:

```
  STRIDING (what Mem-Stride / Mem-CacheBust do today)
    N separate accesses, each entirely inside one line, spaced S apart
    line: [==== access ====]                 [==== access ====]
    cost: N line fills for N accesses. 1:1.

  SPLIT-LINE (what TMR has none of)
    ONE access straddling a line boundary
    line A:            [-- acc --|          line B:  -- acc --]
    cost: 2 line fills, 2 tag lookups, 2 (possibly different) rows/banks
          for ONE instruction. Store buffer cannot coalesce. Load cannot
          be forwarded from a single line.
```

The split case puts **two DRAM accesses in flight per instruction in a fixed phase
relationship** — a traffic shape striding cannot produce.

### `allow_misaligned` exists but does not do this

There's a knob named for it (`tests.rs:708`, `config.rs:468`) and `Mem-Random` sets it `true`
with the comment `// Maximum stress`. It does not deliver:

- It only controls whether the chunk **length** is rounded to the cache line
  (`tests.rs:1125-1159`). It never offsets a base pointer.
- Even the length effect is undone downstream: `calculate_ideal_chunk_size` (`tests.rs:1384-1390`)
  converts to `u64` elements and calls `.next_power_of_two()`, so the chunk becomes `2^k × 8`
  bytes — a multiple of 64 for any `k ≥ 3`. Alignment is restored.

So `Mem-Random`'s "maximum stress" misalignment is inert. That's a silently-lossy knob of the
same family as the memset trap: it reports a capability it doesn't have. Either implement it or
rename it.

### Agreed on page splits, with one refinement

You're right that page crossing is more OS-flavoured. But note the distinction:

- A split across a **4 KiB boundary inside a 2 MiB large page** is *not* a paging event at all —
  same PTE, same physical page. It's a pure cache/tag effect and historically the most expensive
  split class on Intel. **Free to include, worth including.**
- A split across a **2 MiB/1 GiB page boundary** is a genuine second translation. Skip it; you're
  right that it measures the OS.

So: split-line always; 4 KiB-boundary splits yes (they're inside a large page); real page
crossings no.

Recommended shape: `Mem-SplitLine`, base offset `+8` bytes (or a configurable non-multiple of
64), everything else identical to an existing linear test so the two are directly comparable. And
it must be a **separate test, not a global mode** — split accesses run measurably slower, so
turning it on globally would silently reduce total coverage per unit time.

---

## Q9. Chunk-boundary phase — yes, that was the same idea

Correct, that's the same suggestion from the earlier "merge odd-shaped blocks into one address
space" discussion, and it lands naturally on tiles. With uniform tiles it's one line:

```
  tile i's test span starts at  tile_base(i) + (i * salt) % 64
                                              ^^^^^^^^^^^^^^^
  every tile gets a different phase relative to the 64 B line and the 4 KiB boundary,
  deterministically, from a single logged `salt` value
```

Honest caveat, unchanged from before: I have no strong prior that boundary *phase* matters as
distinct from the split-line effect in Q8. Q8 is the one with a mechanism behind it. Treat Q9 as
a nearly-free rider on Q8's implementation (same offset plumbing, different offset choice) rather
than as independently motivated work.

---

## Q10. `BUG` — you're right about `flush_range_to_dram`

My claim that the flush path is "unaffected either way" was wrong. The instruction has no
alignment requirement — that part is true — but **this function under-flushes on a misaligned
base**, which is the thing that matters. `tests.rs:1350-1360`:

```rust
pub unsafe fn flush_range_to_dram(base: *const u8, len_bytes: usize, cache_line_bytes: usize) {
    let line = cache_line_bytes.max(1);
    let line_count = len_bytes.div_ceil(line);     // <-- ignores base's offset within its line
    for i in 0..line_count {
        let addr = unsafe { base.add(i * line) };
        unsafe { std::arch::x86_64::_mm_clflushopt(addr); }
    }
    unsafe { std::arch::x86_64::_mm_mfence(); }
}
```

Worked example — `base = X + 8` (X 64-aligned), `len = 128`:

```
  range covered:  [X+8, X+136)   spans THREE lines:
      line 0 [X,     X+64)    <- flushed (addr X+8)
      line 1 [X+64,  X+128)   <- flushed (addr X+72)
      line 2 [X+128, X+192)   <- NOT FLUSHED.  div_ceil(128,64) = 2, loop ends.
```

The failure mode is the dangerous one: `Mem-Refresh` flushes, sleeps 64 ms, verifies. Bytes in
the missed tail line are still **cached**, so the verify reads them from L2/L3 and they trivially
match — the refresh test silently passes on data that never went to DRAM. That is the exact bug
class TODO #26 was opened for, reintroduced through the tail line.

It is latent today (bases are 64 B-aligned in practice), which is why it hasn't bitten. It
becomes live the moment Q8/Q9 offsets exist. Fix:

```rust
pub unsafe fn flush_range_to_dram(base: *const u8, len_bytes: usize, cache_line_bytes: usize) {
    let line = cache_line_bytes.max(1);
    // Count from the START OF BASE'S OWN LINE, so a misaligned base still covers its tail line.
    let offset_in_line = (base as usize) & (line - 1);          // line is a power of two
    let line_count = (offset_in_line + len_bytes).div_ceil(line);
    let first = ((base as usize) - offset_in_line) as *const u8;
    for i in 0..line_count {
        unsafe { std::arch::x86_64::_mm_clflushopt(first.add(i * line)); }
    }
    unsafe { std::arch::x86_64::_mm_mfence(); }
}
```

Worth doing now regardless of whether salting ever ships — it's correct-in-all-cases for the same
instruction count in the aligned case, and it removes a trap for whoever next passes an offset
pointer. (`line` is a power of two on every x86 CPU; assert it rather than assuming.)

---

## Q11. Should we reshape what "window" means in TMR? — Yes

This is the most valuable question in the set, and the answer follows from Q1/Q2.

**Your history is right, and the conclusion is stronger than you put it.** TM5's window was an
AWE addressability artifact. The *coverage-limiting* job you remember it having was actually done
by **how much memory each core locked** (`dMemForCore`), and the *cache-residency* job was done by
**`Test Block Size`** — the chunk. The window did neither. It just happened to sweep.

TMR inherited the name and gave it two jobs TM5 never gave it:

```
  TMR WindowMode today
    (1) coverage scope       - how much of the allocation this test visits      <- legitimate
    (2) cache-tier targeting - WindowMode::Cache { "L3*2" }, thread-divided     <- wrong stage
    (3) position             - absent                                           <- the coverage hole
```

**(2) is on the wrong stage, and that's a real conceptual bug, not a naming quibble.** Q2
established that all repetitions happen *inside one chunk*. So the resident working set during
the dwell is **the chunk**, not the window. Setting `window = L3*2` does not make the working set
L3*2 — if the chunk is 1 MB, the hot set is 1 MB and everything stays in L2, regardless of the
window. Cache-tier targeting only does what its name says when applied to the chunk. TMR has
`ChunkMode::Cache` too, so today the same intent is expressible at two stages, one of which
cannot deliver it.

### Proposal: three stages, three orthogonal jobs

```
  ┌──────────────────────────────────────────────────────────────────────────┐
  │ ALLOCATION   what we own            (unchanged; opportunistic, page-first)│
  ├──────────────────────────────────────────────────────────────────────────┤
  │ COVERAGE     WHICH tiles, and does it MOVE          <- was "window"      │
  │   scope:   all | tiles(N) | bytes(X) | fraction(f)                       │
  │   advance: none | sequential | strided(K)                                │
  │            ^ sequential == TM5's sliding window, restored                │
  ├──────────────────────────────────────────────────────────────────────────┤
  │ CHUNK        the HOT WORKING SET                    <- owns cache tiers  │
  │   cache(L3*2) | absolute(32MB) | fraction(f) | auto                      │
  │   floored at chunk_floor from Q1                                         │
  ├──────────────────────────────────────────────────────────────────────────┤
  │ DWELL        HOW LONG on each chunk                 <- Q4's knobs        │
  │   write_read_cycles x (test_reps -> [flush] -> verify_reps)              │
  └──────────────────────────────────────────────────────────────────────────┘
```

Each stage answers exactly one question — *which memory / how much is hot / how long*. The two
things you can't currently express both become natural:

- *"small hot working set AND full coverage"* — TM5's actual behaviour:
  `coverage: all, advance: sequential` + `chunk: cache(L3*2)`.
- *"deliberately retest the same addresses"* (a genuine retention test):
  `coverage: tiles(2), advance: none`.

Migration is mechanical and cheap: `WindowMode::Cache{t}` → `coverage: all` +
`chunk: cache(t)`; `WindowMode::Absolute{x}` → `coverage: bytes(x)`;
`WindowMode::FullAllocation` → `coverage: all`. Keep the old JSON keys as deprecated aliases so
existing configs load, and log the translation.

I'd keep the word "window" only if it means coverage+position. If it keeps meaning three things,
rename it `coverage` — the confusion in (2) is costing real test validity, not just clarity.

---

## Q12. Uniform tiles as a flat address space — yes, with one hard constraint

Yes, this is the earlier idea. The constraint that decides the design: **it must be an index
mapping, never a VA mapping, and it must never be consulted inside a hot loop.**

- *Never VA:* CLAUDE.md settles "no VA stitching" — `MEM_RESERVE_PLACEHOLDER` coalescing gives
  one flat pointer but adjacent VA lands at unrelated PA, so a strided walk hits a random
  row/bank at every seam. A tile index space gives the *ergonomics* of a flat space with none of
  that, because it never claims tiles are adjacent.
- *Never in the loop:* "no request-routing through workers" is also settled. A per-access lookup
  would be exactly that.

So resolve at setup, hand the kernel a plain pointer and length:

```
  SETUP (once per chunk, outside every loop — a few hundred ns, amortised over MB of traffic)

     TileSpace { tiles: Vec<Tile> }      Tile { ptr, len, page_type, block_id, index }
            │
            │  coverage/advance pick tile indices for this cycle
            ▼
     for tile in coverage.tiles_for_cycle(cycle) {
         for chunk in tile.chunks(chunk_size) {
             kernel(chunk.ptr, chunk.len);     // <-- plain (ptr, len). No lookup. No indirection.
         }
     }
```

The kernel signature does not change, and it gets *stronger* guarantees than today:
`len` is always exactly `chunk_size` (never a ragged tail), always a power of two, always
64 B-aligned. That is precisely the "simpler, faster kernels" you're after — every kernel can
today be forced to handle a short final chunk; with uniform tiles that branch disappears.

Two properties to keep honest:

- Tiles are uniform in **VA**. Two tiles inside one 1 GiB page are physically contiguous with
  each other; two tiles from different allocations are not. Tiles are a test coordinate system,
  not a DRAM one.
- Blocks smaller than the tile floor (Phase-3 4 KiB mop-up) should be flagged **sub-tile** and
  excluded from uniform-geometry kernels rather than dragging the floor down for everyone.

---

## Q13. Is per-chunk dwell a durable, reusable concept — or only for tests that need repeated hits?

**Durable, and it's already the majority convention.** Counted in `tests.rs`: **28
`run_phased_test` call sites (Tier 1, has the reps) vs 8 `TestRunner::new` (Tier 2, owns its own
loop).** So this isn't a one-test special case — it's the dominant path that happens to be
under-documented and under-wired.

But the framing needs to change to make it durable. "Tests that need repeated hits in quick
succession" undersells it. The three knobs are not one concept, they're three, and each is
meaningful for *any* test with a write phase and a verify phase:

| Knob | What it repeats | Fault class it targets |
|---|---|---|
| `write_read_cycles` | write **and** verify, whole cycle | marginal cells that only fail on some write attempts; write-path timing margin |
| `test_reps` | the test operation only, no verify between | bus/bank/row stress accumulated *between* checks (MirrorMove round trips) |
| `verify_reps` | reads only, against one written pattern | retention, read-disturb, and whether the value survives repeated sensing |

Those are genuinely different stressors, which is why TM5 has two of the three as separate
numbers rather than one "repeat count". They apply to every correctness test. Two exceptions to
document rather than paper over:

- **`Mem-Refresh`** has an intrinsic time element (64 ms sleep). `verify_reps` there means
  "re-verify after the same single sleep", which is a weaker test than "sleep again" — the second
  read is nearly free. Worth an explicit per-test note.
- **Bandwidth/latency tests** — reps are sample count for statistics, not stress. Different
  meaning, same field. Keep them separate or name them differently.

**Recommendation:** make the three knobs part of the harness *contract* rather than a Tier-1
feature — i.e. `TestRunner` should expose and honour them too, so a Tier-2 test that ignores them
is a visible omission rather than an invisible one. Then add the global multiplier you asked for.

One framing worth surfacing in the UI, because it's the real tension: **dwell and coverage are
directly opposed at fixed runtime.** Reps multiply time linearly and add *zero* new addresses;
coverage adds addresses and reduces per-address confidence. So a global `dwell=` knob is
really a sensitivity-vs-coverage dial, and users should be told that, not left to discover that
`dwell=4` quartered their coverage per hour.

---

## Q14. TMR blocks are fixed-size from the allocator — how does that change the tile story?

Exactly right, and it's the sharpest argument for tiles. Now written up as **Part 6 of
`memory_allocation_models.md`**. The short version:

```
  TM5:  pool = a bag of loose 4 KB physical pages. Any page → any window offset,
        for ~35 ms/GB. Reshaping the addressable region between tests = a syscall.

  TMR:  block = a fixed size at a fixed VA, whose large/huge pages exist ONLY
        because that VA is size-aligned. Reshaping = free + realloc: seconds,
        and it risks not getting the same page size back on a fragmented system.
        So block geometry is frozen for the whole run.
```

So TM5's per-test flexibility is structurally unavailable to TMR — AWE on Win64 is 4 KB-only, so
using it would trade away every large page (see Q16).

**But TMR doesn't need to reshape memory, only its description of it.** A tile is a descriptor
(`ptr`, `len`, `page_type`, `index`) produced by pointer arithmetic over an existing mapping.
Varying tile size per test costs nothing — no syscall, no realloc, no page-size risk:

```
  ┌──────── block: 1 GiB, huge pages, fixed VA (frozen for the run) ────────┐
  │  test A, Q = 256 MiB:   [  T0  ][  T1  ][  T2  ][  T3  ]                │
  │  test B, Q =  64 MiB:   [][][][][][][][][][][][][][][][]                │
  │  test C, Q = 512 MiB:   [      T0      ][      T1      ]                │
  └─────────────────────────────────────────────────────────────────────────┘
```

Which makes TMR **more** flexible than TM5 here, not less: TM5 paid ~35 ms/GB and could only vary
its view in window-sized slices; TMR pays setup arithmetic and can vary size, order, and start
offset independently per test. The one constraint TM5 didn't have: a tile can never span two
blocks, because there's no remap to stitch them and VA stitching is ruled out (adjacent VA is
unrelated PA).

---

## Q15. Cycle-flow detail in the markdown

Added as **Part 5 of `memory_allocation_models.md`**: the size hierarchy for both tools side by
side, TM5's verified loop nest, TMR's Tier-1 nest, and a knob→level table with a locality column.
It makes the two findings visible at a glance — TMR has no *window slice* row (the missing slide),
and TM5's `Time (%)` belongs on the bottom rows while TMR's loader puts it on the top one.

---

## Q16. Could TMR-MD shuffle blocks to change the VA→PA mapping?

Technically yes; **it wouldn't tell you anything new, and you don't need a driver to find that
out.** Three layers to this.

**First — you don't need TMR-MD.** AWE (`AllocateUserPhysicalPages` / `MapUserPhysicalPages`)
works on 64-bit Windows from user mode. TMR could do TM5-style physical remapping today. The
catch: **AWE allocates in the system page size — 4 KB only.** There's no large-page AWE. So this
path trades away every 2 MiB/1 GiB page, which is self-defeating for the reason in the next
paragraph.

**Second — you can only shuffle at page granularity, and cross-page relationships are already
arbitrary.** Within a page, physical order is fixed by hardware; you can only permute whole
pages. So:

```
  With 4 KB AWE pages:
     every 4 KB boundary is ALREADY a PA discontinuity — the map is already
     effectively random. Shuffling a random map yields a different random map.
     Information gained: none.

  With a driver handing out 2 MiB / 1 GiB contiguous chunks:
     you may permute 2 MiB-granular blocks. But a stride *inside* a 2 MiB page is
     unaffected by the permutation, and any relationship *across* pages was already
     physically arbitrary. Information gained: none.
```

The general form: **shuffling PA under a fixed VA pattern is equivalent to varying the VA pattern
over fixed PA** — and the second is free. Since TMR deliberately doesn't know the PA map, "some
different arrangement" is all either approach can achieve; neither can achieve "specifically
adjacent" or "specifically same bank", which is what would actually be worth having.

**Third — there *is* one genuinely novel capability, and it fails on scope, not on merit.**
Remapping would let you answer: *does a failure follow the physical page or the virtual address?*
Find an error at VA X, reshuffle, re-test — if it reappears at the VA now backed by the same
physical page, it's a cell fault; if it stays at VA X, it's in the address/translation path. VA
pattern variation genuinely cannot give you that. But that is **diagnostics** — "which cell is
bad" — and the settled charter is stable/not-stable, not bank/row/cell attribution. Same
conclusion and the same reasoning family as the settled *no mirrored/double-mapped VM* decision:
a clever loophole around the no-PFN rule that doesn't earn a test.

**And the part of TM5's behaviour that was actually useful is already free.** TM5's real benefit
wasn't the shuffle — it was *"same kernel, same VA, same alignment, different memory underneath"*,
which keeps TLB and alignment behaviour constant while coverage advances. TMR gets the equivalent
from `coverage: advance: sequential` over tiles: identical kernel, identical alignment, different
tiles. No driver, no page-size loss.

**Verdict: not a TMR-MD revival trigger.** The revival trigger stays what it is — rowhammer,
where physical-address visibility is genuinely mandatory.

---

## Q17. Coverage sequencing beyond what AWE could do — agreed

Yes, and worth stating what the separation actually buys, since it's more than parity with TM5.
Because `coverage` operates on tile *indices* rather than on a remappable aperture, all of these
are the same cost (an index sequence, computed at setup):

| Sequencing | AWE could do it? |
|---|---|
| Sequential advance (TM5's slide) | yes |
| Reverse / strided advance (`advance: strided(K)`) | no — slices were always sequential |
| Random permutation with a logged seed (reproducible) | no |
| Overlapping windows (tile N shared between consecutive cycles) | no — slices were disjoint |
| Per-cycle tile subsets (test 1/4 of tiles, rotating) | no |
| Cross-thread tile exchange (thread A writes, B verifies) | no — pools were per-process |

The last row is the one that isn't just a nicety: it's TODO #19, and it's stressapptest's core
mechanism. It only exists if tiles are interchangeable tokens.

---

## Q18. `BUG` — Do sub-tile-size blocks get left behind at high thread counts?

**Yes, and it's already happening today — worse than you described. TMR can allocate 1 GiB huge
pages and then discard them.** This is a real bug, independent of tiles. Traced end to end:

```
  Trigger: per-thread target lands in [512 MiB, 1 GiB)
  e.g. 32 GiB system, memory=80%, 16c/32t  ->  25.6 GiB / 32 = 819 MiB per thread

  1. create_allocation_plan (allocator.rs:491-504)
     gate is `if remaining_per_thread >= block_size`, evaluated PER THREAD
     819 MiB  ->  plan = [(512 MiB, 32), (256 MiB, 32), (32 MiB, 32)]
     1024 MiB is NOT in the plan, because no single thread can hold one.

  2. Phase 1b (allocator.rs:581-652)
     all_additional_sizes_mb = [1024], filtered by !plan_sizes_mb.contains(1024)
     1024 is absent from the plan, so it IS tried — against the NODE-WIDE deficit.
     => 1 GiB huge-page chunks get successfully allocated.   (slow, and they succeed)

  3. distribute_planned_chunks (allocator.rs:1062)
     `for &(block_size, total_blocks) in plan`  — iterates THE PLAN.
     1 GiB is not in the plan, so these chunks are never distributed here.

  4. off-plan gap filling (allocator.rs:1141-1154)
     takes a chunk only `if filled + chunk.chunk_size <= gap`,
     where gap = target - current_total  <=  819 MiB.
     1 GiB > 819 MiB  ->  the condition can NEVER be true.

  5. allocator.rs:1180
     log::warn!("Still have {} unallocated chunks after gap filling")
     => the 1 GiB huge pages are dropped on the floor.
```

So the huge pages that `plan-pagesize-pref` exists to obtain are exactly the ones thrown away,
on precisely the consumer configurations you named. There's a second path to the same outcome at
`allocator.rs:1065`: if fewer blocks of a planned size arrive than there are threads,
`blocks_per_thread == 0 → continue` skips the size entirely and those chunks also fall through to
gap filling.

Not a leak — the buffers drop at scope exit — but it is wasted allocation time, understated
capture, and a defeated page-size strategy. Testable right now: run at a per-thread target in
`[512 MiB, 1 GiB)` and grep the log for `Still have N unallocated chunks`.

### Answering the tile worry directly: tiles are the fix, not the cause

**Nothing gets left behind because of tiles**, provided two rules:

**Rule 1 — a floor may never discard memory.**

```
  Q = min( rule_value_from_cache_topology, smallest block at-or-above that value )

  blocks below Q  ->  flagged `sub-tile`
                      still tested by ordinary linear kernels
                      excluded ONLY from kernels that assume uniform geometry
```

Small blocks lose *eligibility for some kernels*, never their coverage. That is the opposite of
today, where a too-large block loses coverage entirely.

**Rule 2 — decouple huge-page acquisition from per-thread ownership.** This is what fixes the bug
above, and it's the same decoupling as tiles:

```
  today:  per-thread budget  ->  plan  ->  allocate  ->  whole block owned by ONE thread
          819 MiB/thread means 1 GiB can never be owned  =>  stranded

  with tiles: allocate 1 GiB huge chunk per NUMA node (Phase 1b already does this)
              carve into 4 x 256 MiB tiles
              hand tiles to 4 DIFFERENT threads
          =>  1 GiB huge pages become reachable AND usable at any thread count
```

So the tile layer doesn't just simplify kernels — it **unlocks huge pages that the current
per-thread-first allocation structurally cannot use.** That's a stronger justification than the
kernel-simplicity argument I led with.

### On chunk larger than block

Nothing crashes: `calculate_window_size` already does `.min(allocated_size)` and chunk is capped
at window. But the clamp is **silent**, and that matters given Q1: if the cache-derived chunk
floor exceeds a block, that block *cannot* defeat cache naturally, and the test passes for the
wrong reason. So clamp, then **report it** — and either auto-enable `flush_before_verify` for that
portion or mark that coverage as cache-suspect in the results. A silently clamped chunk is the
same failure family as B3: a test that reads from cache and calls it a pass.

---

## Q19. `BUG` — The clamp changes test locality and doesn't mirror TM5

You're right, and I understated it. It isn't "silently clamped but harmless" — **chunk size varies
per block inside a single test on a single thread**, so one named test runs at several different
localities at once. Verified:

```
  tests.rs:1433-1478   prepare_blocks_for_window()
      window is a per-thread BYTE BUDGET, filled greedily largest-block-first.
      each block gets test_size = full block if it fits the remaining budget,
                                  else prev_power_of_two(remaining)

  test_scaffolding.rs:104 / test_harness.rs:194
      chunk_size_bytes(tb.test_size)   <-- PER BLOCK, from that block's test_size
      tests.rs:1175  raw_chunk_size.min(window_size)
```

Because chunk is derived from each block's own `test_size`, a heterogeneous block set produces a
heterogeneous chunk set. Worked example — thread owns `[512 MB, 256 MB, 32 MB]`, config asks for
`ChunkMode::Absolute { 2048 MB }` (which is what `runner.rs:1901` actually sets), window = full:

```
  block 0: 512 MB   chunk = min(2048, 512) = 512 MB  -> 1 chunk, firmly DRAM-resident
  block 1: 256 MB   chunk = min(2048, 256) = 256 MB  -> 1 chunk, DRAM
  block 2:  32 MB   chunk = min(2048,  32) =  32 MB  -> 1 chunk, PARTLY L3-RESIDENT on X3D

  same test, same thread, same cycle: three different chunk sizes, 16x apart,
  and the dwell reps apply per chunk -> block 2's lines get re-touched 16x more
  densely than block 0's, over a working set small enough to sit in cache.
```

Two further consequences:

- **The chunk knob is inert wherever it exceeds the block.** Every `2048 * MB` chunk setting in
  `runner.rs` (lines 1901–1978) clamps on any consumer per-thread budget, so those tests' real
  chunk size is not "2 GB" — it is *whatever shape the allocator happened to produce*. The config
  says nothing about what runs.
- **`prev_power_of_two(remaining)`** (`tests.rs:1459`) discards budget on the partial block: a
  512 MB block against 300 MB remaining contributes 256 MB and drops 44 MB.

**And TM5 structurally could not do this.** TM5's window was a fixed-size *aperture* — same size
every slice, same size in every process — so `Test Block Size (Mb)` produced one uniform chunk
size everywhere. Uniform window ⇒ uniform chunk ⇒ uniform locality. TMR's window is a *budget*
spread over unequal blocks, which is where the uniformity is lost. So this is not a TM5-fidelity
detail; it's the reason a TM5 config's cache-residency intent cannot survive the trip.

All of the reporting for this is `log::debug!` (`tests.rs:1189`, `1470`, `1493`) — invisible at
default log level.

**Fix is the tile layer** (Q20): uniform tiles make chunk uniform by construction, because the
unit handed to the kernel no longer inherits the block's size.

---

## Q20. Confirming the tile construct, and your reconstitution math

Your model is exactly right, and it's the right shape:

```
  1. ALLOCATE greedily, largest blocks + best page size.
     Goal: fewest allocations, most huge/large pages. Shape is whatever the OS gives.
  2. GRANULARISE with a virtual overlay into uniform segments.
     Goal: every unit handed to a kernel is the same size. Pure arithmetic, no syscall.
```

Your worked example checks out. `1 x 4 GB HUGE + 1 x 1 GB HUGE + 1 x 512 MB LARGE` = 5632 MB:

| Q | from 4 GB | from 1 GB | from 512 MB | tiles | left as sub-tile |
|---|---|---|---|---|---|
| 1 GB | 4 | 1 | 0 | **5** | 512 MB |
| 512 MB | 8 | 2 | 1 | **11** | 0 |
| 256 MB | 16 | 4 | 2 | **22** | 0 |
| 128 MB | 32 | 8 | 4 | **44** | 0 |
| 64 MB | 64 | 16 | 8 | **88** | 0 |

The rule your example demonstrates: **waste is zero iff `Q <= smallest block` and `Q` divides
every block.** With power-of-two block sizes the first condition implies the second, which is
exactly why the sizing rule carries `min(smallest_block_this_thread_owns, ...)`. Your 1 GB row is
the one case that strands something, and it strands it as a *sub-tile* (still tested by linear
kernels), not as dropped memory.

### The caveat you didn't raise: uniform size ≠ uniform page geometry

At Q = 256 MB the 22 tiles are **not interchangeable**:

```
  20 tiles carved from HUGE (1 GiB) blocks:
        each 256 MB tile sits INSIDE one 1 GiB page
        -> VA-contiguous implies PA-contiguous across the whole tile
        -> a 4 MB stride stays in one physical page

   2 tiles carved from the LARGE (2 MiB) block:
        each 256 MB tile spans 128 INDEPENDENT 2 MiB pages
        -> any stride > 2 MiB lands on an unrelated physical page
```

Same size, different substrate. So a tile descriptor must carry `page_type`, and any kernel whose
stride exceeds 2 MiB must filter on it — otherwise the "uniform tile" abstraction quietly lies
about what it's testing, which is the same failure family as the memset trap. Uniform tiles buy
uniform *geometry arithmetic*; they cannot buy uniform *physical contiguity*.

---

## Q21. Pattern gen under re-tiling — the right question, and TMR is already safe

This is the sharpest thing you've raised, because it's the one that could make the whole tile idea
expensive. The answer depends on which coordinate the pattern is a function of. Three classes:

| Class | Form | Survives Q change? | Survives tile shuffle? |
|---|---|---|---|
| **1. Container-absolute** | `P(block_base, offset_in_block)` | **yes** | **yes** |
| 2. Unit-relative | `P(offset_in_tile)` | no | no |
| 3. Sequential / stateful | `P_n = f(P_{n-1})`, seeded per unit | no | no |

Class 1 is immune because the value at a byte depends on *where that byte lives*, and re-tiling
doesn't move any byte — it only changes which descriptor points at it. Classes 2 and 3 make the
value depend on the unit's own boundaries, so moving a boundary changes every expected value and
forces a full regen.

**This is exactly why TM5 could slide its window and remap physical pages with no regen.**
`ST_GeneratePattern` seeds from the address (`(addr>>12)+(addr>>28)`, complement on odd 4 KB
pages) — class 1, and deliberately so.

**Verified: TMR is already class 1.**

```
  test_harness.rs:25-31   ChunkCtx { ptr, chunk_start, chunk_end, .. }
        ptr         = start of the BLOCK  ("Pointer to the start of this block's memory")
        chunk_start = element index WITHIN THE BLOCK

  tests.rs:3067-3069
        let seed = block_seed(ctx.ptr as usize, ctx.thread_id, ctx.cycle);   // block base
        *ctx.ptr.add(idx) = pattern_mode0(idx as u64, seed);                 // block-relative idx
```

So chunk size and chunk visit order are *already* free in TMR — re-chunking cannot change an
expected value. Re-tiling inherits that property for free. **No regen needed.** That's a good
existing design decision that the tile layer just has to avoid breaking.

### But it constrains the descriptor — and corrects my Q12 recommendation

In Q12 I said kernels should keep `(ptr, len)`. **That's wrong for pattern-aware tests.** If a
kernel receives only a raw pointer into the middle of a block and does `block_seed(ptr)`, it hashes
a *different* base than the block base, so the pattern becomes tile-relative — class 2, and the
immunity is lost. The descriptor must be:

```rust
struct Tile {
    block_base: *mut u8,   // pattern coordinate origin — NOT the tile's own start
    offset:     usize,     // byte offset within the block; pattern index origin
    len:        usize,     // == Q, or < Q for a sub-tile
    page_type:  PageType,  // see Q20 — geometry is not uniform
    owner:      usize,     // see below
}
// kernel: ptr = block_base.add(offset); seed = block_seed(block_base, ..); idx from offset
```

Two bytes more per descriptor, resolved at setup, nothing in the hot loop.

### `thread_id` in the seed blocks cross-thread tile exchange

`block_seed(addr, thread_id, cycle)` (`pattern_gen.rs:50`) mixes in `thread_id`. So if thread B
verifies a tile thread A wrote (TODO #19, and Q17's last row), B computes a different seed and sees
every element as an error. Fix is either: carry `owner` in the descriptor and seed from that, or
drop `thread_id` from the seed entirely — `(block_base, offset)` is already globally unique, so
`thread_id` adds no distinctness. The second is cleaner and removes a field.

`cycle` in the seed is fine and intentional (pattern varies per cycle; init and verify share a
cycle), and the dependent-test path already pins `cycle = 0` for stable seeds
(`tests.rs:4872`, `4878`).

---

## Q22. Should the tile list be a circular buffer with the window starting anywhere?

**Yes — and it's the piece that makes the window rework (Q11) clean rather than fiddly.** Model:

```
  per thread:  T[0 .. N)          uniform tiles, flat index space, order = allocation order
  window    :  (start, count)     count = coverage budget in tiles

      window = { T[(start + k) mod N]  for k in 0..count }

  advance per cycle:   start = (start + count) mod N       <- TM5's slide
                       start = (start + stride) mod N      <- strided
                       start = perm[cycle]                 <- logged-seed permutation
                       start = (start + count/2) mod N     <- overlapping windows
```

What the ring buys over a linear list:

- **TM5's slide, exactly, in one line** — and `mod N` is what the sliding window was emulating
  with `wCurrentPage` in the first place.
- **The partial-tail problem disappears.** TM5 dropped a short final slice (Q3); linear indexing
  would too. Modular arithmetic has no partial slice — it wraps into tile 0. That's a strict
  coverage improvement over TM5, not just parity.
- **Coverage becomes countable.** "Visited tile indices" is a set of small integers, so you can
  actually report *which* fraction of memory a short run touched — the thing Karhu's coverage
  metric does and TMR currently can't state.
- **`prev_power_of_two` dies.** Window is a tile *count*, so there's no non-power-of-two byte
  budget to round down and no 44 MB discarded (Q19).

Cost: one compare-and-subtract per tile, at **tile-handoff granularity — once per tile, never
inside a loop**. Don't force `N` to a power of two to make it an `AND`; that would waste memory to
optimise an operation that happens a few thousand times per run. `if i >= N { i -= N }` is exact
and perfectly predicted.

One structural detail: **keep sub-tile fragments out of the ring.** Otherwise a "window of 8
tiles" sometimes contains a short tile and coverage per cycle stops being constant. Two lists:

```
  ring[0..N)     uniform Q-sized tiles          <- window/coverage/advance operate here
  remainder[..]  fragments below Q              <- swept by a linear kernel, accounted separately
```

Nothing is uncovered, and the ring arithmetic stays uniform.

---

## Section A. The bugs, as actionable items

| # | Where | Effect | Fix |
|---|---|---|---|
| **B1** | `config.rs:1455-1459` | TM5 `Test Block Size` 1/2/3 read as MB, not fraction codes → chunk up to **440× too small**. Fires on `1usmus_v3.cfg`. | `0..=3 => fraction(1/(n+1))` (Q5) |
| **B2** | `config.rs:1439-1447` | TM5 `Time (%)` mapped to outermost `cycles` with a 10000 divisor instead of innermost `verify_reps` with 2000; `write_read_cycles` left at 1 instead of 4. **~12× dwell deficit** at defaults; destroys `Check_absolutnew.cfg`'s residency sweep. `[Main Section] Cycles` parsed but never executed. | map to `verify_reps`/`write_read_cycles`; `cycles` ← TM5 `Cycles` (Q4) |
| **B3** | `tests.rs:1350-1356` | `flush_range_to_dram` ignores the base's offset within its line → misses the tail line on a misaligned base → `Mem-Refresh` verifies **cached** data and passes for the wrong reason. Latent now, live as soon as offsets exist. | count from the base's own line start (Q10) |
| **B4** (fixed 2026-10-02, TODO 75 A) | `allocator.rs:491-504` + `581-652` + `1062-1069` + `1141-1154` | When per-thread target is in **[512 MiB, 1 GiB)**, 1024 is absent from the plan → Phase 1b allocates 1 GiB huge chunks off-plan → distribution walks only the plan → gap filling rejects any chunk larger than the gap → `:1180` warns and **the huge pages are discarded**. Hits 32 GiB/32t consumer systems. Second path: `blocks_per_thread == 0 → continue` at `:1065`. | distribute per NUMA node and let tiles split a chunk across threads (Q18) |

| **B5** | `tests.rs:1433-1478` + `1175` + `test_scaffolding.rs:104` | Window is a per-thread *byte budget* spread greedily over unequal blocks, and chunk is derived **per block** from that block's `test_size` → **chunk size varies per block within one test**, up to 16× apart, so one test runs at several localities at once and a small block can stay cache-resident. Every `2048 * MB` chunk setting in `runner.rs:1901-1978` is inert on consumer budgets. `prev_power_of_two` also discards budget. All reporting is `log::debug!`. | uniform tiles: the kernel's unit stops inheriting the block's size (Q19/Q20) |

Plus one mislabelled knob: **`allow_misaligned` cannot produce a misaligned access** — it only
affects chunk length, and `next_power_of_two` re-aligns it anyway (Q8). Implement or rename.

## Section B. Revised recommendation order

Changed from the previous doc: pattern rotation is **gone** (Q7), the reproducibility argument is
**gone** (Q6), and three bug fixes jump the queue.

1. **B4 first.** It's the only one that loses *memory* rather than intensity, it's cheap to
   confirm (grep the log for `Still have N unallocated chunks`), and every measurement taken on a
   32 GiB/32-thread box is currently understating capture. Fixing it also de-risks item 4, since
   the tile layer depends on chunks being splittable across threads.
2. **B1, B2, B3.** Bugs, not design. B2 in particular means TMR is currently not running TM5
   configs at anything like TM5's intensity, which undercuts every "do we match TM5?" comparison.
   (**B5** is also a bug but its real fix is item 4 — in the meantime promote its three
   `log::debug!` sites to `warn!` so the divergence is at least visible.)
3. **Derive `chunk_floor`/`tile_floor` from cache topology; retire the hardcoded 16 MiB** (Q1).
   Cheap, and it's what makes natural cache defeat reliable instead of hopeful. Floor must be
   **soft** — clamp and report, never discard a block (Q18).
4. **Uniform tile layer** (Q12, Q18, Q19, Q20) — flat index space, setup-time resolution,
   `{block_base, offset, len, page_type, owner}` descriptors (Q21), sub-tile fragments in a
   separate remainder list (Q22), and one chunk splittable across threads. The load-bearing
   change: it fixes **B5** by construction, makes huge pages usable (B4), and deletes
   `prev_power_of_two`. Pattern gen needs **no** regen because TMR is already block-relative
   (Q21) — preserve that when defining the descriptor.
5. **Split window into `coverage` (scope + advance) and move cache targeting to `chunk`** (Q11),
   over a **circular** tile ring with `(start, count)` (Q22), with deprecated aliases.
   `advance: sequential` restores TM5's sliding coverage *without* TM5's dropped tail; the richer
   sequencing in Q17 becomes reachable for free. Drop `thread_id` from `block_seed` here so
   cross-thread exchange (TODO #19) is possible.
6. **Global dwell multiplier** over `verify_reps`/`write_read_cycles`, plus an audit that Tier-2
   tests honour them (Q4, Q13). Surface the dwell-vs-coverage tradeoff in the UI.
7. **`Mem-SplitLine`** as its own test, with the 4 KiB-inside-a-large-page splits included and
   real page crossings excluded (Q8). Q9's per-tile phase rides along on the same plumbing.
8. **AMX: still nothing** — unchanged, and Q1 reinforces it: if 32 MB defeats cache, the case for
   exotic wide access units gets weaker, not stronger.
9. **TMR-MD: still parked** (Q16). VA→PA reshuffling is not a revival trigger; rowhammer remains
   the only one.

---

## Q23. Does a chunk "stop early" at a seam? No — and this is my explanation's fault

> *"If the chunk size of the test is 128MB for example, Maybe it'll stop early at 64Mb as it had to
> wrap or hit a block seam etc."*

**No. The chunk always gets its full size.** The seam splits *how it is delivered*, never *how much*.

```
  C = 128 MB, chunk lands 64 MB before a row boundary

  WHAT DOES NOT HAPPEN                    WHAT HAPPENS
  ────────────────────                    ────────────
  kernel called once with 64 MB           kernel called TWICE
  chunk silently becomes 64 MB              call 1: row0 + offset, len = 64 MB
  remaining 64 MB skipped                   call 2: row1 + 0,      len = 64 MB
                                          total worked = 128 MB. Always.

  segs = resolve(start, 128 MB)     ->  [ Seg{row0, off, 64MB}, Seg{row1, 0, 64MB} ]
  for rep in 0..dwell               <- reps OUTSIDE, so the working set is all 128 MB
      for s in segs
          kernel(s)                 <- each call sees a valid, 64-B-multiple span
```

Two properties make each `kernel(s)` call as safe as today's chunk call:

1. **Every segment length is a multiple of 64 bytes** (Q = 64), so `debug_assert!(_len % $simd_w == 0)`
   at `tests.rs:3579` holds for each segment exactly as it holds for a chunk today. The SIMD loop
   always completes on a whole vector; nothing runs off the end.
2. **A segment never crosses a row**, so `base + offset + len` is always inside one VirtualAlloc2
   region. The pointer arithmetic is the same arithmetic the kernel does today.

And the locality question specifically: **the working set is unchanged.** 128 MB is resident and
being hammered whether it arrives as one span or two. Chunk size exists to control eviction-before-
return, and 128 MB does that identically either way. What the seam changes is *sequential virtual
address continuity within the chunk* — one pointer jump — and nothing else.

> *"the tests currently are not built to XOR for example or mirror between blocks"*

Correct, and that is exactly why those tests do **not** get split spans. See Q24 and
`memory_system_design.md` section 5.1: Mirror / BlockMove / Stride declare `SingleSegment` and keep
today's behaviour. Only position-local kernels (touch `i`, derive from `i`) get split, and for those a
split is invisible — they never look at two places at once.

---

## Q24. The wrap and pattern generation — the sharpest question asked so far

> *"if you wrap then the pattern gen is still a challenge as the same physical memory addressed twice
> can't hold two different patterns"*

**You are right that memory gets visited twice, and right that it cannot hold two patterns. The
reason it is not a problem is that TMR's pattern does not depend on which visit it was.**

Take the worked example, `T = 5632 MB`, `C = 1536 MB`:

```
  chunk 0  [   0 .. 1536)
  chunk 1  [1536 .. 3072)
  chunk 2  [3072 .. 4608)
  chunk 3  [4608 .. 6144)  -> wraps, and its last 512 MB IS row0 + 0
                              which is the SAME MEMORY as chunk 0's first 512 MB

  4 x 1536 = 6144 MB of visiting over 5632 MB of memory -> 512 MB visited twice.
```

Now the seed (`tests.rs:3067`, `pattern_gen.rs:50`):

```rust
let seed = pattern_gen::block_seed(ctx.ptr as usize, ctx.thread_id, ctx.cycle);
*ctx.ptr.add(idx) = pattern_gen::pattern_mode0(idx as u64, seed);
```

The value written at a byte is a pure function of **(row base, offset in row, thread, cycle)**. It
contains no chunk index, no visit counter, no logical position. So:

```
  chunk 0 writes row0+0 .. row0+512MB   with   f(row0_base, offset, tid, cycle)
  chunk 3 writes row0+0 .. row0+512MB   with   f(row0_base, offset, tid, cycle)
                                                        ^^^^ identical inputs
  => identical values. The second write is idempotent. Nothing is corrupted.
```

This is the pay-off of the container-absolute coordinate class from Q21 — it was chosen for immunity
to re-tiling, and it turns out to give wrap-overlap immunity for free.

**Two constraints this imposes, which must be written down:**

- **`C <= T`, enforced at plan time.** If a chunk were larger than the whole space it would contain
  the same byte twice *within one chunk*, and a write→verify pass could then race itself. Reject as a
  config error, don't clamp (that is the resolve-or-reject rule).
- **The pattern must never take the chunk index, the logical position, or a visit counter as seed
  input.** It doesn't today. This is now a load-bearing invariant, not a coincidence — worth a comment
  in `pattern_gen.rs` saying why.

**Where the overlap does cost something: coverage accounting, not correctness.** Four chunks over
5632 MB double-covers 512 MB. The fix is not to avoid the wrap — it is to stop expecting one cycle to
equal one clean pass:

```
  11 chunks x 1536 MB = 16896 MB = 3 x 5632 MB

  Over 11 chunks, EVERY byte is visited EXACTLY 3 times. Perfectly uniform.
  No byte favoured, no byte missed, no partial tail chunk.
```

That is the `gcd(1536, 5632) = 512` property. The wrap is not a defect to be tolerated — **it is the
mechanism that makes coverage uniform when `C` does not divide `T`.** TM5's alternative was to drop
the tail.

---

## Q25. "Is there anything that would do that?" — carrying stride phase across a seam

> quoting: *"stride phase must be carried across the seam, not reset per segment"*

Yes, and it is unexciting: the phase is one `usize` threaded through the segment loop.

```rust
// A strided kernel currently restarts from chunk_start every call.
// Carried version: phase in, phase out.
let mut phase = 0usize;                  // bytes into the stride cycle
for s in &segs {
    phase = strided_kernel(s, stride, phase);   // returns where it left off
}

// inside the kernel, the only change is the start offset and the return:
fn strided_kernel(s: &Seg, stride: usize, phase: usize) -> usize {
    let mut i = phase;
    while i < s.len { /* touch s.base + s.offset + i */ i += stride; }
    i - s.len                            // carry the overhang into the next segment
}
```

Cost: one add and one subtract per segment — i.e. one or two per chunk. This is the *entire* "one
genuine kernel-side care point" referred to in the spec. It is not a memory-manager problem and does
not need any infrastructure.

Note this only matters for tests that both (a) stride and (b) are allowed split spans. Under
`memory_system_design.md` 5.1 the strided tests default to `SingleSegment`, so on day one **nothing
needs this at all** — it is what you implement if you later decide `Mem-Stride` should cross seams.

---

## Q26. `CORRECTION` — VA stitching does not need a driver, and a driver would not help

> *"This seems like and reason to go for a KMDF with VA stitching."*

Two things are fused in that sentence and they need separating, because the conclusion changes.

**1. VA stitching is a user-mode feature. It has nothing to do with KMDF.**
`VirtualAlloc2(MEM_RESERVE | MEM_RESERVE_PLACEHOLDER)`, then `VirtualFree(MEM_RELEASE |
MEM_PRESERVE_PLACEHOLDER)` to split, then commit into each slot with `MEM_REPLACE_PLACEHOLDER` — all
Win32, all user mode, available today with zero driver. **If stitching is the answer, it is available
this afternoon and the driver decision is untouched.**

**2. A driver cannot give you seamless VA *and* huge pages unless physical memory cooperates.**
This is the part worth being blunt about. A 1 GiB page is *by definition* one PDPTE with PS=1 mapping
1 GiB of **physically contiguous, 1 GiB-aligned** memory. No amount of ring-0 privilege makes a 1 GiB
page out of scattered 4 KB frames — that is a hardware property of the page tables, not a permission.
So a driver's only extra trick would be MDL-mapping arbitrary physical pages into contiguous VA, i.e.
**reimplementing AWE** — and that lands you back at 4 KB pages, the exact limitation that made AWE
unattractive.

```
  What you want          seamless VA   +   1 GiB pages
  ─────────────────────────────────────────────────────────────────────
  AWE / MDL mapping         YES              NO   (4 KB frames only)
  VA stitching              YES              UNKNOWN — see Q27
  separate blocks + L1      NO (1-2 jumps)   YES  (what we have)
```

Paging itself permits the middle row: four 1 GiB pages at four consecutive 1 GiB-aligned VAs form a
flat 4 GiB span, each PDPTE pointing at its own contiguous physical gigabyte. **The question is purely
whether the Win32 API lets you commit `MEM_LARGE_PAGES` into a placeholder.** That is an API question,
not a paging question, and I do not know the answer. Q27 says how to find out.

---

## Q27. Would stitching remove the need for the logical space? No.

Worth answering before anyone spends a day on the experiment, because it determines build order.

Assume stitching works perfectly and all 5632 MB becomes one flat VA span. The row table collapses to
one row. Now re-run the example:

```
  T = 5632 MB, C = 1536 MB, ONE row

  chunk 0  [   0 .. 1536)   1 segment    <- was 1
  chunk 1  [1536 .. 3072)   1 segment    <- was 1
  chunk 2  [3072 .. 4608)   1 segment    <- was 2   IMPROVED
  chunk 3  [4608 .. 6144)   2 segments   <- was 3   improved, but STILL SPLIT
                            because it wraps past the end of the space
```

**The wrap seam survives stitching.** It is not caused by having separate allocations — it is caused
by `C` not dividing `T`, which is the normal case for any configured chunk size. So:

- `resolve()` is needed either way.
- The `Seg` list is needed either way.
- Reps-outside-segments is needed either way.
- Split-safe position-local kernels are needed either way.

Which means **stitching is a pure optimisation on top of the logical space, not an alternative to it.**
It reduces segments per chunk from 2–3 to 1–2 and makes page geometry uniform. Those are real
benefits. They are not the difference between "works" and "doesn't work".

**Therefore: build L1 first regardless.** It is the load-bearing layer, it works with the blocks you
actually get, and it cannot be invalidated by the stitching experiment failing.

### The experiment, if you want to run it

Roughly two hours, and it is decisive:

1. `VirtualAlloc2(NULL, NULL, 4 GiB, MEM_RESERVE | MEM_RESERVE_PLACEHOLDER, PAGE_NOACCESS, ...)`
2. Split into four 1 GiB slots with `VirtualFree(p, 1 GiB, MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER)`
3. Into each slot: `VirtualAlloc2(NULL, slot, 1 GiB, MEM_RESERVE | MEM_COMMIT | MEM_LARGE_PAGES |
   MEM_REPLACE_PLACEHOLDER, PAGE_READWRITE, ...)`
   — also try the section route: `CreateFileMappingW(INVALID_HANDLE_VALUE, ..., SEC_COMMIT |
   SEC_LARGE_PAGES, ...)` + `MapViewOfFile3(..., MEM_REPLACE_PLACEHOLDER, ...)`

**If step 3 fails, that is the decisive answer** and the question is closed forever — record the error
code (`87` = malformed request, `1450` = no contiguous physical, `1314` = missing privilege).

**If step 3 succeeds, do not believe it.** Per the standing rule that *real page size is not
observable after the fact*, success does not prove you got 1 GiB pages rather than a silent 4 KB
downgrade — and a silent downgrade here would be strictly worse than the seam it was meant to remove.
Prove it with a TLB-sensitive proxy: random 64-B accesses across the whole 4 GiB span. With 1 GiB
pages four TLB entries cover everything; with 4 KB pages nearly every access takes a page walk. The
gap is large (typically 2–4×) and unmistakable. Compare against a plain non-stitched
`MEM_LARGE_PAGES` allocation of the same size as the control.

This is also the honest reason I am not simply telling you "go stitch": **I don't know if it survives
1 GiB pages, and the failure mode is invisible.** Anyone who tells you it definitely works, or
definitely doesn't, without running that measurement is guessing.

---

## Q28. Why is there no modern AWE? And how do databases/Redis solve this?

> *"While AWE is legacy I'm surprised this type of feature (sliding/controllable address space) has no
> modern/64bit equivalent?"*

### AWE still exists — the *problem* it solved doesn't

`AllocateUserPhysicalPages` / `MapUserPhysicalPages` are still present and callable in 64-bit Windows.
Nothing was removed. What changed is why anyone would call them:

```
  32-bit:  2 GB of user VA, up to 64 GB of RAM
           -> VIRTUAL ADDRESS SPACE IS THE SCARCE RESOURCE
           -> so you need a small window and you rotate physical pages through it.
           -> AWE.

  64-bit:  128 TB of user VA, at most a few TB of RAM
           -> VA is ~30,000x more plentiful than RAM
           -> just map everything at once. There is no window to slide.
```

So the "sliding" half of AWE is obsolete by arithmetic, not by deprecation. The "controllable address
space" half absolutely does have a modern successor, and it is precisely the stitching API family:
`VirtualAlloc2` + `MEM_RESERVE_PLACEHOLDER` + `MapViewOfFile3` / `MEM_REPLACE_PLACEHOLDER`. That *is*
64-bit AWE, rebuilt around sections and placeholders instead of a page array. It is what Q26/Q27
discuss. The one capability nobody replaced is AWE's *4 KB granularity of physical control* — and
that is the property TMR least wants.

### What database and cache systems actually do

The short version: **every one of them uses fixed-size units plus an indirection table. None of them
stitches VA into one flat span.**

| System | Unit | Mechanism | Large pages |
|---|---|---|---|
| PostgreSQL | 8 KB block | `shared_buffers` + buffer descriptor array + buffer-mapping hash | `huge_pages=on` |
| SQL Server | 8 KB page | buffer pool + page hash | "Lock Pages in Memory" (= `SeLockMemoryPrivilege`) |
| Oracle | **granule**, 4–16 MB by SGA size | SGA carved into granules, pools resized by moving granules | yes |
| JVM (G1GC) | region, 1–32 MB | heap = array of regions + remembered sets | yes |
| Redis / Valkey | size-class slabs | jemalloc arenas | **actively discouraged** — THP causes fork/COW latency spikes |
| Memcached | 1 MB slab page | slab classes per item size | optional |
| DPDK | hugepage-backed memseg | memseg *lists* — literally a row table like L1 | mandatory |

Two conclusions worth drawing:

**1. The industry's answer to "my memory arrived in unequal pieces" is a lookup table, and always has
been.** Oracle calls its unit a granule; G1 calls it a region; DPDK keeps memseg lists. This is
convergent design across four decades and it is the same shape as L1. That is reassuring rather than
novel — the proposal is the boring answer, which is the right kind of answer.

**2. Their problem is strictly harder than ours, which is why their machinery is bigger.** They
indirect to answer *"where does this data live?"* — an unpredictable, concurrent, evicting,
per-request question, hence hash tables, pin counts, replacement policies, thousands of lines. We
indirect to answer *"where do I walk next?"* — sequential, single-threaded per space, known in
advance. That collapses to a 3-row linear scan resolved once per chunk. **We should not build a memory
manager; we need about 1/1000th of one, and pretending otherwise is how this design got
over-engineered the first time.**

Also note what Redis tells you by omission: it *avoids* huge pages because latency spikes matter more
than TLB reach. TMR is the opposite — TLB reach is most of why large pages are wanted. So their
allocator choices are not transferable, only their structural pattern is.

### And the memory testers

Nobody in this space solved cross-block coverage, because they mostly don't have the problem:

- **MemTest86** runs bare-metal/UEFI with the firmware memory map in hand. It has *physical*
  addresses, so there is no VA/PA divergence to reconcile — a different universe.
- **memtester** (Linux) `mmap`s and `mlock`s one region and tests within it. One block, no seams, and
  it simply cannot test more than the largest single mapping it can get.
- Any **Windows user-mode tester** faces exactly TMR's three options: accept per-block testing (what
  TMR does today), stitch VA (Q27), or build a logical space (L1).

So this is not a solved problem TMR is failing to look up. It is a genuine gap, and L1 is a
defensible answer to it that no competitor has needed to invent because they either had physical
addresses or accepted the limit.
