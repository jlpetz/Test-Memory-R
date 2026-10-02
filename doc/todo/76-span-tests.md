# TODO 76. [TMR-APP] Tests on one VA span per thread

Entry: `TODO.md` 76. This file holds the reasoning and measurements the entry points to.

## Moved from the entry (2026-10-03, verbatim)

Chunk sizing, tied here 2026-09-29 (the user):
- A chunk bigger than a thread's memory (not even one chunk fits) must error with the reason, or be clamped with a warning; decide which. Today it is silently capped to the window (`tests.rs` ~867) and then to each block, which §5 forbids.
- `ChunkMode::Auto` gives every test 8 MB: `calculate_optimal_block_for_test` (`tests.rs` ~943) only matches retired names (`MirrorMove128`, `RefreshStable`...). It is the latency tests' chunk and the generated JSON configs' `default_chunk`. Replace it with per-test defaults, as TM5 sets a block size per test, or delete it. Mem-Refresh sleeps 64 ms per chunk, so small chunks multiply its run time.
- A window smaller than a block is spread one power-of-two piece per block (`prepare_blocks_for_window`), down to KiB slivers: Mem-Refresh, 8 pieces on 1 GiB blocks vs 5 on stitched, 11.7 vs 16.4 GiB/s.
- TM5 `.cfg` block sizes 1-3 are #79.

Bandwidth and latency tests, from #69 D (2026-10-01):
- `bandwidth_tests.rs` and `latency_tests*.rs` (~15 macros) hand-roll what `TestRunner` already provides: window sizing + block prep, the cycle/time gate, the 250 ms progress throttle, the shutdown check. Port them here, since this item reworks their memory prep anyway. `TestRunner` is a toolkit, so they skip the error methods; latency builds its own `LatencyTestStats`. Asm check per macro.

## Power-of-two audit (2026-10-03)

The user asked whether chunks must be powers of two for fast math (masks, shifts), or whether that
comes from the old allocator's fixed power-of-two blocks. A read-only sweep covered every kernel:
`tests.rs`, `test_harness.rs`, `test_scaffolding.rs`, `pattern_gen.rs`, `simd.rs`,
`bandwidth_tests.rs`, `latency_tests_v2.rs`. Items marked (checked) were re-read by hand; line
numbers are approximate.

**Answer: it comes from the allocator and the size producers, not the kernels.** One kernel needs a
power-of-two size. The rest need a multiple of 64 B (vector width and cache line) and line-aligned
chunk starts.

**Needs a power of two:**
- Mem-Random (checked, `tests.rs` ~2190): panics unless the block test size is a power of two, and
  indexes with `rng & (len - 1)`. Fix: multiply-high range reduction,
  `((rng as u128 * len as u128) >> 64) as usize`, one multiply in a latency-bound loop.
- Parameters, not sizes: `rng_sequences` (`trailing_zeros`) and `subdivisions` (a shift). Both are
  checked in `runner.rs`.

**Needs a multiple of N (64 B unless noted):**
- SimpleV2, MirrorV2 and SimpleNT SIMD loops step whole vectors with no scalar tail. A length that
  isn't a multiple overruns `chunk_end`, past the block on the last chunk. A misaligned chunk start
  is a misaligned `std::simd` deref: UB, and an aligned load faults.
- StuckBit and Refresh SIMD, SimpleV2 strided, and the `Spd-*` loops: a tail under one vector is
  skipped or overrun.
- Bench-Init and Bench-Verify mode TM5-2: the pattern chain carried across chunks diverges at a
  chunk end mid-line, giving false errors.
- BlockMove with `copy_directions` 4: the `chunk % 4` tail is never copied but is verified, giving
  false errors.
- Mem-Stride: the chunk must divide by `subdivisions` x 8 B, or the tail is skipped.
- `flush_range_to_dram`: line-aligned base (caller-owned since TODO 79 B3).
- MirrorV2 subblocks (checked, `mirror_swap_subblocks!`): `sub_size = chunk_len / n` must be a
  multiple of the vector width. **Latent bug today:** `Mem-MirrorV2` accepts n = 3 (`config.rs`
  ~520), and a power-of-two chunk / 3 gives misaligned SIMD addresses. No shipped `.cfg` uses 3
  (both use 2 and 4). A chunk that is a multiple of 3 x 64 B would fix it.

**Where power-of-two sizes come from (producers only):** `calculate_ideal_chunk_size` (`tests.rs`
~1048, rounds up: 440 MiB and 293 MiB both become 512), the minimum chunk (~938),
`prepare_blocks_for_window` (~1117, rounds window pieces down), the TM5 import's
`nearest_power_of_two` (`config.rs`), and the request lengths in stitched `power_of_two_split` and
allocator/fill `prev_power_of_two`. Pattern generation keys on block-relative index bits, not on
sizes. Error-check intervals are powers of two by construction and size-independent.

**No dependency:** the `run_phased_test` outer loop (`.min(len)` on the tail), CacheBust (real `%`),
the scalar variants, `latency_tests_v2.rs`, `simd.rs`.

**What dropping power-of-two sizes needs:**
- Change the producers above, and give Mem-Random the range reduction.
- Keep an unconditional granularity. Every chunk mode skips its 64 B rounding when
  `allow_misaligned` is set (checked, `tests.rs` ~815-850); today the power-of-two round-up covers
  for it. 4 KiB or 2 MiB granularity also covers subblocks and subdivisions with power-of-two
  parameters; n = 3 still needs a 3 x 64 B multiple.
- `ChunkMode::Fraction` is a fraction of each block's test size, not of the window: the scaffolding
  passes the block's test size as the window (`test_scaffolding.rs` ~105).

## `ChunkMode::Fraction` probably goes (the user, 2026-10-03)

On one span per thread there is no block share left to take a fraction of. A fraction of the window
doesn't fit either: the window exists to limit how much memory a test covers, to shorten the run,
and shouldn't also set the chunk. So drop `Fraction` with this item. Its users today, each needing a
replacement:
- TM5 block size 0 ("the whole window"): the importer's `chunk_spec` and the built-in TM5 sequence
  (`config.rs` ~1105, ~1406) use `fraction(1.0)`. Needs an explicit whole-window chunk, or the
  window size resolved at import.
- The built-in default suite: five tests at `fraction(0.0625)` (`runner.rs` ~1705-1756), the
  generated default config (`config.rs` ~880) and `demo_comprehensive_test.json`. Need absolute
  sizes.
- The `config.rs` test asserting `Fraction { 1.0 }` for code 0 (~1583).
