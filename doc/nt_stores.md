# Non-Temporal Stores in TMR — Implementation Notes

**Date**: 2026-03-27
**Rust**: 1.95.0-nightly (LLVM 22.1.0)
**Test app**: `../nt-test/` (standalone benchmark for validating approaches)

## Summary

TMR's `Mem-SimpleNT-*` tests use non-temporal (streaming) stores to bypass the CPU
cache and write directly to DRAM. This eliminates read-for-ownership (RFO) traffic,
improving write bandwidth by ~46% over regular stores in multi-threaded tests.

The implementation uses **`std::arch` intrinsics with manual 4x loop unrolling**.
This is not a stylistic choice — it is the only approach that produces correct
non-temporal store instructions in the compiled binary.

## The Problem

There are three ways to emit NT stores in Rust. None of them give us both correct
NT instructions AND LLVM-driven loop optimization:

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

`_mm512_stream_si512` and friends emit the correct `vmovntdq` instruction but as
inline assembly. LLVM treats the `#APP`/`#NO_APP` block as opaque and won't:
- Unroll the loop (each iteration has 1 store)
- Reorder stores across asm boundaries
- Software-pipeline multiple stores

A single NT store per loop iteration means the CPU spends most of its time in loop
overhead (branch, increment, compare) rather than issuing stores. This particularly
hurts wider SIMD where the store itself is fast but the overhead is fixed.

### LLVM -unroll-count flag — Works but suboptimal

`RUSTFLAGS="-C llvm-args=-unroll-count=4"` forces LLVM to unroll loops containing
inline asm. This produces correct NT stores at 4 per iteration, BUT:
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
`#[target_feature(enable = "avx512f")]` function does NOT inherit AVX-512. The
generic function compiles at the baseline ISA (x86-64-v2 in our case).

`#[inline(always)]` is not a guarantee — it's a strong hint. If inlining fails at
any level of the trait impl chain, the code silently falls back to baseline ISA with
catastrophic performance impact.

Macros physically expand code at the call site inside the `#[target_feature]`
function, guaranteeing correct codegen. This is ugly but zero-risk.

## Unroll Factor Benchmark

Tested in `../nt-test/` with 512 MiB buffer, single-threaded, pure constant writes:

| Unroll | 128-bit | 256-bit | 512-bit |
|--------|---------|---------|---------|
| 1x | ~24,000 | ~24,100 | ~24,000 |
| 2x | — | — | ~24,100 |
| 4x | ~24,200 | ~24,300 | ~24,200 |
| 8x | ~24,100 | ~24,400 | ~24,200 |
| 16x | — | — | ~24,100 |

All factors hit the same ~24 GiB/s memory bandwidth ceiling on a single thread.
The unroll factor is irrelevant for pure writes — the memory controller is the
bottleneck, not the CPU pipeline.

In multi-threaded TMR with interleaved pattern compute + verification, 4x provides
enough pipeline depth to hide loop overhead without excessive code bloat or
instruction cache pressure.

## Width Performance (Multi-Threaded TMR)

From TMR `--quick-test` with 4x unroll:

| Width | Throughput | Notes |
|-------|-----------|-------|
| 128-bit | ~69,500 MiB/s | Best — 4 stores fill 1 cache line, clean WCB drain |
| 256-bit | ~68,400 MiB/s | ~1.5% slower — 2 cache lines per iteration |
| 512-bit | ~58,300 MiB/s | ~16% slower — 4 cache lines per iteration, WCB pressure |

512-bit NT stores fill write-combine buffer entries faster (64B = 1 full entry per
store) than the memory controller can drain them. This is a hardware characteristic
of the WCB→DRAM path, not fixable in software.

For comparison, v2 temporal (regular) stores: 128=47,100, 256=45,400, 512=42,500.
NT stores provide +46% bandwidth at 128-bit by eliminating RFO traffic.

## Future: When This Might Change

If LLVM fixes the `!nontemporal` metadata preservation across all optimization
passes, `core::intrinsics::nontemporal_store` would work correctly and we could:
1. Drop the manual unroll
2. Use portable SIMD types (`u64x8`) instead of `std::arch` types
3. Let LLVM choose the optimal unroll factor per target

Track: https://github.com/llvm/llvm-project/issues/56703 (partial fix only)
