# TODO 65. [TMR-APP] Modern x86 Instruction Survey — findings + negative results (REFERENCE)

> Full record, moved verbatim out of `TODO.md` on 2026-09-28 (open item).
> The short entry in `TODO.md` holds the current status; this file holds the reasoning.

**Priority**: Reference (do not re-litigate; recorded so these are not re-proposed)
**Status**: ✅ Survey COMPLETE (2026-07-31 / 2026-08-20). Full data: **`../shuffle-test/FINDINGS.md`**

Surveyed modern x86 primitives for memory testing beyond what TMR already uses (CLFLUSHOPT, NT
stores). Benchmarked as a DRAM→DRAM cache-line rotation, 128 MiB/thread, median of 9,
1/4/6/8 threads, physical-cores-first pinning, on Intel Xeon 6975P-C (Granite Rapids).

**Aggregate MiB/s, median:**

| Method | 1T | 4T | 6T | 8T |
|---|---:|---:|---:|---:|
| NT-512 | 11,394 | **41,518** | 36,177 | 45,283 |
| NT-256 (AVX2) | 11,176 | 41,089 | 36,855 | **46,441** |
| MOVDIR64B | 11,535 | 41,126 | 37,305 | 42,150 |
| cached-512 | 8,911 | 29,662 | 23,149 | 29,494 |
| ERMSB 64 B/call | 8,579 | 28,768 | 23,770 | 29,912 |
| ERMSB bulk | 7,212 | 25,245 | 22,723 | 26,938 |

**Findings that change design decisions:**
1. **No-RFO is worth +21% (1T) to +40% (4T).** A normal cached store must first *fetch* the
   destination line; eliminating that removes ~1/3 of bus traffic for a pure copy. The gap *widens*
   with thread count because bus bandwidth is what runs out first. Largest single effect measured.
2. **Memory saturates at 4T; 6T is ~13% worse.** → #64.
3. **Width is irrelevant once saturated.** NT-256 (AVX2, TMR's portable baseline) is within **1.0%**
   of NT-512 at 4T and *fastest* at 8T. Same conclusion as the CLFLUSHOPT width comparison
   (`simd_codegen_rules.md` Rule 3) — **choosing the portable primitive costs nothing.**

**ACCEPTED (optional, CPUID-gated):**
- **MOVDIR64B** — ties NT at every thread count, so its value is **coverage of a different store
  mechanism, not speed**. One instruction per line (fewer uops), memory-to-memory (a DRAM→DRAM
  shuffle needs no temp buffer — the "needs a cached temp store" objection applies *only* to
  register-sourced pattern fills), and it **forbids write-combining coalescing**, so every line
  becomes its own bus transaction — arguably good for a stress tool. **Portability is the
  constraint**: Sapphire Rapids / Ice Lake-SP+ and Zen 5 only, far narrower than TMR's
  AVX2+CLFLUSHOPT baseline. Must be a runtime-gated variant that skips cleanly, never a default.

**REJECTED — with reasons, so they stay rejected:**
- **ERMSB (`rep movsb`)** — slowest of six at every thread count; microcode-defined behaviour
  invisible from user mode. Full reasoning in #19 Part B step 5 above.
- **MOVDIRI** — 4/8-byte direct store; cannot fill a cache line. Same device-MMIO purpose.
- **CLWB** — writes back but leaves the line **valid**, so a following verify read *hits cache* —
  defeating the entire purpose. The SDM only says the line *may* be retained, so even that is not
  guaranteed. Real use is persistent-memory durability, itself fading as eADR platforms put caches
  inside the persistence domain.
- **MOVNTDQA** (stream load) — on WB memory it behaves as an ordinary load. It only bypasses cache
  on WC/UC memory, which needs a driver (TMR-MD shelved, client code purged — #4/5).
- **`-Z build-std`** — A/B measured 2026-08-20 on the shuffle benchmark (alternating passes, same
  RUSTFLAGS): deltas scatter both directions and are **entirely inside noise**. The yardstick:
  baseline NT-512's own two passes differed by **7.5%** (11,053 vs 11,882 MiB/s, same binary), and
  within-pass spread reached 33% — every A/B delta was smaller than one binary's run-to-run
  variance. Costs: **build time 2.7 s → 24.8 s (9.2×)**, binary +10.8%. Architecturally expected:
  generics/`std::simd` are monomorphised in-crate (already built at `x86-64-v3`), and on
  `windows-msvc` `memcpy`/`memset` come from the MSVC CRT, not `compiler_builtins` — so the
  headline win cannot apply. What remains in precompiled std (`core::fmt`, `std_detect`, panic
  glue) is all cold path. Also: it makes benchmark numbers depend on the local nightly +
  `rust-src`, which is the **same objection that disqualified ERMSB** (behaviour set by an
  invisible layer). *Revival trigger*: a measured delta above noise on a **non-saturated** test.

**Possible upstream contribution — optional, NOT TMR work** (clarified 2026-09-11 when #33 closed):
`MOVDIR64B`, `MOVDIRI` and `CLWB` are all **absent from stdarch but recognized by LLVM**, so adding
`_mm_movdir64b` + feature detection is feasible, and contributing it would be a decent open-source
piece. It buys TMR nothing though — per the verdicts above, MOVDIRI and CLWB are **rejected** and
MOVDIR64B only ties NT stores (slower at 8T), so none of the three beats the primitives TMR already
has (SIMD, SIMD NT writes, CLFLUSHOPT). Detection currently needs raw CPUID (`CPUID.07H:ECX` bit 28).
Note the intrinsic-vs-`asm!` performance question is **moot** for single opaque opcodes —
`../clflush-test/` measured both forms identical for CLFLUSHOPT. It only matters when inline asm owns
a whole *loop* and blocks vectorization (see `doc/nt_stores.md`).
