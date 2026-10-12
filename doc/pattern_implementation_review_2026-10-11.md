# Native pattern implementation review — 2026-10-11

**Follow-up:** the fixes to the findings below have been independently validated. See the
[follow-up review](pattern_followup_review_2026-10-11.md) for the current verdict, 12/24-bit
checks and a tested performance candidate. This document records the earlier snapshot.

The architecture is worth keeping: position-based identities, keyed P/~P pairs, a separate
switching family, and fast OR accumulators with a cold rescan. The implementation is not ready
to merge unchanged. There is a reproduced failure in result finalization on halt, plus
polarity-coverage and evidence-recording gaps. The linepair identity bound is a lower-priority
coverage limitation, not a merge blocker.

Reviewed the complete working tree above `468f23e35446760e1316930629a0b6efcf410cc4`, including
the untracked `src/pattern.rs`. The index was empty; the changes were **unstaged**.
Production sources and Opus's documents were not edited. No TMR application, remote host,
large-page stress run, commit or push was executed.

The source/config/spec hashes are in
[review-inputs.json](../research/pattern_review/review-inputs.json). Findings below refer to
that snapshot. Runnable reproductions and their output are in
[research/pattern_review](../research/pattern_review/README.md).

**Findings and priorities**

| Priority | Finding | Consequence |
|---|---|---|
| P1 | A seal halt before a step's first execution divides by zero during finalization | The release process aborts before saving its structured result |
| P2 | Legal odd-round settings can omit a family's inverse forever | The promised bounded polarity coverage is not supplied |
| P2 | A persistent rescan can omit the phase and original difference mask | Pattern attribution and partially vanished failure evidence are lost |
| P2, measurement | The published cost table includes fill; short runs still fall back to wall time | The claimed 2–4% verification cost is not yet established |
| P3, nonblocking | `linepair` can duplicate particular words above 32 GiB per thread | A narrowly matching misplaced-word access can pass; ordinary bit-flip comparison is unaffected |

**1. Preserve error-only steps without dividing by zero**

`src/runner.rs:1188` correctly stops before dispatching a step when its preceding seal check
fails under `errors=halt`. It records a `TestSummary` with zero duration and zero bytes.
`src/results.rs:424` puts this entry in the aggregate and retains the seal error, but skips
incrementing `runs`. At `src/results.rs:459`, finalization divides by that zero count.
`display_and_save_results` calls finalization before saving.

This happens if the step has not completed in an earlier suite cycle. The same path after an
earlier completed execution does not hit the zero denominator.

The probe calls the actual production `add_cycle` and `finalize` methods and catches:

```text
src/results.rs:459:35: attempt to divide by zero
```

Release uses `panic = "abort"`. Fix the representation/aggregation of an unexecuted step while
retaining its seal errors and run-failure status. Do not discard the entry to avoid the panic.
Add a regression that reaches finalization and result serialization after a pre-step seal halt;
the existing buffer-preservation tests stop too early to catch this.

**2. Scope `linepair`'s identity claim (P3, nonblocking)**

`src/pattern.rs:160` generates an even line with `ident`, then complements it for the odd line.
Although `ident` is a bijection, taking complements of a subset of its outputs does not
automatically leave the combined set injective.

A concrete example through the real epoch/key API:

```text
program:       [linepair]
seed:          0x1707e896db9674fb
suite cycle:   1
step index:    0
round:         0
pair key:      0x405ac1f500000000
thread:        0
byte offsets:  0 and 0x800000040  (32 GiB + 64 B)
both words:    0x405ac1f5405ac1f5
```

The probe evaluates only the two word formulas; it allocates no 32 GiB buffer. `ident` and
`random` distinguish these offsets. The spec already limits `linepair`'s uniqueness claim to
spans below `2^32` words, but neither configuration validation nor allocation enforces that.

Priority revised after discussing practical impact with the owner: making this a pre-merge
fix was too strong. The witness is a collision between particular words for a particular key,
not the entire image repeating with a 32 GiB period. It matters to an access misdirected
between matching positions; a changed bit in a word still compares wrong. Most per-thread
spans do not approach the bound. A matching word alone also does not make an entire
misdirected cache line compare equal.

`linepair` primarily supplies complementary neighbouring virtual lines. It can retain that
role with a documented uniqueness bound, while `ident` and `random` carry the strict global
identity contract. Those families help cover persistent addressing faults; they do not
guarantee that a transient specific to another phase will recur.

If full-domain identity is required from `linepair` too, the repaired seal's disjoint-polarity
construction supplies one option. For a valid global word ID `i`:

```text
pair_word_id = ((i >> 4) << 3) | (i & 7)
x            = (pair_word_id * G + key) & 0x7fffffffffffffff
v            = x ^ (x >> 32)
word         = v ^ (all_ones if i & 8 else 0) ^ phase_mask
```

`pair_word_id` uniquely names each of the eight corresponding word positions in a pair of
lines. Its 63-bit permutation cannot collide; the line-polarity bit separates the two halves
of the output space. This preserves complementary line pairs and works over the full valid
64-bit word-ID domain. It also fixes the top bit across every word of a line before the
line/phase inversion, changing that bit's signal history. This is not an improvement in
every pattern property for free. Assess the intended stress and cost before adopting it;
update the scalar/vector formulas, specification and known answers together if adopted.

A pattern of actual 16-bit stored values must repeat after at most 65,536 distinct values.
A 16-bit transfer width does not impose that limit on TMR's 64-bit words: four ordered
16-bit pieces can represent all `2^64` word values. Strictly unique identity families are
therefore feasible over TMR's supported ID domain, but are not mandatory for every family.

Also validate the separate word-ID limits: `idx < 2^40` and thread ID below `2^24`.
The current `ident`/`random` partition permits 8 TiB per thread, much larger than this
`linepair` problem, but the identity proof still has preconditions.

**3. Make complete P/~P pairs the scheduling contract**

`Program::phase` (`src/pattern.rs:357`) resets polarity from round zero at every dispatch,
while the suite cycle changes the key. A valid step with one own cycle and
`write_read_cycles=3` repeatedly runs:

```text
suite cycle 1: ident-P,  ident-~P,  switch-P
suite cycle 2: random-P, random-~P, switch-P
suite cycle 3: ident-P,  ident-~P,  switch-P
...
```

The probe follows 16 suite cycles: 16 `switch-P` rounds and no `switch-~P`. Reseeding those
unpaired rounds does not provide deterministic polarity coverage. A one-round dispatch has
the same structural issue. Default TM5 imports with four rounds complete their pairs.

The spec acknowledges an unfinished last pair, but an indefinitely repeating valid setting
should not be described as providing bounded P/~P coverage. Either require complete pairs
for native writes-first tests, or schedule complete pairs explicitly while preserving the
intended workload. Carrying a pair across dispatches requires keeping its key until its
inverse is verified. A new key and an inverse flag are not the complement of the old image.

Verify-only tests (`test_reps=0`) are useful and intentionally stay at P. Keep that mode,
label its coverage accurately, and provide a separate paired schedule if both polarities
are required. Time limits and halts can also truncate coverage; do not turn that into a pass
claim for phases which never ran.

**4. Retain the first alarm's evidence on every cold-path outcome**

`native_rescan` (`src/tests.rs:4396`) passes `phase.label()` only through `Rescan::finish`'s
`what` argument. For 1–10 persistent bad words, `finish` returns without logging `what`;
the individual word and reread messages contain no active family, polarity or pair key.
The original accumulator mask (`seen`) is printed only for a wholly clean rescan or with
`rescan=off`.

A probe supplies an initial mask with bits 0 and 47 set, then leaves only bit 0 corrupted for
the rescan. The resulting two log lines show the persistent word and its rereads, but omit
both `random-~P` and the initial `0x0000800000000001` mask. The run still fails correctly;
the missing item is evidence of the initial bit-47 observation and its pattern context.

Emit one bounded cold-path record for every failed check: family/profile, key, polarity,
round, region, original OR mask, and the subsequent rescan outcome. Keep the initial mask
distinct from the rescanned word masks. A nonzero difference in the former need not recur
in the latter. No per-word logging, counting or new branches are needed in the hot loop.

Keep `alarms` separate from rescanned word observations. `not_reproduced` currently means
the **entire rescan** was clean, not that no transient component existed in other alarms.
An OR mask identifies differing bit positions across the region; it cannot recover the
original address, value, multiplicity or direction of those changes.

**5. Correct the benchmark interpretation and short-run accounting**

`doc/todo/97-native-patterns.md` explicitly says the published verify table was measured
**before** the timing fix, with initialization included in elapsed time. Those figures
support a comparison of the old combined measurement, not the quoted cost of verification
alone. A common fill cost can dilute a verification difference.

The new harness's `untimed_init`/`restart_clock` mechanism is appropriate. However,
`src/runner.rs:1250` and `:1322` use the worker's own duration only when it reaches 1,000 ms.
Below that they use wall time, reintroducing the fill. This contradicts an unconditional
"Bench-Verify excludes fill" claim for short runs. Represent whether a timed interval exists
explicitly and retain adequate duration precision rather than using one second as the test.

For full-thread throughput, distinguish the sum of each worker's independently timed rate
from throughput over a shared measurement interval. There is no need to change correctness
workers into synchronized per-operation workers. A benchmark-only boundary after setup can
define a common interval if that is the measurement wanted.

The existing results do not justify deleting `random` for speed. Re-measure its isolated
verification cost with corrected accounting when hardware benchmarking is next convenient.
No additional stress session is needed to fix the accounting.

**Additional bounded findings**

| Item | Evidence and scope | Action |
|---|---|---|
| Native `ErrorCheckInterval` is ignored | Native reductions check at the chunk end. A probe requests interval 8, corrupts words 8 and 40, and gets one alarm for the whole chunk. Current configuration only constructs `PER_CHUNK`; this is an internal API/plan gap, not a currently exposed CLI regression. | Implement interval boundaries outside the inner reduction, or explicitly constrain the native API. Preserve OR accumulation. |
| Native dependent Bench identity is inconsistent | `Bench-Init-Ident` advertises internal pattern mode 300; `PatternId::for_test` expects mode 10 for `Bench-Verify-Ident`. The otherwise matching pair is rejected with `skip_init=true`. `skip_init` is not presently exposed in JSON/CLI. | Give native programs a coherent pattern identity in dependency validation before exposing this mode. |
| New lint suppression bypasses a workspace rule | `native_simple_impl` and `simple_native_widest` add `#[allow(clippy::too_many_arguments)]`. | Bundle cold call parameters and destructure before the loop, as the workspace requires. |

**What the implementation gets right**

The independent fault-injection checks passed 24,948 cases across scalar, 128-, 256- and
512-bit native kernels, all five families, three switch widths, sequential and strided walks,
nonzero chunk starts and 37-line tails. Each word was individually corrupted, bit positions
covered all 64 positions across the buffer, and two equal-bit corruptions did not cancel.
Canaries outside the write range stayed intact. This supports retaining the OR accumulators.

A simulated fast-check alarm whose rescan reads clean still returned failure and incremented
`not_reproduced`. `rescan=off` returned an unlocalized alarm without claiming a rescan result.
A native halt retained the corrupted buffer after localization.

`ident` and `random` are bijections for valid IDs under one family/key/polarity. Native
position purity, the common key across threads, legacy image fallbacks, strict numeric
configuration parsing and saved provenance are substantial improvements. The seal's
63-bit base plus complementary word is a sound injectivity construction for aligned valid
addresses; its shared key over a pair of suite cycles gives the intended complementary
stored image. General identity and verification need no DDR transfer width.

These properties apply to their stated domains. They do not prove that every fault is
observable through controller correction, that software polarity equals physical cell
polarity under encryption, or that every pseudo-random finite sample contains every history.

**Status against the plan**

| Plan area | Assessment |
|---|---|
| Legacy baselines | Modes 0 and 14 remain selectable under the common new harness. This preserves a pattern baseline, not a byte-identical historical executable. |
| Native images and verification | Built and strongly supported by golden tests, corruption probes and assembly. Keep the two-stage design. |
| Validation and halt | Most intended fixes are present; finalization after a pre-step halt needs F1. |
| Epoch and polarity | Built for native SimpleTests and the seal. Complete-pair guarantee needs F3. Epoch/identity conversion of the other named tests remains deferred. |
| Family catalogue | Present. `lines` avoids the old chain collapse; `linepair` has the limited identity domain in F2. `switch` remains a nominal-width model. |
| TM5 conversion | Present, including native Auto dispatch and explicit numbered-mode override. Including `random` is the right amendment. |
| Seal | Identity repair and paired-cycle polarity are present. Error-only run reporting still affects seal failures. |
| DRAM reach | Not generally proven by this change. The documented decision to retain optional Refresh flushing is a plan amendment; do not silently call it a completed unconditional-flush change. |
| SimpleNT and other tests' identity data | Deferred, as Opus reports. "One image everywhere" is not yet an application-wide claim. |
| New access sequences and Mem-Random full sweep | Not implemented. The full sweep remains a detection-coverage item, not a pattern cosmetic. |
| Quiet-victim/header-tail experiments | Not implemented. |
| Hardware ranking and future profiles | Not resolved by the current runs. See the separate transfer-profile research. |

**What the hardware observations establish**

Native `switch` and legacy mode 14 have the same single-line encoder when the pre-hash input
and width match. A direct production-function comparison passed 1,536 matched-input cases.
Their actual streams are different: legacy advances the hash input by byte address
(64 per line), native advances by line ID (1 per line), keys differ, and native writes P/~P.
A single key adjustment cannot make those whole streams identical.

The ABBA comparison also gave legacy mode 14 about ten minutes per block and native switching
about five minutes within a mixed program. Different code, phase history, exposure and
allocation are confounders. Native's zero switch hits do not establish a broken verifier;
the injected-corruption checks found no such loss. They also do not establish equivalent
electrical stress. Keep legacy 14 available.

The sequence 38, 24, 0, 0 is consistent with drift. It does **not** prove "time rather than
pattern" as the cause. Different allocations make comparisons noisier, not mathematically
impossible. Repeated randomized comparisons can still estimate performance; an interleaved
same-allocation comparison reduces one source of variation.

Five verify detections of one word are repeated observations, not five independent fault
events. A word wrong after four rereads and a flush is stronger evidence of persistent
observed corruption, but is not proof of a weak DRAM cell, a stuck-at fault, or a soft error.
[CLFLUSHOPT writes back dirty data before invalidation](https://www.felixcloutier.com/x86/clflushopt).
A corrupted cached copy can therefore be written out by the diagnostic flush itself.
The code's own `Rescan::reread` comment correctly states this limit; the hardware narrative
should match it.

Resolve cheap questions first: exact input-image comparisons, schedule/width/provenance,
correct timing, and family-attributed failure logs. Only then use a short matched-exposure
comparison when the failing host is available and actually producing errors. Report elapsed
family exposure, first-alarm time and distinct observed addresses alongside repeat counts.
No physical-address instrumentation is required for this work.

**Validation performed**

- `cargo test --all-targets --offline --target-dir target/pattern-review-baseline`:
  **169 passed**, binary target has no unit tests. Fresh, separate artifact directory.
- `cargo clippy --all-targets`: no Clippy diagnostics. The environment printed its existing
  home-path canonicalization warning.
- `cargo rustc --release --bin tmr -- --emit asm`: succeeded. Inspected post-LTO native
  write/verify loops, seal stores, rescan rereads and the flush helper.
- Twelve additional isolated review probes passed, including six defect reproductions and
  the 24,948 corruption checks. A passing reproduction confirms the reported defect;
  it is not a claim that the defect is fixed.
- Read the actual XMM/YMM/ZMM store and reduction instructions. The 128-bit native writer
  contains no YMM/ZMM operations. Inspected 512-bit verifies retain four independent
  accumulators and repeated passes. Rescan rereads remain separate loads; the flush helper
  emits `clflushopt` followed by `mfence`; seal fill emits non-temporal stores and `sfence`.
  No `memset`/`memcpy` replacement was found in the inspected functions. This is inspection
  of this build, not a permanent guarantee about future compiler versions.

**Recommended next change**

Fix F1, F3 and F4 first, then correct the benchmark accounting and claims. F2 is optional
formula work unless full-domain identity becomes a requirement for `linepair`.
Keep the native framework,
OR accumulators, seal construction and legacy 14. For ordinary general-purpose SimpleTests,
`[random, switch]` is a sensible candidate default because `random` also provides identity
and adds richer signal histories; measure the actual cost before promising a percentage.
Retain `ident` as the cheap identity/baseline family, and `linepair`/`lines` for their specific
intent. Complete the deferred coverage work separately. Future-width switching should use
the profile compiler described in
[transfer_profile_research_2026-10-11.md](transfer_profile_research_2026-10-11.md).
