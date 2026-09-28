# TODO 26. [TMR-APP] Mem-Refresh Cache Masking Bug — CLFLUSHOPT Required

> Full record, moved verbatim out of `TODO_ARCHIVE.md` on 2026-09-28 (closed item).
> The short entry in `TODO_ARCHIVE.md` holds the current status; this file holds the reasoning.

**Status**: DONE (2026-05-29)

**Resolution**: Added `flush_range_to_dram` (`tests.rs:1292`, inline `asm!`
CLFLUSHOPT + trailing MFENCE — no `_mm_clflush` fallback; the startup CPUID gate
in `main.rs` guarantees the feature). Inserted a flush of each written chunk
before the per-chunk 64ms sleep in all 4 Refresh variants (scalar `2673`,
128/256/512 at `2885/3116/3347`), so the post-sleep verify round-trips through
DRAM instead of hitting the still-hot cache copy. Window changed from `l2*2` to
`l3*2` (`tests.rs:1067`) for wider retention coverage. Chunk size deliberately
left on the user's ChunkMode (not pinned per-test) — sleep is charged per chunk,
so the window/chunk ratio sets total sleep, but that's a config knob, not a bug.
Note: the dead `RefreshStable` arm in `calculate_optimal_block_for_test`
(`tests.rs:1230`) is unused (real name is `Mem-Refresh`, falls through to 8MB
default) — left in place since chunk sizing stays config-driven.

**Original problem (for reference):**

**Problem**: `refresh_stable_multi` and its SIMD variants (`tests.rs:2562+`) do not
actually test DRAM refresh behavior. Two compounding issues:

1. Window is sized to `cache_info.l2_cache * 2` (~1-2 MB), which fits entirely in
   L3 on any modern CPU.
2. Each chunk does `write → MFENCE → sleep(64ms) → verify` in tight succession on
   the same lines. During the 64ms sleep, *nothing* evicts the lines — the verify
   reads from L1 because nothing displaced it.

The MFENCE is correctly placed (orders writes globally before the sleep), but
ordering is not the issue — cache residency is. The test as written is testing
"does L1/L2 forget things in 64ms" (answer: never) rather than "does DRAM forget
bits during the refresh interval" (the actual question).

**Fix**: insert a `CLFLUSHOPT` loop over the chunk, followed by `MFENCE`, BEFORE
the sleep. Pattern:
```rust
for i in 0..len { *base.add(i) = pattern; }
// flush every cache line in the chunk
for line in 0..line_count {
    _mm_clflushopt((base as *const u8).add(line * 64));
}
_mm_mfence();                    // wait for flushes to drain
sleep(64ms);                     // now data really is in DRAM only
for i in 0..len { /* verify */ } // each load round-trips through DRAM
```

**Feature detection** (as built): no runtime fallback. The startup CPUID gate in
`main.rs` requires CLFLUSHOPT and exits cleanly otherwise, so `flush_range_to_dram`
emits it unconditionally behind `#[target_feature(enable = "clflushopt")]`. There is
**no `_mm_clflush` fallback** — pre-2015 CPUs are out of scope for a DDR5 tester.
MFENCE via `_mm_mfence`. (Detection now uses `is_x86_feature_detected!("clflushopt")`,
gated by the unstable `clflushopt_target_feature`; rustc PR #157098.)

**See**: `doc/cache_management.md` for full strategy guide on cache-control
primitives across all TMR tests.
