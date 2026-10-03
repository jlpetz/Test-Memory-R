# TODO 76. [TMR-APP] Tests on one VA span per thread

Entry: `TODO.md` 76. This file holds the reasoning and measurements the entry points to.

## Moved from the entry (2026-10-03, verbatim)

Chunk sizing, tied here 2026-09-29 (the user):
- A chunk bigger than a thread's memory (not even one chunk fits) must error with the reason, or be clamped with a warning; decide which. Today it is silently capped to the window (`tests.rs` ~843) and then to each block, which §5 forbids.
- `ChunkMode::Auto` gives every test 8 MB: `calculate_optimal_block_for_test` (`tests.rs` ~919) only matches retired names (`MirrorMove128`, `RefreshStable`...). It is the latency tests' chunk and the generated JSON configs' `default_chunk`. Replace it with per-test defaults, as TM5 sets a block size per test, or delete it. Mem-Refresh sleeps 64 ms per chunk, so small chunks multiply its run time.
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
- Mem-Random (checked, `tests.rs` ~2160): panics unless the block test size is a power of two, and
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
  multiple of the vector width. **Latent crash today:** n = 3 on a power-of-two chunk gives
  misaligned addresses, and the swaps compile to aligned moves (`vmovdqa`, `vmovdqa64`): every
  256/512-bit swap faults, and 128-bit faults on 2^odd-byte chunks. It's in the SIMD variants
  (`-128/-256/-512/-Auto`); scalar `Mem-MirrorV2` ignores the subblock count. Reachable through
  JSON `Mem-MirrorV2-Auto` with parameter 3 or CLI `parameter=subblocks:3`; no shipped `.cfg`
  reaches it. n = 3 is a valid TM5 mode, so the config layer is right to accept it. The fix is
  TM5's: round each subblock down to 128 B and leave the tail (`mtests0.asm` ~1307-1314). Not, as
  first written here, a chunk that is a multiple of 3 x 64 B. (Corrected 2026-10-03.)

**Where power-of-two sizes come from (producers only):** `calculate_ideal_chunk_size` (`tests.rs`
~1015, rounds up: 440 MiB and 293 MiB both become 512), the minimum chunk (~911),
`prepare_blocks_for_window` (~1064, rounds window pieces down), the TM5 import's
`nearest_power_of_two` (`config.rs`), and the request lengths in stitched `power_of_two_split` and
allocator/fill `prev_power_of_two`. Pattern generation keys on block-relative index bits, not on
sizes. Error-check intervals are powers of two by construction and size-independent.

**No dependency:** the `run_phased_test` outer loop (`.min(len)` on the tail), CacheBust (real `%`),
the scalar variants, `latency_tests_v2.rs`, `simd.rs`.

**What dropping power-of-two sizes needs:**
- Change the producers above, and give Mem-Random the range reduction.
- Keep an unconditional granularity. The 64 B rounding in `calculate_chunk_size` is unconditional
  since `allow_misaligned` went (2026-10-03). 4 KiB or 2 MiB granularity also covers subdivisions;
  subblocks need the 128 B round-down above.
- (Found then, gone now: `ChunkMode::Fraction` was a fraction of each block's test size, not of the
  window, because the scaffolding passes the block's test size as the window.)

## `ChunkMode::Fraction` removed (decided by the user, done 2026-10-03)

On one span per thread there is no block share left to take a fraction of. A fraction of the window
doesn't fit either: the window exists to limit how much memory a test covers, to shorten the run,
and shouldn't also set the chunk. TMR is prerelease, so there is no compatibility to keep. The
user, on the importer's `fraction(1.0)`: "that's horrible and we should 100% fix that". Its users
were:
- TM5 block size 0 ("the whole window"): the importer's `chunk_spec`, and the demo config's
  TM5-style test, used `fraction(1.0)`.
- The built-in default suite: the five Mem-StuckBit variants at `fraction(0.0625)` over a full
  allocation window, and the generated demo config. That was 1/16 of each block piece: 512 MiB on
  an 8 GiB block, down to 2 MiB on a 32 MiB one.

Done 2026-10-03:
- TM5 block size 0 imports as the `.cfg`'s own `Testing Window Size` through the same rounding as
  the other codes (TM5's window isn't TMR's, so "whole TMR window" would be wrong). Every imported
  chunk is capped at the largest power of two in the window, the largest piece
  `prepare_blocks_for_window` can give, so nothing changes at run time: 880 MiB -> 512 MiB,
  1536 MiB -> 1024 MiB. **That cap encodes today's piece limit; drop it with the clamps.**
- The default StuckBit tests and the generated demo config use 512 MiB, clamped to each piece.
  The SIMD StuckBit variants count one error per failing chunk per phase (scalar `Mem-StuckBit`
  counts every bad word), so they report up to 8-16x fewer errors for the same fault, and all five
  run their halt and shutdown checks once per chunk, that much less often.
- `allow_misaligned` went in the same change.
- A second "whole window" spelling remains: chunk `Cache { DRAMFull }` returns `usize::MAX` as a
  "use the whole window" sentinel (`calculate_chunk_size`'s `ChunkMode::Cache` arm; the chunk
  formatter's "full window").

## More for the rework (2026-10-03)

- **Cache-targeted chunks overshoot.** `calculate_ideal_chunk_size` rounds up to a power of two, so
  a `ChunkMode::Cache` target such as a 1.25 MiB L2 becomes 2 MiB and can land in the next tier.
- **Tests that divide a chunk into parts** (MirrorMove halves and subblocks, BlockMove halves and
  quarters, Spd-Copy halves) need each part to be a multiple of the vector width at least (the
  user: copy-style tests have the same need). TM5 rounds the whole block to twice what it moves
  from each end (128 B for MirrorMove, 256 B for MirrorMove128), so halves and quarters stay whole;
  only its 3-way split rounds each part, down to 128 B, leaving the tail. Subblocks are TODO 85's
  first step.
- **StuckBit's default chunk** (the user, 2026-10-03: park it here, with the chunking work). It went
  from 1/16 of each block piece to 512 MiB. The SIMD variants count one error per failing chunk per
  phase (scalar `Mem-StuckBit` counts every bad word), so they report up to 8-16x fewer errors for
  the same fault. All five run their halt and shutdown checks once per chunk: about 3 GiB of
  traffic per thread between checks, against 384 MiB before. Settle the size, and whether the SIMD
  variants should count failing words like the scalar one, with the rest of the chunk rules.
- **Mem-Random's hot loop** needs three changes together: multiply-high range reduction instead of
  the mask (for non-power-of-two sizes), the accumulator pattern instead of a per-element branch,
  and the `log::error!` out of the loop (its arguments are spilled to the stack every iteration).
