# TODO 28. [TMR-APP] Concurrent / Worker Execution Model (Multi-Channel + Coherence Coverage)

> Full record, moved verbatim out of `TODO.md` on 2026-09-28 (open item).
> The short entry in `TODO.md` holds the current status; this file holds the reasoning.

**Priority**: High value (Large effort)
**Status**: Not Started (NEW — from doc/TMR_App_ideas.md)

**Problem — a structural coverage ceiling**: The current coordinator runs each thread
on its own blocks, sequentially, with a phase barrier between tests. This model
*structurally cannot* produce two regimes where overclock instability most often lives:

1. **Simultaneous multi-channel pressure**: at any moment we only exercise the physical
   addresses one block lands on. The IMC never sees aggregate multi-channel request-queue
   depth — exactly the regime where IMC scheduling, FCLK/UCLK, and inter-channel timing
   margins fail. (Passes our tests, crashes in real mixed workloads.)
2. **Cross-core coherence traffic**: nothing forces MESI/snoop/Infinity-Fabric pressure,
   which is where CCX/CCD interconnect and FCLK marginality show up on Ryzen.

**Solution — two-level synchronisation (keep the phase barrier, add intra-phase flow):**

- **Coordinator (cold path)**: chooses test + parameters + topology per phase, assigns
  jobs, collects results. Phase barrier stays — it's correct for phase boundaries.
- **Workers (hot path)**: own and execute all loads/stores locally on a job (a *region
  descriptor*, not routed memory ops). Between jobs (every 100ms+), pop next job / hand
  off to a peer via SPSC channel.

**Decouple allocation granularity from job granularity** (the key insight):
- Keep the existing few-large-blocks allocation (TLB-efficient, contiguous PA, big enough
  for stride tests).
- Subdivide logically into ~32 MB **work units**. A `Job` is a tiny descriptor
  (`block_idx, offset, length, test_id, params, generation`) — fits in a cache line, no
  unsafe-Send pointers, cheap to copy through channels and to log/replay for triage.
- Shared-block model: any worker can address any region of any block.

**Staged migration (do NOT do it all at once):**
1. `Arc<Barrier>` for phase sync (replace current ad-hoc "wait for all workers").
2. MPSC result channel — workers stream `TestResult` records, coordinator drains. Useful
   even before handoffs (don't lose data on crash mid-phase).
3. `ArcSwap<PhaseDescriptor>` to pass per-phase params without restructuring workers.
4. Per-worker SPSC job inboxes + topology-driven handoff. This is the new capability.

**Scope for v1 — resist the topology zoo**: The bulk of the value is (a) running
*different* tests concurrently across threads so all channels see mixed load at once, and
(b) one **coherence-storm** test where threads share lines. The exotic rotations
(cross-CCD ring, random shuffle, producer/consumer) are diminishing returns — defer them.

**Primitives**: `AtomicBool` shutdown (keep, Relaxed, checked at job granularity — never
in SIMD loop), `std::sync::Barrier`, `crossbeam::channel` (bounded inboxes cap 4-16 for
backpressure). NOT async/tokio (wrong tool for core-pinned tight loops).

**Inter-block reference patterns (later)**: a pointer-chase whose nodes point into another
worker's block stresses cross-channel IMC routing — finds things nothing else does. Last,
after the plumbing is solid.

#### SUGGESTED: traversal indexing — granule table + coprime-stride ordering (2026-08-21)

**Problem this solves**: the harness now hands each test its *full* block list, and blocks are
different sizes (all powers of 2, but 512 MiB next to 4 GiB). A test that wants **non-sequential**
traversal — jumping between blocks mid-phase — has no safe cheap index: an offset valid for one
block is out of range for another, so `blocks[i].ptr.add(off)` is a latent OOB. This is the
missing piece for random/shuffled access patterns, and it also removes the "one block at a time"
limitation on window/chunk iteration.

**Two traps to avoid** (both are correctness bugs in a *tester*, not style nits):
1. `blocks[rng % len]` then a per-block offset is **coverage-biased** — a 512 MiB block and a
   4 GiB block get equal hit counts, so the small block is sampled at 8× the per-byte density.
2. Random addresses *with replacement* **do not cover memory**. Draw N addresses from N cells and
   you touch only ~63% of them (coupon collector) — a third of cells untested per phase.

Both dissolve by randomising the **order**, not the **address**.

**Step 1 — flatten to a granule table (setup only, once per phase):**
```rust
// G = granule size, config knob (e.g. 64-256 MiB). Carry (base, len) not just base:
// `len` costs 8 extra bytes/entry and kills every edge case (block < G, non-multiple tail).
let mut slots: Vec<(*mut u8, usize)> = Vec::new();
for b in test_blocks {
    let mut off = 0;
    while off < b.test_size {
        let len = G.min(b.test_size - off);
        slots.push((unsafe { b.ptr.add(off) }, len));
        off += len;
    }
}
```
The ragged block list becomes a **uniform index space**. Tests never see a block boundary or a
size difference again, and a mismatched offset is structurally impossible — every offset is
bounded by the granule it came from. Table size: 128 GiB at G=64 MiB is 2048 entries / 32 KiB
(at 256 MiB: 512 entries / 8 KiB) — L1-resident, read once per granule.

**Step 2 — order the granules by coprime stride ("clock face"):**
```rust
let n = slots.len();
let step = pick_coprime(n, seed);   // any s with gcd(s, n) == 1
let mut i = seed % n;
for _ in 0..n {
    let (base, len) = slots[i];
    i = (i + step) % n;             // outer loop only — once per granule
    // ... sweep within the granule (sequential, or strided with an odd colour offset)
}
```
Stepping by a value coprime with `n` visits **every index exactly once** before repeating (8 slots
stepping 3: 0,3,6,1,4,7,2,5). So it is a *permutation*: unbiased and coverage-complete, fixing
both traps above. The `%` is free — this is the outer loop, ~20 cycles amortised over 64+ MiB of
accesses — and using it instead of power-of-2 masking means `n` can be any value: no padding,
no rejection loop.

**Deliberately NOT used — an LCG (`i = (i*A + C) & mask`)**: its only advantage over a constant
stride is irregular *jump distances*, but hardware prefetchers don't track strides across page
boundaries, let alone 64 MiB ones, so a constant granule hop is already opaque to them. The
irregularity that matters is *within* the granule, where `stride_patterns` / `subdivisions` /
`rng_sequences` already live. (Full-period LCG mod 2^k needs C odd and A ≡ 1 mod 4 — recorded in
case a future pattern genuinely wants varying hop distances.)

**Also NOT needed — a precomputed shuffled descriptor list.** At 64 B granularity, 10 GiB is
167 M entries × 8 B = 1.3 GiB of table reads competing with the test for the exact bandwidth
being measured. The permutation must be arithmetic, not stored.

**Bonus**: the per-block odd colour offset (decorrelating identically-aligned power-of-2 blocks
so they don't all present the same DRAM bank/row phase — see #19) drops in as the starting
offset within each granule. No allocator change, no padding.
