# TMR and fearless_simd: notes and questions for the Linebender #simd channel

Written for the [Linebender Zulip #simd channel](https://xi.zulipchat.com/#narrow/channel/514230-simd),
linked from there rather than posted in full. The measurements behind it are in
[FINDINGS.md](FINDINGS.md). 2026-10-03.

## 1. Who we are and what we found

Hi all! I found fearless_simd through the State of SIMD in Rust 2026 article, spent some time
porting our hot loops to it, and have some results and a few questions. It's long, sorry;
the questions are in sections 2-4.

**The project.** I'm building TMR, a Rust DDR5 memory stability tester for overclockers. It
replaces TestMem5 (TM5), loads its `.cfg` files and runs equivalent tests. It runs entirely in
user mode on Windows: `VirtualAlloc2` gives us 1 GiB and 2 MiB pages without a driver. The hot
loops are the product. Each test writes a pattern across most of RAM on every core, optionally
flushes it out of cache, reads it back and compares, for hours. Repo: https://github.com/jlpetz/Test-Memory-R (current work is on the
`clippy-cleanup-todo20` branch; `main` is older)

**Why the SIMD width matters to us.** Every test has deliberate 128/256/512-bit variants plus an
Auto, and the width is part of what is tested and measured. So a variant must really execute at
the width in its name. Today each width is a `macro_rules!` expansion inside a
`#[target_feature]` fn using `std::simd`. We deleted an earlier generic, trait-based framework
when we found its SIMD silently compiling at the baseline ISA.

**Things we learned the hard way.** Every one was silently lossy (correct results, wrong
code), and we only caught them by reading the asm:

1. **Byte-uniform fills become `memset`.** A store loop of `splat(0xA5A5_A5A5_A5A5_A5A5)` is
   rewritten by LoopIdiomRecognize into a `memset` call, so the 128, 256 and 512 variants
   collapse into the same libcall. The timing didn't give it away: the memset write path was
   sometimes faster, sometimes not, so the numbers looked plausible. It also put a `vzeroupper`
   in front of every `memset` call, because the code dropped out of AVX state to call the CRT.
   A non-byte-uniform constant (`0xA55A_A55A_A55A_A55A`) keeps the real store loop and gets
   rid of the `vzeroupper` too. `from_array([x, x])` doesn't help; LLVM folds it to `splat(x)`.
2. **One accumulator starves memory-level parallelism.** Our verify is
   `acc |= load ^ pattern`. With a single chain, 256-bit cold-DRAM verify on Granite Rapids ran
   at 10.2 GiB/s against 14+ for 128 and 512. Four independent accumulators brought it to 12.8
   (and 116 to 122 GiB/s in L2). It made no difference on Zen 5. Sixteen spills on AVX2.
3. **Non-temporal stores lose their hint.** More in section 2.

**What I tried.** A probe crate with 78 kernels. Each TMR loop is written twice: once in our
macro style, and once as a single generic `#[simd] fn k<S: Simd, V: SimdInt<S, Element = u64>>`,
where the vector type gives the width and the token gives the ISA. I checked them three ways:
- a script that finds every kernel's hot loop in the emitted asm and checks the register width,
  the expected instructions, calls inside loops, memset/memcpy calls and spills;
- equivalence tests (identical output to the macro version; every verify catches a single
  flipped bit, including in the tail loops);
- timings, L2-resident and DRAM at 1-8 threads on 1 GiB pages.

Setup: Rust nightly 1.100 (LLVM 23.1.1), baseline `x86-64-v3`, Xeon 6975P-C (Granite Rapids).

**Results.** All 78 match their expectations:
- Fill, 4-accumulator verify, positional pattern write/verify, a 64-bit LCG pattern and
  prefetching verify give the same width and instruction mix as our macros at 128/256/512. The
  four accumulator chains survive, and the `from_slice` bounds checks are fully elided.
- `mul_u64x4` on AVX2 is four scalar `wrapping_mul`s in the source, but LLVM's SLP pass turns
  it back into the same `vpmuludq` x3 sequence `std::simd` emits. Fine, if a little lucky.
- The memset trap applies to fearless code too (expected): the byte-uniform 256-bit fill
  collapsed. Maybe worth a line in the docs for people writing fill benchmarks.
- **The result I liked most:** I deliberately broke inlining with an `#[inline(never)]` generic
  helper called from the 512-bit verify, the mistake that killed our trait framework. With
  `std::simd` the helper silently compiles `u64x8` as 2x `vxorps ymm` + 2x `vorps ymm`. With
  fearless_simd it calls the out-of-line `__fearless_simd_kernel` fns, which are still
  `vxorps zmm` / `vorps zmm`. So it's slow (about 10x), but it never silently lies about the
  width. For a tester that's the right failure mode.
- **Timing:** single thread L2-resident, then DRAM at 1-8 threads with 2 GiB per thread on
  1 GiB pages. Fearless matches our macros within about ±2% for fill, positional, LCG,
  CLFLUSHOPT, NT stores (22 to 91 GiB/s) and copies, at every width and thread count.
- **One gap, and it wasn't fearless_simd:** 512-bit verify ran at 84% L2-resident (and 83% for
  the prefetching variant at one thread in DRAM). The `chunks_exact` loop gets indexed
  addressing (`[r9 + 8*r10 + 64]`), where our pointer walk gets `[r8 - 192]`. At two 64-byte
  loads per cycle that costs about 16%. The same generic body with a raw-pointer walk runs at
  172.7 vs 171.2 GiB/s; a safe `split_at` walk only half-fixes it. At 128/256 the
  `chunks_exact` form was 3-7% *faster*. On our side the fix is simple: write the verify
  loops as pointer walks, as our current ones already are.
  **A question for you:** would this be worth a note in the fearless_simd docs for other
  users? Or perhaps a small fearless_simd helper for "iterate this slice as `V`s" that
  produces the pointer-walk shape, so people can stay in safe, bounds-check-free code
  without hitting it.

**Small friction notes,** in case they're useful:
- In a `#[simd]` body, even SSE intrinsics like `_mm_sfence` and `_mm_prefetch` need `unsafe`
  (E0133: features from the build config don't count). Inside `kernel!` they're safe.
- `kernel!` only takes safe fns, so pointer intrinsics need an `unsafe` block inside a safe fn,
  with the contract moved to the caller. Understandable, but all our interesting instructions
  take pointers.
- At an `x86-64-v3` baseline, `dispatch!` promotes an `Sse4_2` level to `Avx2`. Harmless for us
  because the type sets the width, but it surprised me.
- The `Avx512` token needs the full Ice Lake set. Your README says that's to avoid the early
  slow implementations, and it lines up exactly with DDR5: every DDR5 platform with AVX-512 I
  know of has the full set, so it costs us nothing. I imagine that's why it's set up this
  way; you've clearly done your homework.

So this looks like a viable way to replace our per-width macros for the plain kernels. The rest
of this write-up is about the instructions a memory tester needs that a SIMD library doesn't
(yet?) have: non-temporal stores, tiles, and cache-line ops, roughly in order of how
SIMD-shaped they are.


## 2. Non-temporal stores (our strongest case)

**Why we use them.** Our NT tests stream patterns with `_mm*_stream_si*`, bypassing the cache
so the write really goes to DRAM. Multi-threaded, that's about 46% more write bandwidth than
cached stores (no read-for-ownership): 69.5 vs 47.1 GiB/s at 128-bit. Full lines matter on this
Intel part: a 64-byte (512-bit) NT store commits in about 5.6 ns, while 8, 16 and 32-byte NT
stores all sit around 25.6 ns. A partial line takes a slow path whatever its size.

**How we got where we are.**
1. `core::intrinsics::nontemporal_store` emits `!nontemporal` metadata, and LLVM passes still
   drop it (llvm/llvm-project#56703 fixed one pass, not all). As of LLVM 22.1 our scalar,
   `u64x8` constant and `u64x8` computed cases all came out as ordinary stores.
2. The stdarch `_mm*_stream_*` intrinsics are correct because they're `asm!` inside (since
   rust-lang/stdarch#1541). But that makes them opaque, so LLVM won't unroll a loop that
   contains them: one NT store per iteration.
3. So we unroll 4x by hand in a macro. The unroll debate settled on "it barely matters": pure
   single-thread writes measured about 24 GiB/s at 1x, 2x, 4x, 8x and 16x, because DRAM is the
   limit. We kept 4x as cheap insurance for loops that also compute the pattern.
   `-C llvm-args=-unroll-count=4` also works, but it applies to every loop in the binary.

**fearless_simd today.** NT stores work through a small extension trait. It wraps `kernel!`
with `#[inline(always)]`, the same pattern the crate uses for its own ops:

```rust
pub trait NtStore<S: Simd>: SimdBase<S> {
    /// # Safety: dst aligned to size_of::<Self>(), and sfence before the memory is used.
    unsafe fn nt_store(self, dst: *mut u64);
}
impl NtStore<Avx512> for u64x8<Avx512> {
    #[inline(always)]
    unsafe fn nt_store(self, dst: *mut u64) {
        kernel!(
            #[inline(always)]
            fn k(_t: Avx512, v: u64x8<Avx512>, dst: *mut u64) {
                unsafe { _mm512_stream_si512(dst.cast(), v.into()) }
            }
        );
        k(self.simd, self, dst)
    }
}
```

A generic `nt_pos_write<S, V: SimdInt<S, Element = u64> + NtStore<S>>` with the same 4x
unroll compiles to exactly our macro's hot loop: 19 instructions, same registers, at all three
widths.

**The memory-model part.** The stdarch contract says the writing thread must `_mm_sfence()`
after its NT stores and before any other access to that memory. Rust's own fences don't do it.
On LLVM 23, `fence(Release)` emits nothing and `fence(SeqCst)` emits
`lock or dword ptr [rsp], 0`, not `mfence`. We sfence once per chunk, after the write loop and
before the verify reads it back. That works, but it's an `unsafe` contract that's easy to break
in a refactor.

**Proposal: a safe, scoped NT writer.** Roughly:

```rust
simd.nontemporal(&mut buf, |nt| {
    for chunk in nt.chunks::<u64x8<_>>() {
        nt.store(v, chunk);
    }
}); // sfence runs here; `buf` is mutably borrowed until then
```

The mutable borrow means nobody can read the memory until the sfence has run, so it can be
exposed as safe. It has to be a closure scope, like `std::thread::scope`, not a guard object:
`mem::forget` on a guard would end the borrow without the fence.

Questions:
1. Would something like this fit fearless_simd?
2. Shape: per-vector `store` plus a `store4` (or chunk-of-lines) helper, since stdarch's `asm!`
   blocks unrolling? Alignment checked once per scope, with a panic or a plain-store fallback?
3. Other targets: on AArch64, `STNP` is a non-temporal pair store. On wasm there's nothing, so
   plain stores plus a no-op fence. A non-temporal hint that degrades to a normal store seems
   fine.
4. Worth documenting that on current Intel only full 64-byte lines take the fast NT path, which
   means 512-bit stores or adjacent pairs of 256-bit ones?

I'm happy to prototype it as a PR if the shape sounds right.


## 3. Tiles (AMX today, ACE later): is 2D in scope?

This one is a scope question. fearless_simd already does what tiles would need: tokens,
runtime feature detection, and safe wrappers over multi-element intrinsics. AMX/ACE are, loosely,
the same idea in two dimensions.

**Our use is unusual: memory access, not matrix math.** A `TILELOADD` reads up to 16 rows of
64 bytes at an arbitrary stride in one instruction, and `TILESTORED` writes the same shape. No
vector load or store makes that DRAM access pattern, which is what a memory tester wants. So a
minimal subset would cover us without any of the `TDP*` math: tile config, load, store, zero,
release.

**Why it isn't a simple extension of the vector types,** as far as I understand it:
- Tile shape is per-thread state set by `LDTILECFG`, so a safe API probably needs a scoped
  "configured tiles" context whose handles can't outlive or contradict the config.
- The register state is about 8 KiB, and the OS must opt the thread in. On Linux that's
  `arch_prctl(ARCH_REQ_XCOMP_PERM, ...)`. The Windows Server 2025 VM I'm testing on reports the
  tile state already enabled in XCR0; I haven't yet checked what else Windows needs per thread.
- LLVM models tiles with its own `x86_amx` type and tile register allocation, and stdarch's AMX
  intrinsics are still unstable (rust-lang/rust#126622).
- ACE (the announced cross-vendor matrix extension): I don't know of shipping hardware or LLVM
  support yet.

Questions:
1. Is tile support something you'd want in fearless_simd eventually, perhaps as a separate
   module or sibling crate reusing the tokens and detection?
2. If so, would a minimal load/store/zero subset be a reasonable first step, or would you want
   the math ops designed in from the start?

We have AMX hardware (Granite Rapids on AWS) and could prototype.


## 4. Cache-line ops (CLFLUSHOPT, CLWB, MOVDIR64B): the least SIMD-shaped

The least related, so I'm completely fine with "out of scope".

**Our use.** We flush each chunk with `CLFLUSHOPT` between writing and verifying, so the
verify reads DRAM instead of a cached copy that could hide a bit error. `CLFLUSHOPT` is about
15x faster than `CLFLUSH` for us. `MOVDIR64B` is a 64-byte direct store (memory to memory, no
cache allocation), which we're evaluating as another write path. Like NT stores it's weakly
ordered and needs an sfence.

**In fearless_simd today.** `_mm_clflushopt` inside a kernel can't inline, because no level
lists `clflushopt`. In a write-then-flush-each-line loop that costs a `vzeroupper`, a call and
a re-broadcast of the pattern per line. In DRAM that measured 2-5% slower until bandwidth
saturates. Two workarounds work fine: inline `asm!`, or a separate
`#[target_feature(enable = "clflushopt")]` fn called once per chunk, which is what we do.

**Why a level doesn't fit.** These instructions aren't on the SIMD ladder:
- `CLFLUSHOPT` is on Skylake client and Zen 1, which have AVX2 and no AVX-512.
- `MOVDIR64B` is on Alder Lake (no AVX-512) but not on Zen 4 (which has AVX-512).

So a rung would lose them somewhere. They look like capability tokens orthogonal to the level,
combined in a kernel:

```rust
kernel!(fn write_flush(avx512: Avx512, cf: Clflushopt, buf: &mut [u64]) { ... })
// the inner #[target_feature] fn gets the union of both feature sets
```

The Struct Target Features RFC (rust-lang/rfcs#3525) would make this native: a function taking
a `Clflushopt` token would get the feature enabled. It isn't on nightly yet; I checked.

I prototyped the token downstream on nightly to see if it holds up. It's a CPUID-proven
`Clflushopt` token, plus a macro that emits one entry fn per level whose `#[target_feature]` is
your level's exact list plus `clflushopt`, around a generic `#[inline(always)]` body. It works:
`_mm_clflushopt` inlines inside the fearless loop with the same 8x unroll as our standalone
flush fn, and the output is identical. The cost is copying your per-level feature strings (we
test them against the CPU), which is exactly the part that belongs upstream or in the language.

**Blockers, and what I'd contribute first:**
- The `clflushopt` target feature and `_mm_clflushopt` are still nightly-only
  (rust-lang/rust#157096). I added both earlier this year when TMR needed them: the target
  feature in rust-lang/rust#157098 and the intrinsic in rust-lang/stdarch#2141. Taking them
  through stabilization is a natural next step for me.
- `MOVDIR64B` has no stdarch intrinsic, no rustc target feature, and std_detect doesn't know it
  (`is_x86_feature_detected!("movdir64b")` doesn't compile). I'd add those upstream first.

Questions:
1. Would capability tokens (or waiting for RFC 3525) interest you?
2. Do you accept nightly-gated features behind a cargo feature, or is it stable-only until
   these stabilize?

Thanks for the crate, and for the article that led me to it. I'm happy to share the probe crate
(kernels, asm checker, equivalence tests) if any of it is useful for your CI.
