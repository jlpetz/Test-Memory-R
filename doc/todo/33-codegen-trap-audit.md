# TODO 33. Audit open Rust + stdarch issues for SIMD codegen traps ✓ CLOSED (2026-09-11)

> Full record, moved verbatim out of `TODO_ARCHIVE.md` on 2026-09-28 (closed item).
> The short entry in `TODO_ARCHIVE.md` holds the current status; this file holds the reasoning.

Raised 2026-07-29 after being bitten twice by compiler behaviour that **silently** degraded SIMD
codegen with no error or warning, both found only by reading emitted assembly: (1) NT stores, where
`core::intrinsics::nontemporal_store` emitted `!nontemporal` *metadata* that LLVM was free to drop,
so the instruction vanished — **not upstream-fixable** (NT stores aren't in the memory model, the
required fence placement isn't derivable from surrounding IR, so there is no sound+optimizable
lowering; stdarch moved to inline `asm!` for soundness, costing optimization and forcing a manual 4×
unroll); and (2) the memset idiom, where byte-uniform fill constants were rewritten to `memset`,
collapsing every SIMD width to one libcall.

**Closed without doing the proposed upstream-issue sweep — deliberately.** What the item was
protecting has already been banked by other means:
- Both traps are diagnosed and documented in `doc/nt_stores.md` and `doc/simd_codegen_rules.md`, with
  the rules folded into `TMR-APP/CLAUDE.md` so they load every session.
- **The intrinsic-surface inventory is done**, which was the hard part. TMR's entire hot-path
  `std::arch` surface is loads/stores (incl. NT `stream`), `xor`, `set1`/`splat`, `setzero`,
  `extract`, `fence`/`clflush`; the `std::simd` surface is `splat`, `|=`, `^=`, `simd_ne`,
  `from_array`. All single-instruction with **no emulation path** — which is what let rust#159831
  (AVX2 variable shifts, where leftover portable-SIMD overflow handling adds
  `vpcmpgtd`/`vpminud`/`vpblendw` around `vpsllvd`) be ruled out in one line: TMR uses zero
  variable-shift intrinsics.
- The standing practice exists **and is demonstrably followed**: `simd_codegen_rules.md` rule 5, and
  the one intrinsic adopted since (`_mm_clflushopt`) carries `verified by --emit asm` at
  `tests.rs:1332`.

**Why the sweep itself was the wrong shape**: it is a point-in-time snapshot of a live tracker, stale
the week after — an item that never truly closes. Both traps that actually bit TMR were found by
reading *our own* emitted assembly, not by reading issue trackers, which is evidence about which
method works here. Given the inventory result, the residual risk is not the current surface but
*future* adoptions, and rule 5 covers exactly that. Per the closure principle: an enforceable rule is
a closed item, a standing audit is an open one.

**Nothing relevant was left outstanding upstream.** The one intrinsic TMR actually needed —
`_mm_clflushopt`, for Mem-Refresh — landed (stdarch#2141, nightly behind `simd_x86_clflushopt`); TMR
switched off inline asm in commit `143913d`. The remaining stdarch gaps (`MOVDIR64B`, `MOVDIRI`,
`CLWB` — absent from stdarch but recognized by LLVM) were **benchmarked in #65 and are not better
than the primitives TMR already has** (SIMD, SIMD NT writes, CLFLUSHOPT): MOVDIRI rejected (4/8-byte
store, cannot fill a cache line), CLWB rejected (leaves the line valid, so a following verify read
hits cache), MOVDIR64B ties NT at 1/4/6T and is *slower* at 8T (42,150 vs 45,283/46,441 MiB/s) —
accepted only as optional coverage of a different store mechanism, not for speed. Contributing those
intrinsics upstream may still be worthwhile as open-source work, but it is not TMR work and should
not hold a codegen-audit item open.

**Consequence**: the `--emit asm` check was promoted from `doc/simd_codegen_rules.md` into
`TMR-APP/CLAUDE.md` (see "Validate the emitted assembly after touching a test"), so the one
enforceable thing #33 produced is in the always-loaded context rather than a doc that has to be
remembered.
