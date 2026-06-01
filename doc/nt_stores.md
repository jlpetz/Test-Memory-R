# Non-Temporal Stores in TMR — Implementation Notes

**Date**: 2026-03-27 (NT codegen), 2026-05-15 (partial-line behavior), 2026-06-01 (merge + strategy)
**Rust**: 1.95.0-nightly (LLVM 22.1.0) for codegen findings
**Test apps**: `../nt-test/` (NT codegen), `../clflush-test/` (flush benchmarks)

This doc covers how TMR emits non-temporal (streaming) stores, why the implementation
looks the way it does, the hardware cliff between partial and full cache-line NT writes,
and — at the end — the cross-cutting strategy for *which* cache-control tool to reach for
(NT writes vs CLFLUSHOPT vs workset>L3) and why we avoid mixing inline-asm and intrinsic
forms in one hot loop.

## Summary

TMR's `Mem-SimpleNT-*` tests use non-temporal stores to bypass the CPU cache and write
directly to DRAM. This eliminates read-for-ownership (RFO) traffic, improving write
bandwidth by ~46% over regular stores in multi-threaded tests.

The implementation uses **`std::arch` intrinsics with manual 4x loop unrolling**. This is
not a stylistic choice — it is the only approach that produces correct non-temporal store
instructions in the compiled binary.

## The Problem

There are three ways to emit NT stores in Rust. None of them give us both correct NT
instructions AND LLVM-driven loop optimization:

| Approach | Correct NT? | LLVM optimizes? | Why |
|----------|:-----------:|:---------------:|-----|
| `core::intrinsics::nontemporal_store` | NO | YES | LLVM strips `!nontemporal` metadata during optimization |
| `std::arch` intrinsics (`_mm512_stream_si512`) | YES | NO | Emits inline asm (`#APP`/`#NO_APP`) — opaque to LLVM |
| `std::arch::asm!("vmovntdq ...")` | YES | NO | Same — inline asm is opaque |

### core::intrinsics::nontemporal_store — Broken

`core::intrinsics::nontemporal_store` lowers to an LLVM store with `!nontemporal`
metadata. LLVM is free to optimize this like any other store — unrolling, reordering,
vectorizing. However, one or more LLVM optimization passes strip the `!nontemporal`
hint, causing all stores to become regular `vmovdqa64`/`vmovaps`.

LLVM bug [#56703](https://github.com/llvm/llvm-project/issues/56703) fixed the
`ArgumentPromotionPass` stripping `!nontemporal`, but other passes still strip it.
Tested with LLVM 22.1.0 — still broken for all cases:
- Scalar u64 → `movups` (regular, auto-vectorized to 128-bit)
- u64x8 constant → `vmovaps` (regular)
- u64x8 with compute (idx ^ base) → `vmovdqa64` (regular)

### std::arch intrinsics — Correct but no auto-unroll

`_mm512_stream_si512` and friends emit the correct `vmovntdq` instruction but as inline
assembly. **The std::arch intrinsics are themselves inline `asm!` inside stdarch** (see
stdarch PR #1541, merged 2024, which switched from droppable `!nontemporal` metadata to
asm for soundness). LLVM treats the `#APP`/`#NO_APP` block as opaque and won't:
- Unroll the loop (each iteration has 1 store)
- Reorder stores across asm boundaries
- Software-pipeline multiple stores

A single NT store per loop iteration means the CPU spends most of its time in loop
overhead (branch, increment, compare) rather than issuing stores. This particularly hurts
wider SIMD where the store itself is fast but the overhead is fixed.

### LLVM -unroll-count flag — Works but suboptimal

`RUSTFLAGS="-C llvm-args=-unroll-count=4"` forces LLVM to unroll loops containing inline
asm. This produces correct NT stores at 4 per iteration, BUT:
- Generates extra `leaq` instructions between each `#APP` block (can't merge addresses)
- Is a **global** flag affecting every loop in the binary, not just NT loops
- Could over-unroll verification loops, allocator code, etc.

## The Solution

**Manual 4x unroll with `std::arch` intrinsics** in the `simple_write_nt_positional_simd!`
macro. This gives:
- Correct `vmovntdq` instructions (verified via `cargo rustc --emit asm`)
- 4 stores per loop iteration with clean address offsets
- No global flags or side effects on other code
- Per-width: `movntdq xmmword` (128), `vmovntdq ymmword` (256), `vmovntdq zmmword` (512)

## Why Macros, Not Generic Traits

`#[target_feature(enable = "avx512f")]` does not propagate through function call
boundaries. A generic function `fn write_nt<T: SimdOps>(...)` called from a
`#[target_feature(enable = "avx512f")]` function does NOT inherit AVX-512. The generic
function compiles at the baseline ISA (x86-64-v2 in our case).

`#[inline(always)]` is not a guarantee — it's a strong hint. If inlining fails at any
level of the trait impl chain, the code silently falls back to baseline ISA with
catastrophic performance impact.

Macros physically expand code at the call site inside the `#[target_feature]` function,
guaranteeing correct codegen. This is ugly but zero-risk.

## Unroll Factor Benchmark

Tested in `../nt-test/` with 512 MiB buffer, single-threaded, pure constant writes:

| Unroll | 128-bit | 256-bit | 512-bit |
|--------|---------|---------|---------|
| 1x | ~24,000 | ~24,100 | ~24,000 |
| 2x | — | — | ~24,100 |
| 4x | ~24,200 | ~24,300 | ~24,200 |
| 8x | ~24,100 | ~24,400 | ~24,200 |
| 16x | — | — | ~24,100 |

All factors hit the same ~24 GiB/s memory bandwidth ceiling on a single thread. The unroll
factor is irrelevant for pure writes — the memory controller is the bottleneck, not the
CPU pipeline.

In multi-threaded TMR with interleaved pattern compute + verification, 4x provides enough
pipeline depth to hide loop overhead without excessive code bloat or instruction cache
pressure.

## Width Performance (Multi-Threaded TMR)

From TMR `--quick-test` with 4x unroll:

| Width | Throughput | Notes |
|-------|-----------|-------|
| 128-bit | ~69,500 MiB/s | Best — 4 stores fill 1 cache line, clean WCB drain |
| 256-bit | ~68,400 MiB/s | ~1.5% slower — 2 cache lines per iteration |
| 512-bit | ~58,300 MiB/s | ~16% slower — 4 cache lines per iteration, WCB pressure |

512-bit NT stores fill write-combine buffer entries faster (64B = 1 full entry per store)
than the memory controller can drain them. This is a hardware characteristic of the
WCB→DRAM path, not fixable in software.

For comparison, v2 temporal (regular) stores: 128=47,100, 256=45,400, 512=42,500. NT
stores provide +46% bandwidth at 128-bit by eliminating RFO traffic.

---

# Partial vs Full Cache Line NT Writes

**Test platform**: AWS EC2 r8i.2xlarge (Intel Xeon 6975P-C, AVX-512), 6 threads

(Originally a separate doc, `nt_write_partial_lines.md`, merged here 2026-06-01.)

## Summary

When using NT writes to commit data to DRAM, **only stores that fill an entire 64-byte
cache line take the optimized hardware path**. NT stores narrower than a cache line all
pay a fixed per-line overhead that's independent of how many bytes are written, producing
a sharp performance cliff between AVX-512 (64B) and everything below.

Validated by `Lat-NTW-DRAM-Write-{Scalar,128,256,512}` tests:

| Variant | Bytes/store | Fill ratio | P50 latency |
|---------|-------------|------------|-------------|
| Scalar (`MOVNTI`) | 8 B | 12.5% | 25.6 ns |
| 128-bit (`MOVNTDQ` xmm) | 16 B | 25% | 25.6 ns |
| 256-bit (`VMOVNTDQ` ymm) | 32 B | 50% | 25.6 ns |
| 512-bit (`VMOVNTDQ` zmm) | 64 B | 100% | **5.6 ns** |

Identical loop structure across all four variants (verified via emitted assembly — see
"Codegen Validation" below). The only thing that changes is the NT store instruction
itself. The 4.6× speedup at 512-bit is pure hardware behavior, not codegen artifact.

## Why partial-line NT writes are slow

DRAM operates on 64-byte transfers — it cannot natively write less than one cache line.
When an NT store is narrower than a cache line, the hardware has two choices:

1. **Read-Modify-Write**: read the existing cache line from DRAM, merge the partial bytes
   into it, write the line back. This effectively becomes a read + a write, doubling
   memory traffic relative to a true write-only operation.

2. **Byte-enabled partial writes**: the memory controller buffers partial fills in a
   write-combine buffer (WCB) and flushes once the line completes. If the line never
   completes (e.g., random scatter writes that touch each line only once), the WCB drains
   the partial line as-is using DRAM's byte-mask write commands — but these commands carry
   their own latency overhead at the DIMM level.

Either path introduces a fixed-cost per-line transaction, on top of whatever the
controller must do for the actual byte-level writes. **The size of the partial fill
(8/16/32 bytes) doesn't matter** — once you're below the 64-byte threshold, you're on the
slow path.

## Why 512-bit NT writes are fast

A 64-byte AVX-512 NT store fills an entire WCB entry in a single instruction. The memory
controller can:

- Skip read-for-ownership entirely (no need to fetch the existing line)
- Skip line-merge logic (the new line is complete)
- Issue a single direct write transaction to DRAM

This is the architecturally intended fast path for streaming writes — designed for
`memcpy`-style code that fills buffers sequentially with full cache lines. AVX-512 NT
stores are the only single-instruction way to hit this path on x86; AVX2 gets there only
with two consecutive 32-byte stores to adjacent halves of the same line, which still works
but requires the controller to recognize the pattern.

## Implications for memory testing

- **Sustained NT-write commit time** (`Lat-NTW-DRAM-Write-512`) ≈ 5.6 ns/op = the most
  optimistic case for write throughput-divided-by-op. Useful as an upper bound on memory
  controller streaming capability.

- **Partial-line NT writes** (Scalar/128/256) all collapse to the same ~25.6 ns plateau
  because they all hit the same RMW/byte-write fallback path. None of them should be taken
  as "scalar latency" or "128-bit latency" — they're "partial-line NT path latency," which
  has a single hardware-determined cost regardless of fill size.

- This is **not** measurable with cached writes (Spd-* tests use cached stores, which go
  through L1 and only commit eventually via cache eviction). NT writes are the only
  operation that lets you observe the memory controller commit path directly.

## Memory timings affect this

The 5.6 ns and 25.6 ns numbers are specific to this hardware. Tightening or loosening DRAM
timings (tCL, tRCD, tRP, tWR, tWTR) directly shifts both numbers — both reflect real DRAM
controller activity. An overclocker comparing stable vs unstable timings would see these
numbers move in lockstep with their DIMM configuration.

The constant ratio between partial and full-line paths (~4.5×) is a property of the
controller's microarchitecture and won't change with timings — only the absolute numbers
do.

## Codegen Validation

All four variants emit a single NT store instruction inside a tight loop with identical
structure. Emitted via `cargo rustc --release --lib -- --emit=asm`:

```asm
; Scalar (Lat-NTW-DRAM-Write-Scalar)
.LBB814_27:
    mov     r10, qword ptr [rcx + r9]   ; load address from table
    #APP
    movnti  qword ptr [r10], rbp        ; 8-byte NT store
    #NO_APP
    add     r9, 8
    cmp     rax, r9
    jne     .LBB814_27

; 512-bit (Lat-NTW-DRAM-Write-512)
.LBB811_27:
    mov     r10, qword ptr [rcx + r9]
    #APP
    vmovntdq zmmword ptr [r10], zmm1    ; 64-byte NT store
    #NO_APP
    add     r9, 8
    cmp     rax, r9
    jne     .LBB811_27
```

The pattern register (xmm/ymm/zmm/rbp) is hoisted out of the loop — only the address fetch
and the NT store change per iteration. The 5 instructions per iteration are identical
across all four variants; the only difference is the width of the NT store. The 4.6×
speedup at 512-bit can only come from hardware: the CPU does the same work per iteration in
software, but the memory subsystem treats a 64-byte NT store fundamentally differently from
anything narrower.

---

# Cache-Control Strategy: Which Tool, and Why Not to Mix Asm Forms

This section ties together the recurring optimization question: given NT writes,
CLFLUSHOPT, and "just make the workset bigger than L3," which do you reach for, and how do
they interact at the codegen level.

## The `#APP`/`#NO_APP` churn cost (why we don't freely mix intrinsic and asm)

Every inline-asm block — whether you wrote it as `core::arch::asm!` or it came from a
`std::arch` intrinsic that is *itself* asm internally (all the `_mm*_stream_*` NT stores,
and the `_mm_clflushopt` intrinsic once stdarch PR #2141 lands) — emits an `#APP` /
`#NO_APP` fence in the instruction stream. LLVM treats everything between those fences as
an opaque blob: it will **not** schedule, reorder, unroll, or hoist across the boundary.

That has a real, measured cost when you go **in and out** of asm repeatedly in one hot
loop — e.g. `intrinsic → your-asm → intrinsic`. Each transition reintroduces a fence, so
the optimizer is fragmented into tiny islands and can't pipeline the loop. We hit exactly
this with the NT-store work: a single NT store per iteration (one `#APP` island) left the
CPU stalled in loop overhead, which is the whole reason the manual 4× unroll exists — it
puts 4 stores inside *one* `#APP`-bounded macro expansion rather than 4 separate islands.

**Practical rule:** within a single hot loop, keep all the asm-emitting operations in the
**same idiom and the same `#APP` region** where possible. Don't alternate between an
intrinsic and a hand-written `asm!` for related operations — that maximizes the churn.

## Consequence: we keep BOTH the intrinsic and the asm form of CLFLUSHOPT

Once stdarch PR #2141 syncs `_mm_clflushopt` into nightly, we will have two ways to emit
the instruction. They produce **identical machine code**, so the choice is purely about
loop context:

- **Standalone flush loop** (today's `flush_range_to_dram`: flush phase runs *after* the
  write phase, *before* the verify) — the intrinsic and the asm schedule identically
  because there's nothing else in the loop to schedule against. Either is fine; we keep the
  `asm!` form for now because the intrinsic isn't in nightly yet, and it needs no extra
  consideration.

- **Flush interleaved with NT stores in one loop** (plausible for some future patterns) —
  here the NT stores are already asm islands. Using the `asm!` form of CLFLUSHOPT keeps the
  flush in the *same* idiom, so you reason about one consistent set of `#APP` boundaries
  instead of mixing intrinsic-asm and your-asm and paying extra transition churn. This is
  the case where keeping the asm form is an actual decision, not just inertia.

So the asm form of CLFLUSHOPT is not legacy to be removed when the intrinsic lands — it's
the right tool specifically for mixed flush+NT-store loops. The intrinsic is the cleaner
choice for any standalone use once it stabilizes.

## Decision guide: NT writes vs CLFLUSHOPT vs workset > L3

The goal in most TMR correctness tests is: **the verify-read must observe what actually
landed in DRAM, not a cache-resident copy that silently masks a DRAM bit error.** (This is
"most of the time" — cache-tier-targeted tests like `Spd-L2-*` deliberately stay resident;
don't flush those.) The three tools attack different parts of the problem:

| Tool | Defeats cache on... | Cost | Guarantee | Reach for it when |
|------|---------------------|------|-----------|-------------------|
| **Workset > L3** | both sides, *via displacement* | ~free | heuristic only | bulk sequential read/write/copy where later writes evict earlier ones before verify — bandwidth tests, large StuckBit/Simple |
| **NT writes** | **write path only** | ≈ cached for sequential | write bypasses cache; read side NOT covered | you want measured write bandwidth to reflect DRAM, or to commit without polluting cache. Pair with flush/workset for the read side |
| **CLFLUSHOPT + fence** | read side (evict so next load misses to DRAM) | a few % over natural | **always** | quiet wait (refresh/bit-fade), cache-resident workset, or any test that must *guarantee* a DRAM round-trip regardless of size/threads |

Key interactions to remember:

- **NT writes and CLFLUSHOPT are complementary, not alternatives.** NT writes defeat cache
  masking on the *write* side; they do nothing for the *verify-read* side. If a line was
  cached before you NT-wrote it (or gets pulled in by a prefetch), the verify can still hit
  cache. For full coverage, combine NT writes with either workset>L3 or a
  CLFLUSHOPT-before-verify.

- **Workset > L3 is a heuristic, not a guarantee.** It relies on *activity* displacing your
  data. During a quiet sleep (refresh tests) nothing displaces anything, so it fails
  regardless of window size — that's exactly the bug `flush_range_to_dram` fixed. On servers
  with 256MB+ L3 it can also silently fail to spill. Use CLFLUSHOPT when you need certainty.

- **CLFLUSHOPT always needs a trailing fence (MFENCE/SFENCE).** It is weakly ordered; without
  the fence a verify-load can issue while flushes are still draining and read stale cached
  data. `flush_range_to_dram` emits the MFENCE for you.

See `doc/cache_management.md` for the full decision matrix, the per-primitive (CLFLUSHOPT /
MFENCE / SFENCE) reference, and the cache-masking problem worked through in detail.

## Future: When the NT codegen story might change

If LLVM fixes `!nontemporal` metadata preservation across all optimization passes,
`core::intrinsics::nontemporal_store` would work correctly and we could:
1. Drop the manual unroll
2. Use portable SIMD types (`u64x8`) instead of `std::arch` types
3. Let LLVM choose the optimal unroll factor per target

This would also remove the NT stores' `#APP` islands, changing the mixing calculus above —
at that point an interleaved flush+store loop could use the CLFLUSHOPT intrinsic freely.
Track: https://github.com/llvm/llvm-project/issues/56703 (partial fix only).

## See also

- `doc/cache_management.md` — full cache-control decision matrix and primitive reference
- `doc/store_buffer_pressure.md` — how store buffer dynamics affect benchmarks per tier
- `Lat-NTW-DRAM-Write-*` test source: `src/latency_tests_v2.rs`
- `../nt-test/` (NT codegen validation), `../clflush-test/` (flush benchmarks)
