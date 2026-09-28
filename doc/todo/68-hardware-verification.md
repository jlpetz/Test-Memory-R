# TODO 68. [TMR-APP] Run-on-real-hardware verification of two landed-but-unexercised changes

> Full record, moved verbatim out of `TODO.md` on 2026-09-28 (open item).
> The short entry in `TODO.md` holds the current status; this file holds the reasoning.

**Priority**: Medium-High (both are already shipped in `main`'s lineage — if either is wrong, it is
wrong *now*, silently)
**Status**: **A DONE 2026-09-21** (and it found a real bug — see below). B still Not Started, and
**cannot be closed on the current machine**. Raised 2026-09-09.

Both items below were written, compiled and code-reviewed, but **never executed on real hardware**.
They are grouped because neither is a coding task — each needs a run and an eyeball, and the code
is already committed. Small, and closing it removes two "probably fine" assumptions.

**A) #63 WHEA monitoring — never run on real hardware** (implemented 2026-08-24, `src/whea.rs`).

> **DONE 2026-09-21 — and this is the item that justified the whole of #68.** Verified by
> **injecting synthetic events** rather than waiting for marginal hardware: this box (EC2
> r8i.2xlarge) has no UEFI EINJ/APEI, so `WheaHct.exe` and the WMI `WHEAErrorInjectionMethods`
> route are both impossible. `Write-EventLog -LogName System -Source 'Microsoft-Windows-WHEA-Logger'`
> is representative because real and synthetic events differ **only** in `<EventData>`, which TMR's
> parser never reads — it reads `<System>`'s `EventID` and `Level` only. Injection one-liner:
>
> ```powershell
> 1..10 | ForEach-Object { Write-EventLog -LogName System -Source 'Microsoft-Windows-WHEA-Logger' -EventID 47 -EntryType Warning -Message "TMR corrected probe $_"; Write-EventLog -LogName System -Source 'Microsoft-Windows-WHEA-Logger' -EventID 18 -EntryType Error -Message "TMR uncorrected probe $_"; Start-Sleep -Seconds 2 }
> ```
>
> **Detection was completely broken and had been since #63 landed** (2026-08-24 → 2026-09-21,
> ~4 weeks in `main`'s lineage). Runs reported `Hardware Errors (WHEA) ✅` with WHEA-Logger events
> provably written *inside the run window*, while `whea_monitored` reported `true`.
>
> **Root cause — one bug, not two: `drain()` never acknowledged the result set.** wevtapi's
> push-subscription contract is: the signal means *at least one* event is available (notifications
> are **coalesced**, not one per event); read with `EvtNext` until `ERROR_NO_MORE_ITEMS`; **that
> return is the acknowledgement that re-arms the signal.** `drain()` had `if count < BATCH { return; }`,
> so a short batch ended the drain without ever reaching `ERROR_NO_MORE_ITEMS`. The notification
> stayed outstanding forever → wevtapi never re-signalled → the gate in `poll()` never re-opened →
> the drain that would have re-armed the gate never ran. Self-locking, and structurally pinned the
> counter at zero after the first event.
>
> Visible in the instrumented log as: every `NO_MORE_ITEMS` arriving on the *following* poll's first
> `EvtNext` rather than as the end of a drain, and events retrieved with `signalled=false`
> (EventRecordID 209759, 209763) because the notification for them was never raised.
>
> `ERROR_INVALID_OPERATION` (4317 / `0x800710DD`) was the API *reporting* this — "you called
> `EvtNext` without a signal". It was initially mis-diagnosed as expected steady-state noise and
> classified as "nothing queued"; that was wrong, and normalising it would have hidden the cause.
>
> **Fix** (3 parts, all in `whea.rs`):
> 1. `drain()` reads to `ERROR_NO_MORE_ITEMS` — the partial-batch early return is gone.
> 2. The signal event is **manual-reset** (`CreateEventW(None, true, true, …)`) and `poll()` calls
>    `ResetEvent` **before** draining. Manual-reset because `poll()` has two callers (reporting
>    thread + coordinator at test boundaries) and an auto-reset event is consumed by whichever peeks
>    first, starving the other; reset-before-drain because an event arriving mid-drain must leave the
>    signal set for the next poll.
> 3. `ERROR_INVALID_OPERATION` now warns with a distinct "signal handling bug" message instead of
>    being folded into "empty". It should never appear; if it does, part 2 has regressed.
>
> Both invariants in part 1+2 are load-bearing *together* — either alone is still broken. Recorded
> in the `whea.rs` module docs and in a protocol note on `poll()`.
>
> **Re-verification owed**: checks 1-5 were confirmed end to end against an interim build that
> drained unconditionally (no gate). The *downstream* path verified there — parse, severity split,
> attribution, reporting, verdict — is untouched by the final fix, but the **transport** (parts 1+2)
> changed afterwards and needs one more injected-event run to confirm. Expect zero
> `ERROR_INVALID_OPERATION` warnings; their presence means part 2 regressed.
>
> **All five checks pass** on that interim build: per-test attribution
> (`3/3`, `1/1`, `3/0`, `2/2`), correct corrected split (ID 47 → `Level` 3 → corrected; ID 18 →
> `Level` 2 → uncorrected), conditional columns absent on the clean test and present on the dirty
> ones, live status line (`⚠️ WHEA: 9 (6 corrected)`), overview row (`9 (6 corrected, 67%)`), and a
> failing verdict. `format_message` resolves the real publisher template via
> `EvtOpenPublisherMetadata` (synthetic events leave `%2`/`%7` unresolved — cosmetic only, since the
> insertion strings a real event supplies are not part of TMR's parse).
>
> Check 1 needed **no code fix**: `whea_monitored` is only ever set from
> `progress.whea.is_active()`. The hardcoded `whea_monitored: true` in `converters.rs` was in a
> function with zero callers, deleted 2026-09-21 (see #69 item E).
>
> **Residual gap, deliberately left open**: `whea_monitored` still means "the subscription opened",
> not "the transport is known-good". That is exactly the distinction this bug hid for a month — a
> dead drain is indistinguishable from a quiet system. Tracked as a new item under #63.

What was checked, cheapest first:
1. **Subscription succeeds.** `EvtSubscribe` failing must be visible, not swallowed — if it errors,
   `whea_monitored` has to come out `false`. A silent failure reporting `whea_total: 0` is exactly
   the "clean bill of health it did not earn" case the flag exists to prevent.
2. **Healthy run is visually unchanged.** The WHEA columns are conditional, so a clean run should
   look identical to pre-#63 output. Any always-on empty column is a bug.
3. **`whea_monitored: true` reaches the result file**, now that #67 removed `serde(default)` (so a
   missing field fails the load loudly rather than reading as `false`).
4. **An actual event is observed end to end.** The hard part — needs a marginal overclock, or some
   other way to make WHEA-Logger fire. Until this is done, the *detection* path is unproven even if
   1-3 pass. Note the subscription is almost certainly future-events-only, so pre-existing
   WHEA-Logger entries in the event log will **not** appear; don't read their absence as a pass.
5. **Any WHEA event fails the run** (the verdict now derives from `progress.total_errors` after the
   shadowed-`success` fix) — confirm with 4.

**B) #66's `Vec<u8>` → `Vec<u64>` alignment fixes — never exercised** (3 sites in
`cpu_topology.rs`, commit 43db10b).

`SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX` / `SYSTEM_CPU_SET_INFORMATION` contain `KAFFINITY`
(u64), but the backing buffers were `Vec<u8>` (1-byte alignment guarantee). Fixed by allocating
`vec![0u64; …div_ceil(8)]` and keeping the walk in bytes via an explicit `*const u8` base.

- Run `--debug-topology` and diff against known-good output: physical/logical core counts, P/E core
  classification, NUMA nodes, cache sizes.
- **The alignment matters most on multi-processor-group machines** (>64 logical processors), where
  `GroupMask` is walked for more than one group. On a single-group box the code path is exercised
  but the misalignment was harmless, so a pass here is necessary, not sufficient.
- **Blocked on hardware as of 2026-09-21**: the current dev box is 4C/8T, single processor group,
  i.e. precisely the "necessary, not sufficient" case. Unlike A, there is no injection trick here —
  processor-group count is not fakeable from user mode. Needs a >64-logical-processor machine.
- Same trap as the `Vec<u16>`/`PWSTR` one in `whea.rs`, which is worth re-reading while here.

**Not included**: #67's own verification (fresh result file + `--compare-results` against a new
baseline) — being done as part of landing #67, not tracked here.
