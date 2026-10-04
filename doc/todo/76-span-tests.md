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

(The producers and needs below are as found on 2026-10-03; the implementation section at the end
says what changed.)

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
  1536 MiB -> 1024 MiB. **That cap encodes today's piece limit; drop it with the clamps.** (Dropped
  in the implementation below.)
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
  (Fixed below: the chunk now rounds to 4 KiB only.)
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

## Implementation, first step (2026-10-03, committed 2026-10-04)

Design brief: a workflow mapped the design doc, the delivery path, every test's block assumptions
and the stitched span, then proposed this step. The full LogicalSpace / `resolve()` / segment loop
(§3, §5.2) is not built: under stitched every chunk is one segment by construction, so it would
only help the legacy allocator.

What changed:
- **Regions.** `AllocationBlock.joins_next` (set by stitched from address equality) and
  `test_memory::regions`: a stitched thread is one region, a legacy thread one per block.
- **Window pieces.** `test_memory::window_pieces`: the first W bytes of the regions, whole regions
  then the remainder, each a multiple of 4 KiB. No power-of-two slivers: 880 MiB is one piece.
  `TestBlock` holds a pointer, so a piece can span joined blocks.
- **Chunks.** Resolved once per test from the window (`TestMemoryConfig::calculate_chunk_size`):
  the configured size, at least the test's minimum, at most the window, rounded up to 4 KiB. A
  piece shorter than the chunk gets one chunk of its own length (legacy only, at block ends).
  `calculate_ideal_chunk_size`, `get_safe_chunk_size` and the minimum's power-of-two rounding are
  gone. Chunks may cross the 1 GiB to 2 MiB seam.
- **TM5 import.** The power-of-two cap is gone, and block sizes come out as TM5's own: codes 0-3
  are window/(N+1) floored to the `.cfg`'s Lock Memory Granularity (now parsed; 16 MiB in 1usmus,
  64 MiB in Check_absolutnew), MiB values are clamped to the window, then both floor to 4 KiB
  (`MainThread.asm` ~627-661). So 1usmus Test6/Test7 are 432 and 288 MiB, as in TM5.
- **Mem-Random.** Multiply-high index (any length), accumulator plus cold replay; asm checked: no
  mask, branch, call or spill in the loop.
- **Halt** is checked per chunk in `run_phased_test` (one piece can be the whole window), and its
  bytes and operations are counted per chunk, so a halted or interrupted test reports what ran.
- **Mem-Stride `subdivisions`** is capped at 512: its shift no longer divides a 4 KiB-multiple
  chunk past that, and part of each chunk would go untested (found by the review; a test pins it).
- **Window below 4 KiB** logs an error and tests 4 KiB. Before, a 0-byte window tested all of
  block 0 and a 64 B-4 KiB window a power-of-two sliver of it.
- **Unchanged:** the bandwidth and latency tests keep `prepare_blocks_for_window` (power-of-two
  pieces) until their port onto `TestRunner` (this item, from 69 D): latency builds heap chains
  as big as a piece.
- **Per-thread log line** lists each piece with its page sizes and chunks; it went into the
  "before" binary too, so before/after logs compare directly.

Tests: 125 unit tests. The canary runs every built-in correctness and bandwidth test over three
joined heap blocks (640 KiB, not a power of two) with a 68 KiB chunk: zero errors, guard zones
intact, and no window word left untouched by a writer. Others: an injected fault counted once in
log and halt mode with progress reported; Mem-Random's replay against a plain count; the
subdivisions cap. Post-LTO asm: Mem-Random's loop is xorshift, `mulx`, xor-load, `or`, with no
mask, branch, call or spill. Live runs: see the TODO 76 entry.

Decided provisionally, for the user (2026-10-03):
- Q1: §4.6's "chunks are powers of two" replaced by "multiples of 4 KiB" (design doc updated).
- Q2: an oversized chunk is clamped to the window, not an error (built-in Mem-Refresh relies on it).
- Q3 (fidelity, outside 76): TM5 slides its window over all of each core's memory; TMR tests only
  the window. A full-allocation window fed by TM5's block codes would be faithful, at several times
  the run time. Candidate TODO.

Not in this step: a resolved chunk column in the plan table (it shows the spec); `ChunkMode::Auto`
(still 8 MB for unnamed tests); the bandwidth/latency port (from 69 D); chunks across legacy
blocks; per-cycle window rotation.

Found by the review, not changed here:
- Bench-Init/Verify modes 2 and 12 give false errors when `write_read_cycles` > 1: their chain
  state carries across the repeated `test_fn` calls on one chunk (predates this item).
- Mem-Stride under `errors=halt` doesn't halt: `break 'stride_loop` only leaves the piece.
- Tier 2 tests count bytes per piece, so an interrupted one reports less (errors are exact).
- Mem-Random's read budget is per piece, so a stitched thread does fewer reads per cycle; the
  built-in test is time-based, so it runs more cycles instead.
- Under plan-pagesize-pref a partial window now lies in the first blocks, not as slivers in each.

Measured (2026-10-04, half memory, default threads, one run each, before/after binaries
interleaved; 1usmus_v3, Check_absolutnew and a built-in subset under both allocators): 0 errors and
0 WHEA in all 12 runs. The old code under stitched tested 768 of 880 MiB in every 1usmus test and
768 of 977 MiB in built-in Refresh: its power-of-two pieces ran out of blocks. Refresh is 1.2-1.8x
faster (fewer 64 ms sleeps per pass). Strided SimpleTests with large chunks are 0.73-0.95x: they now
run TM5's real block sizes instead of mostly small slivers. Check_absolutnew under legacy has an
identical layout before and after and still swung up to 20% (the noise floor of single runs).
**Bench-Verify-TM5-2 regressed**: ~18k MiB/s before, 12-16.5k after, repeatable on 4 cores with an
identical layout, so codegen; its per-element loop is unchanged in the asm. The user: keep an eye
on it; the loop is rewritten for overlapping chunks anyway (accumulator, no per-element log).

## Next: stitched only (the user, 2026-10-04)

- **Commit to the stitched allocator.** No software overlay for the legacy allocator: it would only
  serve something we will drop. The tests are reworked to one span per thread; removing
  `plan-pagesize-pref` comes as a deletion list for approval. Stitched becomes the default (TODO 80).
- **Full-size chunks covering the whole extent**, overlapping instead of a short last chunk: every
  chunk exactly the configured size. Even spread (§4.5) or one overlap at the end is still to
  settle. Either needs position-pure patterns: Bench modes 2 and 12 (chains) get a jump-ahead.
  The future thread-handover test uses exact tiles with no overlap (§4.7).
- **TM5's window is not TMR's window** (the user). TM5's is an AWE mapping aperture that slides
  over all of a core's memory; TMR has no AWE. A TM5 test covers all memory, so an imported test
  should too; the `.cfg` window size only matters to translate the block-size codes (window/2 ...).
  TM5 runs each block to the end before the next and never repeats a window, so its window has no
  testing purpose to adopt. Renaming TMR's window (the design doc calls it "extent") is open.
- **The latency tests' heap scratch:** they shuffle the chain order in a heap `Vec<usize>` (8 B
  per node: v1 one node per u64, so as big as the tested memory; v2 one per cache line, two
  permutations) before writing the chain into test memory. Build it in place (Sattolo) before they
  get one-piece windows.
