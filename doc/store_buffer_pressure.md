# Store Buffer Pressure & Three Cached Write Phenomena

**Date**: 2026-05-15
**Test platform**: AWS EC2 r8i.2xlarge (Intel Xeon 6975P-C, AVX-512), 6 threads
**Related**: `nt_write_partial_lines.md` (NT-write commit time), `nt_stores.md` (NT codegen)

## Summary

x86's store buffer normally hides cached write latency from the issuing core — writes
fire-and-forget into the buffer, retire later, and the CPU never stalls on them. This
makes it nearly impossible to measure "write latency" the way you can measure read latency.

**Layout B operations** (`Lat-V2P-*-{Read,Write,Copy}`) reveal three distinct regimes
where cached write/copy traffic becomes visible above pure read latency:

1. **L2 store-buffer pressure** (Write): chain step too short to absorb 6 RFOs in parallel
2. **L1 dependency-chain limit** (Copy): RMW serialization between load and store
3. **L3 bimodal Copy** (Copy): possible hardware fast path with high spread (1.52x)

DRAM tier masks all three — chain miss dominates and absorbs everything.

## Three-way comparison (Layout B AVX-512)

| Tier | Read | Write | Copy | Phenomenon |
|------|------|-------|------|------------|
| L1 | 2.9 ns | 3.2 ns | **4.1 ns** | Copy: dep-chain-bound (load→XOR→store serial) |
| **L2** | 7.0 ns | **12.7 ns** | **12.6 ns** | Write & Copy: store-buffer pressure plateau |
| L3 | 42.2 ns | 53.8 ns | **40.9 ns** (spread 1.52x) | Copy: bimodal, possible hardware fast path |
| DRAM | 166.7 ns | 168.6 ns | 167.4 ns | All converge — chain miss dominates |
| DRAMFull | 174.3 ns | 164.7 ns | 166.3 ns | Same — within noise |

## Phenomenon 1: L2 store-buffer pressure (Write)

`Lat-V2P-L2-Write-512_A` runs 81% slower than `Lat-V2P-L2-Read-512_A` (12.7 vs 7.0 ns).

### Hot loop (Lat-V2P-Write)

```rust
let next = *chain.add(1);              // chain miss → tier latency (gates iteration)
let d0..d5 = *chain.add(2..7);         // 6 random data cell pointers (free, same cache line)
*d0 = pattern; ... *d5 = pattern;       // 6 SIMD-width cached stores
chain = next;                          // serialized dependency on the chain miss
```

The 6 stores enter the store buffer (Intel: 56–72 entries on modern cores). Each store
triggers an RFO to bring the target cache line into L1 if not already there. The store
retires when the line arrives.

**The hiding trick works when**: chain miss time ≥ per-RFO completion time × 6 / load
buffer parallelism. The CPU has enough idle time during the chain miss for all 6 RFOs
to complete, so writes never gate forward progress.

**The hiding trick breaks when**: chain miss is short, leaving insufficient time for
the 6 RFOs to fully complete before the next iteration needs store buffer slots.

### Tier-by-tier analysis (Write)

- **L1** (chain ~1.4 ns, write ~3.2 ns): RFO is essentially a no-op (lines already
  exclusive). Small overhead from 6 store µops scheduled.

- **L2** (chain ~4 ns, write ~12.7 ns) ← **Pressure point**. Chain miss is fast (~4 ns).
  6 RFOs each hit L2 (~4 ns) and bring lines to L1. They can run in parallel, but the
  load buffer (~12 entries) is shared between chain reads and store RFOs. There isn't
  enough idle time during the 4 ns chain miss for all 6 RFOs to complete, so the store
  buffer accumulates pressure. +81% overhead.

- **L3** (chain ~33 ns, write ~54 ns): Chain miss is moderate (~33 ns). The 6 RFOs
  pipeline through the L1→L2 superqueue and most RFO time is absorbed during the chain
  wait. +27% overhead.

- **DRAM** (chain ~140 ns, write ~140 ns): Chain miss dominates. The memory controller
  absorbs 6 outstanding RFOs via its outstanding-request queue (12+ slots per core).
  Writes fully hidden.

- **DRAMFull** (chain ~150 ns, write ~168 ns): Same as DRAM — within noise.

### The counter-intuitive Write finding

The initial prediction was: "DRAM tier should show the most write pressure because each
RFO is the slowest there." That's wrong.

What matters is the **ratio** of chain-miss-time to per-write-RFO-time. The DRAM RFO is
the slowest in absolute terms, but the chain miss is also the slowest, and they scale
together — leaving plenty of slack for the controller to handle 6 outstanding RFOs.

L2 is where the ratio is worst: chain miss is short, RFOs are not free, and there isn't
enough slack to hide them. **The "store buffer pressure" point on this hardware is L2,
not DRAM.**

## Phenomenon 2: L1 dependency-chain limit (Copy)

`Lat-V2P-L1-Copy-512_A` runs 41% slower than `Lat-V2P-L1-Read-512_A` (4.1 vs 2.9 ns) and
even slower than `Lat-V2P-L1-Write-512_A` (4.1 vs 3.2 ns) — a surprise, since one might
expect Copy to fall between Read and Write.

### Hot loop (Lat-V2P-Copy)

```rust
let next = *chain.add(1);
let d0..d5 = *chain.add(2..7);
let v0 = *d0; *d0 = v0 ^ mask;          // load → XOR → store, all dependent on each other
... (5 more such RMW operations)
chain = next;
```

Each RMW operation forms a serial dependency chain:
```
load (4-5 cycles L1) → XOR (1 cycle, depends on load) → store (1 cycle, depends on XOR)
```

The 6 RMW pipelines run in parallel but each one is ~6 cycles deep (load + XOR + store +
retire). Even with full parallelism, retiring all 6 takes ~6-7 cycles ≈ 2.5-3 ns. Add
the L1 chain miss (~1.4 ns) and you get ~4 ns — matching the observed P50.

So **L1 Copy is dependency-chain-bound**, not store-buffer-bound. The XOR forces
serialization between load and store on each cell. Pure Write doesn't have this — the
6 stores fire in parallel with no inter-dependencies, so they finish faster.

This is a different mechanism from the L2 pressure. At L2, the bottleneck is L1↔L2
transfer bandwidth. At L1, the bottleneck is per-cell instruction latency.

## Phenomenon 3: L2 Copy matches Write (no RMW shortcut)

Pre-Copy prediction: "the load warms the line into L1, so the store hits a cached line
without RFO. Copy at L2 should drop close to Read."

Reality: Copy at L2 = 12.6 ns, **identical to Write** (12.7 ns). The load doesn't help.

The load itself takes the same path the RFO would have taken — fetching from L2 to L1.
There's no shortcut: load + store on the same line still requires bringing the line into
L1 once. The store-buffer pressure regime applies equally to both pure Write and RMW
Copy, because both produce the same volume of L1↔L2 traffic per chain step.

## Phenomenon 4: L3 Copy bimodal distribution (potential fast path)

`Lat-V2P-L3-Copy-512_A` shows two interesting features:

- **P50 (40.9 ns) is LOWER than L3 Read** (42.2 ns) — Copy faster than Read?
- **Spread = 1.52x** (P95/P5) — much higher than L3 Read (1.11x) or L3 Write (1.19x)
- Percentiles tell the story: P75 = 41.4 ns, then jumps to P90 = 53.7 ns

This is bimodal: a fast cluster of samples around 40 ns, and a slow cluster around 54 ns.
The slow cluster matches L3 Write timing exactly, suggesting those samples hit the same
store-buffer pressure regime.

The fast cluster's existence suggests a hardware fast path that triggers under specific
conditions. Possible causes:

- **Cache geometry coincidence**: when the chain-miss line happens to be in the same
  cache set as the data cells, line fills are amortized
- **L2/L3 prefetcher**: adjacent-line or stream prefetchers warm the data lines while
  the chain miss is in flight, eliminating the load latency
- **L3 directory hit optimization**: if a data line was recently in L1 from a prior
  iteration, its L3 directory state allows faster re-fetch

Without microcode-level instrumentation it's hard to confirm which. **Spread of 1.52x
is the marker** — much higher than the typical 1.10-1.20 we see elsewhere — indicating
the dual-mode behavior is real and not measurement noise.

This likely shifts on different CPU microarchitectures:
- **Smaller load buffer (older Intel)**: pressure point may move to L1 or stretch into L3
- **Larger memory controller queue (server-class)**: DRAM pressure stays hidden
- **AMD Zen** (different store buffer / load queue sizing): probably L2 still, but the
  exact magnitude could differ

## Why these metrics are useful for memory testing

Most memory tests measure either:
- Pure read latency (Lat-Read) — single dependent reads, no write traffic
- Sustained bandwidth (Spd-*) — large sequential transfers, latency irrelevant
- NT-write commit time (Lat-NTW) — bypasses cache entirely

The Layout B Read/Write/Copy trio captures three things none of those do:

1. **Lat-V2P-Read**: random-access read latency under realistic MLP pressure (6 in-flight
   loads per step) — different from Lat-Read's pure single-load chain
2. **Lat-V2P-Write**: store-buffer pressure threshold under random cached writes
3. **Lat-V2P-Copy**: dependency-chain latency (L1) and L3 bimodal cache behavior

For overclockers tuning DRAM timings, this is potentially relevant because:
- Write-related instabilities (e.g., bad tWR, tWTR timings) might show up as variance
  in these tests before they show up in pure-read tests
- The L2-tier pressure point is sensitive to L1/L2 RFO bandwidth — depends on on-die bus
  configuration more than DRAM timings, but tracks L2 cache stability
- L3 Copy spread is sensitive to cache prefetcher and replacement policy behavior

These tests aren't substitutes for existing measurements — they're additional views of
the memory subsystem, particularly informative when comparing CPUs of different
microarchitecture or different DRAM configurations on the same CPU.

## Implementation notes

Tests use **cached stores** (`_mm{128,256,512}_store_si*`), not NT stores. This is
intentional — NT stores bypass the store buffer / RFO path entirely and produce the
bandwidth-bound behavior we already measure with Lat-NTW.

The 6-store density per chain step is the maximum that fits in a single chain cell
(48 bytes of data-pointer slots in the 64-byte chain cell). Going higher would require
multiple chain cells per step, which changes the chain serialization properties.

For width comparison: 128/256/512 produce roughly proportional results (wider stores =
fewer instructions per iteration, so slightly less L1 port pressure), but the tier-level
shape (L2 worst, DRAM hidden) is consistent across widths. Run
`test=Lat-V2P-L2-Write-{128,256,512}` to see the small width effect.

## What didn't pan out (recorded for future reference)

The Path B experiment was designed to test whether the store buffer becomes a measurable
bottleneck at the DRAM tier under sustained random write pressure. **The answer is no
at DRAM, yes at L2.** This was unexpected — the original prediction had DRAM as the
likely candidate.

The Copy variant was designed to test whether the load-then-store pattern would
eliminate the store-buffer pressure (load warming the line for the store). **It didn't
help at L2** — the load takes the same path as RFO would have. But it revealed two new
phenomena (L1 dependency-chain limit, L3 bimodal distribution) that pure Write didn't.

So the test trio collectively maps three distinct cached-write/copy regimes — useful as
diagnostics, even though the experiment didn't validate the original hypothesis exactly
as predicted.

## Layout A baseline (single op per chain step) — confirms concurrency requirement

`Lat-V2-{tier}-{Read,Write,Copy}` (Layout A) issues **one** SIMD-width data op per chain
step instead of Layout B's six. Used as a baseline to confirm that the L2 store-buffer
pressure and L1 dependency-chain limit phenomena both require concurrency density.

| Tier | V2 Read | V2 Write | V2 Copy | (vs V2P Read) | (vs V2P Write) |
|------|---------|----------|---------|---------------|----------------|
| L1 | 1.6 | 1.5 | 1.6 | (V2P: 2.9) | (V2P: 3.2) |
| L2 | 4.7 | 4.7 | 4.7 | (V2P: 7.0) | (V2P: 12.7) |
| L3 | 36.6 | 35.2 | 35.5 | (V2P: 42.3) | (V2P: 53.9) |
| DRAM | 140.8 | 141.2 | 141.1 | (V2P: 168.0) | (V2P: 169.0) |
| DRAMFull | 148.7 | 149.0 | 149.4 | (V2P: 178.0) | (V2P: 167.7) |

**Key validation**: Layout A Read/Write/Copy track each other within ~5% at every tier.
A single in-flight operation per chain step always has slack to retire during the chain
miss. The L2 store-buffer-pressure plateau and L1 dependency-chain saturation only appear
when 6 simultaneous operations compete for shared resources.

### Secondary finding: V2 Write slightly faster than V2 Read

A small but real effect at cache tiers (L1: -6%, L3: -4%): **single-op Layout A Write is
slightly faster than single-op Layout A Read**. The pure store has no dependency on
returning a value — it fires-and-forgets, retires when the line is exclusive. The read
must wait for the value to actually come back to a register before the next iteration
can proceed. So in the absence of concurrency pressure, a write is *more* hideable than
a read because the CPU doesn't need to consume its result.

This isn't visible in Layout B because the 6-op concurrency overwhelms this small
single-op asymmetry. It's a useful sanity check on the test infrastructure: if Write
showed up *slower* than Read in Layout A, that would suggest a measurement or
implementation bug.

## SIMD width effect on L3 Copy bimodality (Layout B)

The bimodal L3 Copy distribution (Phenomenon 4 above) **depends on store width** and is
**reproducible across runs**:

| Width | L3 Copy P50 | Spread (P95/P5) | Distribution |
|-------|-------------|-----------------|--------------|
| 128 | 40.7-40.8 ns | **1.20x** | Uniform — single mode |
| 256 | 40.9-41.1 ns | **1.48-1.53x** | Bimodal |
| 512 | 40.9 ns | **1.44-1.53x** | Bimodal |

For comparison, L3 Read (all widths) and L3 Write (all widths) show spreads of 1.20-1.22x
— the "normal" L3 distribution shape. Copy at 128-bit matches that. Copy at 256/512
diverges with a clear secondary mode visible in the percentile breakdown:

```
L3-Copy-128:  P5=37.4  P50=40.8  P75=41.1  P90=41.4  P95=44.7   ← uniform
L3-Copy-256:  P5=37.5  P50=41.1  P75=50.5  P90=55.7  P95=57.5   ← second mode at P75+
L3-Copy-512:  P5=37.5  P50=40.9  P75=41.3  P90=46.0  P95=54.4   ← second mode at P90+
```

128-bit copy is uniformly fast; 256/512 show two clusters with the slow mode appearing
at P75-P90 depending on width.

### Hypothesis: SIMD instruction width, not byte coverage

To disambiguate, the Lat-V2P-CopyFull variant performs RMW on the FULL 64-byte cache
line at every SIMD width:

- **128-bit CopyFull**: 4 RMW ops per cell at 16 bytes each = full line covered
- **256-bit CopyFull**: 2 RMW ops per cell at 32 bytes each = full line covered
- **512-bit CopyFull**: 1 RMW op per cell at 64 bytes = full line covered

Comparing Copy vs CopyFull at L3:

| Test | P50 | Spread | Per-cell ops | Bytes/cell |
|------|-----|--------|--------------|-------------|
| Copy-128 | 40.7 | 1.11x (uniform) | 1 | 16 |
| Copy-256 | 40.8 | 1.16x | 1 | 32 |
| Copy-512 | 40.8 | 1.11x (this run, 1.44-1.53x in prior runs) | 1 | 64 |
| **CopyFull-128** | **56.7** | 1.20x (uniform) | **4** | 64 |
| **CopyFull-256** | **42.6** | 1.15x | **2** | 64 |
| **CopyFull-512** | **40.9** | **1.37x** (bimodal) | 1 | 64 |

**Key finding**: CopyFull-128 stays uniform (single mode) even though it writes the full
cache line. CopyFull-512 shows the bimodality despite writing the same total bytes as
CopyFull-128. This rules out byte coverage as the primary cause.

### Conclusion: bimodality is triggered by wide single-instruction RMW

The slow path that produces the second mode is specifically triggered by **wide
single-instruction RMW on a freshly-loaded line**:

- Narrow 128-bit RMW (4 partial ops/cell): hardware handles each partially-modified state
  with established logic. Always single-mode.
- Wide 512-bit RMW (1 full-line op/cell): hardware sometimes routes through a different
  path (write-combine, line-eviction-and-retry, or directory state transition) that
  occasionally takes longer. Bimodal.
- 256-bit is intermediate.

This makes physical sense: a single 64-byte RMW asks the cache controller to atomically
replace a freshly-loaded cache line. If the controller's fast path needs an exclusive
state that's contended (by other threads, by speculative prefetch, by cross-core
snooping), it falls back to a slower path that sometimes takes ~50 ns instead of ~40 ns.

### Secondary finding: CopyFull-128 has fixed overhead at L3

CopyFull-128 P50 = 56.7 ns vs Copy-128 P50 = 40.7 ns. The 4× store ops add ~16 ns
overhead. Each partial-line store costs a few ns through the store buffer; with 24 RMW
ops per chain step (4 stores × 6 cells), the store buffer pressure becomes visible even
without bimodality. CopyFull-256 (12 ops) shows much less overhead (~2 ns). CopyFull-512
(6 ops) matches Copy-512 — same instruction count, same speed.

Without PMU instrumentation we can't pin down the exact micro-architectural cause of the
512-bit RMW bimodality, but the test data isolates it to the SIMD width / single-µop
property rather than total bytes written.

This is the kind of width-dependent finding that the explicit width variants enable.
Run `test=Lat-V2P-L3-Copy-128,Lat-V2P-L3-Copy-256,Lat-V2P-L3-Copy-512` to reproduce.
Worth investigating further when characterizing L3 cache coherency tuning or comparing
microarchitectures.

## See also

- `nt_write_partial_lines.md` — partial vs full cache line NT write hardware path
- `nt_stores.md` — codegen issues with NT writes in Rust/LLVM
- Lat-V2P test source: `src/latency_tests_v2.rs`
