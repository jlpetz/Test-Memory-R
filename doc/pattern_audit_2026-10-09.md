# TMR test, pattern, and seal audit

2026-10-09 · audited commit `10068d2747ed975c1ee90a13d2e48ec566e4cea6`

Follow-up: [fast verification, signal histories, and cache exposure](pattern_followup_2026-10-10.md)
clarifies intentional grouped error counts and adds measured history/transform results.

## Recommendation

Keep a small **portfolio of patterns and access sequences**, with one shared definition of each
pattern across scalar, SIMD, benchmarks, and verification. Fix the discrepancies in the current
implementations before judging new generators by their error counts.

The best immediate combination is:

1. A fast, position-based, bijective word pattern for detecting misplaced data.
2. Explicit `P` / `~P` phases, with a real suite epoch, for complementary logical bit coverage.
3. Mode 14 as a dedicated switching pattern at the known data beat width.
4. The existing seal mechanism, strengthened with an epoch and a precise uniqueness contract.
5. Moving inversions and a protected-victim/modulo test, because changing bytes alone cannot
   replace missing access sequences.

I would **not** adopt the previous four proposed changes as one undifferentiated patch. Replacing
sequential mode 2 with native mode 12 is sensible. The universal-triple and blanket 2× speed claims
need correction. Changing mode 0's mathematical pattern is not a prerequisite for speeding it up.

I also developed two experimental compromises:

- **Balanced near-flip:** exactly 30/32 or 62/64 logical data bits change at each adjacent beat,
  while a complete line encodes a unique 64-bit identifier. It survives the modeled DDR4 DBI rule.
  Its first implementation is too expensive to recommend as the default.
- **Balanced identity header plus alternating tail:** about 90% switching at 32 bits, or 92.9% at
  64 bits, with a provably unique complete line. It is much cheaper than the first candidate.
  It deserves a controlled experiment, not immediate replacement of the hardware-tested mode 14.

These are logical-data properties. They are not measurements of physical DQ pins or proof that a
pattern finds more overclocking faults.

## Scope and evidence

Production source was not changed. The pre-existing, uncommitted
[pattern_research.md](pattern_research.md) was preserved.

I inspected the generators, all correctness-test families, the harness, worker seal transitions,
configuration validation, relevant TM5 assembly, and the prior simulator and benchmark. I respected
the existing architecture: virtual addresses, stitched allocation, pinned workers, and per-width
macros. No physical-address mapping or driver is needed for the recommendations here.

Verification:

| Evidence | Result |
|---|---|
| Existing `cargo test --lib` | 142 passed |
| Existing `cargo clippy --all-targets` | No Clippy warnings |
| Release build and assembly emission | Completed; selected hot loops inspected |
| Added bounded reproductions against a source copy | 13 passed, demonstrating the described current behavior |
| Clippy on the source copy including audit modules | All targets passed with `-D warnings` |
| Independent Python simulation | 1 MiB per pattern; finite-width counterexamples; seal collision; candidate decoding |
| Rust/Python candidate comparison | 24 complete lines agreed |
| Local CPU-cost experiment | One thread, 256 KiB and 32 MiB buffers, five rounds |

The added tests deliberately assert the current defects. Their passing does **not** mean those
defects were fixed. They are reproductions to convert into regression tests during implementation.
The reproduction buffers are ordinary aligned heap allocations, with guards. I did not run
`tmr.exe`, the real memory allocator, remote machines, or a prolonged stress test.

Artifacts and reproduction commands are in
[research/pattern_audit/README.md](../research/pattern_audit/README.md).

## 1. Findings in the current implementation

### 1.1 High priority: selecting SIMD can change the pattern

In [tests.rs](../src/tests.rs), `simple_test_v2_impl!` around lines 3694–3890 implements modes 12
and 14 specially. Most other modes use `index XOR constant`.

| Requested mode | Scalar sequential SimpleV2 | SimpleV2-128 / -256 / -512 / Auto |
|---|---|---|
| 0 | Multiply-based words, page inversion | Counter, equivalent to mode 10 |
| 1 | Subtract-by-three words, line inversion | Counter, equivalent to mode 11 |
| 2 | Chained page/line generator | Counter, equivalent to mode 10 |
| 10 | Counter | Matching counter |
| 11 | Counter with combined parameter | Matching counter |
| 12 | Address-hashed seed and odd step per line | Matching mode 12 |
| 13 | SplitMix finalizer per word | Counter, equivalent to mode 10 |
| 14 | Width-dependent bus-flip lines | Calls the scalar implementation |

The probe compared 4,096 words at the **same address and settings**. All words differed between
scalar and every available SIMD variant for modes 0, 1, 2, and 13. Modes 10, 11, 12, and 14 matched.

This is already partly acknowledged in TODO 3, but it also affects mode 13. It is a coverage
problem: a user selecting a width can silently lose the intended data pattern. An Auto run can
therefore test a different pattern from a plain SimpleV2 run.

The community `.cfg` importer maps `SimpleTest` to plain `Mem-SimpleV2`
([config.rs](../src/config.rs), around line 1485), so do not generalize this finding into a claim
that every imported community test currently runs a counter.

**Fix:** generate exactly the same image at every width. Until an implementation exists, use a
correct fallback and identify the effective kernel. Mode 14 already follows the correctness part
of that policy; its width labels are not evidence of a 512-bit mode-14 store loop.

### 1.2 High priority: “new every cycle” means a local cycle, not the suite cycle

[test_scaffolding.rs](../src/test_scaffolding.rs), `TestRunner::new`, initializes `cycle` to zero.
Each invocation starts counting again. The outer suite cycle in
[runner.rs](../src/runner.rs), `execute_test_cycle`, is passed into log context, not the pattern
configuration. All four write/read rounds of a SimpleTest chunk also receive the same `ChunkCtx`.

For modes 0, 1, 2, 12, and 14, the probe established:

- Two separate one-cycle invocations at the same address produce identical data.
- A second **internal** cycle changes the data.
- Repeating a one-cycle imported step in a later suite cycle does not supply a new pattern epoch.

Thus, a source function accepting `cycle` is insufficient evidence that suite repetitions expose
new backgrounds. Duration-based tests can vary their local cycles; the limitation is not that
these modes are always static.

**Fix:** pass a small immutable pattern context containing a run seed, suite epoch, step identity,
local iteration, and phase. Derive a key outside the hot loop. A key change gives variety;
`P` followed by `~P` gives a deterministic guarantee that each tested **logical bit** is requested
in both polarities. Random key changes alone give no finite-run guarantee of that property.

Use one common epoch key when proving global address uniqueness. Independently choosing a key for
each worker can invalidate a proof that assumes one permutation over the entire address space.

### 1.3 High priority: the SIMD refresh rationale confuses extent with chunk

[tests.rs](../src/tests.rs), `refresh_impl!` around lines 1541–1630, performs:

```text
for each chunk:
    fill this chunk
    optionally flush this chunk
    sleep 64 ms
    verify this chunk
```

Its no-flush justification says that an extent larger than cache will evict its own data. But the
test has not written that whole extent before checking this chunk. A small chunk can remain cached
through the sleep even when the overall extent is enormous.

The scalar refresh implementation always flushes before waiting; the SIMD implementation makes
that conditional on `flush_before_verify`. Changing width can therefore change the DRAM-retention
guarantee. This is a source-level finding; I did not pretend to inject a physical DRAM-only fault
under a cached line.

**Fix:** a test advertised as refresh/retention should commit the tested data before waiting and
read it from memory afterward. Keep a cache-resident wait/read experiment separately labeled if it
is useful. Use explicit writeback/invalidation or a carefully verified NT-fill path. An NT store
alone does not justify assuming every later repeated verify bypasses cache.

Sleeping does not disable the controller's refresh. This tests retention under the current refresh
policy, not an unrefreshed 64 ms decay experiment.

### 1.4 High priority: JSON permits correctness passes with no verification

The CLI repetition knobs require 1–100, but `apply_repetition` in
[config.rs](../src/config.rs), around line 267, copies JSON values without equivalent validation.
The runner validates native stride/RNG/subdivision parameters, but not these three repetition
fields.

Accepted `Mem-SimpleV2` JSON cases:

| Setting | Observed or direct loop consequence |
|---|---|
| `verify_reps: 0` | Writes run without verifying their test data |
| `write_read_cycles: 0` | The intended write/verify rounds do not execute |
| `test_reps: 0`, **seal off**, mode 0 | Init uses local epoch 0; verify expects epoch 1; false errors on healthy memory |

The 32 KiB JSON reproduction of the last case reported **81,920 errors**: 4,096 mismatches
repeated over the default 20 verifies. Direct one-verify probes reproduced the epoch mismatch in
modes 0, 1, 2, 12, and 14.

The seal-on path initializes per chunk and can avoid that particular false-error case; that does
not make zero-write configuration valid or restore omitted test-data verifies.

**Fix:** validate applicable correctness settings before allocation. Preserve intentional
write-only/read-only benchmark semantics instead of banning every internal zero indiscriminately.

### 1.5 Mode 2 deteriorates before complete collapse

TODO 95 is real. The recurrence in [pattern_gen.rs](../src/pattern_gen.rs), `mode2_evolve`, is:

```text
s_next = s * param0 + param1    modulo 2^64
t_next = t * s_next            modulo 2^64
line[j] = s + j*t
```

Let `v2(t)` be the number of trailing zero bits in `t`, capped at 64. Then:

```text
v2(t_next) = min(64, v2(t) + v2(s_next)).
```

Trailing zeros accumulate and cannot recover. If `v2(t) = k`, the low `k` bits are identical in
all eight words of that line. When it reaches 64, the whole line collapses.

For 16,384 lines at the documented synthetic base, using community parameters
`0x77777777, 0x33333333`:

- **1,989 lines, 12.14%,** contain eight identical words.
- **5,377 lines, 32.82%,** already share at least their low 32 bits.

The second result matters more than describing the defect as “a few collapsed lines.”
TM5's packed `PMULLW`/`PADDD` operations are not equivalent to this scalar u64 recurrence.

**Fix:** for a modern native policy, use mode 12 consistently for sequential and strided mode 2.
This also removes the present access-order-dependent semantic difference. If byte-faithful TM5
behavior is retained as a separate promise, implement and verify the actual packed arithmetic.
Simply forcing the current step odd changes the algorithm and is not a faithful repair.

### 1.6 “Own address” is currently only an offset within one worker

Modes 10, 11, and 13 use a relative index and configured constants. They do not incorporate the
actual address or worker identity in these paths.

Two different heap regions, invoked with different thread IDs, produced the same 4,096 words in
each of these modes. A transfer to the same offset in another worker's span can consequently be
invisible.

**Fix:** use a full virtual word address, or a globally unique span/offset identifier. This needs
no physical addresses. The low-cost permutation in section 6 provides a direct proof.

There is also a smaller parameter mismatch: `Bench-Init` and `Bench-Verify` mode 10 hard-code
`0xDEADBEEFDEADBEEF`, while SimpleV2 uses `param0`. The probe reproduced all 4,096 words differing
when a custom parameter was supplied. A shared pattern specification should eliminate this
duplicate implementation.

### 1.7 Error counts and checking intervals have different units

`simple_verify_positional_simd!` around [tests.rs](../src/tests.rs):3515 adds **one per bad batch**.
The scalar path and strided SIMD cold rescans count bad words.

Reproduction:

- Three corrupt words: scalar reports **3**, sequential SIMD reports **1**.
- Two corrupt words with `check_mask = 7`: 128-bit reports **2**, 256-bit reports **1**.

The latter happens because the SIMD interval counts vectors, so its byte range grows with width.
Some scalar paths check a whole chunk instead of applying that mask.

This is an intentional performance tradeoff, not weaker first-pass mismatch detection. XORing
each actual/expected pair and OR-accumulating all differences cannot cancel a mismatch. Raw error
*counts* across kernels nevertheless describe different observations.

**Clarified recommendation (2026-10-10):** preserve grouped accumulator detection. Complete useful
cold rescans under TODO 27 and document the counting units. Distinguish failing batches from bad
words seen again; a rescan does not recover the exact original fault count. Preserve the existing
“at least one error” rule if a mismatch disappears. Do not change batch sizes or add per-word
checks just to equalize counts. When refactoring latency-sensitive reductions, retain four
independent accumulators rather than introducing a serial dependency. See the
[follow-up](pattern_followup_2026-10-10.md) for the reread and evidence-preservation contract.

### 1.8 Two narrower harness issues

**Halt ordering:** a corrupt pre-test seal is counted, but `ErrorMode::Halt` is checked after the
chunk body and reseal. The reproduction confirms the write body executes and overwrites the
evidence first. Check the halt condition at the existing chunk boundary immediately after the
seal failure; no per-word branch is necessary.

**Dependent-plan validation:** the validator's `PatternId` omits the actual seed epoch and written
extent. It accepts a SimpleV2 local-epoch-1 writer followed by an epoch-0 benchmark verifier, and a
16 KiB writer followed by a 32 KiB verifier. Those probes reported 4,096 and 2,048 false errors.
However, arbitrary `skip_init` reader chains are currently an internal API/deferred feature, not
a normal exposed JSON setting. Treat this as a latent TODO 13 limitation, not a reason to reopen
the deferred runtime framework immediately. Reject unsupported chains until their state contract
is complete.

## 2. The seal: keep the mechanism, strengthen its contract

### What is already good

The seal is valuable because it checks memory after dwell under load and catches writes outside
the currently active chunk. Its position-based layout supports overlapping chunks. The NT fill,
fence, SIMD comparisons, and cold error rescan are appropriate.

I inspected the compiled 512-bit fill: its hot loop contains `vmovntdq` ZMM stores and ends with
`sfence`. The corresponding check uses four independent accumulators and real loads. The 128/256
dispatch paths also contain NT stores. These are not just source-level intentions.

The seal does **not** need to maximize adjacent-beat switching. Its main job is identity and
retention. A complementary pair beside a word is a useful background, but it does not make that
same word experience both polarities over time.

### Two actual weaknesses

First, [seal.rs](../src/seal.rs):213 has no epoch:

```text
q = address >> 4
x = q * G
v = x XOR (x >> 32)
word = v or ~v according to the half of the 16-byte unit
```

Repeated fills put back exactly the same bytes. A stale seal from a previous cycle at the same
address passes. The seal itself also never changes the logical polarity at that address.

Second, the uniqueness guarantee is true for **whole aligned 16-byte pairs**, not for every u64.
The odd multiply and XOR shift are bijections, but an even-half value can equal an odd-half
complement elsewhere.

The independent mathematical construction and actual `tmr_word` function agree on:

```text
tmr_word(0x6B394CE37B10) = 0x405AC1F540548C70
tmr_word(0x6B494CE37B18) = 0x405AC1F540548C70
```

These are synthetic valid low-canonical address numbers, separated by 64 GiB + 8 bytes.
No memory at those addresses was allocated or touched.

This does not mean seal collisions are common, or that whole-line misdirection normally passes.
It specifically disproves the stronger claim that **any misplaced 8-byte write always shows**.
Temporal repetition is the broader practical gap.

### A simple stronger paired seal

Reserve one bit to distinguish the two halves:

```text
q = address >> 4
x = (q * G + epoch_key) AND (2^63 - 1)
v = x XOR (x >> 32)
word = v XOR (odd_half ? ALL_ONES : 0) XOR polarity_mask
```

Proof, for the supported address domain:

1. Odd multiplication and addition permute the 63-bit `q` domain.
2. `x XOR (x >> 32)` is invertible and preserves the cleared top bit.
3. Base values therefore have top bit zero; their complements have top bit one.
4. Both halves are internally injective and cannot collide with each other.
5. XOR with a common phase mask preserves uniqueness.

Thus **every u64 address has a different value within an epoch**, while each adjacent pair remains
complementary. Mask **before** mixing; masking the final hash would need a different proof.
All practical user virtual addresses fit comfortably in this domain.

Use a key for two successive polarity phases, then change it for the next pair. Keep one expected
seal epoch throughout a suite cycle. Check old data before switching epochs; do not reseal it
with a new key before its final check. Mirror verification and transitions from unsealed steps
must use the same expected epoch.

The added operation is a mask, but an integrated speed result still needs measurement. My generic
probe measured a modest slowdown; it is not a replacement for the existing optimized seal kernel.
Using an injective full-address word pattern instead is also sound if paired backgrounds are not
required.

### Coverage limits to state accurately

- The harness wraps chunks only during a step's first internal cycle. Later local cycles do
  their own work and the extent is resealed at the end. It is not a seal check around every
  repetition.
- Unsealed tests own their data. `seal_before_step` can overwrite that data when restoring the
  seal; `FinalCheck` skips the known unsealed prefix. A seal cannot retroactively verify a
  bandwidth buffer or the unvisited part of a random-read test.
- Dwell varies by address and test schedule. Repeatedly rewriting a seal creates short dwell at
  that location, even if the overall run is long.
- Retention can depend on background data. “Any pattern works for retention” is too strong.
  The DDR3 study in source S6 establishes the mechanism; it does not quantify DDR5 behavior.

## 3. Other test coverage gaps

The data generator and the access sequence are separate dimensions. A more sophisticated pattern
does not automatically make a cache-local, sparse, or write-first test thorough.

| Test family | Useful present behavior | Gap or limitation | Recommended direction |
|---|---|---|---|
| StuckBit | Pattern, inverse, pattern; deterministic two-polarity requests | Fixed backgrounds; identical data can mask address aliases; cached variants can stay in cache | Retain; combine with identity patterns and an explicitly DRAM-reaching variant |
| SimpleV2 | Full chunk fill/read, sequential or line-strided; repeated checks | SIMD substitutions; local epochs; repeated small chunks can be cache-local | Repair semantics; distinguish cache stress from DRAM sweeps; add explicit pattern phases |
| SimpleNT | Real NT stores, cheap positional data | A particular fixed counter family; not a general consumer of every `pattern_mode`; reads can subsequently cache | Useful bandwidth/stress complement, with an explicit pattern contract |
| Refresh | Fill, wait, verify | SIMD flush optional; fixed background; small chunk can mask DRAM data | Commit before dwell, complementary epochs, useful dwell choices |
| CacheBust | Strided line traffic and verification | Default 1 MiB chunks can fit cache; virtual stride does not prove a DRAM row change | Keep for cache/TLB pressure; do not describe every access as a new physical row |
| Stride | Includes a contiguous pass and larger strides | Sparse passes revisit fixed residues; faults elsewhere may be overwritten before another check | Rotate residues or verify the full background before overwriting it |
| Random | Irregular read checks over a large extent | Read-only, relative-index data, bounded samples, no concluding full sweep | Add verified random writes or a permutation walk, plus a deterministic final sweep |
| BlockMove | Copies source to destination and verifies destination | A late source-only corruption can escape; no inverse background; copies are not cross-worker coherence traffic | Verify source at a useful boundary; vary data phases; keep cross-worker work as a separate future test |
| Mirror | Real swap loads/stores, seal verification afterward | A round trip is an involution: dropping both corresponding operations can leave the final image correct | Preserve compatibility; consider a distinct native test checking the intermediate permutation |
| Bandwidth / latency | Useful load, traffic, timing and WHEA opportunity | Generally not full expected-data verification | Keep their role explicit; do not count their bytes as correctness coverage |

In [tests.rs](../src/tests.rs), relevant entries start around lines 1281, 1541, 1913, 2081, 2201,
2316, 2912, 3334, and 4082. Compiled `mirror_move_v2_256_multi` contains the expected 256-bit
load/store swaps; the round-trip criticism is about fault coverage, not a claim LLVM deleted it.

For **Mem-Random**, the default eight sequences execute at most 50,000 reads per local iteration.
An independent replay over a 16 GiB extent reached 49,999 distinct words:

- **0.00233% of words** per iteration.
- **0.0186% of lines** per iteration.

A duration run performs many iterations, so these are not the coverage percentages for an entire
25-second run. Nevertheless, “random” does not imply full coverage, and resetting the local epoch
reuses the initial sequences on later invocations. The initializer itself writes all words but
does not verify all of them.

The two most useful additional sequences are already compatible with TODO 19:

1. **Moving inversions:** fill `P`; ascending read-expected/write-`~P`; descending
   read-expected/write-`P`; complete the required final verification. Preserve ordering and
   choose chunk/cache policy deliberately.
2. **Modulo/protected victims:** leave one residue class holding its expected data while repeatedly
   writing the other locations; check the victims before overwriting them; rotate the residue and
   polarity.

These keep selected data vulnerable while other writes occur. They catch situations a “write the
whole chunk, then read it” schedule can erase. They do not require claiming virtual neighbors are
physical neighboring cells. Source S7 describes why these sequences complement each other.

## 4. What the earlier research gets right, and what needs correction

### DBI in plain language

DBI lets an interface send a byte upside down and signal “invert this back” on another pin. The
application receives its original data; the physical data-bit transitions can change.

For the DDR4 rule modeled here, a byte with more than four zeros is inverted. Four-one/four-zero
bytes survive unchanged. DDR4 x8/x16 support and read/write enablement are conditional; x4 is
different. Micron's 2020 guide also reports that popular controllers typically did not use these
features. This is historical evidence, not a census of current machines. [S1]

Conventional DDR5 must not simply inherit that simulation: Micron lists **CAI
(command/address inversion)** for DDR5 versus DDR4's **DBI**. These act on different signals. [S2]

Therefore: retain DBI-on and DBI-off modeled results where applicable, but do not discard paired
patterns for DDR5 because a hypothetical DDR4 data transformation degrades them.

### Flips are one useful measurement, not “more flips = more faults”

Three different changes matter:

| Change | Example | Purpose |
|---|---|---|
| Within a burst | Consecutive beats carry opposite data | DQ switching/history |
| At the same address over time | Write `P`, later `~P` | Logical transitions and complementary backgrounds |
| Across locations | Two addresses carry different identifiers | Detect misplaced data |

A high score in the first column does not imply either of the other two.

Electrical sensitivity also involves simultaneous switching, rising/falling balance, quiet victims
beside aggressors, burst/idle transitions, read/write turnarounds, and pattern history. Intel
explicitly discusses pattern energy exciting resonances and scrambling to mitigate that. [S3]
AMD's DDR4 IP documentation gives an example involving all-zero read bursts separated by idle
time. [S4] A periodic maximum-toggle pattern is useful, but not an exhaustive worst case.

CPU GiB/s is also not automatically DRAM GiB/s: cached verifies and write allocation alter the
relationship. Optimize **verified exposure per second and time to first error**, with workload
coverage, rather than maximizing one simulated toggle score.

### Scrambling and burst order need conditional claims

For an XOR scrambler:

```text
transmitted[i] XOR transmitted[j]
  = data[i] XOR data[j] XOR mask[i] XOR mask[j].
```

A common mask cancels. A beat-varying mask can change the switching. Encryption can transform it
differently again. Neither “the simulator shows 100% on the real wires” nor “scrambling always
makes every pattern random” follows without the actual mapping.

Fixed bit/byte wiring permutations that preserve byte groups do preserve Hamming distance and
balanced-byte weight. Intel documents such allowed data swaps. [S5] Beat-order permutations are
a separate issue. Reversal preserves the number of adjacent transitions; arbitrary reordering
does not. A `A, ~A` pattern retains full switching only when the actual order alternates its two
parities.

The simulations assume increasing logical byte order, conventional payload widths, and no
scrambling. They exclude ECC/check bits, DBI pins, DQS, and command/address signals. SMBIOS alone
does not establish all these properties.

### The two-thirds theorem needs its boundary condition

For three bits `a,b,c`:

```text
[a != b] + [b != c] + [a != c] <= 2.
```

For a stationary/infinite or circular sequence, summing gives:

```text
2*p(W) + p(2W) <= 2,
```

so both average rates cannot exceed 2/3. This is a useful result.

But the earlier simulator counts only edges **inside a finite 64-byte line**. For 16 32-bit
beats, let `S1` count adjacent changes on one bit lane and `S2` changes two beats apart. Then:

```text
2*S1 + S2 <= 2*(16-2) + first_edge + last_edge <= 30.
min(p32,p64) <= 30/(2*15 + 14) = 15/22, approximately 68.18%.
```

It is possible to exceed 2/3 at both finite metrics. The script constructs a line whose bytes
all remain balanced and whose scores are:

```text
p32 = 41/60 = 68.3333%
p64 = 19/28 = 67.8571%
```

This is a counterexample to the unconditional statement, not a proposal to chase a tiny endpoint
advantage in the product. It also does not establish that 15/22 is the attainable optimum.
More broadly, the bound does not imply there are “only three possible designs.”

### The triple is useful at selected widths, not universally safe

The repeating 16-bit blocks `A, ~A, C` have a 48-bit period.

- At a 48-bit lag, they produce **zero** transitions.
- At a 24-bit lag, valid balanced payloads range from **0% to 100%**, although the mean over all
  65,536 payloads is 50%.
- Concrete zero case: `A = 0xF00F`, `C = 0xF0F0`. Its bytes repeat `0F F0 F0`, so moving three
  bytes forward changes nothing.
- At a 12-bit lag, the exhaustive balanced-payload mean is **41.2%**, with a 32.8%–49.6% range.

The 12-bit calculation is a mathematical counterexample to width universality, **not** a physical
LPDDR6 prediction. Its real payload packing and link features must be modeled first.

Moreover, independently hashing each line and then inverting odd lines does not make neighboring
lines complements. Inversion preserves the average distance between independent random values.
Share a base within a pair if that pair must be complementary. Requiring **every** neighboring
line to be the exact inverse forces a two-line repeating pattern, which necessarily sacrifices
unique line identity.

### Channel count and future memory

For conventional PC DDR4, a payload burst uses a 64-bit channel; for DDR5, a 32-bit subchannel.
Adding another independent channel does not turn the individual DDR5 burst into 64-bit beats.
Channel count still changes concurrency, bandwidth, and electrical loading, and TMR's imported
stride calculation separately uses the channel setting.

Universality should mean **one pattern family and one verification contract**, with a selected
transfer profile. It need not mean one immutable byte sequence maximizes switching on all buses.

The earlier claim that LPDDR6's layout is unpublished is out of date. JEDEC published JESD209-6
in July 2025; its announcement specifies two 12-DQ subchannels per die and 32/64-byte accesses.
Desktop DDR6 is a separate standard. [S8] Do not infer a complete desktop DDR6 profile, or an
LPDDR6 data mapping, from a guessed width.

For unknown hardware, use the general identity/varied-data pattern as the safe default. Optionally
run named 16/32/64-bit switching profiles across phases and report their assumptions. Do not
silently promise “universal worst-case switching” or assume 32 bits is known.

## 5. Measured logical pattern table

Each sample contains **16,384 lines / 131,072 u64 words = 1 MiB** at base `0x1E980000000`.
`Flips` is the mean number of changed payload bits between adjacent beats inside a line:
`p(W) * W`. No cross-line or idle transitions are counted.

These are the scalar generator images. The SIMD substitutions in section 1 still apply.
Unless identified otherwise: thread 0, local cycle 1, parameters `0x5DEECE66D, 0xB`.
Modes 10/11 below use their SimpleTest defaults to match the earlier 8.6/32 comparison.

| Pattern | Flips /64 | Flips /32 | Distinct u64 / 131,072 | Distinct lines / 16,384 |
|---|---:|---:|---:|---:|
| TM5 mode 0, one fixed block, 64-bit-bus reference | 49.14 | 15.73 | 6 | 1 |
| TMR 0 | 31.55 | 16.00 | 131,072 | 16,384 |
| TMR 1 | 3.14 | 13.19 | 131,072 | 16,384 |
| TMR 2, community sequential parameters | 18.43 | 16.00 | 116,193 | 16,384 |
| TMR 10, default parameters | 1.57 | 8.60 | 131,072 | 16,384 |
| TMR 11, default parameters | 1.57 | 8.60 | 131,072 | 16,384 |
| TMR 12 | 32.50 | 16.01 | 131,072 | 16,384 |
| TMR 13 | 31.99 | 16.01 | 131,072 | 16,384 |
| TMR 14 / 32-bit profile | 0.00 | 32.00 | 14,539 | 14,539 |
| TMR 14 / 64-bit profile | 64.00 | 16.00 | 32,768 | 16,384 |
| TM5 seal, 64-bit-bus reference | 48.64 | 15.79 | 1,532 | 768 |
| Current TMR seal | 50.44 | 16.02 | 131,072 | 16,384 |
| Cheap full-address permutation | 31.41 | 16.02 | 131,072 | 16,384 |
| Proposed tagged paired seal, one epoch | 50.44 | 16.02 | 131,072 | 16,384 |
| Balanced 16-bit triple | 43.43 | 21.33 | 43,458 | 14,495 |
| Balanced near-flip / 32 | 6.43 | 30.00 | 106,665 | 16,384 |
| Balanced near-flip / 64 | 62.00 | 16.00 | 127,930 | 16,384 |
| Identity header + flip tail / 32 | 6.87 | 28.79 | 47,355 | 16,384 |
| Identity header + flip tail / 64 | 59.43 | 15.99 | 49,152 | 16,384 |

Important qualifications:

- 3.14/64 is **4.9%**, not 3.14%. Always distinguish a bit count from a percentage.
- Some scores depend strongly on the seed and parameters. For example, mode 10's /32 score is
  19.60 rather than 8.60 with the benchmark parameters used elsewhere in the simulation.
- A sampled distinct count is not a proof over all addresses. The seal counterexample demonstrates
  this directly.
- Mode 12's odd step proves eight different words **within a line**. Its injective first seed
  proves distinct whole lines for one common key. It does not by itself prove all words across
  all lines are different.
- At 32 bits, mode 14 repeats the same u64 eight times. Whole-line collisions and within-line
  word permutations are different blind spots.
- Current mode 14 uses only 16 of the 70 possible four-one bytes. Its 32-bit profile therefore
  encodes just 16 independent payload bits per line: at most 65,536 distinct lines. This is a
  coding choice, not the entire capacity of balanced bytes.
- The previous “512 bits of variety per random line” confuses output size with independently
  selected information. A deterministic line derived from one address and a fixed key does not
  contain 512 independently chosen random input bits.

For the modeled DDR4 DBI rule, the /64 **transition fractions** are:

| Pattern | Raw | After modeled DBI |
|---|---:|---:|
| TMR 0 | 49.3% | 46.5% |
| Current seal pairs | 78.8% | 35.2% |
| TM5 seal reference | 76.0% | 32.4% |
| Balanced triple | 67.9% | 67.9% |
| Mode 14 / 64 | 100% | 100% |
| Balanced near-flip / 64 | 96.875% | 96.875% |
| Identity header + flip tail / 64 | 92.9% | 92.9% |

This confirms that the transformation matters **when enabled**. It does not tell us it is enabled
on a particular PC, and it is not a DDR5 data-bus table.

## 6. Candidate algorithms and what each is for

### Fast full-address word permutation

```text
q = byte_address >> 3
x = q*G + K                  modulo 2^64, G odd
word = x XOR (x >> 32)
```

Odd multiplication, addition, and the XOR shift are all invertible. For a fixed common key,
different aligned word addresses therefore have different values, including across workers.
Verification regenerates the full expected word and compares it; it does not merely check a
checksum or whether two words happen to complement each other.

For a contiguous vector loop, initialize the affine state once and advance by vector additions.
The source definition remains position-pure: strided traversal recomputes the line's start state
from its address. “Position-pure” does not forbid a local recurrence that is exactly equivalent
to the direct formula.

This is suitable for identity checks, full-memory verifies, and one general SimpleTest phase.
It is **not** statistically equivalent to SplitMix. Its correlations and avalanche behavior are
weaker. Retain a stronger decorrelated pattern for separate coverage if its cost is justified.
The [follow-up](pattern_followup_2026-10-10.md) proves a concrete limit: at a 64-bit logical beat,
each upper bit of this cheap permutation has at most ten five-bit histories, out of 32 possible,
even when the starting address and additive key vary.
Consider native 32-bit-vector generators or AES-based counter blocks only as later measured
alternatives; they have different identity granularity and ISA requirements.

### Balanced near-flip with a recoverable identifier

This repairs the naive “hold one physical bit” idea, which can break byte balance and trigger DBI.

For width `W` in 16/32/64:

1. A payload of `k = W/2` bits expands into bytes `[nibble, inverse-nibble]`.
2. Encode the next beat by complementing the payload **except one payload bit**.
3. That held payload bit corresponds to **two held physical bits** in the same byte. Every byte
   stays four-one/four-zero. Exactly `W-2` data bits change at each transition.
4. Initial payload and the index of each held bit encode a 64-bit line identifier.

Information capacity:

| W | Beats in 64 B | Initial bits | Choice bits per transition | Total capacity |
|---:|---:|---:|---:|---:|
| 16 | 32 | 8 | 3 | 101 bits |
| 32 | 16 | 16 | 4 | 76 bits |
| 64 | 8 | 32 | 5 | 67 bits |

All can carry a complete 64-bit identifier. Use a bijective hash of the full line address plus a
common epoch key. The supplied decoder reconstructs the identifier, which proves complete-line
injectivity rather than merely demonstrating no collisions in a sample.

The proof assumes the modeled ordering and an intact complete line. It does not guarantee every
word is unique, or that partial misdirected transfers always differ. The current encoder also has
a dependency chain and bit-manipulation overhead; the CPU measurements below are a reason to keep
it experimental.

Unused identifier capacity is padded deterministically in this prototype. Held-bit choices
therefore are not uniformly distributed across every beat/pin. A later optimized design should
vary those choices without weakening decodability; the transition-count proof alone does not
establish equal stress on all data lines.

### Cheaper identity header plus alternating tail

Encode the 64-bit line identifier losslessly into the first **16 bytes**, using the same
four-bit-to-balanced-byte mapping. After the header, alternate the last header beat and its
inverse through the rest of the 64-byte line.

All bytes are expected, verified data. “Header” does not mean unverified bookkeeping stored beside
the test pattern. The first 16 bytes alone recover the line identifier.

For 32-bit beats the four header beats contain independent-looking 16-bit payloads:

```text
A B C D ~D D ~D D ~D D ~D D ~D D ~D D
```

Twelve of the fifteen transitions are full flips. The other three average half flips under
well-mixed identifiers: **90% average**, **80% lower bound** for this intra-line metric.

For 64-bit beats:

```text
A B ~B B ~B B ~B B
```

Six of seven transitions are full flips: **92.857% average**, **85.714% lower bound**.

This uses one line hash, two balanced expansions, and repeated stores. It gives much better
whole-line identity than mode 14, but only a few distinct words in a line. It complements a
word-identity phase; it does not replace it. Reverse beat order or explicitly complement a whole
phase to vary exposure while preserving the relevant guarantees.

### Pattern choice by intent

| Intent | Preferred pattern/sequence | Why; limitation |
|---|---|---|
| General correctness and misplaced data | Full-address permutation plus its inverse | Cheap and provable word identity; not maximum switching |
| Maximum targeted switching | Existing mode 14, width profile chosen outside the loop | Preserves the useful hardware observation; poor identity |
| Switching plus whole-line identity | Experimental header/tail, then consider near-flip | Quantified tradeoff; benchmark and fault-test before shipping |
| Retention under load | Epoch-aware seal, complementary phases and multiple backgrounds | Dwell and identity matter more than burst toggle score |
| Neighbor interaction / overwrite coupling | Moving inversions and modulo victims | The access sequence preserves vulnerable data long enough to check |
| Signal history / crosstalk variety | Decorrelated data, solids, checkerboards, walking/aggressor backgrounds | Covers patterns not represented by one maximum-toggle sequence |
| Unknown transfer profile | General word identity/varied data; optional profile rotation | Useful without making an unsupported wire-level guarantee |

## 7. Performance: optimize the kernel before replacing the mathematics

The prior probe called a candidate through a function-pointer interface per line. That source
shape, temporary line construction, accumulator layout, and vectorization can affect results.
It is not sufficient to attribute all the cost to the hash.

Mode 0 uses an affine multiply, not a strong per-word hash. Mode 1 does not even need a 64-bit
multiply for its subtract-by-three operation. Their slow existing verifies cannot both be
explained simply by “a strong hash halves throughput.”

My probe uses separately compiled, statically inlined generators, a function dispatch only per
buffer, and four SIMD verification accumulators. It is a **CPU-cost experiment**, with temporal
stores, on an Intel Xeon 6975P-C VM, Rust `1.101.0-nightly`, LLVM 23.1.3, x86-64-v3.
Neither buffer is claimed to exceed the host's LLC; the thread was not pinned.

Median GiB/s over five rounds, with rotated/reversed candidate order:

| Generator formula in the probe | Fill, 256 KiB | Verify, 256 KiB | Fill, 32 MiB | Verify, 32 MiB |
|---|---:|---:|---:|---:|
| Counter | 49.00 | 35.86 | 27.28 | 25.24 |
| **Current mode 0 formula** | **49.43** | **45.46** | **27.31** | **27.78** |
| SplitMix per word | 11.84 | 12.12 | 11.75 | 12.23 |
| Cheap full-address permutation | 38.95 | 48.63 | 27.59 | 27.80 |
| Current seal formula | 50.88 | 23.08 | 27.30 | 22.70 |
| Tagged paired seal formula | 44.45 | 21.36 | 27.50 | 19.71 |
| Mode 14 / 32 formula | 46.53 | 24.28 | 27.47 | 23.13 |
| Balanced triple | 19.87 | 14.07 | 19.64 | 13.90 |
| Balanced near-flip / 32 | 7.80 | 5.78 | 7.87 | 5.64 |
| Balanced near-flip / 64 | 10.92 | 8.89 | 10.82 | 8.76 |
| Identity header + flip tail / 32 | 26.10 | 16.37 | 25.81 | 16.15 |
| Identity header + flip tail / 64 | 23.66 | 14.41 | 23.81 | 14.54 |

These are not TMR's integrated rates and must not be mixed into a table of full DRAM test speeds
without that distinction. Timing ranges are preserved in the raw output. The short unpinned VM
experiment supports broad implementation tradeoffs, not small percentage claims.

Assembly observations substantiate two important conclusions:

- The probe's current-mode-0 verification loop uses vector adds/XORs and **no multiply
  instruction**. LLVM strength-reduced its sequential affine computation.
- Its SplitMix verifier uses multiple `vpmuludq` operations to implement 64-bit products on AVX2.
  The cheap verifier uses one scalar multiply per line plus vector arithmetic, and could be
  further strength-reduced with an explicit affine vector state.

The new probe's fill/verify kernels contain no per-line indirect generator calls and no
`memset`/`memcpy` replacement of their data loops.

The practical conclusion is **not** that the exact median will transfer to every processor.
It is that the existing mode-0 formula can be fast, and the proposed near-flip's better toggle
score does not automatically compensate for a roughly fourfold slower first verifier.

Also, the earlier probe's own reported DRAM figures gave 8.3/6.6 ≈ **1.26×** for cheap data versus
mode 0, and 8.3/3.9 ≈ **2.13×** versus SplitMix. Neither supports “2× for mode 0 and 13 with no
other change.” The existing independent Bench-Verify rates also include initialization, as
already recorded in TODO 90.

## 8. Recommended implementation order

### First: make current results trustworthy

1. Reject invalid repetition settings; prevent verification-free correctness configurations.
2. Make scalar/SIMD/benchmark patterns agree, including custom parameters.
3. Fix refresh's chunk-level DRAM commitment and halt-before-overwrite behavior.
4. Preserve accumulator detection; complete useful TODO 27 cold rescans and clarify counting
   units without forcing per-word checks or identical batch sizes.
5. Add a real suite epoch and explicit polarity phases.
6. Replace collapsing native mode 2 with mode 12 under an explicit pattern-version policy.

The previous research's mode-specific recommendations then become:

| Mode | Recommendation |
|---|---|
| 0 | Keep useful identity/general-purpose role; implement efficient matching SIMD. Evaluate a full-address permutation as a deliberate semantics improvement, not a presumed 2× speed fix. |
| 1 | De-emphasize the current linear pattern in native plans. Use identity/complement plus dedicated switching phases for its coverage; preserve the legacy ID until a versioned change. If explicit line-pair backgrounds are wanted, derive one base per pair and invert the second line. |
| 2 | Use mode 12 for native sequential and strided behavior; separate any true TM5 compatibility promise. |
| 10 | Retain a cheap address baseline; use global virtual word identity and complementary phases for correctness work. |
| 11 | Preserve as a compatibility alias if needed; it contributes no separate algorithm from 10. |
| 12 | Keep; good position-pure line generator. Document line versus word uniqueness accurately. |
| 13 | Retain as a stronger-mixing option; repair SIMD and epoch/address scope before replacing it with a weaker mixer. |
| 14 | Keep as the dedicated switching experiment; correct unknown-profile handling and physical claims. |
| Seal | Keep wrapping/NT/verify architecture; add epoch/polarity and a proved identity contract. |

### Then: improve complementary coverage

Use a native four-phase policy such as:

```text
identity P -> verify
identity ~P -> verify
width-profile flip F -> verify
width-profile ~F -> verify
```

This can use the existing four write/read rounds without increasing the number of full data
passes. It still changes computation and exposure duration, so it is not literally “free.”
Select the generator at the phase boundary, outside the word loop.

Add moving inversions and a modulo-victim sequence before adding many near-duplicate hashes.
Extend the seal with useful long-dwell/complementary-background schedules. Keep cross-worker
coherence work separate, consistent with TODO 28.

### Finally: evaluate new hybrids

Implement the header/tail candidate behind an experimental mode before spending effort optimizing
the more complex near-flip codec. Check that its additional identity coverage actually improves
detection per unit time. Do not silently replace mode 14 and lose the existing comparison.

For maintainability, separate:

```text
PatternSpec: family + parameters + epoch + polarity + optional transfer profile
AccessPlan: sequential / strided / inversion / copy / protected-victim
Kernel: scalar / 128 / 256 / 512, all implementing the same PatternSpec
```

This is a description of the contract, not a proposal for traits, closures, virtual calls, or
request routing inside hot loops. Continue using the project's macro-based width implementations.

## 9. Validation required for the next implementation

The present unit suite passing alongside these findings shows why agreement tests must cross
implementation boundaries.

Add focused checks for:

- A fixed reference image at every width, including non-default parameters, page boundaries,
  overlapping chunks, sequential/strided traversal, and large synthetic addresses.
- Two workers and two suite invocations: identity and epoch changes actually occur.
- `P` and `~P` at the **same** locations; reseal and mirror expectations use the right epoch.
- Injected single/multiple-word corruptions, word/line swaps, stale earlier images, partial writes,
  and mismatches that disappear before a cold rescan.
- Known detection limitations: mode 14 should not be tested as if it promised unique words.
- Invalid JSON rejected before allocation; unsupported dependent chains rejected, not “repaired”
  by silently overwriting their data.
- Assembly width, real loads/stores, NT/fence/flush placement, and no accidental libc fill/copy
  substitution in width-specific kernels.

For later hardware comparisons, use the same machine at both a known-stable setting and several
controlled marginal settings. Alternate order (ABBA or randomized blocks), record temperatures
and warm-up state, keep timing/allocation/threads/cache policy equal, and report time to first
error as well as whether each run detected anything. Count repeated detections separately from
distinct bad addresses, in the existing cold reporting path.

The supplied 8600G result is good evidence to **retain mode 14**: it repeatedly detected failures
the compared mode 0 did not, at similar reported speed. It does not establish that:

- the 1,010 detections are 1,010 independent failing cells;
- bit 31 in the first word identifies one physical DQ pin;
- warming caused the falling rate, rather than another time/order effect;
- a clean stable DDR4 control proves absence of all software false positives;
- the best pattern for that fault is the best pattern for every unstable system.

The existing hardware result supports a useful complementary pattern. The additional source
reproductions establish software defects independently of unstable hardware.

## Sources

Primary vendor/standards material was preferred. The quantitative pattern results and proofs above
come from the supplied scripts, not from these sources.

- **S1 — Micron, TN-40-40, DDR4 Point-to-Point Design Guide**, Rev. H, August 2020.
  DBI details, pp. 5–6; controller-use guidance, pp. 29–30.
  <https://www.mouser.com/pdfdocs/micron_ddr4_design_guide.pdf>
- **S2 — Micron, DDR5 DRAM feature comparison**, particularly burst length, inversion,
  on-die correction, and interface features.
  <https://www.micron.com/products/memory/dram-components/ddr5-sdram>
- **S3 — Intel, 12th Generation Core processor datasheet, Data Scrambling**.
  <https://edc.intel.com/content/www/us/en/design/ipla/software-development-platforms/client/platforms/alder-lake-desktop/12th-generation-intel-core-processors-datasheet-volume-1-of-2/001/data-scrambling/>
- **S4 — AMD, UltraScale Memory IP, DM_DBI Parameter**. An interface-design example, not proof
  of PC-controller DBI configuration.
  <https://docs.amd.com/r/en-US/pg150-ultrascale-memory-ip/DM_DBI-Parameter>
- **S5 — Intel, same datasheet, Data Swapping**.
  <https://edc.intel.com/content/www/us/en/design/ipla/software-development-platforms/client/platforms/alder-lake-desktop/12th-generation-intel-core-processors-datasheet-volume-1-of-2/001/data-swapping/>
- **S6 — Liu et al., An Experimental Study of Data Retention Behavior in Modern DRAM Devices**,
  ISCA 2013. Experiments were on DDR3.
  <https://www.pdl.cmu.edu/PDL-FTP/NVM/dram-retention_isca13_abs.shtml>
- **S7 — PassMark, MemTest86 Test Algorithms**, moving inversions and modulo methodology.
  <https://www.memtest86.com/tech_memtest-algoritm.html>
- **S8 — JEDEC, LPDDR6 standard announcement**, July 9, 2025; JEDEC-issued release on
  Business Wire.
  <https://www.businesswire.com/news/home/20250709315796/en/>

Local primary sources: current `src/`, `../TM5/bin/mtests0.asm`, existing TODO entries 3, 13, 19,
27, 28, 90, and 95, the prior `doc/pattern_research.md`, and `../patterngen-test/`.
