# TODO 4/5. [TMR-MD] Driver Assessment + Sync ✓ CLOSED — client code purged (2026-09-14)

> Full record, moved verbatim out of `TODO_ARCHIVE.md` on 2026-09-28 (closed item).
> The short entry in `TODO_ARCHIVE.md` holds the current status; this file holds the reasoning.

**Status**: DONE (2026-09-14). Decision: **remove all driver client code from TMR-APP**;
`TMR-MD/` left on disk, untouched, for possible resurrection. Gate met — `cargo clippy
--all-targets` clean, `cargo build --release` exit 0.

#### Why: no allocation capability the driver adds is wanted

The driver provided no meaningful capability beyond what the VA2 backend already delivers
for TMR's testing scope:

- **1GB huge pages**: working from user mode via `MEM_EXTENDED_PARAMETER_NONPAGED_HUGE`
  + `MEM_LARGE_PAGES` with 1GB-aligned size. Confirmed to allocate true 1GB pages
  in `src/memory/backend.rs`.
- **Cache bypass for testing**: CLFLUSHOPT in WB memory (see #19) is faster than
  driver-allocated UC/WC memory and covers the same testing intent. UC paths are
  ~50-100× slower, reducing memory coverage per unit time. WC is worse still for the
  *verify* half — uncached reads, no prefetch, one line at a time.
- **NUMA**: best-effort + post-alloc detection ("we asked for node N, here's where
  it landed, here's the local/remote split") is the right behavior for a testing
  tool — strict would just refuse to test rather than test+report.
- **Physical addresses**: only valuable for bank/row/channel-targeting tests
  (e.g. rowhammer), which require per-platform reverse-engineering of the DRAM
  controller's address mapping. Out of scope for TMR.
- **Batch allocation** (`IOCTL_TMR_BATCH_ALLOCATE`): amortises IOCTL round-trips — a
  cost that *only exists because a driver is in the path*. Self-inflicted, not a win.
- **Does NOT unlock "test all of RAM."** `MmAllocateNodePagesForMdlEx` draws from the
  same free list `VirtualAlloc2` does. Testing pages the OS is using needs pre-boot,
  which is a different product.

Two further points that decided it:

1. **Distribution is the real killer.** A KMDF driver needs an EV cert plus Microsoft
   attestation signing. Without it every user must enable test signing — disabling
   Secure Boot/HVCI, and breaking anti-cheat on exactly the gaming rigs this audience
   runs. TM5's users double-click an exe. That disqualifies a driver from the shipping
   product independent of technical merit.
2. **Even the revival scenario does not need *this* code.** The legitimate ring-0 wins
   for a DDR5 tester are *observability*, not allocation: prefetcher disable via
   MSR 0x1A4, direct MC-bank reads, IMC PCI-config / DDR5 on-die-ECC error counters.
   None of those are served by `IOCTL_TMR_ALLOCATE`. Reviving means writing a
   *different, much smaller* driver. The machine-check half is already covered from
   user mode — `whea.rs` uses `EvtSubscribe`, no driver needed (#63).

**Triggers to revive**: a specific test type that genuinely cannot be done from
user mode (e.g. rowhammer, where physical address visibility is mandatory). Not
before then — and note per (2) above that revival almost certainly means a new
MSR/PCI driver, not a sync pass on the allocation IOCTLs.

#### Preservation

`TMR-MD/` is **not under version control** — the git root is `TMR-APP`, and `TMR-MD` has no
`.git` of its own, so a delete would have been unrecoverable. It was left in place, as-is;
nothing in this item touched it. The `_mm_mfence`/`_mm_sfence` audit it needed is moot while
it is shelved.

#### What was purged (as executed)

Guiding rule: **delete the transport, keep the vocabulary.**

**Kept deliberately** — the seam a revival plugs into, each with a comment saying why: the
`Backend` trait, `BackendType` (left an enum rather than collapsed to a bool, since a ring-0
backend returns as one more variant), `memory::buffer::MemoryType`, `PageType`,
`PageSizeLevel`, and `SegmentInfo.physical_address`.

**Re-home turned out to be de-duplication** — the abstractions already existed in `memory/`:
- `driver::MemoryType` merged into `memory::buffer::MemoryType`, which the live allocator path
  already used. The duplicate `WriteCombined` spelling was dropped (`WriteCombining` is the
  one); `WriteThrough`/`WriteProtected`/`Uncached` were kept as the vocabulary a ring-0 backend
  would map onto `MEMORY_CACHING_TYPE`, even though only `WriteBack` is constructed today.
  `tests.rs` and `thread_pool.rs` repointed; the translation layer in `allocator.rs` deleted.
- `driver::PageSize` was an exact clone of `memory::allocator::PageSizeLevel`
  (Regular=0/Large=1/Huge=2) — deleted, `PageSizeLevel` kept.

**Deleted (transport + UI):**
- `src/driver/` entirely (1,395 LOC: `interface.rs`, `mod.rs`, `statistics.rs`, `types.rs`,
  `utils.rs`), plus `pub mod driver` and the driver re-exports in `lib.rs`.
- `DriverBackend`, `BackendType::Driver` and its match arms in `allocator.rs` / `runner.rs`.
- `batch_allocate_driver` and its dispatch — `batch_allocate` is now plain sequential
  `VirtualAlloc2` (there is no batch syscall to amortise, and allocation happens once at
  startup, off the hot path).
- `MemoryBackend::KernelDriver`, `RuntimeConfig.driver_available`, `use_driver_chunking`;
  `detect_runtime_capabilities` simplified and no longer prints driver availability.
- Config surface: `use_driver`, `driver_chunking`, `remap_mode`, `default_memory_type`, and the
  whole `impl MemoryAllocationConfig` block that existed to serve them (`parse_page_size`,
  `parse_memory_type`, `to_dma_config`). **Backward-compatible**: no struct uses
  `serde(deny_unknown_fields)`, so existing config files carrying those keys still parse (the
  keys are ignored). The three checked-in JSON configs were cleaned anyway to avoid implying
  the knobs still do something.
- CLI: `--driver-chunking` and `--batch-remap` — `ParamDef`s, usage lines, override arms and
  params handling — plus the whole `ADVANCED FEATURES:` / "DMA Memory: install kernel driver"
  help block.
- `main.rs` startup/shutdown: `check_kmdf_version()` / `verify_kmdf_compatibility()` and their
  call site, the driver-chunking + `DriverStatusReport` block, the remap-mode block including
  `set_use_remap_all(false)`, the `is_driver_connected()` shutdown stats block,
  `check_dma_driver_status()`, `use tmr::driver::DriverHandle`, and the `GetFileVersionInfoW` /
  `GetFileVersionInfoSizeW` / `VerQueryValueW` imports left unused by the KMDF check.
- Reporting: `DriverStatusReport` / `DriverVersion` / `DriverStatistics`,
  `create_driver_status_report`, `create_block_allocation_report_from_driver`,
  `format_driver_status` + `prepare_driver_stats_table`, and `report_driver_status` on the
  reporter trait.
- Docs: driver rows and claims removed from `TMR/CLAUDE.md`, `TMR-APP/CLAUDE.md` and
  `TMR-APP/AGENTS.md`; the settled-decision entry rewritten from "PARKED" to "PURGED" with the
  revival trigger. (The TMR-APP source-file map was recounted while in there — it had drifted
  ~5K LOC in both directions, e.g. `tests.rs` 7430→5452, `runner.rs` 2214→4393.)

**Deleted as already-dead** (found during the audit, driver-adjacent):
- `DmaConfig` + `for_testing` / `for_bandwidth_test` / `for_latency_test` — never constructed.
- `AllocationConfig::to_dma_config` — never called; its only callers would have been the above.
- `src/memory/allocator_BACKUP.rs` — orphan file, in no `mod` declaration.

**Two live bugs fixed during the purge** (these were not merely dead weight):
1. `auto_detect_backend` tried the **driver first**, so a stale or broken installed driver
   silently became the preferred backend. Now large-pages-then-regular on privilege.
2. `system_info_builder.rs` reported `huge_pages_available` as "is the driver handle open?" —
   so the report claimed **no 1GB pages on every machine**. 1GB pages come from VirtualAlloc2
   and gate on `SeLockMemoryPrivilege` exactly like 2MB, so it now derives from
   `check_large_page_privilege()`, worded as *permitted*: the privilege is necessary, not
   sufficient, since a 1GB page also needs 1GB of contiguous physical memory.

**One deliberate non-change**: `detect_runtime_capabilities` still returns
`MemoryBackend::NativeLargePages` unconditionally, exactly as the non-driver path always did.
`WindowsBackend` decides per allocation whether large pages are usable and falls back on its
own; selecting `NativeRegular` when the privilege is missing would convert that soft fallback
into a hard "Backend doesn't support large/huge pages but they are required" error.
`large_pages_available` is reported separately, for display only.

**Confirming the client was already rotted** — which is why this closed as "purge", not "sync":
`driver/mod.rs` exported `set_use_remap_all` / `RemapAllInput` / `BatchRemapInput` and
`interface.rs` reserved `_IOCTL_TMR_MAP_USER_SPACE` / `_IOCTL_TMR_SET_CPU_AFFINITY` /
`_IOCTL_TMR_UNMAP_USER_SPACE`, but `TMR-MD/src/ioctl.rs` implements **no remap, map, or
affinity IOCTL at all**. App and driver had already diverged.
