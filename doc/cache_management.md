# Cache Management for Memory Tests

When and how to use the cache-control primitives available to TMR. This doc
exists because the choice between "rely on natural cache eviction", "use NT
stores", "use explicit CLFLUSHOPT", and "use a fence" is not always obvious,
and the wrong choice can silently invalidate a test (errors masked by cache).

This is a working reference for **future test authors** — when you sit down
to write a new test pattern, this doc tells you what cache-control strategy
fits the test's intent.

## TL;DR Decision Matrix

| Test intent | Strategy | Fence required |
|---|---|---|
| Bulk bandwidth at >L3 working set | Natural eviction | MFENCE between phases (cheap, defensive) |
| Cache-tier-targeted bandwidth (L1/L2/L3) | Cache-resident BY DESIGN | SFENCE between phases |
| Sequential correctness on huge workset | Natural eviction | MFENCE between write/verify phases |
| **Refresh / bit-fade, extent >= 2x cache** | Natural eviction sufficient; CLFLUSHOPT optional | MFENCE between write and sleep |
| **Refresh / bit-fade, cache-resident extent** | **CLFLUSHOPT REQUIRED** | MFENCE after flush, before verify |
| **Repeated-access tests (SimpleTest write-read cycles)** | **CLFLUSHOPT REQUIRED to reach DRAM** | MFENCE after flush, before verify |
| **Stuck-bit with strict DRAM guarantee** | CLFLUSHOPT optional (chunk-size dependent) | MFENCE after flush, before verify |
| NT-store-based test | NT bypasses cache writing | SFENCE before re-store; MFENCE before verify-load |
| Cross-thread page exchange | Producer/consumer ordering | MFENCE on each side |

## The Three Primitives

### CLFLUSHOPT

**What it does**: Evicts a 64-byte cache line from L1/L2/L3 across the cache
hierarchy. If the line is dirty, the data is written back to DRAM as part of
the eviction. After CLFLUSHOPT completes, the next access to that address
must round-trip through DRAM.

**What it does NOT do**: Does not order memory operations. CLFLUSHOPT itself
is weakly ordered — multiple CLFLUSHOPTs to different lines can complete in
any order, and a subsequent load can issue before the flushes have drained
unless you place an MFENCE between them.

**Asynchronous nature** (the question you'd most likely have):
- The instruction enqueues a flush request and the front-end can issue the
  next instruction immediately. Multiple back-to-back CLFLUSHOPTs pipeline.
- The actual eviction + DRAM write-back happens asynchronously in the cache
  controller. You don't wait per-instruction.
- **You DO wait at the MFENCE that follows the flush loop.** That fence
  blocks until every outstanding flush has completed (line invalidated,
  dirty data in DRAM). This is non-negotiable — without the fence, a verify
  load can issue while flushes are still draining and read stale cached data.
- Net effect: the *flush phase* runs at memory-controller-write-back
  bandwidth (similar wall-clock cost to writing the block back via natural
  eviction, just guaranteed instead of best-effort).

**When to use**: you need a guarantee that subsequent reads come from DRAM,
regardless of working set size, other threads, or cache pressure. This is
the user-mode equivalent of UC (uncached) memory access.

**Hardware availability**: Intel Skylake+ (2015), AMD Excavator+ (2015) / Zen+.
CPUID feature flag: `clflushopt` (leaf 7, EBX bit 23). NOT part of any psABI
microarch level (v1-v4) — it is a standalone flag, so the compiler baseline
cannot guarantee it. TMR **requires** it: a startup CPUID gate (main.rs)
rejects CPUs lacking AVX2 + CLFLUSHOPT and exits cleanly. We therefore emit
CLFLUSHOPT unconditionally (behind `#[target_feature(enable = "clflushopt")]`)
with **no `_mm_clflush` fallback** — pre-2015 CPUs are out of scope (all
DDR3/DDR4-era; a DDR5 tester never runs on them). Older CPUs do have plain
CLFLUSH (SSE2, globally serialized, functionally equivalent) but we do not
target them.

**Cost per line**: ~50-100 cycles to issue, dominated by memory-controller
bandwidth when the dirty line actually gets written back.

### MFENCE

**What it does**: Serializing memory barrier. All loads + stores issued
before the MFENCE complete (globally observable) before any load or store
issued after the MFENCE can complete. It's a full barrier in both
directions.

**What it does NOT do**: Does not flush cache. Lines stay where they are.

**When to use**:
- Between two phases that include both reads and writes (write phase →
  verify phase).
- After NT stores when subsequent operations include loads.
- After a CLFLUSHOPT loop, before any verify-load.
- When two threads are sharing memory and order matters.

**Cost**: ~30-50 cycles, depending on CPU and pipeline state. Cheap if used
between phases (one MFENCE per phase boundary), expensive if used per-element
(don't do that).

**TMR usage today**: `std::sync::atomic::fence(Ordering::SeqCst)` — on x86
this lowers to MFENCE. ~15+ sites in `tests.rs`, present in most v1 tests
between write and verify phases.

### SFENCE

**What it does**: Store-only ordering barrier. All stores issued before the
SFENCE become globally observable before any store issued after the SFENCE.
Does not order loads in either direction.

**What it does NOT do**: Does not order loads. Does not flush cache.

**When to use**: After NT stores, when the **subsequent code is also
storing** (e.g. you want store-A to land before store-B becomes visible,
but no one's reading yet). Cheaper than MFENCE.

**When NOT enough**: If a load (verify) follows the stores, SFENCE does
not order the load against the stores. The CPU can issue the load
speculatively before NT stores have drained from the WCBs. **Use MFENCE
in that case.**

**Cost**: ~10-20 cycles. Cheaper than MFENCE.

**TMR usage today**: bandwidth_tests.rs and latency_tests_v2.rs after NT
store loops. This is correct — those tests use NT-store-then-store patterns,
not store-then-load patterns.

## The Cache Masking Problem (why this matters)

Imagine: write `0xAAAA...` to address X with a normal cached store, then
verify it.

What actually happens on standard cached (write-back) memory:
1. Write to X → lands in L1, eventually trickles to DRAM via write-back
2. Verify-read of X → returns whatever's in L1 (still hot — you just wrote it)

If a DRAM cell at X has a stuck bit, **the verify still passes**, because
you read the L1 copy that has the correct value. The cache silently masks
the very errors you're trying to find.

Three strategies to defeat cache masking:

| Strategy | How it works | Cost | When it fails |
|---|---|---|---|
| **Workset > L3** | Write so much memory that early writes are evicted by later writes before verify reaches them | Effectively free | Refresh-style "quiet wait" tests; small worksets; servers with very large L3 |
| **NT stores** | Bypass cache going down — writes go straight to DRAM via write-combining buffers | ~equal to cached writes for sequential | Verify-load still returns cached data if the line was previously read into cache |
| **CLFLUSHOPT + MFENCE** | Explicitly evict every line before verify | A few percent overhead vs natural | Never — guaranteed |

**Critical insight**: NT stores defeat cache masking on the **write side
only**. They do nothing for the verify-read side. If you write with NT
stores then read normally, and the line happened to be cached from a prior
read or prefetch, the verify hits cache. Combine NT stores with
CLFLUSHOPT-before-verify (or workset > L3) for full coverage.

## When "Workset > L3" Is Sufficient

The cheapest strategy and what TMR uses for most tests today.

**Works well for**:
- Bulk sequential read/write/copy at >L3 size. By the time you've written
  64MB, the first 32MB is long gone from L3.
- Bandwidth tests at DRAMSmall/DRAMFull tiers (`Spd-DRAM*-*`).
- Long sequential SimpleTest/MirrorMove/StuckBit at large windows.
- Multi-threaded tests where threads contend for L3 (each thread's writes
  evict others'). Note: this is contention-dependent; with one thread on a
  big-L3 server, this assumption breaks.

**Breaks down for**:

1. **Refresh / bit-fade tests** — the issue isn't cache pressure during
   the wait, it's that during the wait, *nothing else is touching memory*.
   The lines you wrote stay in cache the whole time because nothing's
   evicting them. Workset size doesn't help — you wrote 1MB, you sleep
   64ms, that 1MB is still in L1/L2/L3 when you verify.

2. **Cache-tier-targeted tests** — `Lat-V2-L3-*`, `Spd-L3-*`, etc.
   *deliberately* fit in cache. That's the point. If you want a "DRAM
   verify after L3-sized write" variant, you need explicit flush.

3. **Tail-end of a sweep** — when you reach the last block in an extent
   and start the verify, the last few MB you wrote are still hot in L3.
   The first 90% of the verify hits DRAM via natural eviction; the last
   10% might hit cache. Usually fine for bandwidth tests; a small coverage
   gap for stuck-bit-style tests at the edges.

4. **Random access at cache-fitting working sets** — if random access
   keeps returning to lines that fit in L3, those lines stay hot
   indefinitely. Workset size in *bytes* doesn't tell you what's in cache;
   *reuse distance* does.

5. **Small-block correctness tests on systems with huge L3** — server
   CPUs with 256MB+ L3 can swallow surprisingly large worksets. A test
   sized to spill on a desktop chip might fully fit on a Threadripper
   Pro / Xeon. Test-time decisions baked at compile time can be wrong on
   some hardware.

## When NT Stores Are Sufficient

NT stores write through Write Combining Buffers (WCBs) directly to DRAM,
bypassing the L1/L2/L3 cache hierarchy on the **write path**.

**Works well for**:
- Tests where you only need the write to bypass cache, not the read.
- Tests that immediately re-read the data they just wrote — the verify
  load brings the line back into cache cleanly (and if there's a DRAM
  error, the cache now contains the *bad* value).
- Bandwidth tests for DRAM where you want measured bandwidth to reflect
  actual DRAM write speed, not cache speed.

**Important constraints**:
- **Always SFENCE after NT stores** before any subsequent store, or
  MFENCE before any subsequent load. WCBs drain at hardware-controlled
  times, not at instruction boundaries.
- **NT stores don't defeat read-side cache masking.** A line that was
  cached before you wrote it via NT may still be in cache for the verify.
  This is unusual in TMR's flow because we don't pre-read before writing,
  but worth knowing.
- **Partial cache line NT writes are slow.** See the partial-vs-full-line
  section of `nt_stores.md` — full 64-byte writes hit a fast path; partial
  writes share a fixed hardware overhead.
- **Order matters per region.** NT stores within the same WCB combine for
  efficiency, but NT stores to widely scattered addresses defeat
  combining and run slowly.

**TMR usage today**: bandwidth_tests.rs (DRAM tiers), latency_tests_v2.rs
NT-write variants, Bench-Init when NT mode is selected.

## When CLFLUSHOPT Is Required

The only strategy with a guarantee. Use when:

1. **Quiet wait between write and verify** — refresh, bit-fade, retention
   tests. Natural eviction relies on activity displacing your data; if
   you're sleeping, nothing's displacing.

2. **Cache-resident workset** — you want stuck-bit-style verification but
   the test's working set fits in cache. Either resize the working set
   (which changes what's being tested) or flush.

3. **Tests adapted from memtest86+/stressapptest** — these were written
   for environments without virtual memory and assume direct DRAM access.
   Their test logic implicitly expects loads to hit DRAM. Adding
   CLFLUSHOPT between phases keeps the test semantics intact.

4. **Cross-test guarantees** — when test A's results depend on what
   test B left in DRAM (not cache). E.g. dependent test patterns where
   one writes and another verifies after a delay.

**Implementation pattern**:
```rust
// Phase 1: write
for i in 0..n { *base.add(i) = pattern; }

// Phase 2: flush every cache line in the range.
// In TMR this is `tests::flush_range_to_dram`, which emits CLFLUSHOPT via
// inline `asm!` (the `_mm_clflushopt` intrinsic isn't in nightly yet — stdarch
// PR #2141 — and asm keeps the same `#APP` idiom as the NT-store paths; see
// `doc/nt_stores.md`). The helper carries #[target_feature(enable="clflushopt")].
let line_size = 64; // x86 cache line
let line_count = (n * size_of::<u64>() + line_size - 1) / line_size;
for line in 0..line_count {
    let addr = (base as *const u8).add(line * line_size);
    core::arch::asm!("clflushopt [{a}]", a = in(reg) addr,
                     options(nostack, preserves_flags));
}
std::arch::x86_64::_mm_mfence();  // wait for flushes to drain

// Phase 3: verify — every load now misses to DRAM
for i in 0..n {
    let v = *base.add(i);
    if v != pattern { /* error */ }
}
```

**Use CLFLUSHOPT, not CLFLUSH.** CLFLUSHOPT pipelines correctly; plain CLFLUSH
(the SSE2 `_mm_clflush`) globally serializes and is much slower for bulk
eviction (~15× in `../clflush-test`). TMR emits CLFLUSHOPT via inline `asm!`
today: the `_mm_clflushopt` intrinsic (stdarch PR #2141) hasn't synced into
nightly yet, and the asm form keeps the flush in the same `#APP` idiom as the
NT-store paths — which matters if flush and NT stores ever share a hot loop.
The two emit identical machine code; see the strategy section of
`doc/nt_stores.md`.

**No runtime fallback in TMR.** CLFLUSHOPT is guaranteed by the startup CPUID
gate (main.rs requires AVX2 + CLFLUSHOPT, exits cleanly otherwise), so the
flush helper emits CLFLUSHOPT unconditionally behind
`#[target_feature(enable = "clflushopt")]`. We do **not** carry a `_mm_clflush`
path — pre-2015 CPUs are out of scope for a DDR5 tester (see "Hardware
availability" above).

## TMR Today (May 2026)

What's actually wired:

| Test | Cache-control strategy | Status |
|---|---|---|
| `Spd-DRAM*-*` (bandwidth) | Workset >> L3, NT stores for write tier | ✓ correct |
| `Spd-L1/L2/L3-*` | Cache-resident by design, cached stores | ✓ correct |
| `Lat-V2-DRAM-*` | Workset >> L3 | ✓ correct |
| `Lat-V2-L*-*` | Cache-resident by design | ✓ correct |
| `Lat-NTW-DRAM-*` | NT stores + SFENCE | ✓ correct |
| `Mem-StuckBit*` | Workset >> L3 (typically), MFENCE between phases | ✓ correct for large worksets |
| `Mem-SimpleV2*` | Workset >> L3, MFENCE between phases | ✓ correct |
| `Mem-Mirror*` | Workset >> L3, MFENCE between phases | ✓ correct |
| `Mem-CacheBust` | L3-sized extent, intentional cache pressure | ✓ correct (purpose-built) |
| `Mem-Refresh*` | Extent L3*2, 64ms sleep, CLFLUSHOPT each chunk before sleep | ✓ **FIXED 2026-05-29** |

### The Mem-Refresh Cache Masking Bug (FIXED 2026-05-29, Task #46)

`refresh_stable_multi` and its SIMD variants previously had a correctness
gap: the lines written stayed hot in cache across the 64ms quiet wait, so
the verify read from L1/L2/L3 instead of DRAM and could not observe real
DRAM bit decay. (Workset size doesn't help here — during a quiet sleep
*nothing* evicts the lines, regardless of extent size.)

**Fix applied**: each chunk is now flushed to DRAM before the sleep via
`flush_range_to_dram` (`tests.rs:~1292`, inline-asm CLFLUSHOPT + trailing
MFENCE, 1× per line, no unroll) in all 4 variants. The extent was bumped
`l2*2` → `l3*2`. Chunk size left on the user's `ChunkMode`. The verify load
now round-trips through DRAM — the canonical refresh-test pattern from
memtest86+ and similar tools.

The MFENCE was always correctly placed (orders writes globally before the
sleep); ordering was never the issue — cache residency was, and the
CLFLUSHOPT loop is what addresses it.

### Considered and deferred: software prefetch (`prefetchnta`)

TM5 (the reference implementation) used `prefetchnta` ahead of both its fill
and verify loops (`TM5/bin/mtests0.asm:362,453`), prefetching `8 × jump`
ahead, and its comments benchmark a real 1.2-1.5× throughput win on a Core2
Q6600/E8400. TMR deliberately does **not** use it. Reasons:

- **The win came from an obsolete bottleneck.** Core2-era CPUs had weak
  hardware prefetchers; an explicit hint hid latency the CPU couldn't.
  Modern CPUs auto-prefetch strided/sequential patterns aggressively, so for
  TMR's sequential bandwidth tests an explicit `prefetchnta` is usually
  redundant and can *hurt* by competing for load buffers.
- **It changes what the test measures.** TMR bandwidth tests aim to report
  honest DRAM/cache-tier bandwidth. A hand-tuned prefetch distance turns the
  number into "bandwidth with this specific prefetch tuning," which is
  hardware-specific (TM5 hardcoded `8 × jump` for a Q6600 — not portable).
- **NTA specifically conflicts with cache-residency tests.** `prefetchnta`
  pulls into a non-temporal-friendly location (bypassing L2 on many CPUs),
  which would defeat a cache-tier-targeted test (`Spd-L2-*`) that *wants*
  data resident in that tier.
- **Latency tests must NOT prefetch** — hiding latency is the opposite of
  measuring it. So the one place HW can't predict (pointer-chasing) is also
  the place a prefetch would be wrong.

*Revival trigger*: a sequential bandwidth test measurably falling short of
the DRAM ceiling, OR an *irregular*-access bandwidth test (not latency) where
the HW prefetcher can't keep up. In that case A/B test a software prefetch
with a per-CPU-calibrated distance — measure, don't assume. See
[[tm5-cache-strategy]] for the full TM5 finding.

## Implementation Notes

**Don't intermix CLFLUSHOPT with the hot loop.** Flush after the entire
write phase, not after each store. Flush-per-store eliminates pipelining
and turns CLFLUSHOPT into something close to UC-write performance.

**64-byte cache line stride is correct on every modern x86.** Don't
hardcode this with `#[cfg]` — read it from CPUID at startup if you want
to be defensive, but in practice every CPU TMR runs on uses 64.

**Always MFENCE after a flush loop.** Even if "it works" in your test,
without the fence the CPU is allowed to issue verify-loads while flushes
are still draining. Test passes today, fails on next CPU revision.

**Workset > L3 is a heuristic, not a guarantee.** Document the assumption
in any test that relies on it. Specifically: a test that depends on
natural eviction should fail loudly (or at minimum log a warning) if it's
asked to run on a working set smaller than L3.

**Don't flush in tests that are intentionally cache-resident.** Cache-tier
bandwidth/latency tests are measuring *cache* performance. Flushing turns
them into DRAM tests. The two test types coexist; don't conflate them.

## Measured: what the flush actually buys (2026-07-30)

`Mem-StuckBit128`, same binary, chunk size swept via **fixed absolute chunks**
(`test_configs/flush_chunk_sweep.json`), flush off vs on. Intel Xeon 6975P-C
(Granite Rapids, 48 KiB L1d / 2 MiB L2 per core), MiB/s. One run per
configuration — treat single-digit-percent differences as noise; the effects
below are 20-500%.

**Scope note**: the `ChunkMode::Cache{L2}` sizing that `Mem-StuckBit-Flush*`
now uses is *inferred* from this absolute-chunk data. It has not itself been
benchmarked.

```
                 4 threads (1/core)          8 threads (SMT)
  chunk        off      on   penalty       off      on   penalty
  64 KiB   134,500  23,061    5.83x    150,104  31,680    4.74x
   1 MiB   120,763  36,285    3.33x    134,165  48,574    2.76x
  16 MiB    55,725  36,629    1.52x     61,726  47,846    1.29x
 256 MiB    42,944  36,064    1.19x     56,338  45,250    1.24x
```

The penalty column is the obvious read: flushing costs more the smaller the
chunk. **The important read is the two data columns separately.**

- **flush=off varies 3.1x with chunk size** (134,500 -> 42,944). What the test
  measures is a *function of chunk-vs-cache*: at 64 KiB the "verify" reads
  SRAM written microseconds ago and **never touches DRAM at all**.
- **flush=on is flat within 1.6%** for chunks >= 1 MiB (36,285 / 36,629 /
  36,064). It is a genuine DRAM round-trip at every chunk size.

So the flush's real value is not "slower and therefore more thorough" — it is
**making the test chunk-size-independent**. Corollaries:

- Flush belongs where the chunk is **cache-resident**. `Mem-StuckBit-Flush*`
  therefore uses `ChunkMode::Cache { L2, scale 1.0 }`, not the plain variants'
  large chunk (512 MiB; a ~6% fraction before 2026-10-03).
- At large chunks (>= ~16 MiB here) natural eviction already forces DRAM
  reads, so flushing buys ~20-30% less bandwidth for **no change in what is
  tested**. Don't add it there.
- **There is a lower bound too — don't go below ~1 MiB.** Flush at 64 KiB
  (4T: 23,061; 8T: 31,680) is *slower* than at 256 MiB (36,064 / 45,250) for
  identical work: the per-chunk fence + call + trailing MFENCE is paid ~4096x
  more often per GiB, with too little work to overlap the drain against. Tiny
  chunks pay the fixed cost without extra benefit. **No minimum-chunk floor
  exists** beyond SIMD alignment (`calculate_minimum_chunk_size` returns
  8-64 *bytes*), so nothing clamps a too-small cache target for you.
- Mind the **SMT divisor** when picking a scale: `CacheTarget::L1`/`L2` divide
  by *active threads per core* (`size_bytes_cpuid`), so `scale 0.5`
  on a 2 MiB L2 gives 1 MiB at 1 thread/core but 512 KiB under SMT — i.e. it
  silently drops into the fixed-cost-dominated zone above on exactly half the
  run configurations. `scale 1.0` gives 2 MiB / 1 MiB, both in the flat zone.
- The two chunk regimes are **not interchangeable measurements** — never
  compare a flush number against a non-flush number at a different chunk size.

### Why Refresh does NOT need it unconditionally (revised 2026-07-31)

TODO #26 fixed a real bug — the 64 ms retention delay was being defeated by a
cached copy — but the fix landed as an *unconditional* flush, and that was
over-correction. Three things make natural eviction sufficient at a sane extent:

1. **The extent is `CacheTotal 2.0x` by design** — 2x the whole hierarchy. On a
   482 MiB-cache box that is a 964 MiB extent, so writing it evicts its own
   earlier half. The multiplier exists precisely so this holds across machines
   with different cache sizes.
2. **L3 is shared across threads.** At 1 thread up to ~50% of the extent could
   still be resident; at 8 threads the per-thread L3 share is 1/8, so <= 6.4%
   can be. More threads means less residency, and real runs are multi-threaded.
3. **The verify sweeps forward, the same direction as the write.** So any
   surviving tail line is read *last* — after the verify's own reads have pulled
   roughly a whole extent through the cache. The residual is simultaneously the
   smallest part of the range and the least likely to still be cached. (Verifying
   *backwards* would be the pathological case; forwards is self-cleaning.)

With 8 concurrent worksets each 2x total cache against a shared, virtualized L3,
a line surviving from write to verify is a fluke. Paying ~20-30% throughput on
every run to insure against a fluke is a bad trade for a tool where **throughput
is coverage per unit time** — faster cycles find more errors than a marginally
stricter single pass.

So it is now a flag (`flush_before_verify`), default **off**, with
`Mem-Refresh-Flush` registered for when you want the DRAM round-trip
architecturally guaranteed rather than dependent on replacement policy
(non-inclusive caches, prefetchers, another VM's L3 pressure).

**The flush is still mandatory where eviction cannot help**: any test that
re-reads the chunk it just wrote. `SimpleTest`'s TM5-faithful
`(1 write + N reads) x write_read_cycles` is exactly that shape — no extent
sizing defeats the cache when the re-reads target the chunk you just wrote.

## See Also

- `doc/nt_stores.md` — NT store implementation details (why std::arch
  intrinsics + manual unroll is required), partial vs full-line NT write
  paths, and the asm-vs-intrinsic `#APP` churn / tool-selection strategy
- `doc/simd_codegen_rules.md` — SIMD codegen rules and traps: byte-uniform
  fills get rewritten to `memset` (erasing SIMD width), the N=4
  multi-accumulator rule for latency-bound verify loops, vector register
  budgets, macros-not-generics for per-width variants, and benchmark
  methodology (flush outside the timed region, DCE traps)
- `doc/store_buffer_pressure.md` — how store buffer dynamics affect
  benchmarks at different cache tiers
- `doc/extent_chunk_modes.md` — Extent/Chunk hierarchy that determines
  workset size for each test
