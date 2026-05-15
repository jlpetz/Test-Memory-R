# Non-Temporal Writes: Partial vs Full Cache Line Behavior

**Date**: 2026-05-15
**Test platform**: AWS EC2 r8i.2xlarge (Intel Xeon 6975P-C, AVX-512), 6 threads
**Related**: `nt_stores.md` (covers the LLVM `!nontemporal` metadata problem)

## Summary

When using non-temporal (NT) writes to commit data to DRAM, **only stores that fill an
entire 64-byte cache line take the optimized hardware path**. NT stores narrower than a
cache line all pay a fixed per-line overhead that's independent of how many bytes are
written, producing a sharp performance cliff between AVX-512 (64B) and everything below.

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

Either path introduces a fixed-cost per-line transaction, on top of whatever the controller
must do for the actual byte-level writes. **The size of the partial fill (8/16/32 bytes)
doesn't matter** — once you're below the 64-byte threshold, you're on the slow path.

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

For TMR's memory testing purposes:

- **Sustained NT-write commit time** (`Lat-NTW-DRAM-Write-512`) ≈ 5.6 ns/op = the most
  optimistic case for write throughput-divided-by-op. Useful as an upper bound on memory
  controller streaming capability.

- **Partial-line NT writes** (Scalar/128/256) all collapse to the same ~25.6 ns plateau
  because they all hit the same RMW/byte-write fallback path. None of them should be
  taken as "scalar latency" or "128-bit latency" — they're "partial-line NT path latency,"
  which has a single hardware-determined cost regardless of fill size.

- This is **not** measurable with cached writes (Spd-* tests use cached stores, which
  go through L1 and only commit eventually via cache eviction). NT writes are the only
  operation that lets you observe the memory controller commit path directly.

## Memory timings affect this

The 5.6 ns and 25.6 ns numbers are specific to this hardware. Tightening or loosening
DRAM timings (tCL, tRCD, tRP, tWR, tWTR) directly shifts both numbers — both reflect
real DRAM controller activity. An overclocker comparing stable vs unstable timings would
see these numbers move in lockstep with their DIMM configuration.

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

; 128-bit (Lat-NTW-DRAM-Write-128)
.LBB809_27:
    mov     r10, qword ptr [rcx + r9]
    #APP
    movntdq xmmword ptr [r10], xmm10    ; 16-byte NT store
    #NO_APP
    add     r9, 8
    cmp     rax, r9
    jne     .LBB809_27

; 256-bit (Lat-NTW-DRAM-Write-256)
.LBB810_27:
    mov     r10, qword ptr [rcx + r9]
    #APP
    vmovntdq ymmword ptr [r10], ymm2    ; 32-byte NT store
    #NO_APP
    add     r9, 8
    cmp     rax, r9
    jne     .LBB810_27

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

The pattern register (xmm/ymm/zmm/rbp) is hoisted out of the loop — only the address
fetch and the NT store change per iteration. The 5 instructions per iteration are
identical across all four variants; the only difference is the width of the NT store.

This means the 4.6× speedup at 512-bit can only come from hardware: the CPU is doing the
same work per iteration in software, but the memory subsystem treats a 64-byte NT store
fundamentally differently from anything narrower.

## See also

- `nt_stores.md` — covers the broken `core::intrinsics::nontemporal_store` and the
  `std::arch` + manual unroll workaround used in Spd-* tests
- `Lat-NTW-DRAM-Write-*` test source: `src/latency_tests_v2.rs`
