# Writing SIMD Code in TMR — Codegen Rules and Traps

**Date**: 2026-07-29
**Rust**: 1.99.0-nightly (findings verified via `--emit asm` at the project's `x86-64-v3` baseline)
**Test apps**: `../refresh-test/` (memset idiom, MLP sweep, Intel-vs-AMD control), `../nt-test/`,
`../clflush-test/`

TMR's hot loops are the product. This doc collects the compiler traps we have actually hit —
each one found by reading emitted assembly, not by guesswork — and the rules that follow. Every
claim here was verified on hardware; where something is inferred rather than proven, it says so.

If you only read one thing: **the SIMD width you write is not necessarily the width you get.**
Check the asm.

---

## Rule 1 — Fill patterns must NOT be byte-uniform (or LLVM turns your SIMD into `memset`)

This is the trap that silently invalidated our width variants for months.

### First, what "byte-uniform" means

A **byte is 8 bits = exactly 2 hex digits**. So `0xA5` is *one* byte (not two things — `A` and
`5` are two hex *digits* of a single byte).

A 64-bit value is 8 bytes. Split them apart:

```
BYTE-UNIFORM (every byte identical → gets rewritten to memset):

  0xA5A5A5A5A5A5A5A5  →  [A5][A5][A5][A5][A5][A5][A5][A5]    all A5   ✗
  0xAAAAAAAAAAAAAAAA  →  [AA][AA][AA][AA][AA][AA][AA][AA]    all AA   ✗
  0x5555555555555555  →  [55][55][55][55][55][55][55][55]    all 55   ✗

NOT byte-uniform (at least one byte differs → your loop survives):

  0xDEADBEEFCAFEBABE  →  [DE][AD][BE][EF][CA][FE][BA][BE]    all differ  ✓
  0xA55AA55AA55AA55A  →  [A5][5A][A5][5A][A5][5A][A5][5A]    A5/5A alt   ✓
  0xAA55AA55AA55AA55  →  [AA][55][AA][55][AA][55][AA][55]    AA/55 alt   ✓
```

### Why byte-uniformity is exactly the barrier

`memset(ptr, VALUE, len)` fills memory with **one repeated byte** — that is all it can do. So
LLVM's **LoopIdiomRecognize** pass can replace your loop with a `memset` call **only if** the
loop's entire output is one byte repeated:

```rust
let p = u64x8::splat(0xA5A5A5A5A5A5A5A5u64);   // every byte is 0xA5
for i in 0..n { *base.add(i) = p; }            // == memset(base, 0xA5, n*64)
```
→ compiles to `jmp memset`. **The `u64x8` is gone.** All of `u64x2`/`u64x4`/`u64x8` collapse to
the *same* libcall, so "128 vs 256 vs 512" measures nothing on the write path.

Change one thing — the constant — and the loop comes back:

```rust
let p = u64x8::splat(0xA55AA55AA55AA55Au64);   // bytes alternate A5,5A
for i in 0..n { *base.add(i) = p; }
```
→ `vpbroadcastq` + `vmovdqa64 %zmm0, …` — a real, width-honest store loop.

### Two ways to defeat it (either works; you do NOT need both)

| Route | Example | Works? |
|---|---|---|
| **1. Vary bytes *within* the 64-bit lane** (lanes may be identical) | `splat(0xA55AA55AA55AA55A)` | ✓ |
| **2. Vary *across* lanes** (each lane may be byte-uniform internally) | `from_array([0xAAAA…, 0x5555…])` | ✓ |

**`from_array` by itself does NOT help.** Verified: `from_array([0xA5A5…, 0xA5A5…])` still
becomes `memset`, and LLVM literally *aliases* `from_array([x, x])` to `splat(x)` (same symbol in
the asm). The constructor is cosmetic — **only the resulting byte pattern matters.**

### Important scoping notes

- Idiom recognition is **independent of the vectorizer and of `target-cpu`**. It acts on the
  loop's *effect*, not the element type. Lowering the baseline does not save you.
- **Width still needs the `target_feature`.** A surviving `u64x8` loop emits `ymm` (2× AVX2) at
  the `x86-64-v3` baseline; it only becomes `zmm` inside a fn carrying
  `#[target_feature(enable = "avx512f,…")]`.
- **Pure copy loops (`*dst = *src`) are the same class of risk** (`memcpy` idiom). It did not fire
  in our isolated probe, but treat it as live.
- **Inline `asm!` is immune** — LoopIdiomRecognize cannot see through an `#APP` block. This is a
  second reason the NT-store paths use asm (see `nt_stores.md`).

### What TMR does

| Test | Pattern | Note |
|---|---|---|
| `Mem-Refresh*` | `REFRESH_PATTERN = 0xA55AA55AA55AA55A` | Same 4-of-8-bits-set retention stress as the old `0xA5A5…` |
| `Mem-StuckBit*` | `STUCKBIT_P1 = 0xAA55AA55AA55AA55`, `STUCKBIT_P2 = 0x55AA55AA55AA55AA` | **Exact bitwise complements** (`P1 ^ P2 == !0`), so every bit is still tested as both 1 and 0 across the phases — the stuck-at coverage is unchanged |

The pattern generators (`pattern_gen.rs` modes 0/1/2/10/11/12) are address-derived/evolving and
therefore **never** byte-uniform — this trap only bites hardcoded constants.

### The vzeroupper side effect (bonus)

The `memset` call was *also* forcing a `vzeroupper` immediately before it (the asm shows
`movb $-91,%dl` — the `0xA5` fill byte — then `vzeroupper`, then `callq memset`). Fixing the
constant removes that too, for free.

The remaining `vzeroupper` instructions sit before the `flush`/`sleep`/logging **calls** and are
**irreducible**: `vzeroupper` is an ABI-safety guard inserted before *any* opaque call from
AVX-dirty code, because the compiler cannot prove the callee is free of legacy-SSE. It is not
about whether the callee touches vector data. **Never globally disable it**
(`-x86-use-vzeroupper=0`) — that removes the guard, not the ~70-cycle SSE/AVX transition penalty,
so it is likely net-slower. They are also harmless here: ~70 cycles, once per chunk, against a
64 ms (~64,000,000-cycle) sleep. **There are zero `vzeroupper` inside any hot loop.**

---

## Rule 2 — Latency-bound reduction loops need multiple independent accumulators (N=4)

### The problem: one accumulator = one serial chain

```rust
// BEFORE — single accumulator
let mut acc = u64x4::splat(0);
for i in 0..n {
    acc |= *base.add(i) ^ p;    // every iteration reads+writes the SAME acc
}
```

Each `|=` must wait for the previous one — a single dependency chain. The `|=` itself is fast
(1 cycle); the problem is that the chain limits how many **loads stay outstanding**. DRAM
bandwidth ≈ (requests in flight) × (bytes per line) ÷ latency (Little's Law), and a core only has
~10-12 Line Fill Buffers to track outstanding misses. Starve them and you get a fraction of peak.

```rust
// AFTER — 4 independent chains
let (mut a0, mut a1, mut a2, mut a3) = (splat(0), splat(0), splat(0), splat(0));
let mut i = 0;
while i + 4 <= n {
    a0 |= *base.add(i)     ^ p;   // four chains, none waits on another →
    a1 |= *base.add(i + 1) ^ p;   // four loads can be in flight at once
    a2 |= *base.add(i + 2) ^ p;
    a3 |= *base.add(i + 3) ^ p;
    i += 4;
}
let acc = (a0 | a1) | (a2 | a3);  // merge ONCE, off the hot path
```

OR/XOR are associative and commutative, so the result is identical.

### N=4 is measured, not assumed

Swept N = 1/2/4/8/16 in two regimes on the Intel box (median GiB/s):

```
              cold DRAM              L2-resident (warm)
   N      128    256    512       128     256     512
   1     14.3   10.2   14.1      80.8   116.0   150.6
   2     13.6   10.2   14.0      80.5   118.9   148.8
   4     13.2   12.8   14.0      80.9   121.8   145.9
   8     14.1   13.2   13.4      81.2   121.0   166.5
  16     14.1   10.9   13.8      68.3    91.6   166.0   ← 128 & 256 SPILL
```

- **N=4 is the minimum that fixes 256** (cold 10.2→12.8, warm peak 121.8) and is safe at every
  width. It is the best single universal choice.
- **N=16 regresses 128 and 256** — 16 ymm registers can't hold 16 accumulators + the pattern +
  temps, so it spills to stack (confirmed: 7 `vmovdqu %ymm, (%rsp)` stores in the N=16/256 loop).
  512 is immune (32 zmm) and even gains slightly.
- 512-only could take N=8 in L2 (~+10%) — not worth a per-width N.

### Register budget (why N must stay small)

In 64-bit mode there are **three separate register files** — they do not compete:

| File | Count | Used for | Competes with accumulators? |
|---|---|---|---|
| Vector `xmm`/`ymm` | **16** | loads, accumulators, pattern, temps | **YES — this is the budget** |
| Vector `zmm` (AVX-512) | **32** (+8 `k` mask regs) | same | **YES** |
| General purpose | 16 | pointers, index, counter | No |

So: `usable ≈ total − 1 (pattern) − ~2 (temps)` → **~13 on AVX2**, ~29 on AVX-512.
**Cap N to the narrowest target ISA.** N=4 has huge margin everywhere.

Note the AVX-512 **mask registers `k0–k7` are a separate file** — `simd_ne(...).any()` produces a
`k` register for free and does *not* consume a `zmm`.

Also: **more registers ≠ more memory parallelism** past the LFB limit. Don't hand-hoist 15 loads
into named registers; the out-of-order engine already runs ahead. You only need enough
*independent chains* (~4) to avoid serializing.

### SMT does not share architectural registers

Each logical thread has its **own complete** architectural register state — thread 0's `ymm0` and
thread 1's `ymm0` are different registers. **Never reduce N for SMT reasons.** What siblings *do*
share is the memory pipeline (LFBs, load/store ports, L1/L2) — so for bandwidth-bound work, pin
one worker per **physical** core (see `cpu_selection.rs`; we measured ~26k → ~39k GiB/s on Intel
just by spreading 4 threads off SMT siblings).

### Adopt it, but don't churn for it

The technique is portable (a serial chain limits in-flight loads on any out-of-order core) and
free where unneeded. The **payoff is hardware-specific**: ~13-30% for 256-bit on Intel Granite
Rapids, and **zero on AMD Zen 5** (which never showed the starvation). So: write new/refactored
verify loops with 4 accumulators; do not mass-rewrite chasing one machine's numbers.

---

## Rule 3 — Width only matters when you are not bandwidth-bound

Once a test saturates DRAM (multi-threaded, large working set), **all widths land within ~1%** —
DRAM is the ceiling, not compute. Width differences only appear single-threaded or
cache-resident. Corollaries:

- Don't tune width on a bandwidth-bound test (e.g. `Mem-Refresh` multi-threaded).
- Cached (WB) stores move data one **64-byte cache line** at a time regardless of store width;
  "wider stores = fewer round trips" is a **misconception** — the RFO/line-fill count is
  `bytes / 64`, not `bytes / width`.
- A non-monotonic result (middle width slower than *both* neighbours) is a red flag for a
  *software* cause — that is exactly how we found the single-accumulator starvation.

---

## Rule 4 — Use macros, not generics/traits, for per-width variants

`#[target_feature]` does **not** propagate through trait-method or closure boundaries. A generic
`TestPattern` trait + `run_interleaved_test()` was tried and **deleted (2026-04-13)** because SIMD
silently fell back to baseline ISA.

The safe pattern — and the project standard for all new/updated tests — is a `macro_rules!` that
**expands the SIMD body at the call site inside the `#[target_feature]` fn**. See
`stuck_bit_impl!` / `refresh_impl!` / `mirror_move_v2_impl!` in `tests.rs`. This collapsed 6
hand-written SIMD impls (~470 LOC of duplication) into 2 macros with no perf cost, and means a
fix or a new width is a one-line change instead of a 3-way copy-edit.

---

## Rule 5 — Benchmark methodology (or you will measure noise)

Hard-won from the `refresh-test` work:

1. **Keep CLFLUSHOPT/flush OUT of the timed region** (run it as an untimed prime). It was ~40% of
   a naive "verify" number and deflated true read bandwidth ~2×.
2. **Report median + [min..max], never best-of.** Best-of hides variance and inflates.
3. **Multi-thread aggregate = coordinator wall-clocks a fixed-work concurrent batch** (two
   barriers, `Instant`). Summing per-thread best-of produced fake **super-linear** results (AMD 8T
   appeared as 150-237 GiB/s). Per-core `RDTSC` is the wrong clock for cross-core wall time.
4. **Beware dead-code elimination.** A warm-loop bench doing `sink |= verify(const_ptr)` per rep
   got deleted by LLVM — `|=` is idempotent and the pointer was hoistable, yielding fake round
   numbers (625000 GiB/s). Fix: `black_box` the pointer each rep **and** fold with
   `wrapping_add`. **Implausibly round numbers mean DCE, not fast hardware.**
5. **Code layout affects tight loops.** Adding unrelated code shifts alignment and can swing
   timings ~10-25% (Intel 32B fetch/DSB windows; Mytkowicz et al., ASPLOS 2009). If a result
   flips when you add unrelated functions, suspect layout — isolate per-width builds.
6. **Use a different-vendor control.** Running the same binary on AMD Zen 5 is what proved the
   256-bit dip was Intel-specific rather than our bug. Invaluable and cheap.

---

## Upstream Rust/LLVM issues — what actually affects TMR

Checked 2026-07-29. TMR's `std::arch` surface is deliberately tiny: loads/stores (incl. NT
`stream`), `xor`, `set1`/`splat`, `setzero`, `extract`, and the `fence`/`clflush` family. Our
`std::simd` surface is `splat`, `|=`, `^=`, `simd_ne`, `from_array`. All lower to single
instructions with no emulation layer.

| Upstream issue | Affects TMR? | Why |
|---|---|---|
| [rust#159831](https://github.com/rust-lang/rust/issues/159831) — poor codegen for AVX2 variable shifts (`_mm256_sllv/srlv_epi32`) leaves redundant range checks | **No** | We use **zero** variable-shift intrinsics. The bug arises from stdarch expressing those intrinsics via portable-SIMD overflow handling; none of our ops have an emulation path. |
| `core::intrinsics::nontemporal_store` drops `!nontemporal` | **Yes — worked around** | We use `std::arch` stream intrinsics + manual 4× unroll. **Not upstream-fixable** (no `llvm.x86.*.movnt` intrinsic exists); see `nt_stores.md`. Do not chase. |
| `_mm_clflushopt` missing from stdarch ([stdarch#2141](https://github.com/rust-lang/stdarch/pull/2141)) | **Was — now fixed** | Landed in nightly behind `simd_x86_clflushopt`; TMR switched from inline asm to the intrinsic (commit `143913d`). |

**Lesson for practice**: the class of bug in #159831 — an intrinsic that is *emulated* in stdarch
leaving optimization residue — is worth watching whenever we adopt a *new* intrinsic. Before
adding one to a hot loop, **read the emitted asm and confirm it is the single instruction you
expect**. That check is cheap and has caught real problems twice now (NT stores, and this class).

If we ever do need variable shifts, gather/scatter, or masked ops, re-check upstream first —
those are the intrinsic families most likely to carry emulation overhead.

---

## Checklist for new SIMD code in TMR

1. Is the fill constant **byte-uniform**? → Change it, or you are benchmarking `memset`.
2. Is it a **reduction/verify** loop? → Use **4 independent accumulators**, merge once at the end.
3. Does N exceed the **narrowest target's register budget** (~13 on AVX2)? → Reduce it.
4. Per-width variants? → **`macro_rules!` expanded inside the `#[target_feature]` fn**, never
   traits/generics/closures.
5. New intrinsic in a hot loop? → **`--emit asm` and verify** it is the instruction you expect.
6. Measuring? → flush outside the timed region · median not best-of · `black_box` against DCE ·
   pin to distinct physical cores.
