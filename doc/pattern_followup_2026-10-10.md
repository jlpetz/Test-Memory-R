# Pattern follow-up: fast verification, signal histories, and cache exposure

2026-10-10. Follows [the main audit](pattern_audit_2026-10-09.md).
Production code and the parent TODO file remain unchanged. This work used source inspection,
bounded logical simulations, and a compile-only experiment; no TMR or hardware stress run.

## Preserve accumulator verification

The user's clarification is correct: grouped error reporting is an intentional performance
choice. It is not inherently weaker mismatch detection.

For the values actually loaded, the reduction is:

```text
difference[i] = actual[i] XOR expected[i]
alarm = OR of all difference[i]
```

Provided every intended load reaches the reduction, any nonzero difference makes `alarm`
nonzero. Two identical errors cannot cancel. This differs from XORing all the differences
together, where cancellation would be possible. Multiple independent accumulators can be
OR-reduced at the boundary without weakening this property.

TODO 27, **Two-Tier Error Isolation Verifier**, already describes the right architecture.
Its "Not Started" status understates the implementation:

| Current path | What happens after a detected mismatch |
|---|---|
| `verify_words` and several scalar/strided tests | Cold rescan counts/logs observed wrong words; retains one alarm if rescan is clean |
| Mode 12 SIMD and strided positional SIMD | Cold scalar rescan |
| Seal verification | Cold rescan, including the disappearing-mismatch fallback |
| Sequential positional SIMD | One error per failing batch; no corresponding detailed rescan |

The remaining job is to complete appropriate cold paths and describe the observations
accurately. **Do not add per-word branches, logs, or counters to the successful hot path.**
Retain the existing reduction approach and the project's four-accumulator rule when refactoring
latency-sensitive loops. Different batch sizes need not be changed merely to equalize a counter.

The previous audit's phrase "standardize error counting" was too broad. A failing batch,
three words observed wrong on a later rescan, and three independent hardware failures are
different things. A cold rescan cannot reconstruct an exact original error count.
Document the counting unit and distinguish first-pass alarms from rescan observations, using
the existing error-reporting path. For pattern comparisons, detection/no detection and time
to first failure are more useful than raw counts with different units.

A bounded cold-path procedure should:

1. Preserve the failing range, original accumulated difference, and pattern epoch/phase.
   The accumulated mask is an aggregate, not the XOR of one identifiable bad word.
2. Rescan before writing or resealing the region. Record a bounded number of actual observations:
   address, expected, actual, and their XOR. Expected values use the original immutable phase.
3. If the rescan is clean, retain the original alarm and say **not reproduced on rescan**.
4. If a word still differs, perform a small number of explicit reloads if useful. Record which
   rereads reproduced it. Rate-limit additional diagnostics in continue-after-error runs.
5. Respect halt mode before subsequent destructive test writes. Do not replace a detected
   failure with success because its evidence later disappears.

"Not reproduced" does not identify a timing cause. "Reproduced" does not prove a permanent
DRAM defect: a transiently corrupted read can populate a cache and reproduce from there.
TODO 27's proposed mappings from lane shape to addressing/DQ faults are hypotheses, not reliable
diagnoses. With virtual addresses alone, neighboring addresses do not establish neighboring
DRAM rows or physical data pins. Keep the report descriptive.

### Repeated rereads must actually reload

The compile-only [reload probe](../research/pattern_audit/reload_probe.rs) requests four reads
and ORs their differences. With the existing Rust toolchain, `-O`, and `x86-64-v3`:

| Source operation | Emitted memory reads |
|---|---|
| Four ordinary `ptr.read()` calls | One |
| Four `ptr.read_volatile()` calls | Four |

This does not show that today's cold rescans are missing their loads. It demonstrates a
requirement for TODO 27's proposed repeated rereads. Use volatile reads or an appropriately
specified assembly helper **in that cold diagnostic loop**, and inspect its assembly.
Volatile enforces compiler-visible accesses; it does not bypass cache, provide a delay, or make
concurrent non-atomic access safe. [F1]

### What "reject verification-free settings" meant

This is unrelated to accumulation:

| Setting in a standalone SimpleTest correctness step | Current problem |
|---|---|
| `verify_reps: 0` | Writes run, but the test-data verify loop never runs |
| `write_read_cycles: 0` | The intended write/verify rounds do not run |
| `test_reps: 0` with the seal off and an epoch-dependent mode | The existing initialization/verification epochs disagree, producing false failures |

These were reproduced through accepted JSON in the main audit. A seal check verifies its own
markers; it cannot substitute for checking data written and then overwritten by the test.

Validate these settings according to the test's purpose before allocation. Explicit write-only
stress, read-only verification with valid initialized data, and benchmark pairs remain legitimate
roles. The requirement is that a step advertised as correctness coverage actually verifies its
promised data. It does not require one comparison branch per word.

## Add history coverage to pattern scoring

DDR5's DFE addresses interference from preceding symbols. Keysight's DDR5 analysis documentation
uses four preceding symbols and discusses reseeding at burst boundaries. This motivates measuring
five-bit windows: four preceding bits plus the current one. It does not establish an exact model
for every controller or a fault probability. Micron confirms DFE's DDR5 role, and Altera's DDR5
board-simulation guidance uses PRBS with physical channel models. [F2–F4]

I scored 1,024 synthetic 64-byte lines per candidate. Each table entry measures logical payload
bits at a **32-bit beat**, excludes windows crossing bursts, and includes no idle/preamble,
scrambling, ECC, or analog channel model.

| Pattern | Bits changing per beat | Five-bit histories seen per logical bit, out of 32 |
|---|---:|---:|
| Mode 14 | 100.00% | 2 |
| Mode 13, scalar mathematical reference | 49.88% | 32 |
| Cheap full-address identity | 50.14% | 24–32 |
| Current seal | 50.07% | 16–24 |
| Balanced triple | 66.67% | 6 |
| Balanced near-flip | 93.75% | 22–31 |
| Balanced identity header / flip tail | 89.93% | 16 |

Seeing a history once is not the same as exercising it frequently. The simulation also records
per-bit histograms: mode 13's mean five-bit entropy was 4.998 bits; mode 14's was 1.000.
These are distribution measurements, not a universal pattern-quality score.
The mode-13 row uses its intended scalar formula; the previously identified production SIMD
substitution remains an implementation issue.

Mode 14 creates alternating histories within a burst: `01010` and `10101`. That is appropriate
for its switching purpose, but it cannot replace a mixed-data phase. The seal's narrower
history coverage is likewise not a reason to discard an identity/retention mechanism.

### A proved limitation of the cheap identity proposal

For the proposed fast permutation:

```text
x[n] = n*G + K  (modulo 2^64)
y[n] = x[n] XOR (x[n] >> 32)
```

The upper 32 bits of `y` are unchanged upper bits of `x`. Consider one such bit `b` and five
consecutive 64-bit words. Write `t = x[n] mod 2^(b+1)`. Each of the five observations asks
whether `t + j*G` lies in the upper half of this modular circle. Each observation introduces
two boundaries. There are therefore at most ten intervals on which the entire five-bit
history can differ: **at most 10 of the 32 histories**, over all starting addresses and keys.
In general the bound is `2h` for an `h`-bit history of one bit of an arithmetic progression.
Complementing the sequence does not remove the bound.

The 64-bit-beat simulation reached ten histories for each upper bit, while mode 13 reached
32 for every bit in this sample. A reduced exhaustive check covers all 128 odd steps and
256 starting phases of an eight-bit circle and confirms the bound for that finite case.
The argument above, rather than that finite check, establishes the wider result.

**Decision:** retain the cheap permutation for address identity. Keep a separate strongly
mixed family; do not replace all hashing with the cheap mixer based only on distinctness,
average toggles, and speed. Changing an epoch by adding `K` does not fix this correlation.

### A quiet victim is another distinct condition

On one transition, if `t` of `W` logical data bits toggle, there are `t*(W-t)` ordered pairs
consisting of a quiet bit and a different toggling bit. At maximum switching, this is zero.
For random independent bits, a given ordered pair has this relationship approximately 25%
of the time.

The modeled 32-bit mode 14 gave 0%; mode 13 gave 24.98%; near-flip gave 6.05%. These are
logical pair counts, not physical coupling probabilities. Unknown wiring means software
cannot pick an actual neighboring victim/aggressor pair.

This supports researching rotating quiet-bit/mask phases and varied runs as complementary
stimuli, rather than ranking every pattern by the largest toggle count. Balanced-byte
encodings also impose correlations: the current nibble/complement encoding always makes each
low-nibble bit the opposite of its high-nibble partner. Different masks/permutations can vary
which logical bits are tied together across phases.

## Controller transformations: test compositions, not just isolated properties

Intel documents scrambling intended to reduce pattern-dependent electrical stress. It does
not follow that every scrambler changes switching in the same way. [F5]

For XOR masks `M[t]`, the transition is:

```text
(D[t] XOR M[t]) XOR (D[t-1] XOR M[t-1])
```

A common mask cancels; a changing mask generally does not. Fixed pin permutations preserve
total Hamming distance but change which pins carry particular signals.

The follow-up applies illustrative transformations to mode 14:

| Transformation model | Resulting toggle fraction |
|---|---:|
| No transformation | 100% |
| Fixed permutation within each byte | 100% |
| One common XOR mask across a burst | 100% |
| XOR masks alternating between zero and all ones | 0% |
| Independently mixed XOR mask for each beat | 50.08% |
| DDR4 DC-byte DBI applied directly to balanced bytes | 100% |
| Common XOR mask, **then** that DBI rule | 27.71% in the sample |

The last result has a simple explanation. A common XOR mask preserves complementation but may
destroy four-ones-per-byte balance. DBI then maps both members of a complementary byte pair to
the same data byte unless they are balanced. Exactly `C(8,4) = 70` of 256 possible masks leave
that pair balanced, giving an exhaustive mean of **70/256 = 27.34375%**.

This is a conditional composition experiment, **not** an assertion that any PC uses that
ordering or mask distribution. Applying the DDR4 byte rule to a 32-bit model is not evidence
that DDR5 uses DQ DBI. Micron distinguishes DDR4 DBI from DDR5 command/address inversion. [F2]

The practical change is precise wording: "balanced at the DBI input" is the invariant.
Balanced software bytes alone are insufficient if an earlier transformation changes them.
General identity verification remains hardware-independent: a correctly functioning controller
must return the software-visible data that was stored. Its physical stress properties need
separate qualification.

## Cache exposure and cold rescans

The existing flush helper uses `CLFLUSHOPT` followed by `MFENCE`. Keep the phase-level fence.
However, the descriptions "equivalent to UC" and "the next access must go to DRAM" in
`doc/cache_management.md` are too strong. Cache invalidation does not change the memory type;
speculative fetching can refill a line. `CLWB` alone is unsuitable as a replacement when
invalidation is required because it may retain the line. [F6]

For a retention-oriented access sequence, the important order is:

```text
write intended data -> commit/flush and order -> dwell without rewriting
                   -> invalidate any cached copies as needed -> verify
```

A flush only after the dwell can write previously dirty cached data over the DRAM contents,
so the intended value may never have spent the dwell in DRAM. A flush only before the dwell
does not establish that no clean copy was fetched during the dwell. Policy must follow the
actual chunk/reuse schedule, not just the total extent size. This strengthens the earlier
refresh finding; it does not justify flushing every cache-stress loop.

For diagnostics, capture the immediate rescan **before** an optional flush/reload experiment.
Flushing can discard evidence held in a clean cache line, or write back a dirty line; it changes
the experiment. Record subsequent observations separately. Neither stage can locate the fault
in a particular DRAM cell, cache, or controller.

For broader interference coverage, preserve selected victim data while performing activity
elsewhere, then verify it before overwriting. A modulo-style access plan is an established
example [F7]. Use the existing virtual-address architecture and make no row-adjacency claim.

## Revised priorities and inexpensive validation

1. Keep accumulator detection. Complete cold evidence capture where useful, retaining
   disappearing alarms and avoiding unsafe causal labels.
2. Fix actual coverage defects: disabled verify loops, kernel image disagreement, phase/epoch
   handling, collapsing mode 2, and refresh/cache ordering.
3. Use complementary intent profiles: address identity, strongly mixed history, switching,
   and retention/protected-victim sequences. Evolve epochs without changing an expected image
   between its write and verification.
4. Evaluate candidate histories per logical bit, polarity, runs, quiet/toggling relationships,
   identity granularity, transformation assumptions, and generated instruction cost.
5. Use bounded corruption injection and cross-kernel agreement before any expensive comparison.
   Reserve hardware A/B work for a small candidate set with a distinct coverage benefit.

The offline work can reject implementation mistakes and prove some pattern limitations.
It cannot establish which candidate finds the most real overclock failures per minute.
Mode 14's existing hardware result remains reason to retain it while adding complementary
coverage.

Reproduction files and commands are in
[research/pattern_audit/README.md](../research/pattern_audit/README.md).
The simulation finishes in a few seconds here. The reload probe was compiled with warnings
denied and its assembly inspected: one ordinary load versus four volatile loads. No production
build or hardware run was needed for these new findings.

## Sources

- **F1 — Rust, `read_volatile` documentation.**
  <https://doc.rust-lang.org/std/ptr/fn.read_volatile.html>
- **F2 — Micron, DDR5 feature comparison:** DFE, DDR4 DBI versus DDR5 CAI, training patterns.
  <https://www.micron.com/products/memory/dram-components/ddr5-sdram>
- **F3 — Keysight, Equalization (DFE) for DDR5 Signals:** four-tap analysis and burst reseeding.
  This is instrument guidance, not a complete model of every controller.
  <https://helpfiles.keysight.com/csg/d9300a/Help/Infiniium-UG/Content/Topics/Clock_Recovery/DDR5_Equalization_DFE.htm>
- **F4 — Altera, DDR5 Simulation Strategy**, 2026-08-11. PRBS and physical-channel modeling.
  <https://docs.altera.com/r/docs/772538/26.1.1/external-memory-interfaces-agilex-7-m-series-fpga-ip-user-guide/ddr5-simulation-strategy>
- **F5 — Intel, Data Scrambling, 12th Generation Core processor datasheet.**
  <https://edc.intel.com/content/www/us/en/design/ipla/software-development-platforms/client/platforms/alder-lake-desktop/12th-generation-intel-core-processors-datasheet-volume-1-of-2/001/data-scrambling/>
- **F6 — Intel instruction descriptions, reproduced in the Felix Cloutier instruction reference:**
  <https://www.felixcloutier.com/x86/clflushopt> and <https://www.felixcloutier.com/x86/clwb>.
  Official manual landing page: <https://intel.github.io/SDM/sdm.html>.
- **F7 — PassMark, MemTest86 Test Algorithms**, moving inversions and modulo methodology.
  <https://www.memtest86.com/tech_memtest-algoritm.html>

Local sources: `../TODO.md` TODO 27; `src/tests.rs` verification helpers and SIMD macros;
`src/seal.rs` cold rescan; `src/config.rs::apply_repetition`; `src/test_harness.rs`;
`doc/cache_management.md`; the existing audit's reproduction tests.
