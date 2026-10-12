# Pattern overhaul plan (2026-10-10)

> **Status 2026-10-11: built**, uncommitted, as TODO 97 (`doc/todo/97-native-patterns.md`; the
> spec is `doc/pattern_spec.md`). Stages 0-4 are in, plus the follow-up's and the review's
> amendments. Stages 5 and 6's families are not. What changed from the plan below:
> - **Verify.** It keeps its accumulators; a failed check is rescanned (TODO 27), and `alarms`
>   are counted apart from bad words. No single error unit.
> - **Pairs.** A pair's P and ~P share one key.
> - **The default program** is `[ident, switch, random, switch]`, so mixed data runs too.
> - **An unknown width** alternates the 64- and 32-bit `switch` profiles; no switching floor is
>   promised.
> - **The seal** takes a key per pair of cycles.
> - **The SIMD Refresh flush** was a measured choice (4b2e11a), not a regression, so it stays.
> - **Word ids, not addresses**, so `seed=` replays.

Goal: patterns and verification that find errors, by intent first; TM5 faithfulness second. TM5
`.cfg` files are converted to native patterns on load. Built from five code mappers and checked by
two adversarial verifiers: 198 premises confirmed, 25 corrected, 32 gaps added (raw output:
`%TEMP%\tmr76\patres\plan_workflow.json`). It builds on `doc/pattern_audit_2026-10-09.md`.
`doc/pattern_research.md` is superseded.

## Properties a pattern, its verify and its access policy must have

| # | Property | Definition | Measured by |
|---|---|---|---|
| 1 | Position purity | A word's value is F(address, family, params, epoch key, phase) only, so it is right for any chunk layout or walk order | golden image test: overlapping chunks, sequential vs strided |
| 2 | One spec, every kernel | scalar, 128/256/512, NT and Bench write and expect the bit-identical image | cross-width golden test (none exists today for SimpleV2, SimpleNT or Bench) |
| 3 | Global identity | within one epoch, every word address of the run has a distinct value, across all threads (identity families; copy tests excepted) | injectivity proof + sampled check |
| 4 | Temporal variation | the first write of every round, phase, step and suite cycle differs from what the address holds | runner-level test across two suite cycles |
| 5 | Polarity coverage | every bit is written and verified as 1 and as 0 within a bounded schedule (explicit P then ~P) | same-address P/~P test |
| 6 | In-burst switching | in a switching phase, the share of data lanes toggling between beats at the transfer width (64 DDR4, 32 DDR5, 16 LPDDR) | bit-level simulation per profile |
| 7 | Background diversity | keyed and structured backgrounds (solid, stripe, checkerboard), so neighbouring bits see all value combinations | pairwise combination coverage |
| 8 | DRAM reach | a verify whose intent is DRAM reads DRAM, not a cached copy | per-test policy; flush/NT placement checked in asm |
| 9 | Dwell | the time between a word's write and its verify, in both polarities | schedule analysis |
| 10 | Verify fidelity | every word compared; one error unit (bad words); transients counted apart but still fail the step and halt; distinct addresses and bit signatures | injected-corruption tests per kernel |
| 11 | Evidence on halt | under errors=halt, the first bad word is still in memory | halt tests incl. mirror and Seal-Check |
| 12 | Verified coverage | each phase writes and verifies every word; nothing is overwritten unverified | coverage tests |
| 13 | Access stress | row misses, read/write turnarounds, prefetch distance: the strided walk's job, measured per step | uProf on metal, timing |
| 14 | Cost | gen/verify at or above the per-thread DRAM share at full threads; no per-word branch, log or heavy op in sequential kernels | Bench per family, post-LTO asm |
| 15 | Provenance | results record the pattern spec and version, seed, transfer profile and its source, and the kernel; old results still load | results tests |
| 16 | Honest claims | each property labelled end-to-end (survives scrambling, inversion, swizzles, ECC) or wire-level (doesn't) | doc review |

Width matters only to #6. Identity (#3), epoch (#4) and polarity (#5) need no bus knowledge, so
they suit DDR4, DDR5 and DDR6 unchanged.

## Plan

**Stage 0: freeze a baseline.** Today's mode 0 (scalar `Mem-SimpleV2`, as in the 8600G A/B's
`ab_m0.json`) and mode 14 stay runnable as named legacy families until stage 6 has ranked them
against the new ones under the same harness.

**Stage 1: make results trustworthy (no pattern changes).**
1. **Honest SIMD dispatch.** A golden cross-width test for SimpleV2, SimpleNT and Bench. SIMD
   variants route modes they don't implement (0, 1, 2, 13) to the scalar image, as mode 14 already
   does, instead of silently writing `idx ^ param0` (TODO 3).
2. **Validation before allocation.** A per-knob table of which tests read `write_read_cycles`,
   `test_reps` and `verify_reps`. Zero is refused where it skips writing or verifying (Seal-Check
   included); values are bounded; ignored fields are refused. The `.cfg` parser errors on
   unparsable fields instead of silently defaulting (e.g. Parameter becomes 0: strided turns
   sequential).
3. **One error unit.** `simple_verify_positional_simd!` and SimpleNT get the cold rescan and count
   bad words. Transients are counted separately but still fail the step and halt. The interval is
   redefined in words; it is not deleted (a CLAUDE.md rule). This is part of TODO 27.
4. **Halt keeps the evidence.** Check after the pre-chunk seal check and after each verify. Mirror
   and Seal-Check must not refill a failing chunk under halt. The runner must not dispatch a step
   whose between-steps seal check failed.
5. **DRAM reach.** Restore SIMD Refresh's unconditional pre-sleep flush (a regression of closed
   TODO 26). Name each test's cache policy (CacheBust and Stride stay cache-level by design).
   Flush-before-every-verify waits for its cost and benefit to be measured.

**Stage 2: epoch and polarity.**

6. **Epoch key.** A run seed (recorded, and replayable with `seed=`) plus {suite cycle, step, round,
   phase} reach each step through its per-step `TestMemoryConfig`, not the hot loop. One key is
   derived per chunk; there is no thread id in the key, so identity holds across threads.
   - Writes-first tests alternate P and ~P across their write rounds.
   - Data written once per step (Random, BlockMove's source, an up-front fill) is keyed per step.
   - Bench pins its key and polarity.
   - A runner-level test covers two suite cycles.

**Stage 3: the native family catalogue.** The family is picked by an enum match once per round,
calling per-width `#[target_feature]` kernels made with `macro_rules!`, as the seal does.

7. **ident (identity):** `x = (addr >> 3) * G + key; word = x ^ (x >> 32)`. Every word address is
   distinct within an epoch, it needs no width, and it costs adds and XORs per vector. It also
   supplies the values for Random, CacheBust, Stride and BlockMove; that is TODO 19's
   address-in-address.
8. **lines:** mode 12, keyed from the epoch, replacing mode 2's chain in both sequential and
   strided order (ends TODO 95's collapse).
9. **random:** mode 13, keyed by address, as the decorrelated family, with SIMD kernels.
10. **switch:** mode 14 as a P/~P phase, with the profile picked once per step from SMBIOS. For an
    unknown type, the 64-bit profile (at least 50% at any width, 94% on DDR4).
11. **linepair:** one base per pair of lines, the second inverted, keeping TM5 mode 1's
    neighbouring-lines-opposite intent.
12. **Seal:** mask to 63 bits before mixing, so every word is unique, plus a per-cycle key and
    polarity. All seal transitions (cycle start, per chunk, mirror, final) use one expected epoch.

**Stage 4: conversion and provenance.**

13. **Phase programs.** A test runs an ordered list of families, each as a P/~P pair over the write
    rounds, rotating with the cycle when there are more pairs than rounds.
    - The default SimpleTest is `[ident, switch]`, so every step gets identity and switching.
    - `.cfg` Pattern Mode 0 imports as `[ident, switch]`, 1 as `[linepair, switch]`, 2 as
      `[lines, switch]`.
    - A pattern version, the seed and the profile are written to results with serde defaults, so
      old results load.
    - The importer uses `Mem-SimpleV2-Auto` once all widths agree.
    - Deleting old mode code needs the owner's approval of the list first.

**Stage 5: access sequences.**
- Moving inversions, protected victims (modulo-X) and random writes, in TODO 19's agreed order.
- A final full sweep for Mem-Random.

**Stage 6: experiments and hardware.**
- The audit's identity-header + alternating-tail family, as an experimental mode.
- ABBA hardware runs, at least 3 blocks per arm:
  - Hosts: the 8600G (DDR5) at its failing setting and a milder one; the 5700X (DDR4) as the stable control.
  - Arms: legacy mode 0, legacy mode 14, `[ident]`, `[switch]`, `[ident, switch]`, header-tail.
  - Reported: time to first error, distinct bad addresses, and errors per hour.

Every stage: clippy, the full test suite, post-LTO asm for new or changed hot loops, and a
byte-uniform-constant check (StuckBit's solid backgrounds would collapse to `memset`).

## Research questions

- **Scrambling:** is it on on the test hosts, and does mode 14's lead survive with it switched off?
- **The 8600G errors:** which beat and lane, and do they need a burst boundary?
- **What provokes the fault:** the switching rate, or the balanced-byte values?
- **Cache:** what share of each test's verify is served from cache on each host? Is flushing worth
  its cost?
- **DDR5 inversion and burst order:** what do AMD and Intel IMCs actually use?
- **Backgrounds:** how many give full pairwise coverage?
- **Verify throughput:** does every family's verify keep up at full thread count?
- **NT stores:** what happens to an already-dirty line, and is the reseal in DRAM before the next
  check?
- **On-die ECC:** which errors does DDR5's hide?
- **Dwell:** which dwell time catches an extended tREFI?
- **DDR6/LPDDR6:** what transfer profiles will they need?
- **Statistics:** how should pattern comparisons be judged?
