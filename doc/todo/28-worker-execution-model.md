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

**From TODO 19 (2026-10-22): the GSAT-style page exchange comes first** (the owner, correcting
a "candidate" wording of the same day). A thread claims a page, checks the pattern its last writer
left, writes its own and releases it; pages cross threads, CCDs and NUMA nodes on purpose. It
stresses the coherence fabric and the inter-CCD/NUMA links, which nothing in TMR touches. This is
the producer/consumer handoff the paragraph above deferred: the owner's call overrides that for
the exchange. Its memory is an exclusive tile grid (`doc/memory_system_design.md` §4.7, tile
mode), apart from the overlapping chunks of the per-thread tests (`76-span-tests.md`). The
different-tests-at-once mode (a) and the line-sharing storm come after it. Stages 1-3 below are in
the code already (the per-step barrier, the result channel, per-worker inboxes; each step's config
travels in its work item).

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

---

## The page exchange as built (2026-10-24)

`Mem-PageExchange`, `src/exchange.rs` (tests `src/exchange/tests.rs`), built overnight at the owner's
request, the open choices left as defaults to revisit. In plan `all` only, and not yet run on hardware.

**Memory.** Each thread's extent (the whole span by default) is cut into tiles of the test's chunk
(`ChunkMode::Absolute`, 2 MiB by default), apart and never overlapping, the last one shorter when
the chunk doesn't divide the extent, so every word is tested (as this TODO's entry had it).

**The claim protocol** (the one-owner convention's exception, now in `memory/buffer.rs`'s `Sync`
note). Each tile has a state word, an `AtomicU64` on a 64 B line of its own, allocated by its home
thread: 0 (no write this step: the seal in a sealed step), a write (its writer + 1, its claim, its
polarity), or that with a busy bit. Claim: one compare-and-swap, free to busy, Acquire. Release: one
store, Release, after the fill's own SFENCE (its stores are non-temporal). A busy tile is skipped,
never waited for: with far more tiles than threads it is rare, and no thread ever sleeps. The
state word is the only signal; nothing is sent.

Cost, against the work it rides on: a 2 MiB tile is 4 MiB of traffic (a check and a write), about
0.5 ms at a desktop thread's bandwidth and about 2.3 ms at a 256-thread server's share; the claim
is one atomic add (the pool's counter), one CAS and one store, a few hundred ns even across nodes.
The owner asked (2026-10-24) whether a messaging layer like iceoryx2 was needed: no, it moves large
payloads between processes; here the threads share memory and the "message" is a tile's number.

**Which tile.** A pool's claims are numbered by one counter; claim `t` takes the pool's tile
`(t * s + o) mod N` for its pass `t / N`, with `s` coprime with `N` and `o` both drawn from the
step's key and the pass. Each pass covers every tile once, and arranges them anew, so a tile goes
to different threads from pass to pass even when the threads run in step (a plain `t mod N` would
give each thread the same tiles every pass on a uniform machine).

**Sharing groups** (`exchange_share`): `any` (every thread's tiles, one counter), `node` (the
claimer's node's threads', a counter a node), `cross` (the other nodes', a counter a claiming node;
on one node it is `any`). A thread's node is its CPU's (`ThreadPool::cpu_assignments`); the memory
a worker fills is on its node first.

**The pattern** is the seal's `tmr` (an address hash under a key), keyed from the step's key and the
tile's state word, so the next claimer checks it from that word alone; odd passes invert it. Written
by the seal's fill and checked by its verify, at the seal's width: no new kernel, so no new hot loop.
A check's bad words are logged by the seal's rescan, then one line names the tile, its home thread,
its last writer and the checker, with their nodes.

**The seal and the end.** In a sealed step a tile's first claim checks the seal (`TestRunner::
check_seal_of`, new: `check_seal` indexes from the thread's own extent); its errors are seal errors.
A barrier ends the claims; then each thread checks its own tiles' last writes (or the seal, on a
tile no one claimed) and they are resealed: per tile in a one-cycle step, the whole extent as the
test finishes otherwise (`TestRunner::finish_*`). A halt or Ctrl+C still meets both barriers.

**Dispatch.** The step's workers share one `exchange::Step` (their nodes, tiles, the pools'
counters, the barrier), which `ThreadPool::execute_test` creates for this test and hands every
worker in `TestMemoryConfig::exchange`. Called without one (the seal's and the suite's unit tests),
a thread exchanges its tiles with itself.

**Defaults taken, for the owner to revisit:**
- Non-temporal stores (the seal's fill): a claimer reads the tile from DRAM, over the links when it
  is on another node. Normal stores, so a tile could pass cache to cache, need a normal-store `tmr`
  fill kernel (and an asm check).
- A tile is handed on whenever its next claim comes; no cache-to-cache mode. That mode would be a
  small lock-free ring of recently released tiles per pool.
- Plan `all` only, until the owner puts it in a plan.

**Next:** a run on a machine with more than one CCD or NUMA node (the US VM resized to two nodes,
or the metal box): errors 0; the per-thread debug line (`RUST_LOG=debug`: claims, of other threads'
tiles, on other nodes, found busy); its speed beside Mem-SimpleV2's. Then the rest of v1: different
tests running at once across threads, and a shared-line coherence storm.

**Review (2026-10-24, one agent, adversarial) and its fixes.** It confirmed one thread at a time a
tile, the Acquire/Release pairing after the fill's SFENCE, both barriers met on every path but a
panic in a debug build (release aborts), and no access outside a tile. Fixed: under
`errors=halt` a claim overwrote the bad words it found and other threads kept claiming and could
reseal them; now the claim releases its tile unwritten, a step-wide flag stops every thread, and
none rechecks or reseals (`TestRunner::keep_memory`). A first claim's seal errors now name their
tile and its home thread too, and the per-thread line and the results file describe the tiles as
they are (apart, the last shorter), not as overlapping chunks. Added: a sealed four-thread step and
a halt test. Left as concerns: a thread's node is its CPU's, not its memory's (the allocator's
`ThreadSpan` knows the memory's); no CCD-level group, so on one node only `any` crosses CCDs; the
per-thread time includes the wait at the second barrier (a step's speed takes the slowest thread's
anyway); the results file records no SIMD width for it (it runs at the seal's).

## The owner's direction for the exchange (2026-10-25)

After comparing the build with stressapptest (GSAT), the owner asked for an **operation switch**,
each operation paired with its pattern and verify, and a **pattern switch** beside it. The
comparison, in short: the exchange keeps GSAT's pool of pages crossing threads and nodes and its
two-tier detection, but its core operation differs. GSAT copies a full page into an empty one,
checking a checksum on the way, so data moves between addresses (and between nodes, in one
operation) and a fault anywhere on the path shows at the destination; the exchange checks a tile
and rewrites it in place. GSAT also runs a library of patterns (walking bits, checkerboards, solid,
pseudo-random, by bus width) where the exchange has one. GSAT runs for its `-s` time, copy threads
looping without passes; the exchange runs for its step's time too, a pass being only its
arrangement's unit.

The operations, as discussed:
- **Copy** (GSAT's): claim a full tile and an empty one, copy while checking the source, swap
  their roles. Needs a pattern from the offset within a tile, not the address (the seal's `tmr` is
  address-hashed, so copied data wouldn't match its new place): a new kernel, with its asm check.
  One pass reads and writes, where the rewrite takes two.
- **Rewrite, NT stores** (today's): the next claimer reads the tile from DRAM.
- **Rewrite, cached stores**: lines stay in the writer's cache, so a tile handed on soon after its
  write passes cache to cache (coherence traffic), which needs that handing on (a small ring of
  recently released tiles a pool). Cached stores with a CLFLUSHOPT after give about what NT does
  (the data in DRAM) for more traffic (each store first reads its line), so not worth a mode of its
  own; other tests exercise the flush (`flush_before_verify`).
- **Invert** (GSAT's invert threads): each word read and written back inverted, in place.

On cache residency: GSAT picks pages at random from gigabytes, so a page is rarely still in the
caches (tens of MB) when it comes round again; most of its traffic is DRAM, and a just-copied page
is hot only briefly. Deliberate cache-to-cache handoff needs the ring above.

Tiles of 1 MiB and up (the owner); at 1 MiB the pool's counter takes twice the claims it does at
2 MiB, still about 2% of what one cache line can serve on a 256-thread machine.

## The plan (2026-10-25)

stressapptest's source, read for it (`sat.cc`, `worker.cc`, `adler32memcpy.cc`, `sattypes.h`): pages
of 1 MiB, about 2/5 kept empty as copy targets; by default copy threads only, one a CPU (invert
`-i` and check `-c` threads are off unless asked); strict on, so every copy checks its source.
The copy is a plain C loop (cached stores) summing an Adler checksum as it goes, and a mismatch
rechecks the source word by word. NT stores (`movntdq`) are only in `-W`'s warm copy. Invert works
32 bits at a time in place, up, down, down, up (ending as it began), checked before and after only.
A page's pattern comes from its library at the fill and moves with its data.

The owner's plan, from that:
- **Three tests on one engine**, the claim protocol, sharing groups, seal and tiles as built:
  `Mem-PageExchange` copies (GSAT's default), `Mem-PageExchange-Invert` checks and flips in place,
  `Mem-PageExchange-Rewrite` checks and writes a new image (the build of 2026-10-24). Each op a
  loop of its own and a traffic of its own, so the results tell them apart, and as named tests
  they run as steps of one run, on the same memory.
- **Stores** (`exchange_stores`): cached or NT, for each. Copy and invert default to cached
  (GSAT's), the rewrite to NT (as built). A cached store usually reads its line first and the
  cache writes it back on eviction: three transfers a copied line to NT's two, a different
  read/write mix. No ring of recently released tiles: GSAT has none, and its cache-to-cache
  traffic is incidental.
- **Patterns**: the native families by `program` (all seven by default), each tile's image its
  own family, key and polarity, named by its state word. A word's value comes from its position
  in its tile, not its address, so a copy is right where it lands. Polarity is the pattern's:
  new images alternate it a program lap in turn; a copy moves data as it is; invert flips it.
- **Copy**: each thread holds one empty tile, not GSAT's 2/5. A claim copies its tile into the
  held one, checking it on the way; the held tile goes back to the pool with the image, and the
  claimed one is held next. One claim a copy, and nearly all memory holds checked data. A tile's
  first claim fills it (after the seal's check, in a sealed step); a short last tile can't be
  copied into a full one, so it is checked and rewritten in place.
- **Checking** compares every word against the pattern (TMR's accumulator), not GSAT's checksum:
  exact, where a plain sum misses two flips of a word's top bit. If generating the pattern in the
  copy loop can't keep up, a GSAT-style checksum is the fallback.
- **Invert** is one checked pass a claim, upward only (with no physical addresses, the walk's
  direction says little about cell order).
- **Not planned**: GSAT's warm copy (CPU heat) and its check threads (every op checks its reads).
- **Acceptance**: petz007's fault with the three and a reference test as steps of one run,
  daddy-petz clean over a long run, a two-node machine for scaling; the results pick the plan.

After the exchange, TODO 28's v1 keeps one part: the coherence storm, its own test. GSAT's
`--cc_test` (off by default) gives each thread its own byte in each of a few shared lines; a
thread bumps its byte in random lines, so the lines bounce between cores (across CCDs and nodes)
with almost no DRAM traffic, and at the end it checks its bytes add up to its increments (a lost
write is a coherence failure). Dropped (the owner, 2026-10-25): different tests at once across
threads, since GSAT runs only its own thread types together. That GSAT form, copy and invert
threads on one pool at once (`-m 8 -i 4`), falls out of the three ops: a setting giving some of
the exchange's threads invert, off by default.

## As built (2026-10-25)

The plan above, with what the build settled:
- **One engine, three tests** (`src/exchange.rs`): the state word names an image (its writer, its
  family's place in the program, its number, its polarity), and a second word on the tile's line
  names who last wrote its bytes, for a bad word's story. A first claim fills; then the copy copies
  into the held tile, the invert flips in place, the rewrite writes a new image. A copy's short last
  tile is checked and rewritten in place. A claim that finds bad words writes the image whole again
  (the held tile, the inverted tile), so each is counted once; under a halt the source keeps them.
- **The held tile**: each copier takes its first full tile before the barrier (its seal checked in
  a sealed step; if the check fails, the tile holds no image and gets one at the end). At the end a
  held tile goes back with the image it holds, or a new one if it never held one (a one-pass step),
  so the end's check covers it.
- **`exchange_invert`** (percent) gives that share of the copy's threads invert, spread evenly over
  thread indices: GSAT's copy and invert threads at once.
- **The kernels** (`src/exchange/kernels.rs`), at 256 and 512 bits: the cached fill is native
  SimpleTest's write; the check, copy and invert take spelled-out steps, 4 vectors for the families
  computed as vectors and a line for the switching ones, each arm of `with_phase_vectors!` naming
  only its own (its `switching` body). The first build left the steps to the compiler: at 512 bits
  the heavier families' accumulators went to a stack array (28 SIMD round trips over the kernels).
  Spelled out, the switching families still spilled their scalar hashes at a step of two or four
  lines; a line a step they don't. A branch on the family beside the dispatch wasn't folded into its
  arms, so both shapes were compiled for every family until the macro took the second body. Final
  asm: no round trip in any exchange or storm loop (the table's 3 and 3 are the baseline's), the
  rest of the table unchanged; every NT variant has `vmovntdq` and its SFENCE, no cached one does,
  no `memcpy` or `memset`, and the 256-bit kernels use no zmm. The storm's bump is the pick, a
  pointer load and an unlocked `incb`, unrolled twice.
- **Mem-CoherenceStorm** (`src/storm.rs`): GSAT's `--cc_test` with two changes. A line's 64 sharers
  are spread over the threads (`t % group`), so on a big machine they sit on different CCDs, where
  GSAT's are neighbours; and each thread replays its picks to check every byte exactly, where GSAT
  compares a sum's low byte. Defaults are GSAT's: 2 structures, 1000 bumps a check.
