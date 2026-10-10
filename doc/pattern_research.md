# SimpleTest pattern research (2026-10-09)

> **Superseded by [pattern_audit_2026-10-09.md](pattern_audit_2026-10-09.md).** That audit checked
> this research against the code and found several claims here wrong. Among them:
> - "New values each cycle": the cycle restarts with every test invocation.
> - The triple's "50% floor": 50% is only an average, and valid instances flip 0% at 24 bits.
> - The unconditional 2/3 bound: it doesn't hold for a finite 64-byte line.
> - The 2× speed claim: it isn't supported, even by this document's own numbers.
> - DBI ruling out pairs on DDR5: DDR5 has command/address inversion, not DDR4's DBI.
> - The mode 1 proposal: inverting independent lines doesn't make neighbours complements.
> - The seal's per-word uniqueness: it holds per 16-byte pair, not per word.
>
> Kept for the record and for its scripts. Use the audit's findings and implementation order.

What TMR's data patterns should be on DDR4 and later (DDR5 the main target, DDR6 to come), judged
on measured properties rather than on what TM5 did. TM5 was designed in the DDR2/DDR3 era.

Everything here was generated or measured, not estimated. The scripts are in
`../patterngen-test/sim/`: `candidates.py` scores the patterns, `tm5_vs_tmr.py` simulates TM5 from
its asm. The speed probe is `../patterngen-test/`. The hardware results are the mode 14 A/B runs of
the same day.

## Terms

- **Beat:** one transfer on the memory bus. A 64 B cache line goes out as a burst of beats on one
  channel: 8 beats of 64 bits on DDR4, 16 beats of 32 bits on DDR5 (two 32-bit subchannels per
  DIMM), 32 beats of 16 bits on LPDDR4/5. DDR6's layout is not public yet.
- **Flips:** the share of data lines whose value changes from one beat to the next inside a burst.
  More flips means more electrical noise on the bus, which marginal overclocks fail under.
- **Variety:** how many different values the pattern can put at different addresses. A read or
  write that lands at the wrong address only shows if the two addresses hold different values.
- **DBI (data bus inversion):** a power-saving feature of DDR4 x8/x16 chips. Per beat, a byte with
  more than four 0 bits is sent inverted, with an extra pin saying so, and the receiver inverts it
  back. The data is unchanged, but a byte our pattern meant to flip may not flip on the wire. Whether
  DDR5 desktops use it I could not confirm. Patterns whose bytes hold exactly four 1s are immune.
- **Scrambling:** some controllers (Intel's by default) XOR data with an address-keyed stream before
  the bus. TMR can't see it. If it is on, every pattern looks random on the wire. TMR assumes it is
  off.
- **Channels:** single or dual channel doesn't matter here. A burst goes to one (sub)channel, so
  the beat width is fixed by the memory type; the channel count only decides which lines go where.

## Criteria, and which test cares

| Criterion | What it means | Weighs most for |
|---|---|---|
| Flips | data lines toggling per beat, at the real beat width | bus noise: marginal overclocks (the 8600G's failing data line) |
| Variety | distinct values per address | misplaced accesses: address or command errors |
| New each cycle | values change between cycles, so every cell holds both 0 and 1 | stuck bits |
| Position-pure | a value depends only on its address (and key) | every TMR test: chunks overlap; the strided walk visits lines out of order |
| Cheap | generate and verify faster than one thread's DRAM bandwidth | everything: verify is 20 of a SimpleTest step's 24 passes |
| Robust | survives DBI and the controller's burst order | flips |
| Universal | one implementation for DDR4, DDR5, LPDDR, DDR6 | maintenance |

Retention (data sitting untouched under load) is the seal's job. Any pattern works for it, so long
as it changes polarity over cycles and is unique enough to catch a write that lands in the wrong
place.

## Finding 1: flips at two widths can't both exceed 2/3

For one bit position, look at three consecutive 16-bit blocks. Two of the three must hold the same
bit, so at most two of the three neighbouring pairs differ. Summed over the line: if a pattern flips
a share `a` of lines at width W and at 2W, then `2a + a <= 2`, so `a <= 2/3`.

An exhaustive search of repeating patterns (periods 2-12) confirms the bound, and that period 3
reaches it at every power-of-two width. So:
- **Width-specific (mode 14):** 100% at the width it was built for, and 0% at double that width.
- **Universal (period 3):** 67% at 16, 32, 64 and 128 bits from one implementation. At a
  non-power-of-two width (24 bits, in case DDR6 uses one) it falls to random's 50%, never below.
- **Random data:** 50% everywhere.

Nothing does better than these three. Universality costs a third of the maximum switching.

## Finding 2: candidate scores

Flips at beat widths 16 / 24 / 32 / 64 bits, then with DBI at 16 / 32 / 64, over 4096 lines:

| Candidate | Flips 16/24/32/64 | With DBI 16/32/64 | Variety per line |
|---|---|---|---|
| TM5 mode 0 (reference) | 0.41 / 0.50 / 0.49 / 0.77 | 0.42 / 0.48 / 0.33 | none: every line the same |
| random per word (modes 0, 13) | 0.50 everywhere | 0.46 | 512 bits |
| pairs `[v, !v]` (seal `tmr`) | 0.50 / 0.50 / 0.50 / 0.79 | 0.46 / 0.46 / **0.35** | 256 bits |
| flip at 32 (mode 14, DDR5) | 0.50 / 0.50 / **1.00** / **0.00** | 0.50 / 1.00 / 0.00 | 16 bits |
| flip at 64 (mode 14, DDR4) | 0.50 / 0.50 / 0.50 / **1.00** | 0.50 / 0.50 / 1.00 | 32 bits |
| near-flip at 32 (all but one line flip) | 0.50 / 0.50 / 0.97 / 0.06 | 0.46 / **0.29** / 0.15 | ~107 bits |
| **triple, 16-bit, balanced bytes** | **0.68 / 0.50 / 0.67 / 0.68** | **0.68 / 0.67 / 0.68** | 16 bits |
| triple, 16-bit, random bytes | 0.68 / 0.50 / 0.67 / 0.68 | 0.40 / 0.40 / 0.40 | 32 bits |
| triple, new values each period | 0.68 / 0.50 / 0.50 / 0.50 | — | ~340 bits |
| triple, 8-bit blocks | 0.67 / **0.00** / 0.67 / 0.67 | — | 16 bits |

**The triple:** 16-bit blocks `A, !A, C` repeating, one `A` and `C` per line from a hash of its
address, every byte four 1s. The line is three 64-bit words repeated as `w0 w1 w2 w0 w1 w2 w0 w1`.

What the table shows:
- **DBI decides between near-equals.** Pairs and TM5's mode 0 owe their DDR4 edge to unbalanced
  bytes, so with DBI on they fall below random. Only the balanced-byte designs (mode 14, the
  balanced triple) keep their switching.
- **Switching and variety trade off within a line.** Every-beat flipping fixes the whole burst from
  its first beat, so variety is at most one beat's width (less with balanced bytes). No pattern gets
  both in one line.
- **The triple needs 16-bit blocks.** With 8-bit blocks it gives 0% at 24 bits, because 24 bits is
  exactly one period.

## Finding 3: speed is the hash, not the pattern

One thread, this dev box (Xeon VM), GiB/s, two runs agreeing:

| Candidate | Fill (cache) | Verify (cache) | Fill (DRAM) | Verify (DRAM) |
|---|---|---|---|---|
| counter (mode 10) | 36 | 16 | 9.5 | 11.5 (the DRAM ceiling) |
| random per word, splitmix (mode 13) | 11.5 | 10 | 9.9 | 3.9 |
| random per word, cheap mix | 32 | 15.4 | 9.5 | 8.3 |
| TMR mode 0 now (a multiply per word) | 24 | 14 | 9.3 | 6.6 |
| flip at 32 (mode 14) | 22 | 15.5 | 9.3 | 6.8 |
| triple, cheap mix | 15.8 | 13.9 | 10.0 | 6.3 |

- **The cheap mix:** a Weyl step and an xorshift, `x = unit * G` once per line, then `+ G` and
  `x ^ (x >> 32)` per unit. It is a bijection, so values stay distinct (32768 of 32768 words),
  with exactly half the bits set and the same flips. It is what the seal already uses.
- **Strong hashing halves verify speed.** A strong hash per word (splitmix, 2 multiplies) halves
  verify speed, which matches TMR's own benchmark: modes 0, 1 and 13 verify at 28-31 GiB/s against
  about 57 for the others, on 4 threads.
- **Writes are never limited by the pattern:** every mode filled at 38-41 GiB/s in TMR's benchmark.
- **The seal does pairs fast.** The probe's naive version is slow, but the seal's SIMD kernel
  checks the same pattern at about 60 GiB/s on 4 threads.

## Finding 4: on hardware

The mode 14 A/B: sequential SimpleTest, the whole allocation, 10 minutes a run, alternating.
- **The 8600G box** (DDR5-6000 on a CPU rated for 5200): mode 0 found 0 and 0 errors; mode 14 found
  840 and 170. Every bad word was bit 31 of a line's first word, one data line failing under full
  switching. Mode 0 never provoked it.
- **The 5700X box** (DDR4-3600, stable): 0 errors in all four runs, mode 14 included, at the
  same speed. So mode 14 gives no false alarms on a stable machine.

## Recommendations

**Per intent:**
- **Bus noise:** flip at the memory type's beat width (mode 14), which is the clear winner where the
  width is known. Where it isn't, use the triple: 67% at any width, where a wrong guess at mode 14's
  width gives 0%.
- **Misplaced accesses:** random per word with the cheap mix, a new key each cycle.
- **Stuck bits:** a new key each cycle, or an inverted pass.
- **Retention:** the seal, as it is.

**Changes, smallest first:**
1. **Mode 14 without a known memory type:** use the triple instead of assuming DDR5's 32 bits. A
   wrong guess at 32 bits on DDR4 flips 0%; the triple flips 67%.
2. **Modes 0 and 13: the cheap mix.** It doubles verify speed and changes no property. Mode 0
   keeps its per-cycle key and page inversion.
3. **Mode 1: TM5's intent.** Switching inside the line, with neighbouring lines opposite: the
   balanced triple with every other line inverted. That is 67% at every width, immune to DBI, and
   neighbouring lines opposite. It replaces a counter that flips 3% on DDR4.
4. **Sequential mode 2: mode 12's lines,** as strided mode 2 already writes. That ends the collapse
   (TODO 95), and it is fast and unique.
5. **Not now:** rotate patterns across a step's 4 write passes (flip, its inverse, random, its
   inverse), so one step gets maximum switching, variety and both polarities. That needs the harness
   to pass the pass number to the pattern, and it departs from TM5's same-pattern passes; an
   option for a TMR-plus plan.

**What this changes from my last message:** that message recommended the `[v, !v]` pairs for
modes 0 and 1, to beat TM5's DDR4 switching. The DBI simulation, run afterwards, shows pairs drop
below random once DBI is on. So the recommendation is now random with the cheap mix for mode 0,
and the triple for mode 1.

## Unknown, and what would settle it

- **DBI on DDR5:** whether it exists, and whether desktops enable it. The balanced-byte designs
  don't depend on the answer.
- **Scrambling:** can't be seen, so TMR assumes it is off. An A/B on a machine that fails (like
  the 8600G) with scrambling switched off in the BIOS would show how much it matters.
- **DDR6 and LPDDR6:** the beat layout isn't public. The triple's floor is random-level, and a
  width-specific flip can be added when the layout is known.
- **On-die ECC (DDR5):** it hides single-bit cell errors inside the chip, whatever the pattern.
