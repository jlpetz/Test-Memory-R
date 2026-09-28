# TODO 66. Audit `unsafe` FFI Boundaries for Stored-Pointer Lifetime Bugs ✓ (2026-09-09)

> Full record, moved verbatim out of `TODO_ARCHIVE.md` on 2026-09-28 (closed item).
> The short entry in `TODO_ARCHIVE.md` holds the current status; this file holds the reasoning.

Triggered by a real dangling-pointer bug: `memory/backend.rs` stored a raw pointer to a
block-scoped `MEM_ADDRESS_REQUIREMENTS` into `extended_params`, then called `VirtualAlloc2` —
where the *kernel* dereferences it — after that scope had ended. On every allocation path, in two
duplicated blocks, and invisible to both the borrow checker (the raw cast erases the borrow) and
clippy. Fixed by hoisting to function scope (commit 43db10b).
- **All 26 sites read** against the rule "does the pointer outlive the expression that creates it"
  (not "does it cast to a raw pointer"). **No further instances** — every other FFI call uses the
  direct-argument form, where the temporary lives for the whole call.
- **Bonus, a different bug class**: `Vec<u8>` backing buffers for structs containing `KAFFINITY`
  (u64). `Vec<u8>` guarantees 1-byte alignment; fixed at 3 sites in `cpu_topology.rs` and 1 in
  `privileges.rs` by allocating `vec![0u64; …div_ceil(8)]` and keeping the walk in bytes via an
  explicit `*const u8` base. Verification of these is tracked as **#68**.
- **`SAFETY:` pass complete** — all 26 sites in the 5 FFI files documented (was 7). The shared
  `DeviceIoControl` invariant is stated once at the `impl DriverHandle` level so the per-site notes
  say only what is specific, rather than 8 near-identical paragraphs that train you to skip them.
- **Made enforceable, scoped**: `#![warn(clippy::undocumented_unsafe_blocks)]` on the 5 FFI files.
  Deliberately not crate-wide — the SIMD test kernels' `unsafe` is a repetitive story already
  covered by `test_fn_safety.md`. Enabling it immediately caught **4 gaps the manual pass missed**,
  including that `// SAFETY (Send):` does not match the `SAFETY:` prefix the lint looks for, so
  `buffer.rs`'s carefully-written `Send`/`Sync` justification was not actually registering.
- **Hardening from check 3**: `total_allocations` from the kernel is now clamped to the 128-slot
  array callers slice with. A bad count was safe (checked indexing) but would panic the run.
- **Not worth doing**: Miri — it cannot execute `VirtualAlloc2`/`DeviceIoControl`, so it never
  reaches these calls. This class has to be caught by reading, hence the lint.
