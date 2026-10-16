# TODO 97: native patterns and two-tier verify (pattern spec v2)

Modes 20 and 30 are this file's names for 12 and 14, renumbered 2026-10-16 (the same bytes);
the runs below called them 12 and 14.

Built 2026-10-11 on top of 468f23e, committed 2026-10-12 (f69df41). The spec is `doc/pattern_spec.md`;
the plan it came from is `doc/pattern_plan_2026-10-10.md`. Both rest on the audit
(`doc/pattern_audit_2026-10-09.md`) and its follow-up (`doc/pattern_followup_2026-10-10.md`).

## What changed, by the plan's stages

**Stage 0, baseline.** TMR's numbered modes (spec 1) are unchanged and still run when a config
sets `pattern_mode`. `patterns=legacy` imports a TM5 `.cfg` with them. The A/B below runs both
under one binary.

**Stage 1, trustworthy results.**
- **SIMD dispatch.** `Mem-SimpleV2-128/256/512/Auto` run modes 0, 1, 2, 13 and 14 on the scalar
  build, so each writes its own image; before, 0, 1, 2 and 13 wrote mode 10's (TODO 3).
- **Two-tier verify (TODO 27).** The hot loops are unchanged. Every kernel's cold path
  (`Rescan`, `src/tests.rs`) rescans a failed check before anything overwrites it:
  - volatile reads; each bad word logged with index, address, expected, actual and XOR;
  - the first 4 reread 4 times, then once after flushing their line;
  - a clean rescan is "not reproduced on rescan" and still counts one error.

  The sequential SIMD verify and SimpleNT, which only counted failed batches, now rescan too.
  `alarms` and `not_reproduced` go to results apart from `errors`, and `rescan=off` skips the
  rescan.
- **Halt keeps the evidence.**
  - The harness stops right after the seal check before a chunk, and after each verify, and
    counts the chunk's work so far.
  - `TestRunner` doesn't reseal a halted extent.
  - The loop-owning tests halt after their seal check, between StuckBit's phases, and before
    their reseal (`halt_here!`).
  - The mirror tests and Seal-Check don't refill.
  - The runner doesn't dispatch a step whose between-steps seal check failed.
- **Validation.** `verify_reps` 0, `write_read_cycles` 0, a mirror's `test_reps` 0, and
  `test_reps` 0 with a cycle-keyed mode are refused, and every knob is at most 10,000. These are
  checked when the config loads and again after the command line's overrides. A knob the test
  doesn't read is logged. Mem-SimpleNT keeps SimpleTest's four rounds but no longer shows five
  verifies it never ran. A `.cfg` value that doesn't parse is an error naming it, and a `.cfg`
  SimpleTest with one of TMR's modes (10, 11, 13, 20, 30) keeps it.
- **Bench accounting (TODO 90, part).** Every Bench-Verify leaves its fill out of its time and
  bytes. A step's speed, on the console, in results and in the per-step averages, divides by its
  threads' own timed duration: a `Duration`, `None` when the test didn't run, used at any length
  (it fell back to wall time, fill included, below a second).
- **The ORed differences.** A failed check's ORed differences go to the rescan's report. A check
  the rescan doesn't reproduce still says which bits differed: the 8600G's errors are mostly
  that kind.

**Stage 2, epoch and polarity.**
- **The epoch.** The runner gives each dispatch a `StepEpoch` (run seed, suite cycle, step). The
  harness gives each write round its index (`ChunkCtx::round`); a verify-only step stays at round
  0, so its fill and verifies agree.
- **Pairs.** Rounds `2n` and `2n + 1` are P and ~P under one key, and keys never include the
  thread or the chunk. A chunk's rounds run back to back and an odd `write_read_cycles` rounds up
  to even (the owner's call), so every chunk ends each cycle on whole pairs. A verify-only step
  writes once a suite cycle, so its P and ~P are a suite cycle apart, one key over the two.
- **Seeds.** The run seed comes from `seed=` or the clock, and is logged, printed and saved. Word
  ids, not addresses, so a seed replays the bytes on the same layout.
- **The seal.** A key per pair of cycles, the pair's second cycle inverted
  (`SealKernel::for_cycle`), from the seal at a cycle's start to its final check. So every sealed
  bit takes both values over a pair, and a stale seal fails. `tmr` keeps 63 bits, so no two words
  are equal; the audit's colliding pair now differs.

**Stage 3, families.**
- **The families:** `ident`, `random`, `switch`, `lines`, `linepair` (`src/pattern.rs`).
- **Line functions.** Scalar ones, plus vector ones at each width. The vector ones keep the
  128-bit variant 128-bit: computed from scalar words, its stores were merged to 256 bits.
- **Kernels.** One native kernel per width for scalar, 128, 256 and 512, sequential and
  strided, matching the family once per chunk.
- **The transfer width:** `beat=`, else SMBIOS, else alternating 64 and 32 from one `switch`
  pair to the next, with a warning. `beat=` takes a width or a memory type; LPDDR6's 12 and
  DDR6's 24 are models (below).
- **The command line.** `pattern-mode=` replaces a test's program; it used to be ignored on
  imported tests while still being reported.
- **Bench pairs.** `Bench-Init-/Bench-Verify-{Ident,Random,Switch,Lines,Linepair,Quiet,Header}`.

**Stage 4, conversion and provenance.**
- **The default.** No `pattern_mode` runs `[random, switch]` (the owner's call, 2026-10-15, after
  the corrected cost table; it ran `[ident, switch, random, switch]` before).
- **TM5 imports.** Modes 0, 1 and 2 import as `[ident|linepair|lines, switch, random, switch]`
  on `Mem-SimpleV2-Auto`. Mirror, refresh and blockmove imports no longer get pattern fields.
- **Provenance.** Results record `pattern_version`, `pattern_seed`, `transfer_bits`,
  `transfer_source`, `rescan` and each test's `program`, all with serde defaults so old results
  load. Step lines show the program.

**Since the commit (2026-10-15/16).**
- **Failing bits.** Every failed check counts each bit its ORed differences had, and every bad
  word its rescan finds counts each wrong bit, 0 to 1 or 1 to 0 (`src/failing_bits.rs`). Only the
  cold path counts. The step line names them after its counts (`3 errors (bit 31) + 0 seal`; more
  than four bits read `(17 bits)`). The final summary's Failing Bits table has a row per test,
  the seal's, and all of them (the eight most failed bits a row, then how many more). Results
  have `failing_bits` per step and over the run, and `seal_failing_bits`, every bit.
- **Rescan records.** Mem-Random's replay counts a bad word once however often its reads land
  on it, and its record counts the repeat reads. StuckBit's phase moved into its record line,
  which it had a line of its own for.
- **A dependent verify-only native SimpleTest** (`skip_init`, `test_reps` 0) is a plan error,
  whatever precedes it: no other step writes its step-keyed image. Only a Bench pair shares a
  native image (TODO 13's close).
- **Numbering by kind** (the owner's call, 2026-10-15). TM5's 0-2 stay; TMR's modes go by tens:
  1x identities (10, 11, 13), 2x in-line arithmetic (20, was 12), 3x switching (30, was 14).
  12 and 14 are refused, not reused, so an old config fails loudly. The native families take
  numbers in the same groups (15 `ident`, 16 `random`, 17 `linepair`, 21 `lines`, 31 `switch`,
  32 `quiet`, 33 `header`), and `pattern_mode`/`pattern-mode=` with one runs that family alone.
  The Bench names follow the numbers (`Bench-Init-TMR-20`). No image changed: the mode number
  isn't in any key.
- **Stage 6's families.** `quiet` (32) is `switch` with one pin held a line, 1 under P and 0
  under ~P, picked by the line's hash so every pin gets turns; DDR4's DBI leaves P's held byte
  alone and LPDDR's ~P's. `header` (33) is `switch` with `ident`'s word heading each 32 B half
  (words 0 and 4, not the 0 and 1 first proposed, so LPDDR's two bursts both carry one), so no
  half-line passes for another, and six words of eight switch. Both are in `pattern_spec.md`,
  with vector forms and Bench pairs; at 12 and 24 bits their vectors come from scalar words.
  Python written from the spec alone, without the code, gave 90 known answers, all matching;
  its three questions (a letter used twice, the codes, a partial transfer) are answered there.
  Neither is in a default program until the hardware session.
- **A review of this batch** (one reviewer, the diff before `quiet` and `header`): no high
  findings, twelve low, all taken. The rescan counts bits in a fixed table (all-bad 4 MiB
  chunks took 248 ms each with a search, 14 ms now); `failing_bits` is a test's own checks at
  every level, the seal's apart; a check counts only the bits its differences had; a `.cfg`'s
  native number imports at the widest width; `pattern-mode=` with a native number and no
  SimpleTest warns; 12 and 14 are refused saying what they became; a JSON `Bench-*-TMR-30` with
  no `pattern_mode` runs mode 30, not 10; the leftover `m12` names went.
- **The other tests' values.** Mem-Random, CacheBust, Stride and BlockMove add each word's id
  (`pattern::word_base(thread) | idx`, the native one) to their values, so no word of one thread
  holds what one of another does: they held the index, the same in every thread, or one apart
  (TODO 19's address-in-address, which `ident` and `random` already are). CacheBust adds the
  cycle too, so a write that didn't land shows unsealed; Stride's pass moved to the top byte,
  clear of the thread's bits. Only Mem-Random's loop changed: one `or` a read, off its chain.
- **Stage 5's tests, part.** Mem-Random ends with a sweep of every word in order (the reads
  sample a few thousand words a cycle; the fill has sat since the test began). `Mem-MovingInv`
  is memtest86's moving inversions, a March X over each chunk, and `Mem-ModuloX` its modulo-20,
  protected victims at line granularity; both write `ident`'s word of each word id under a key
  per step and cycle, with chunks twice the L3 so each pass reaches DRAM, and both loops are
  256-bit in the release asm.
- **`Mem-Sequence`** (the owner's idea, 2026-10-16; a separate test, to bench against SimpleTest
  and Mem-Random before it replaces anything). A list of steps, `W`, `R` or `RW` with a count and
  an `@order`, run `write_read_cycles` times a chunk; orders forward, reverse, random (every line
  once, each power-of-two block scrambled by a bijection, anew each pass) and TM5's stride. `RW`
  checks each line and writes its inverse in the same visit. First argued as the only pass that
  stresses read-write turnarounds; the review corrected that: a `W` pass of ordinary stores reads
  each line for ownership and writes back evicted ones, so its DRAM traffic mixes reads and writes
  too, but those reads aren't checked. `RW`'s are: it checks reads made in a stream with writes in
  it. Neither mix is measured (uProf's UMC counters on the metal box could). `WR` was left out: its
  read would be answered by the cache. Each pass is one loop, in a function per operation at 256
  and 512 bits; the release asm is at width with no `memset`/`memcpy`. A review: no wrong read;
  taken were `pattern-mode=` on it (a numbered mode is left off it, a native number reaches it), a
  stride of 0 (divided by zero; now in order), rounds (an `RW` that writes the next round counts as
  it, so `W 2R RW 2R`'s second `W` writes the next pair, not ~P over ~P; odd rounds run once more,
  as SimpleTest's), and the read-then-write rescan (a cell stuck at the value the pass then wrote
  read back right: now what was read is written back, flushed and rescanned, Mem-MovingInv's
  passes too). At 512 bits the 12- and 24-bit loops keep their accumulators on the stack (their
  scalar-built lines are too big to unroll); every 16-, 32- and 64-bit loop keeps them in
  registers. Two build lessons: its 132-byte sequence inline in `TestMemoryConfig` made the
  release build 3x slower (the registry builds ~200 configs by value; boxed, 1m15 against 1m22
  before), and accumulators declared once for a function of 57 loops left one a stack slot in most
  of them (declared per loop, none in the common ones).

## Review (2026-10-11)

- **A second review** of the changes after the first raised 10 findings (3 medium, 7 low). The
  skeptics confirmed all 10, and all are fixed:
  - a verify-only step's rotation skipped `switch`;
  - a halted step's seal errors were missing from the verdict line;
  - a second bad word in one line was reread after the first's flush;
  - Mem-Stride's bytes on a halt;
  - millisecond speeds;
  - ops/s and the per-thread rows on wall time;
  - the reread budget;
  - a halted step counted in the averages;
  - the spec's accumulator wording;
  - the native Bench pairs' `write_read_cycles`.

  Its test gaps (accumulator coverage, the strided SIMD verify) are closed by
  `native_verify_sees_every_word_of_a_group`.

- **Four reviewers, a skeptic each:** kernels, verify and halt, config and CLI, and the seal. They
  raised 21 findings, the skeptics confirmed all 21, and all are fixed or reverted:
  - one high: `pattern-mode=` ignored on imported tests;
  - seven medium:
    - the rescan's flush mid-scan, now after the scan;
    - the seal's checks in the test's alarm counts;
    - the seal's key per cycle, which broke "each bit both ways";
    - the native Bench-Init unpinned;
    - the console speed still over wall time;
    - two more.
  - The SimpleNT rounds change was reverted.
- **An independent implementer** wrote Python from `doc/pattern_spec.md` alone. All 133 of its
  known answers match.
  - It found a design flaw: the unknown-width rule `(pair >> 1) & 1` fixed the width for some
    program shapes, so DDR4 could get no switching. It is now each `switch` pair's ordinal.
  - It found 17 spec ambiguities, now resolved in the spec.

- **Astra's implementation review** (`doc/pattern_implementation_review_2026-10-11.md`, with
  probes in `research/pattern_review/`), all taken but the word-id limits:
  - **P1:** a step a halt stopped before its first run divided by zero in the averages, so a
    release build aborted before saving results. Fixed, with a test through finalization and
    serialization.
  - **Pairs:** with 3 rounds `switch` got P every cycle and never ~P. Odd counts now round up.
  - **Evidence:** every failed check leaves one record: the region (for a native kernel its
    phase, family, profile and polarity, key and round), the check's ORed differences, the
    rescan's outcome and its bad words' differences, and the bits the rescan didn't see again.
  - **Timing:** the one-second fallback (above).
  - **Smaller items:** the two new `too_many_arguments` suppressions went (the program moved into
    `NativeRun`); a native Bench-Verify's identity is its family's pinned image, so a dependent
    one follows its own Bench-Init; the native kernels' per-chunk check is documented
    (`ErrorCheckInterval` isn't read; only per chunk is built).
  - **Wording:** the balanced bytes beat count-based DBI only (DDR4's, LPDDR4/5's DC DBI), not
    LPDDR6's programmable masks; the claims below are narrowed.
  - **Left as documented (P3):** `linepair`'s words can repeat at an id gap of an odd multiple of
    2^32 (32 GiB a thread). A misdirected access between those two words alone would pass; a
    changed bit still compares wrong. Astra's 63-bit construction would remove it, at the cost of
    a fixed top bit before the line's inversion.
  - **Not done:** a check of the word-id limits (`idx` under 2^40, the thread under 2^24: 8 TiB
    of one thread's span, 16 M threads). No machine reaches them; nothing enforces them.
- **DDR6 and LPDDR6** (`beat=24|ddr6`, `beat=12|lpddr6`): one packer for every width (a burst's
  payload fills its transfers in order and starts afresh each burst), which reproduces the 16-,
  32- and 64-bit images exactly, shared by mode 30 and `switch`. Both new profiles are models,
  labelled so in the log and `transfer_source`: LPDDR6's burst geometry is public (12 DQ, 24
  transfers, 256 payload bits of 288) but not where its overhead sits, and DDR6 isn't published
  (24-bit subchannels from a vendor's draft summary; 64 B bursts assumed). Simulated on Astra's
  layouts (`research/pattern_review/switch_model_toggle.py`): 100% of payload-to-payload
  transitions toggle with the payload first; on the illustrative layout (a patent's overhead
  positions, made-up payload order; side by side for 24) 83% (12) and 85% (24) on average; the
  16-, 32- and 64-bit profiles give 25-60% on a 12- or 24-pin bus. SMBIOS has no code TMR knows
  for either, so they need `beat=`. A table-driven profile (Astra's research) waits for a
  published packet map.
  - **Checked:** an independent implementation written from the spec alone (40 known answers at
    12 and 24 bits, all matching); every kernel width writing the spec's image at every beat,
    sequential and strided, and localising a flip; 12/24 loops at width in the post-LTO asm (128:
    four xmm stores a line, 256: two ymm, 512: one zmm; no table loads, memset or memcpy).
  - **Encoders** (Astra's follow-ups: `doc/pattern_followup_review_2026-10-11.md`,
    `doc/switch24_simd_research_2026-10-11.md`), decided by A/Bs on both desktops at 50% of memory:
    - **Kept: the 24-bit SIMD encoder** (`src/pattern_switch24.rs`): the SIMD kernels build a
      24-bit line from byte shuffles, 48 instructions a line to 28 in the 256-bit writer (128:
      53 to 31, 512: 45 to 25), the same bytes (all 4,096 seeds against an independent byte
      reference at each width). It raised the 8600G's 24-bit `switch` verify 2.9% (58,026 to
      59,712 MiB/s, level with the other widths) and left the 5700X's level (-0.4%).
    - **Rejected: the seed mask** (cut the hash to the nibbles a width uses before spreading it).
      It saved 4-8 instructions a line at 16, 24 and 32 bits, but the 5700X's `switch` verify ran
      2.1% slower at 16 and 32 bits: 6 interleaved runs a side at each width over two A/Bs, every
      run of each side 0.1-0.35% from the others and none overlapping (95% CI -2.25 to -1.96% at 16,
      -2.14 to -1.99% at 32; both mask builds, all 8 threads, no drift, the same 2 MiB pages; blocks
      at 24 and 64 bits in the same order showed nothing). The final build, without it, measured
      the old level in one run at each width. The 8600G gained nothing (its run-to-run noise is
      5-12 times the 5700X's, but excludes a 2% loss). Instruction counts didn't predict speed.
      The verify loops compared (`research/seed_mask_2x2/`): the masked one is the simpler code (76
      instructions per 2 lines against 95, the same loads, no spill), so nothing in it is a defect
      to fix. A 2x2 on the 5700X the next day (mask on and off, each with every loop aligned to 64
      bytes; 1 GiB pages after a fresh boot) found the mask about 1.3% slower at 16 bits for both
      alignments, and all four builds equal at 64 bits: 64-byte loop alignment didn't remove the
      regression, and the cause inside Zen 3 isn't identified. The 64-bit verify (DDR4's default)
      sits on the masked level, about 1.25-1.3% below the unmasked 16-bit loop.
    - **Left: the 16-bit path's extra op** (45 instructions a line against 44 before the shared
      packer), which neither desktop could measure.

## How it was checked

- **Tests.** 178 pass, and clippy reports nothing (`cargo clippy --all-targets`). They cover:
  - Each family's lines against the spec's formulas written out word by word.
  - **133 known answers from an independent implementation:** Python written from
    `doc/pattern_spec.md` alone, without the code, all matching; and 40 more at 12 and 24 bits
    from a second one.
  - Every width writing one image, sequential and strided over overlapping chunks.
  - Exact bad-word counts with failed-check counts.
  - Halt leaving the bad words in memory, after a verify and after a seal check.
  - A new image every suite cycle and step, and the same image again from the same epoch.
  - SIMD widths writing each numbered mode's own image.
  - Refused repetition, strict `.cfg` values, and the import's programs.
  - Every registered test sizing its chunk by name (the first live run panicked on the new Bench
    names; TODO 19's rule).
- **Assembly** (post-LTO `--bin` asm): the write loops store 128-bit xmm, 256-bit ymm and 512-bit
  zmm at their widths, and the verify loops OR at those widths (512 as `vpternlogq`). There are no
  `memset`/`memcpy` calls, and the phase is computed once per chunk.

## Measurements

**Cost, corrected** (Bench pairs, physical cores, 50% of memory, the evening of 2026-10-11).
Verify MiB/s with the fill left out of the time, one run each (run-to-run noise about 1-2%):

| Family | 8600G (DDR5-6000, AVX-512, 6 cores) | 5700X (DDR4-3600, AVX2, 8 cores) |
|---|---|---|
| ident | 59,722 | 46,089 |
| linepair | 59,783 | 46,249 |
| switch | 58,956 (32-bit) | 45,338 (64-bit) |
| lines | 59,816 | 45,617 |
| random | 59,593 (-0.2%) | 45,130 (-2.1%) |
| mode 10 (TMR-10, scalar) | 59,670 | 46,520 |
| mode 13 (TMR-13, scalar) | 58,213 | 45,799 |
| mode 30 (TMR-30, scalar) | 59,576 | 45,647 |

- **Writes:** 36.0-36.4 GB/s on the 8600G and 21.5-21.7 GB/s on the 5700X for every family.
- **Verifies:** `random` costs 0.2% against `ident` on the 8600G and 2.1% on the 5700X.
- **Widths:** `switch` and mode 30 verify within about 2% of each other at 12, 16, 24, 32 and 64
  bits on both (the 8600G's 24-bit `switch` 2% low until the SIMD encoder, above).

**Cost, first measurement** (the morning, the fill included in the step's time; superseded):

| Family | 8600G (DDR5-6000, AVX-512, 6 cores) | 5700X (DDR4-3600, AVX2, 8 cores) |
|---|---|---|
| ident | 51,429 | 37,428 |
| linepair | 51,314 | 37,561 |
| switch | 50,988 | 37,114 |
| lines | 50,485 | 37,488 |
| random | 50,374 (-2.1%) | 36,044 (-3.7%) |
| mode 10 (TMR-10, scalar) | 51,472 | 37,895 |
| mode 30 (TMR-30, scalar) | 50,665 | 37,479 |

- **Writes:** every family wrote at the same speed, 36.5 GB/s on the 8600G and 21.6 GB/s on the
  5700X, so the pattern doesn't limit them.
- **Verifies:** `random`'s fill plus verify was 2-4% slower than `ident`'s; the corrected table
  above has its verify alone. The default program spends a quarter of its rounds on it.

**Detection** (ABBA, the 10-09 A/B's setup: scalar Mem-SimpleV2, full allocation, 880 MiB
chunks, 4×(1+5), 10 min a block, one binary):

| Block | 8600G errors | 5700X errors |
|---|---|---|
| legacy mode 30 | 38, all not reproduced on rescan | 0 |
| native default | 24 (23 in `random` phases, 1 `ident`, 0 `switch`), all not reproduced | 0 |
| native default | 0 | 0 |
| legacy mode 30 | 0 | 0 |

- **Variance.** The 8600G's rate swings widely from block to block: 24 then 0 for one program, and
  843 then 170 for mode 30 on 10-09. The fall (38, 24, 0, 0) fits drift but doesn't prove time
  rather than pattern. Every run allocates other physical memory, which makes comparisons between
  runs noisier; steps of one run share it.
- **The interleaved run.** Every pattern was a step of one run, each step one pass, on the same
  memory, for 30 minutes (`il.json`: legacy mode 30, then native `[switch]`, `[random]` and
  `[ident]`):
  - **The 8600G:** 46 cycles, 0 errors. Its fault stayed quiet after about 12:04.
  - **The 5700X, the "stable" control:** 19 cycles and 5 errors, all in the `[random]` step. One
    word (thread 1, cycle 17) had bit 47 set (XOR `0x0000800000000000`). It stayed wrong on 4
    rereads and after its line was flushed and read again. That is persistent observed
    corruption, not proof of a weak cell: the flush writes a dirty line back before evicting it,
    so if the line was dirty a bad cached copy could have gone out with it. At DDR4-3600 with two
    DIMMs a channel. The
    other patterns, on the same memory in the same cycles, didn't hit it. It was counted once per
    verify pass, 5 in all: five observations of one word, not five faults.
- **What the errors are.** Almost every error is a read that a rescan moments later doesn't
  repeat: 791 of 843 on 10-09, all of them today.
- **A surprise.** Native `switch` found nothing in ~5 min of its phases, where mode 30 found 38
  in 10 min. Native `random` found 23 in ~2.5 min. The two switch streams differ (mode 30 hashes
  the byte address, `switch` the line id, and writes P/~P), and so do exposure, phase history and
  allocation: this shows neither a verifier that misses (the injected-fault checks find no loss)
  nor equal stress. Mode 30 stays. The evening reversed it: 50 in `switch`, 34 in mode 30 (below).

**The evening runs** (18:47-20:40, this round's builds, physical cores, 50% of memory):
- **The interleaved run again** (`il.json`, 30 min):
  - **The 8600G:** 46 cycles and 102 failed checks: 34 in mode 30, 50 in `switch`, 9 in `random`,
    6 in `ident`. Every one was bit 31 of the 64-bit word alone, on all six threads, from 19:01 to
    19:29, and all but one were reads a rescan didn't repeat.
  - **The 5700X:** 19 cycles, 0 errors. Its bit-47 word didn't come back.
- **The 8600G's bit 31.** It kept failing through the A/Bs: 3 checks at 19:36 (bit 39, mode 30
  at 24-bit beats), then 793 in 12 seconds at 20:18 (bit 31, mode 30 and `switch` alike, both
  builds). Every word reread after a flush read right, so DRAM held the right data: a read
  fault on one data line, as the user suspected of this overclock. The evening's errors all
  came through the record's ORed differences, which a not-reproduced check now keeps.
- **A TM5 `.cfg` end to end** (1usmus_v3): 5 cycles native, 0 errors on both (57,631 and 39,944
  MiB/s); 1 cycle at `beat=24`, 0 errors on both (58,374 and 40,538 MiB/s).

**A TM5 `.cfg` end to end** (1usmus_v3, 5 cycles):
- **The 8600G, native import:** 0 errors, 58,187 MiB/s. Its SimpleTests ran as
  `Mem-SimpleV2-512_A` with `ident`, `linepair` or `lines` and switch, random, switch, strided where
  the `.cfg` asks, beside the mirrors and Seal-Check.
- **The 8600G, `patterns=legacy`:** 5 cycles and 10 errors, all from the seal: two cells of one
  64 B line at `0x247c7afa958` (word 3 bit 8, word 7 bit 9).
  - They first failed the mirror's seal verify in cycle 4, then the next two steps' seal checks
    after the mirror's refill.
  - The bad values stayed each time (4 of 4 rereads, and after a flush, with the flush's
    writeback caveat above).
  - Persistent corruption, found by the retention check. Both settings share the seal, so this
    says nothing about the SimpleTest patterns.
- **The 5700X, native import:** 0 errors in 57 steps (3.6 cycles, `Mem-SimpleV2-256_A`). It ran
  past its 18-minute limit, about 3.5 minutes a cycle.

## Open

- **Mem-SimpleNT** still writes `idx ^ base`, the same in every thread.
- **History coverage** is measured offline only (the follow-up). `quiet` holds one pin among
  toggling ones; which pins are neighbours on the board TMR can't know.
- **One encoder for every width?** The byte-shuffle encoder fits every byte-aligned width (16,
  24, 32, 64; 12 isn't), so it could replace the closed forms, but the seed mask showed that
  fewer instructions can measure slower: only with an A/B on both desktops.
- **A one-line native verify** (mode 30's shape), a later experiment for when another change
  already needs a 5700X benchmark: same bytes and coverage, against today's two-line loop at 64
  bits. One line at 256 bits is two vectors, so it feeds two of the four accumulators
  (`research/seed_mask_2x2/README.md`). A 1% throughput gain isn't a 1% detection gain: the
  access-sequence and coverage work (stages 5 and 6, above) comes first.
- **Equal halves.** At 12, 16, 32 and 64 bits a `switch` line's 32 B halves are equal, so a
  misplaced half-line passes that phase (`ident` and `random` catch it). Separate seeds per half
  cost no switching only if the line really is two bursts; LPDDR4/5 can send it as one 32-transfer
  burst, where it breaks one transition a pin. `switch` not changed; `header` gives each half an
  identity word instead, at the cost of two switching words.
- **Plan validation after the command line.** `memory_effect` isn't derived again after the
  overrides, so `validate_test_plan` sees each step's effect from before them. No live effect:
  nothing on the command line sets `skip_init` (the review of 2026-10-16).
- **The SIMD Refresh** still flushes only on request. Whether its verify reads cache needs a
  measurement first (the plan's research list).
