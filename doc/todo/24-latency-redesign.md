# TODO 24. Latency Test Redesign — Layout A/B Pointer Chase + NT Write Experiment ✓ (2026-05-15)

> Full record, moved verbatim out of `TODO_ARCHIVE.md` on 2026-09-28 (closed item).
> The short entry in `TODO_ARCHIVE.md` holds the current status; this file holds the reasoning.


Delivered substantially more than the original two-part scope:

**Layout A** (`Lat-V2-{tier}-{Read,Write,Copy}` × {128/256/512/Auto} = 60 tests):
- Single chain cell + single data cell per block, 1 op per chain step (baseline)
- Bidirectional chain (prev/next), 64-byte clean data cells (full AVX-512 capable)
- Validates that single-op concurrency hides cached writes at all tiers

**Layout B** (`Lat-V2P-{tier}-{Read,Write,Copy}` × {128/256/512/Auto} = 60 tests):
- Packed: 1 chain cell + 6 data cells per block (448B blocks)
- 6 SIMD-width data ops per chain step (high MLP)
- Reveals L2 store-buffer pressure (+81% Write vs Read), L1 dependency-chain limit
  (+41% Copy vs Read), and L3 Copy width-dependent bimodal distribution

**Layout B Full** (`Lat-V2P-{tier}-{WriteFull,CopyFull}` × {128/256/512/Auto} = 40 tests):
- Same chain structure but covers full 64B cache line per cell at every width
  (4 ops × 16B at 128, 2 ops × 32B at 256, 1 op × 64B at 512)
- Proves L3 Copy bimodality is **SIMD-instruction-width driven, not byte-coverage driven**:
  CopyFull-128 stays uniform with full coverage; CopyFull-512 shows bimodality at same
  byte count
- Reveals narrow-store per-op overhead: CopyFull-128 is ~16 ns slower than Copy-128
  due to 24 store-buffer entries vs 6

**Layout C** (`Lat-NTW-DRAM-Write-{Scalar,128,256,512,Auto}` = 5 tests):
- NT streaming write saturation; one tier (NT bypasses cache)
- Demonstrates 4.6× cliff at AVX-512 (5.6 ns vs 25.6 ns) — full vs partial cache line writes
- Scalar/128/256 all hit the same ~25.6 ns plateau — partial-line NT writes share a fixed
  hardware overhead regardless of fill ratio

**Documentation**:
- `doc/store_buffer_pressure.md` — five phenomena across Layout A/B/Full (L2 store-buffer
  pressure, L1 dep-chain limit, L3 Copy bimodality investigation, Layout A baseline,
  CopyFull width-isolation experiment)
- partial vs full-line NT write hardware path — now the "Partial vs Full Cache Line
  NT Writes" section of `doc/nt_stores.md` (merged 2026-06-01)

**Codegen validation**: Verified all width variants emit identical hot-loop structure
(only the SIMD width of load/XOR/store differs). The L3 Copy bimodality is confirmed as
a pure hardware characteristic — wide single-instruction RMW on freshly-loaded lines
sometimes takes a slow path (write-combine, line-eviction-and-retry, or directory-state
transition).

**Total: 165 new tests** (60 Layout A + 60 Layout B + 40 Layout B Full + 5 Layout C)
