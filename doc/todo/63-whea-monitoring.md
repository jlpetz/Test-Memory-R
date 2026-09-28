# TODO 63. [TMR-APP] WHEA Error Monitoring (machine-check visibility)

> Full record, moved verbatim out of `TODO.md` on 2026-09-28 (open item).
> The short entry in `TODO.md` holds the current status; this file holds the reasoning.

**Priority**: Medium-High (closes a real detection blind spot)
**Status**: **IMPLEMENTED 2026-08-24; detection path repaired and verified 2026-09-21.** As shipped,
`poll()` gated `EvtNext` on the subscription signal event and the gate never opened, so **no WHEA
event was ever counted for ~4 weeks** while `whea_monitored` reported `true`. Found and fixed under
#68 item A using synthetic `Write-EventLog` injection — full diagnosis, the injection one-liner, and
the five verified checks are recorded there. Two corrections to the design notes below:

- The `WaitForSingleObject` gate described under *Transport* **stays**, but the event is now
  **manual-reset** and `poll()` calls `ResetEvent` before draining. Auto-reset was wrong because
  `poll()` has two callers and a successful peek consumes the signal, starving one of them.
- **`drain()` must read to `ERROR_NO_MORE_ITEMS`.** That return is wevtapi's acknowledgement and is
  what re-arms the signal; the original partial-batch early return left the notification outstanding
  forever and is what broke detection. This is the single most important invariant in the file.
- **Every bounded exit from `drain()` re-sets the signal by hand** (`DrainEnd::Unfinished` →
  `SetEvent` in `poll`). `MAX_BATCHES_PER_DRAIN = 64` (1024 events) exists so a flood cannot hold the
  lock indefinitely — but falling out of that loop is *the same unacknowledged-result-set state* as
  the original bug, so on its own the cap was a dormant second instance of it, needing only a
  1024-event burst to fire. A flapping PCIe link or a failing DIMM produces exactly that, i.e. it
  would have re-broken detection precisely when detection mattered. Same treatment for an unexpected
  `EvtNext` error, which also makes a transient failure self-healing rather than terminal.
- `poll()` now takes the mutex with `lock()`, not `try_lock()`. `try_lock` let the coordinator's
  test-boundary poll silently do nothing if the reporting thread happened to be mid-drain — dropping
  exactly the boundary precision that poll exists to provide. Neither caller is on a pinned hot path,
  so blocking for a sub-millisecond drain is free.
- `ERROR_INVALID_OPERATION` (4317) means *we* called `EvtNext` without a signal, and now warns
  distinctly. It should never appear.

**Report visibility reworked 2026-09-23.** A WHEA-only failure used to print a failing run verdict
over a per-test report reading "0 errors" and a thread table of all-green ✅ — the counts existed but
never reached the tables. Now:

- **Per-test topline** carries a verdict glyph and both error sources:
  `📊 Test report - Cycle 1 - Mem-StuckBit512: ❌ 3.3s, 0 errors + 2 WHEA (2C/0UC), …`. WHEA is shown
  even at zero (so a clean line states that we were watching); the `(xC/yUC)` split only appears when
  non-zero. This is the **only** per-test PASS/FAIL in the output — `success` in
  `execute_test_cycle` is the `ErrorMode::Halt` trigger, not a verdict.
- **A dedicated `Pass` column** (✅/❌) on the thread-timing, per-thread, per-CPU, per-core and
  final per-test tables; every error cell is now a plain number. The old convention rendered *zero*
  as ✅ and non-zero as a bare number, so failures had **less** visual weight than passes and ❌
  never appeared anywhere. One column owns the glyph, so ✅/❌ means pass/fail everywhere and cannot
  be confused with the 🔴/🟢 in `cycles_info`, which marks *which limit ended the test*.
- **WHEA is its own column, never summed into `Errors`** (superseded the 09-23 `0 +2 WHEA`
  aggregate cell on 09-26, see below): the events are system-wide and not attributable to a
  thread/CPU/core (the XML's `Execution ProcessID` is the logging service), so a combined total
  above a column of per-row zeros would be arithmetic that visibly does not add up. Side benefit of
  putting `Pass` last: the emoji's display width vs `str::chars()` mismatch can no longer misalign a
  following column.
- **No footer for the corrected/uncorrected split.** One was written and removed the same day: the
  split is already in the per-test topline (`0 errors + 3 WHEA (0C/3UC)`), and the three breakdown
  tables each printed an identical "system-wide, not attributable" line, so it was duplication in
  the place with the least room for it. The app should be data-rich but compact, and a reader who
  needs the shorthand explained can read the manual.
- **Final Overview** leads with a `Result  PASS ✅ / FAIL ❌` row, then `Total Errors` and
  `Total WHEA` (the latter in the one WHEA form below). `not monitored` is still distinct from `0`
  there.
- `results.rs` needed no change — per-test, per-aggregate and run-level WHEA counts were already
  persisted.

**One form per quantity, 2026-09-26.** The 09-23 pass left WHEA in five different shapes (`0 +2
WHEA` aggregate cell, `WHEA`+`WHEA Corr` columns, `N (x% corrected)` overview, `(xC/yUC)` topline,
a separate end-of-run tally) and data-verify errors under three names (`data`, `Err`, `Errors`).
Now:

- **One WHEA rendering**, `N (xC/yUC)` or plain `0`, from `impl Display for WheaCounts` (+
  `split_suffix()` for prose, where the label sits between count and split: `3 WHEA (0C/3UC)`).
  Used by the live progress line, topline, verdict line, log line, every table and the overview.
  `corrected_percent()` is gone. Test: `whea::tests::display_form`.
- **One column** in every table (thread timing, per-thread/CPU/core, final per-test, cycle report),
  appended by the shared `with_verdict_headers`/`push_verdict_cells` so the tail
  `Errors | WHEA | Pass` cannot drift. Gated on a non-zero run total, so a clean run has no WHEA
  column at all. Breakdown rows get a **blank** WHEA cell (not attributable), the Avg row carries
  the count. The four breakdown structs now carry `whea: WheaCounts`, not `whea_total`.
- **"errors" everywhere** for data-verify failures: `Errors` header, `N errors + M WHEA` on the
  topline and the (now single-branch) suite-failure line. "data" collided with the `Data` column.
- **Final per-test table** headers aligned with the cycle report (`Time`/`Data`/`Speed`); footer
  states which columns are per-cycle averages and which are sums. `TestSummaryEntry.total_data_gib`
  renamed `average_data_gib` — it was always an average; the name was wrong.
- **End-of-run WHEA tally removed** — the `Total WHEA` overview row prints it a screen later.
- **Cycle report brought in step** with the final table but still not called (#69 F).

**Adjacent, not fixed:** the `Error Summary by Test:` block `progress.rs` prints on completion
repeats the final per-test table's `Errors` column (HashMap order, so not even sorted). Candidate for
removal under the same no-repeat rule; left for a decision.

**Description storage removed 2026-09-23.** `recorded: Mutex<Vec<String>>` + `recorded()` are gone,
along with the end-of-run loop in `progress.rs` that reprinted every event after they had already
been shown live. The end of a run now prints only events that arrived after the last drain (the
tally line went too, 09-26). `pending` **stays** — it is the
live console feed (and the subscription point a GUI log pane would use); it is drained and emptied on
each display tick, so no description outlives its own print. `MAX_RECORDED` became `MAX_DESCRIBED`
(same value, 32) because the cap now bounds *output* rather than storage: it stops a flapping link
flooding the console and log, and suppressed events still show up as per-test WHEA counts.

**Minor, not fixed:** `read_record()` calls `render_xml` + `EvtFormatMessage` for every drained
event, including those past `MAX_DESCRIBED = 32` whose description is then discarded. Correctness is
unaffected (they still count); it is wasted work on exactly the flood path. Cheap fix is to check the
running total before rendering.

**Still open — `whea_monitored` overstates what it knows.** It means "the subscription opened", not
"the transport is known-good", so a dead drain remains indistinguishable from a quiet system. That
is precisely the failure mode above, and the flag did not catch it: it read `true` throughout. The
hard part is that there is no positive liveness signal on a healthy box — zero events is the correct
observation almost always. Options, cheapest first: count consecutive unexpected `EvtNext` errors and
degrade the flag; or record the raw drain-outcome tally in the result file so a post-hoc read can
tell "never drained" from "drained clean". Not yet designed; do not close #63 as fully done until
this is decided.

**Why this matters — the blind spot is structural, not a bug in our tests.** DDR5 has **on-die
ECC**, which silently corrects single-bit errors inside the DRAM die. TMR's verify reads therefore
see **correct data** for a fault that genuinely occurred: the memory is marginal, the overclock is
unstable, and every pattern test passes. On-die ECC is mandatory in the DDR5 spec, so this affects
every target system TMR runs on — it is not an edge case. The second half of the gap is timing:
an uncorrectable error can land during a **bandwidth or latency** test, i.e. a phase that performs
no verification at all, so nothing in TMR would ever notice.

Windows logs these under provider `Microsoft-Windows-WHEA-Logger` on the **System** channel. That
is the **only** user-mode visibility into corrected errors — no CPUID or MSR read is available to
us without a driver (TMR-MD is shelved and its client code purged; this does not justify
reviving it).

**What was built**

- **Transport: `EvtSubscribe` in the signal-event-handle form**, not the callback form and not
  polling. The subscription sets a Win32 event; the existing ~2s `progress_reporter` thread does a
  zero-timeout `WaitForSingleObject` on each 500ms tick and only calls into wevtapi when that says
  something is queued. Rationale:
  - Polling (`EvtQuery`) costs a log scan whether or not anything happened, and pays for every
    other provider's traffic in the System log. Subscription cost scales with *our* event rate,
    which on a stable system is zero.
  - The **callback** form would have wevtapi run our code on a threadpool thread of *its* choosing
    — on a fully-subscribed box that means a wevtapi thread landing on a core pinned to a test
    worker. The handle form keeps everything on the reporting thread.
- **Filter is by provider, no Event ID list.** The ID→meaning mapping varies across Windows builds
  and platforms, so IDs are reported numerically for lookup rather than interpreted. Catching every
  hardware WHEA event (CPU, cache, memory, PCIe, platform) is deliberate: any hardware error during
  a memory-overclock run is evidence the overclock is unstable, and the *source* is what
  distinguishes a marginal DIMM from a marginal NIC.
- **Severity split** on the event's own `<Level>`: Critical(1)/Error(2) → uncorrected,
  Warning(3)/Information(4)/Verbose(5) → corrected. A proxy for the payload's `ErrorSeverity`,
  which would require decoding the binary `WHEA_ERROR_RECORD`; `Level` is what WHEA-Logger derives
  *from* it. Unparsable events default to uncorrected — the safe direction for a stability verdict.
- **Rendering**: `EvtRenderEventXml` + narrow string extraction for `EventID`/`Level` (cold path,
  minimises unsafe FFI surface vs `EvtRenderEventValues` + `EVT_VARIANT` unions), plus
  `EvtFormatMessage` for the human-readable text Event Viewer shows — that is where the component /
  source lives. **Trap handled**: `EvtFormatMessage` returns *failure*
  (`ERROR_EVT_UNRESOLVED_VALUE_INSERT` and friends) for messages whose data values it cannot
  substitute — common for WHEA — while still filling the buffer. Treating those as fatal would have
  discarded the source text for exactly the events that matter most.
- **Reporting**, per the requested shape (`10 WHEA, 5 corrected → "10 (5 corrected, 50%)"`):
  - New `TestSummary.whea_total` / `.whea_corrected` (per-test deltas), carried through
    `TestResult` → `TestAverage` → `OverallStats` (all `#[serde(default)]` so `--compare-results`
    still reads pre-WHEA baselines) → `TestSummaryEntry` → the console tables.
  - Per-test and per-cycle tables gain **WHEA / WHEA Corr** columns, and the overview a
    **Hardware Errors (WHEA)** row — all **conditional**, following the existing `has_latency`
    pattern, so a healthy run's output is byte-identical to before.
  - Counts, never averages: a hardware error is an event, and averaging over cycles would dilute a
    single-cycle fault into "0.3 errors".
  - Live progress line gains `| ⚠️ WHEA: N (M corrected)` once non-zero; individual events are
    cached and flushed at the 2s display tick (never inline), with the full XML at `debug` and the
    formatted message at `warn`/`error` in `./logs`.
  - `OverallStats.whea_monitored` (also `#[serde(default)]`, defaulting to `false` so pre-WHEA
    baselines read as *not* monitored) disambiguates the zero: without it, a saved
    `whea_total: 0` looks identical whether the run was clean or the subscription never opened.
    The overview row prints **`not monitored`** in that case rather than `✅`. Captured from
    `is_active()` *before* `stop()`, which tears the subscription down.
- **Verdict**: any WHEA event fails the run, with a distinct message for the "no data mismatch but
  the platform logged a fault" case. Deliberately **not** wired into `ErrorMode::Halt` — WHEA
  attribution is asynchronous, so tearing down mid-test on it would be acting on a fuzzy signal.
- **Graceful degradation**: if the subscription cannot be created (log ACL, wevtapi unavailable)
  the monitor stays inert, every method is a no-op returning zeros, and startup prints the reason.
  Testing never depends on WHEA being available.
- **Shutdown / interrupt path**: CTRL+C only sets `SHUTDOWN_REQUESTED`; the cycle loop breaks and
  falls through the *same* tail as a normal finish, so an interrupted run shuts the monitor down
  identically. Handle release does not depend on `stop()` being reached at all — `Subscription`'s
  `Drop` (2× `EvtClose` + `CloseHandle`) runs when the `Arc<ProgressTracker>` drops, which covers
  the two early `return false` paths (allocation failure, empty test filter; both before the
  reporter thread is spawned, so the `Arc` is uniquely owned there).
  **Ordering fixed**: `stop()` takes a final drain, and the window it covers is real work — the
  reporter thread stops polling at its `join()`, and freeing tens of GiB of large pages happens
  after that. The original code snapshotted `counts()` *before* `stop()`, silently discarding
  whatever that last drain found — on an interrupted run, the entire tail of the run. Now it is
  `is_active()` → `stop()` → `counts()`, and any description the last drain produced is flushed to
  the console there too (the reporter has already exited by then).

**Attribution caveat (documented in the module, not fixable from user mode)**: the OS logs WHEA
asynchronously (kernel WHEA → ETW → EventLog service), so a per-test delta is approximate at test
boundaries no matter how often we drain. The **run-level** total comes from the monitor's own
cumulative counter and is exact — which is why `OverallStats` is set from that counter rather than
summed from the per-test figures.

**Fixed along the way**: the outer `success` flag in `run_tests_with_layout_and_timing_filtered`
was shadowed by a same-named local inside `execute_test_cycle`, so thread-reported memory errors
**never reached the final verdict** — `final_success` was effectively always `true` and
"Test suite failed due to memory errors" was unreachable. The verdict now derives from
`progress.total_errors` (the accurate aggregate, summed from every thread in `complete_test`) plus
the WHEA total.

**Possible follow-ups** (not required):
- Decode `WHEA_ERROR_RECORD` from the event's user data for exact `ErrorSeverity` and DIMM
  identification, instead of inferring severity from `Level`.
- Feed events into #29's transcript with timestamps for time-correlation.
- A CLI switch to disable monitoring (currently always on when available).
