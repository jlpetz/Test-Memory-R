# Pattern spec v2: SimpleTest's native patterns

Normative: `src/pattern.rs` implements this, and its tests check the implementation against
these formulas. Results record `pattern_version: 2`. Version 1 is the TM5-numbered modes (0-2,
10, 11, 13, 20, 30, `doc/pattern_gen_modes.md`), which still run when a config sets `pattern_mode`.

All arithmetic is on unsigned 64-bit integers, wrapping. `^` is XOR (never a power: powers are
written out, `0x7FFF_FFFF_FFFF_FFFF`), `!` is bitwise NOT, `>>` and `<<` are logical shifts,
`rotl(x, n)` rotates left. Bits, nibbles and bytes count from the least significant: nibble `b`
of `h` is bits `4b..4b+3`, byte `b` of a word bits `8b..8b+7`.

## Ids

- **Word id.** Word `idx` of thread `t`'s span is `id = t << 40 | idx`, `t` the worker's index
  from 0 (the thread pool's order, not a CPU number). A test's extent starts at its span's start,
  so `idx` is the word's offset from the extent's base.
- **Line id.** Each 64 B line holds 8 words; line `n` of thread `t`'s span is
  `lid = (t << 37) + n`, and its word `j` (0..7) is `id = lid * 8 + j`.

Ids, not addresses: a run's bytes don't depend on where the allocation landed, so `seed=`
replays them on the same layout (the same thread count and span sizes). The seal is the
exception: it is keyed by address (below).

## Mixing

```text
G         = 0x9E3779B97F4A7C15
fmix(z):  z ^= z >> 30; z *= 0xBF58476D1CE4E5B9; z ^= z >> 27; z *= 0x94D049BB133111EB; z ^= z >> 31
mix(a, b) = fmix(a ^ fmix(b + G))
```

`fmix` is splitmix64's finalizer (TMR mode 13); each step is invertible, so it is a bijection.

## Families

For line `lid`, key `k`, the line's 8 words `w[0..7]` before polarity:

| Family | No. | `w[j]` | Intent |
|---|---|---|---|
| `ident` | 15 | `x ^ (x >> 32)`, `x = (lid*8 + j) * G + k` | every word of the run distinct (a bijection of its id): a misplaced read or write shows |
| `random` | 16 | `fmix(lid*8 + j + k)` | strongly mixed: every data line sees every short bit history (DDR5 equalisers) |
| `linepair` | 17 | `ident`'s word `j` of line `lid & !1` under the same `k`, inverted when `lid` is odd | TM5 mode 1's neighbouring lines opposite: each aligned pair complementary |
| `lines` | 21 | `s + j*d`, `s = fmix(lid + k)`, `d = (rotl(s ^ k, 29) * G) \| 1` | TM5 mode 2's in-line arithmetic chain, hashed per line (no collapse) |
| `switch` | 31 | per profile, below | every data line toggles on every beat at the transfer width |
| `quiet` | 32 | `switch`'s, one pin held at 1 (below) | one data line steady while every other toggles: the victim `switch` lacks |
| `header` | 33 | `ident`'s for `j` = 0 and 4, `switch`'s for the rest | `switch` that is an identity per 32 B half: a misplaced half-line shows |

The numbers sit in the numbered modes' groups of ten (`doc/pattern_gen_modes.md`): 1x identities,
2x in-line arithmetic, 3x switching. `pattern_mode` or `pattern-mode=` with one runs that family
alone; a program names families.

**`switch`.** `h = fmix(lid + k) & 0xFFFFFFFF`. Byte `b` (0..7) of `a` is `n | ((!n & 0xF) << 4)`
with `n` nibble `b` of `h`, so every byte holds four 1s. A transfer (beat) carries `W` bits, the
transfer width (below); `s` is `a`'s low `W` bits. A burst carries `B` payload bits of the line,
which fill its transfers in order, `W` a transfer from transfer 0, and each burst starts afresh.
So bit `p` of the line (bit `p mod 64` of `w[p / 64]`) is at place `q = p mod B` of its burst, on
pin `q mod W` of transfer `q / W`, and holds

```text
bit (q mod W) of s, inverted when q / W is odd
```

Every pin holds `s`'s bit at one transfer and its inverse at the next, so it toggles between
every two transfers that both carry payload on it. Where `W` doesn't divide `B` (12 and 24), the
payload's last transfer is partial: transfer 21 carries pins 0-3 at 12 bits and 0-7 at 24, and
the rest of the burst is the memory's own (overhead), which TMR doesn't write. At 12 bits a line
is two 32 B bursts, and they are equal (each starts afresh). The profiles:

| `W` | `B` | Memory | Status | `w[j]` |
|---|---|---|---|---|
| 64 | 512 | DDR to DDR4: one 64-bit channel a DIMM | standard | `a` for even `j`, `!a` for odd |
| 32 | 512 | DDR5: two 32-bit subchannels a DIMM; LPDDR3 | standard | every `(a & 0xFFFFFFFF) \| (!a << 32)` |
| 16 | 256 | LPDDR4, LPDDR5: 16-bit channels | standard | `p = (a & 0xFFFF) \| ((!a & 0xFFFF) << 16)`, every `p \| (p << 32)` |
| 12 | 256 | LPDDR6: two 12-DQ subchannels a die, 32 B a 24-transfer burst | model | the rule |
| 24 | 512 | DDR6 as drafted: four 24-bit subchannels; 64 B a burst assumed | model | the rule |

For 64, 32 and 16, `B` is a multiple of `2W`, so starting each burst afresh changes nothing.
**The two models** take the standard's burst geometry where it is public and assume the rest:
- **LPDDR6** (JESD209-6, 2025): a 24-transfer burst on 12 pins is 288 bits, 256 of them payload
  and 32 overhead (metadata, link ECC or DBI). TMR hasn't verified where the overhead sits or how
  a controller places a line's bytes around it (vendor material shows it inside the burst); the
  model puts the payload first, in transfer order.
- **DDR6:** TMR has verified no layout. The 24-bit subchannel is a controller vendor's summary of
  the draft;
  the 64 B burst is an assumption, with the payload first as for LPDDR6: 21 transfers and 8 pins of
  a 22nd, any overhead after it.

Overhead inside a burst moves payload bits between pins and transfers. On an illustrative
layout (a Samsung patent figure's overhead positions for 12 pins, with made-up payload numbering,
and a side-by-side copy of it for 24, which is no DDR6 map), the rule toggles on average 83% (12)
and 85% (24) of the transitions between two payload bits, 75-92% line by line, against 100% with
the payload first; the 16-, 32- and 64-bit profiles toggle 25-60% on either. A simulation, not
hardware: `research/pattern_review/switch_model_toggle.py`, 2026-10-11, on
`transfer_profiles.py`'s layouts.

The packer implements this contiguous rule only: a repeated `s`, a shift and an inversion mask
per word. Overhead inside a burst changes which pin carries successive payload bits partway
through a word, which no shift of a repeated `s` expresses for every `s`: such a packet map needs
a general bit map (`doc/transfer_profile_research_2026-10-11.md`), not a new width.

**Bus inversion.** A count-based data bus inversion (DC DBI, as in DDR4, LPDDR4 and LPDDR5)
inverts a byte when more than four of its bits are at the costly level, so it never inverts a
byte of four 1s and four 0s, and never undoes a flip where the profile's bytes are `a`'s (64, 32
and 16; at 24 each 8-pin lane is one of `a`'s bytes or its inverse too, but whether DDR6 has such
a DBI isn't known; at 12, pins 8-11 carry `n1` alone). No other scheme is covered. LPDDR6's
programmable XOR masks can turn such bytes into steady data. A scrambler or memory encryption
changes what reaches the pins: a fixed XOR on a pin keeps every transition, but a key that
changes from beat to beat, or a cipher, can make the flips random-looking.

**`quiet`.** `switch`'s line under the same `h`, `W` and `B`, with one pin held: with `H` the
high 32 bits of `fmix(lid + k)`, the held pin is `v = (H * W) >> 32` (0 to `W - 1`), and every
bit `p` of the line on pin `v` (`(p mod B) mod W = v`) is set:

```text
w[j] = switch's w[j] | (each bit i of the word with ((64 j + i) mod B) mod W = v)
```

(`i` is a bit of the word, 0..63, not the key.) A partial last transfer (12 and 24) holds pin
`v` only if it carries payload there: transfer 21 for `v` below 4 at 12 bits, below 8 at 24.

So under P pin `v` is 1 at every transfer of the line and under ~P 0, while every other pin
toggles as in `switch`. The pin comes from the hash, not from `lid`, so a channel or bank that
takes every other line still sees every pin held. The held pin's byte has four or five 1s a
transfer under P and four or three under ~P: a count-based DBI that counts 0s (DDR4) leaves P
alone and one that counts 1s (LPDDR4, LPDDR5) leaves ~P alone.

**`header`.** `switch`'s line under the same `k` and `W`, except words 0 and 4: those are
`ident`'s words 0 and 4 of line `lid` under `k` (word ids `lid*8` and `lid*8 + 4`). The two
heads are distinct over the run, so every 32 B half-line differs from every other; the six
other words switch as in `switch`; the transitions into and out of a head aren't flips.

**Polarity.** A phase's mask `m` is `0` (P) or all ones (~P); the stored word is `w[j] ^ m`.

**What each family does give, and doesn't.**
- `ident`'s and `random`'s words are distinct by construction within a family, key and
  polarity: each is a bijection of the word id. So are `linepair`'s while a span is under 2^32
  words: an even line's word and an odd line's complement meet only at an id gap that is an odd
  multiple of 2^32. `lines`' words are distinct only by hash. `switch` is not an identity: a line
  carries 8-32 bits of its hash (8 at 12- and 16-bit beats, 12 at 24, 16 at 32, 32 at 64), so at
  12- and 16-bit beats there are 256 distinct lines.
- `ident`'s bits are bits of an arithmetic progression. On a 64-bit beat no data line sees all 32
  five-bit histories: each upper bit at most 10 (`2h` for an `h`-bit history), whatever `k`; bit 0
  alternates. `random` covers them at any width.
- `switch` gives each data line only `01010` and `10101` in a burst, and no quiet line next to a
  toggling one: `quiet` holds one in each line. Which pins are physically neighbours TMR can't
  know (boards swizzle bits within a byte lane), so `quiet` holds one pin among all toggling
  rather than aiming at a neighbour.
- `header`'s heads are identities, so a half-line can't pass for another; its other words carry
  `switch`'s 8-32 bits, and it switches six words of eight.

## Programs and phases

A program is 1-8 families. A step's write rounds run it as pairs: rounds `2n` and `2n + 1` are
pair `n`'s P and ~P under one key, so every bit is written and verified as 1 and as 0.

**The round.** `wrc` is the step's `write_read_cycles` and `c` its own cycle, from 1: the
cycles it runs within one dispatch (a TM5 step runs one). A step writes first when `test_reps` is
above 0; then write round `r` (0 to `wrc - 1`) of own cycle `c` is round `g = (c - 1) * wrc + r`.
A chunk's rounds run back to back, and an odd `wrc` rounds up to even when the run loads (with a
warning), so every chunk ends each cycle on whole pairs, under any cycle count or time limit.
A config's SimpleTest and a TM5 import take 4 by default, the built-in suite's 2.

A step that verifies a fill (`test_reps` 0) writes one image a suite cycle, so its pairs span
two, as the seal's do (below): suite cycle `c` writes pair `(c - 1) / 2`'s P when `c` is odd
and its ~P when even, under the epoch's suite cycle `ceil(c / 2)` in `step_key`.

**The phase of round `g`.** With the step's epoch `(seed, suite_cycle, step)`; `pair` below is
the global pair index, of which the step's own rounds `2n`, `2n + 1` are pair `n + rotation`:

```text
rotation = (suite_cycle - 1) * ceil(wrc / 2)        (0 when suite_cycle is 0)
pair     = g / 2 + rotation
family   = program[pair mod len]
step_key = mix(mix(mix(seed, suite_cycle), step), 0x5EED)
key      = mix(mix(step_key, pair), code(family))
mask     = all ones if g is odd, else 0
codes:   ident 0x1D, random 0x2A, switch 0x3C, lines 0x4B, linepair 0x5E, quiet 0x69, header 0x78
```

- **Rotation.** A TM5 step runs one cycle of its own with 4 rounds, so it covers the program's
  next two pairs each suite cycle.
- **No thread or chunk in the key.** Every thread's words differ through their ids, and a word's
  value doesn't depend on which chunk wrote it.
- **Edge cases.** A step with many own cycles a suite cycle covers more pairs than the rotation
  advances, so suite cycles repeat pair numbers; the suite cycle is in the key, so their images
  still differ. A halt (`errors=halt`) can stop a chunk between its P and its ~P, and a run that
  ends after an odd number of suite cycles leaves a verify-only step's last pair without its ~P.
- **The epoch.** The runner sets it per dispatch: `seed` is the run's, `suite_cycle` counts from
  1, and `step` is the step's place in the cycle from 0, so a test listed twice is two steps. All
  zero outside a run, and for the Bench pairs.

**The transfer width** for `switch`, resolved once, in this order:
1. `beat=`: a width (`12`, `16`, `24`, `32`, `64`) or a memory type (`ddr4`, `ddr5`, `ddr6`,
   `lpddr4`, `lpddr5`, `lpddr6`; `ddr3` and `lpddr3` too).
2. The DIMMs' SMBIOS memory type: DDR-DDR4 64, LPDDR3 and DDR5 32, LPDDR4/5 16. TMR knows no
   SMBIOS code for DDR6 or LPDDR6 yet: on those, set `beat=`.
3. Unknown: successive `switch` pairs alternate 64 and 32, and the run logs a warning. Pair
   `pair`'s ordinal among the `switch` pairs is `n = (pair / len) * s + s_before`, with `s` the
   program's `switch` entries and `s_before` those before position `pair mod len`; it takes 64
   when `n` is even, 32 when odd. (An earlier rule, `(pair >> 1) & 1`, fixed the width for some
   program shapes; the independent implementation found it.)

This promises no switching floor on an unknown bus. Measured as the share of beat-to-beat
transitions inside one 64 B line on which a data line toggles, in the worst line, the 64-bit
profile gives 46.7% (7/15) at 32-bit beats, 22.6% (7/31) at 16, and 0% at 128. Results record
`transfer_bits` as null and the source as unknown.

**Program or mode.** A JSON test takes `program` or `pattern_mode`, not both (a load error). The
command line's `pattern-mode=` replaces any test's program. A `.cfg`'s Pattern Mode is TM5's
and imports as below; one of TMR's own (10, 11, 13, 20, 30) stays that numbered mode, and a native
family's number (Families) is that family alone.

**Programs TMR sets.**
- **No `program` and no `pattern_mode`:** `[random, switch]`: a pair of each every cycle at
  four rounds. `random` is an identity too and gives every data line all short histories.
- **TM5 `.cfg` SimpleTest:** Pattern Mode 0 imports as `[ident, switch, random, switch]`, 1 as
  `[linepair, switch, random, switch]`, 2 as `[lines, switch, random, switch]`, all on
  `Mem-SimpleV2-Auto`. Switching runs every suite cycle, the mode's intent and mixed data every
  other.
- **`patterns=legacy`:** the mode stays TMR's numbered mode on the scalar `Mem-SimpleV2`.

## The seal (spec v2)

Each suite cycle `c` (from 1) has a kernel of its own. Cycles go in pairs, `p = ceil(c / 2)`:
`key = fmix(seed ^ fmix(p + 0x5EA1))`, and the cycle is inverted when `c` is even. So each bit takes
both values over a pair, and memory still holding the last cycle's seal fails: inverted within a
pair, another key across pairs. One kernel serves the whole cycle: the seal at its start, every
check and reseal, and the final check.

- **`tmr`:** for the word's virtual byte address `A`,
  `x = ((A >> 4) * G + key) & 0x7FFF_FFFF_FFFF_FFFF` and `v = x ^ (x >> 32)`. The word is `v` when
  `A & 8 == 0`, `!v` otherwise, then inverted in an inverted cycle. `v` has its top bit clear and
  `!v` set, and `x` is a bijection of the 16 B unit modulo 2 to the 63rd, so no two words of one
  cycle are equal. Keyed by address, so `seed=` replays it only at the same addresses.
- **`tm5`:** TM5's `RS_GeneratePattern` motif (`seal.rs` `tm5_motif`, checked against TM5's
  binary), bit for bit in odd cycles, every word inverted in even ones. The cycle's key doesn't
  apply to it.
- `seal=tmr|tm5|off` picks the pattern (`doc/test_parameters.md`).

## Verification

- **Tier 1 (hot).** Each word's `actual ^ expected` is ORed into independent accumulators (4 in
  the native kernels and most others; one in some SIMD and strided ones, three in the `tm5` seal),
  with one branch per chunk, or per batch where an `ErrorCheckInterval` is set (read only by the
  sequential SIMD positional verify, modes 10 and 11 and SimpleNT; every other kernel, the native
  ones among them, checks per chunk whatever it says; only per chunk is built today); no per-word
  branch, count or log.
- **Tier 2 (cold, only on a hit).** Before anything overwrites or reseals the region:
  - It is rescanned with volatile loads. Each bad word is counted, and the first 10 are logged
    with index, address, expected, actual and XOR.
  - The first 4 are reread 4 times, then once more after flushing their line, and the results
    logged.
  - The first 16 rescans of a test reread.
  - A rescan that finds every word right logs "not reproduced on rescan" and counts one error.
  - Every failed check leaves one record: the region, and for a native kernel its phase (family,
    profile, polarity), key and round; the check's ORed differences; what the rescan found, its
    bad words' differences ORed; and the bits the check saw that no bad word of the rescan had. An
    ORed mask names bit positions over the region, not which words, how many or which way.
- **`rescan=off`.** Tier 2 is skipped and a failed check counts one error.
- **Counts**, per test and thread:
  - `errors`: the bad words the rescans found, plus one for each not reproduced.
  - `alarms`: the test's failed checks. The seal's own checks (before a chunk, between steps, the
    final check) are seal errors and not in it; a test that verifies with the seal (a mirror,
    Seal-Check, Bench-Verify-Seal) counts its checks.
  - `not_reproduced`: the test's failed checks whose rescan found nothing; none under
    `rescan=off`.
  - `failing_bits`, by bit position: the failed checks whose ORed differences had it, and the bad
    words the rescans found with it wrong, 0 to 1 and 1 to 0; `seal_failing_bits`, the seal's own
    checks'. Under `rescan=off`, checks alone.
  - A bad word counts once a rescan, even where Mem-Random's replayed reads land on it again; its
    record counts the repeat reads.
  - "The first 16 rescans" count per test run on each thread.
- **What the rescan can't establish.**
  - A word still wrong on reread may be a cached copy of a bad read.
  - The flush writes a dirty line back before it evicts it, so a word wrong after the flush may be a
    bad cached copy the flush wrote out, and one right after it may have been fixed by the
    writeback. Neither proves a weak cell, a stuck bit or a soft error.
  - "Not reproduced" names no cause.
- **`errors=halt`.** The test stops right after the check that failed: the seal check before a
  chunk, or the verify. It neither writes the chunk again nor reseals. The mirror tests and
  Seal-Check don't refill, and a step whose between-steps seal check failed isn't dispatched.
  TMR doesn't overwrite what it found; a transient can still change on its own.

## Run settings and provenance

- **`seed=`** (decimal or `0x` hex) sets the run seed. By default it comes from the clock. Either
  way it is logged, printed and saved as `run_config.execution.pattern_seed`.
- **`beat=auto|12|16|24|32|64|<memory type>`**: the transfer width above. Saved as
  `transfer_bits` and `transfer_source`, which says when the profile is a model.
- **`rescan=on|off`**: Tier 2, on by default. Saved as `rescan`.
- **`patterns=native|legacy`**: how a `.cfg` imports.
- **Per test**, results record `program` (the family names) when it ran one.
