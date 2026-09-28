# TODO 19. [TMR-APP] Test Scaffolding + Additional Test Patterns

> Full record, moved verbatim out of `TODO.md` on 2026-09-28 (open item).
> The short entry in `TODO.md` holds the current status; this file holds the reasoning.

**Priority**: Medium
**Status**: In Progress (2026-06-01)

#### Execution Model — three tiers, NOT one unified framework (decided 2026-06-01)

A single "one framework for every test" was tried and **deleted** (`test_framework.rs`,
2026-04-13): a generic `TestPattern` trait + `run_interleaved_test()` tanked performance
because `#[target_feature]` does NOT propagate through trait-method/closure-call boundaries,
so SIMD silently fell back to baseline ISA. Do not re-attempt a unified loop harness.

Instead TMR has **three execution models at different levels of loop autonomy**, layered so
they share bookkeeping where possible:

| Tier | Mechanism | Who owns the loop | Tests |
|---|---|---|---|
| 1 — Clean phased | `run_phased_test` (test_harness.rs) | Harness owns loop; test supplies init/test/verify closures (monomorphized at call site) | SimpleTest, MirrorMove, Bench-Init/Verify, CLFLUSH-verify, REP MOVSB |
| 2 — Full loop control | `TestRunner` scaffolding (Part A, to build) | **Test owns the loop**; scaffolding does only between-chunk bookkeeping | StuckBit, Refresh, CacheBust, RandomTorture, StrideAccess, BlockMove, moving inversions, address-in-address |
| 3 — Concurrent / mailbox | Worker model (#28, to build) | Coordinator routes *jobs* (region descriptors, never loads/stores); workers own access, hand off via SPSC | Cross-thread page exchange, coherence-storm |

These are complete for every in-scope **correctness** test (the only escapees — Rowhammer /
bank-thrash — are deferred for needing physical addressing, a *capability* gap, not a framework
gap). Tier 1 and Tier 2 are not independent copies: Part A step 3 refactors `run_phased_test` to
use `TestRunner` internally, so block prep / timing / progress / stats have one source of
truth. Tier 3 (#28) stays separate by design — routing memory ops through a queue would
destroy the signal being measured (see settled decisions in TMR-APP/CLAUDE.md).

**Scope correction (2026-09-14):** the table above covers the correctness tests only. Three
further harness families use **neither** Tier 1 nor Tier 2 — `bandwidth_tests.rs`,
`latency_tests.rs`/`latency_tests_v2.rs` (both hand-roll window sizing, the cycle timer, the
progress throttle and `TestStats`), and `calibration.rs` (outside the pipeline entirely: its own
`ProbeEngine`, single-threaded, no `TestStats`). Partly principled — no error checking makes half
of `TestRunner` moot, bandwidth deliberately does not chunk, and latency returns
`LatencyTestStats` — partly duplication that predates the scaffolding. Tracked as #69 item D.
**Full per-test tier map, the loop-shape reason each Tier-2 test needs to own its loop, and the
decision rule for placing a new test: `TMR-APP/doc/test_harness_tiers.md`.**

#### Part A: Shared Test Scaffolding (`test_scaffolding.rs`) — ✅ COMPLETE (2026-06-05)

Extracted `TestRunner` (zero-cost between-chunk bookkeeping — window sizing, block prep,
timer/cycle loop, shutdown, throttled progress, error-mode dispatch, `TestStats`) and made it
the single source of truth for both Tier-1 (`run_phased_test`) and Tier-2 (loop-owning) tests.
NOT a loop harness — tests still own their entire hot inner loop; scaffolding methods are all
`#[inline]` field reads/writes called only between chunks. Hot paths stayed byte-identical;
2-run A/B per test confirmed throughput within VM-noise and 0 errors. See implementation order
below for the per-step record and the module doc-comment in `src/test_scaffolding.rs` for the
"why not a trait/closure" rationale (the `test_framework.rs` `#[target_feature]` trap).

#### Part B: New Test Patterns

**Reference sources**:
- Memtest86+: `github.com/memtest86plus/memtest86plus` — key file: `tests/test_funcs.c`
- stressapptest: `github.com/stressapptest/stressapptest` — key file: `src/worker.cc`

**Tier 1 — High value, tests fundamentally different hardware paths:**

- **Moving inversions** (Memtest86+ tests 5/6/7): Read-modify-write per element, ascending
  then descending. TMR has NOTHING that does per-element read→modify→write. All our tests
  separate read/write into bulk phases. Moving inversions specifically stresses store-to-load
  forwarding and detects address coupling faults (writing N corrupts N+1).
  **Standalone implementation using TestRunner scaffolding.**

  **March family (defer the macro):** moving inversions IS a march test — the academic
  shape "sweep all cells in a direction doing a fixed read-expected→write-new sequence per
  cell," in both directions. March C-, GALPAT/galloping are the same loop skeleton with
  different (direction, expect, write) tuples. They catch coupling / address-decoder /
  transition faults — exactly the marginal-tWR/tRCD regime that passes solid-pattern
  fill-verify but crashes in real workloads. The skeleton CANNOT be shared via a generic
  `fn march<F>(op: F)` (closure breaks `#[target_feature]`, same trap as `test_framework.rs`);
  the ONLY safe share is a `march_element!` **macro** expanded at the call site inside the
  `#[target_feature]` fn (like `auto_dispatch!`). Per "three similar lines beat a premature
  abstraction": write moving inversions as ONE concrete tier-2 test first; only extract the
  macro once a SECOND march-shaped test (March C-, galloping) actually appears. Flagged now
  so we recognise the pattern then — do NOT build the macro speculatively.

- **Cross-thread page exchange** (stressapptest): Thread A writes pattern to page, puts on
  shared queue. Thread B dequeues, verifies A's pattern, writes its own, re-queues. Tests
  cache coherency, memory ordering, NUMA interconnect.
  **Standalone implementation — thread coordination doesn't fit any harness.**

- **⭐ CLFLUSHOPT-based verify (HIGH STRATEGIC VALUE)**: Write pattern → CLFLUSHOPT
  every cache line → MFENCE → verify reads come from DRAM (cold cache).
  **Why this matters**: This is the user-mode replacement for what an UC (uncached)
  kernel driver would give us. Without it, every WB-cached "verify" risks reading
  the value the test just wrote out of L1/L2/L3 — a flipped DRAM bit gets masked
  by the still-cached good copy. CLFLUSHOPT forces the CPU to evict each line so
  the verify load actually round-trips through DRAM and back through the memory
  controller. This is how memtest86+, stressapptest, and modern memory testers
  achieve true DRAM validation without needing a kernel driver. **This single
  test type is the primary reason we don't need TMR-MD (#4/5, archived) for testing scope.**
  Reuse the existing `tests::flush_range_to_dram` helper (CLFLUSHOPT via inline
  `asm!` + trailing MFENCE, `#[target_feature(enable = "clflushopt")]`, 64-byte
  stride from detected `cache_line_bytes`). No `_mm_clflush` fallback — the startup
  CPUID gate guarantees the feature. Should be wired as a phase variant for all v2
  tests (init → flush → verify), not just one standalone test. Low implementation
  effort, fits cleanly into `run_phased_test`.

**Tier 2 — Medium value:**

- **Block move via REP MOVSB**: Fill A, hardware copy A→B, verify B. **Fits phased harness.**
- **Modulo-X with offset shifting**: Sparse writes + shift. **Standalone with scaffolding.**
- **Invert thread**: Read-modify-write (bitwise NOT). **Standalone with scaffolding.**
- **Extended bit fade**: RefreshStable with 1-60+ minute delays.
- **Address-in-address**: each cache line stores its own address (as 64-bit words) plus a
  per-line salt. Verify each line "knows where it lives." Catches wrong-row delivery /
  addressing faults that data-pattern tests miss — the controller handing back data from
  the wrong row reads as correct under a solid-bit pattern but fails here. Pattern is
  fully address-derivable, so it pairs naturally with the #27 cold-path verifier (expected
  value is trivially recomputed). **Standalone with scaffolding (or phased harness).**

**Tier 3 — Specialized:**
- Walking bit, Rowhammer (needs ring-0 physical-address visibility), AVX-512 scatter/gather
  random access.

**Implementation order**:
1. ✅ DONE (2026-06-01) Extract `TestRunner` scaffolding from `test_harness.rs` boilerplate
2. ⚠️ PARTIAL (2026-06-01; gap found 2026-06-12) Migrate existing v1 tests to scaffolding.
   **Corrected claim**: only the 6 *scalar entry* functions were migrated to `TestRunner`
   (`stuck_bit_test_multi`, `refresh_stable_multi`, `cache_busting_multi`,
   `random_torture_multi`, `stride_access_multi`, `block_move_multi` — each calls
   `TestRunner::new` near its top; the two macro sites are `stuck_bit_impl!` / `refresh_impl!`.
   *Cite the function names, not line numbers — an earlier revision of this item listed
   tests.rs:1516/2542/3371/3517/3658/3765, all of which had drifted by ~250-950 lines within
   three months.* The **SIMD `_impl` variants are NOT migrated** —
   `stuck_bit_test_{128,256,512}_impl` and `refresh_stable_{128,256,512}_impl` still carry the
   full hand-written boilerplate (window sizing, timer loop, progress throttle, TestStats
   construction, shutdown/error-mode dispatch) — ~500 LOC of duplication. The original "2-run
   A/B confirmed parity" was misleading for these: the SIMD code was unchanged, so it compared
   old-code to itself and passed trivially. Correctness is unaffected; this is dead-boilerplate
   debt, not a bug. **Remaining work tracked as step 2b below.**
2b. ✅ DONE (2026-07, commit d818d44) Migrate the SIMD `_impl` variants to `TestRunner`, AND
   macro-ize them, AND fix the memset idiom, AND apply N=4 MLP — all in one pass (they touch the
   same loops). The 6 hand-written SIMD `_impl` fns (StuckBit/Refresh × 128/256/512, ~470 LOC of
   duplication) collapsed into **2 macros** (`stuck_bit_impl!` + `refresh_impl!`, plus a
   `stuck_bit_write_verify!` helper); the runtime-gate `_multi` wrappers stay hand-written. Each
   impl now uses `TestRunner` (window/timer/progress/stats/shutdown shared with the scalar
   entries + `run_phased_test`). **Decision (settled): all updated/new tests use per-width
   macros** — no perf penalty (macro expands the SIMD body at the call site inside the
   `#[target_feature]` fn, so width is honored; this is NOT the deleted `test_framework.rs`
   trait/closure mistake). Also folded in:
   - **memset defeat (#61 code)**: StuckBit `0xAA`/`0x55` → `STUCKBIT_P1/P2` (`0xAA55…`/`0x55AA…`,
     exact complements — every bit still tested both ways); Refresh uses shared `REFRESH_PATTERN`
     (`0xA55A…`). Verified in asm: real per-width `vbroadcast`+`vmovdqa` stores, no memset.
   - **N=4 MLP verify (#62)**: 4 independent accumulator chains merged once; fixes 256-bit MLP
     starvation. The per-element `ErrorCheckInterval` intermediate-check mode was DROPPED (exact
     error localization is the job of the #27 two-tier verifier).
   Measured (Intel Xeon 6 VM, `--quick-test`, 2 runs, 0 errors, data volumes match): StuckBit256
   +13% (39.2k→44.4k), Refresh256 +13% (28.5k→32.2k), other widths flat-to-up, **nothing
   regressed**. StuckBit did NOT drop despite losing the memset fast-path (verify MLP gain
   outweighs the write-phase cost). 128-fastest ordering persists = Intel load-path property
   (AMD Zen 5 was flat/monotone), now visible because writes are width-honest. See
   `memory/simd-loop-optimization.md` + `refresh-test/FINDINGS.md`.
3. ✅ DONE (2026-06-05) Refactor `run_phased_test` to use `TestRunner` internally.
   Block prep / window sizing / info log / timers / progress / stats now flow through
   `TestRunner` (one source of truth shared with Tier 2). Bespoke phased accounting
   (`total_operations`, ops_per_wrc byte math, `block_metas`, `check_mask`, `skip_init`)
   stays hand-written. 2-run A/B on all phased tests (SimpleV2/SimpleNT/MirrorV2/Bench-*)
   confirmed parity, 0 errors, exact data-volume match. One intentional behavior change:
   Ctrl+C shutdown now reports `stopped_by_time_limit=false` (was sometimes `true`) — matches
   v1 tests, normal completion path unchanged. **Part A COMPLETE.**
4. ✅ **DONE (2026-07-30, commit 69ceca9) — ⭐ CLFLUSHOPT verify phase.**
   `TestMemoryConfig.flush_before_verify` (default false) + `.with_flush_before_verify(true)`,
   consumed in **`run_phased_test`** (between the write fence and the verify reads — so any
   phased test can opt in) and in the **`stuck_bit_impl!` macro** (one edit → all 3 widths, plus
   scalar). Flags hoisted to locals; the branch+call sit *between* the write and verify loops,
   never inside an element loop (asm-verified). New tests `Mem-StuckBit-Flush{,128,256,512}`
   registered so existing behaviour/timings are untouched. Also patched the two name-keyed
   lookups the new names would have fallen through (`get_operation_metadata`,
   `calculate_minimum_chunk_size` — the latter would have given a wrong SIMD alignment minimum).
   **Lesson: new test names need auditing against name-based dispatch, not just registration.**

   Validated Intel Xeon 6975P-C + AMD EPYC 9R45, 2 runs each: 0 errors, 144.00 GiB unchanged,
   throughput −16%…−26%.

   **Key measurement finding**: the drop is much smaller than "verify now reads DRAM" implies,
   because at the default chunk (6.2% of 3 GiB ≈ 192 MiB) the working set already vastly exceeds
   L1/L2/L3 — natural eviction meant the verify was **already DRAM-bound**. So the ~20% is the
   cost of the *flush itself* (clflushopt issue rate + MFENCE + forced write-back), not a
   cache→DRAM transition. This vindicates `cache_management.md`'s "workset > L3 is sufficient"
   reasoning. The flush only changes *behaviour* when the window/chunk would otherwise stay
   cache-resident — demonstrating that needs a small-window config (there is **no `chunk=` CLI
   param**; only JSON configs / test definitions set chunk mode → ties to #25 curated plans).
   Flushing also **flattens per-width differences**, since it becomes the bottleneck.
5. ❌ **CUT (2026-08-20) — Block move / REP MOVSB.** Two independent reasons, both decisive.
   *Coverage*: `Mem-BlockMove` already does fill-A → copy → verify-B; REP MOVSB would only swap
   the copy primitive, not add a pattern. *Measurement*: benchmarked head-to-head in
   `../shuffle-test/` (see #65) and ERMSB was the **slowest of six primitives at every thread
   count** — 0.69× of NT-512 at 4T, and slowest of all in bulk mode (which rules out
   per-invocation startup as the cause). It tracks *cached-512* to within 3%, the signature of a
   path still paying read-for-ownership, i.e. the documented ERMSB no-RFO streaming optimisation
   is not engaging here. Worse for a validation tool: that behaviour is microcode-defined with
   thresholds invisible from user mode (this part reports `fsrm=false`, plausibly a hypervisor
   masking the CPUID bit), so a failure could not be attributed to memory rather than microcode.
   **The streaming axis ERMSB nominally offers is already covered, controllably, by NT stores.**
   User agreed on coverage grounds before the benchmark ran ("Not sure I see value in this above
   what mirror move and other tests are already doing?").

**Remaining order (agreed 2026-08-20), easiest-first so each lands on proven scaffolding:**

5. **Address-in-address** — Tier-2 spec above. Low effort, no new machinery: pattern is
   address-derivable, so it reuses `TestRunner` + pairs with #27's cold-path verifier.
6. **Random writes** — low effort. `Mem-Random` is **reads-only** today, so random-access *write*
   traffic (and therefore random-order RFO / write-combining behaviour) is entirely uncovered.
7. **Moving inversions** — medium effort, **highest value of the four**. The only test with
   per-element read→modify→write; catches address coupling (writing N corrupts N±1). Write it as
   ONE concrete Tier-2 test; do **not** build the `march_element!` macro until a second
   march-shaped test appears (see the March-family note above).
8. **Cross-thread page exchange** — high effort, GSAT-style. Stresses the coherence fabric and
   inter-CCD/NUMA interconnect, which nothing in TMR touches. Needs bespoke thread coordination
   (no harness fits) — do it last.
9. **Extended bit fade** — config variant of RefreshStable with 1-60 min delays. Trivial once the
   above land; mostly a curated-plan entry (#25).

**Rejected for this batch — mirrored virtual memory** (`sliceable-ring-buffer`-style
double-mapping): see the assessment in #65. Provides physical aliasing without PFNs, but aliasing
is not layout, an ordinary shared buffer generates the same coherence traffic more simply, and it
would force 4 KB pages (the crate creates its section without `SEC_LARGE_PAGES`), costing TMR's
2 MB/1 GB large-page path for no new coverage.

**From the 2b work — ✅ DONE (2026-07-29, commit 74a8b65)**: SIMD codegen rules documented.
`doc/simd_codegen_rules.md` written as worked examples (byte = 2 hex digits, byte-layout
diagrams, why memset needs byte-uniformity, both defeat routes incl. that `from_array` alone
does NOT help, the vzeroupper interaction, the measured N=1/2/4/8/16 accumulator sweep,
register-file budgets, macros-not-generics, and benchmark-methodology traps). Concise rule set
added to CLAUDE.md "Settled Design Decisions"; cross-linked from `nt_stores.md` +
`cache_management.md`.
