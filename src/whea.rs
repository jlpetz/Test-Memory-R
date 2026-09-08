//! WHEA (Windows Hardware Error Architecture) monitoring — TODO #63.
//!
//! # Why TMR needs this
//!
//! DDR5 has on-die ECC. A single-bit fault inside the DRAM array is corrected *before* the data
//! leaves the DIMM, so a verify read sees the correct value and TMR's accumulator never trips —
//! the error happened, and our test is blind to it. Platforms with link/ECC reporting log those
//! corrections to WHEA. Uncorrected errors matter even more: they can fire while a *bandwidth* or
//! *latency* test is running, i.e. during a phase that does no verification at all.
//!
//! `Microsoft-Windows-WHEA-Logger` on the `System` channel is the only user-mode visibility into
//! this, so TMR subscribes to it for the duration of a run and counts what arrives. We deliberately
//! take **every** hardware WHEA event (CPU, memory, cache, PCIe, platform) rather than filtering to
//! memory-only: any hardware error during a memory-overclock run is evidence the overclock is not
//! stable, and the *source* is exactly what the operator needs to see to tell a marginal DIMM from
//! a marginal NIC. The formatted message (same text Event Viewer shows) carries that source, so it
//! is recorded verbatim rather than reduced to a count.
//!
//! # Transport: subscribe, not poll
//!
//! `EvtSubscribe` in its **signal-event-handle** form (not the callback form) — wevtapi sets a
//! Win32 event when something matching the query is queued for us. Rationale:
//!
//! * A poll (`EvtQuery` every N seconds) costs a log scan *whether or not* anything happened, and
//!   pays for every other provider's traffic in the System log. Subscription cost scales with
//!   **our** event rate, which on a stable system is zero.
//! * The callback form would have wevtapi run our code on a **threadpool thread of its choosing**,
//!   which on a fully-subscribed box means a wevtapi thread landing on a core pinned to a test
//!   worker. The signal-handle form keeps everything on the existing reporting thread: it does a
//!   `WaitForSingleObject(handle, 0)` — a local, non-blocking check that costs nothing when the
//!   system is healthy — and only calls into wevtapi when that says there is something to fetch.
//!
//! Nothing here runs on the hot path: the reporting thread drains, and the coordinator reads
//! cumulative counters at test boundaries.
//!
//! # Severity classification
//!
//! `corrected` vs `uncorrected` comes from the event's own `Level` field: Critical(1)/Error(2) are
//! uncorrected or fatal, Warning(3)/Information(4)/Verbose(5) are corrected or informational. This
//! is a proxy — the authoritative value is `ErrorSeverity` inside the `WHEA_ERROR_RECORD` payload
//! — but reaching that means decoding the binary error record, and `Level` is what WHEA-Logger
//! derives *from* it. Event IDs are deliberately **not** used to classify: the ID→meaning mapping
//! varies across Windows builds and platforms, so the ID is reported raw for lookup instead.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use windows::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_EVT_MAX_INSERTS_REACHED,
    ERROR_EVT_UNRESOLVED_PARAMETER_INSERT, ERROR_EVT_UNRESOLVED_VALUE_INSERT,
    ERROR_INSUFFICIENT_BUFFER, ERROR_NO_MORE_ITEMS, ERROR_TIMEOUT, HANDLE, WAIT_OBJECT_0,
    WIN32_ERROR,
};
use windows::Win32::System::EventLog::{
    EVT_HANDLE, EvtClose, EvtFormatMessage, EvtFormatMessageEvent, EvtNext, EvtOpenPublisherMetadata,
    EvtRender, EvtRenderEventXml, EvtSubscribe, EvtSubscribeToFutureEvents,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::core::{HRESULT, PCWSTR};

/// True when a `windows` error carries the given Win32 status code.
///
/// The crate reports Win32 failures as `HRESULT`s (`HRESULT_FROM_WIN32`), so comparing against a
/// bare `WIN32_ERROR` needs the same wrapping — masking the low bits instead would collide with
/// genuine COM facility codes.
fn is_win32_error(err: &windows::core::Error, code: WIN32_ERROR) -> bool {
    err.code() == HRESULT::from_win32(code.0)
}

/// Provider that carries all hardware WHEA reports (CPU, memory, cache, PCIe, platform).
const WHEA_PROVIDER: &str = "Microsoft-Windows-WHEA-Logger";

/// Channel the provider writes to.
const WHEA_CHANNEL: &str = "System";

/// XPath filter: every event from the WHEA provider, no Event ID restriction (see module docs on
/// why we do not filter by ID).
const WHEA_QUERY: &str = "*[System[Provider[@Name='Microsoft-Windows-WHEA-Logger']]]";

/// Handles fetched per `EvtNext` call.
const BATCH: usize = 16;

/// How many event descriptions we keep verbatim for the final report. A run that produces more
/// than this has already answered the question ("not stable"), so the rest are counted only.
const MAX_RECORDED: usize = 32;

/// Guard against spinning if wevtapi keeps re-signalling while we drain.
const MAX_DRAIN_ROUNDS: u32 = 8;

/// Cap on `EvtNext` batches per drain, so a machine emitting events as fast as we read them cannot
/// hold the reporting thread indefinitely. Anything left over is picked up on the next tick.
const MAX_BATCHES_PER_DRAIN: u32 = 64;

/// Cumulative WHEA counts. Monotonic for the life of the run, so a per-test figure is the
/// difference between two snapshots.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WheaCounts {
    /// Every hardware WHEA event seen, corrected or not.
    pub total: u64,
    /// Subset that the OS reported as corrected (see module docs on classification).
    pub corrected: u64,
}

impl WheaCounts {
    /// Events the OS did *not* report as corrected — the ones that indicate data actually moved
    /// wrong or the platform gave up.
    pub fn uncorrected(&self) -> u64 {
        self.total.saturating_sub(self.corrected)
    }

    /// Corrected share as a percentage, or `None` when nothing was seen.
    pub fn corrected_percent(&self) -> Option<f64> {
        if self.total == 0 {
            None
        } else {
            Some(self.corrected as f64 * 100.0 / self.total as f64)
        }
    }

    /// Difference between two snapshots (`self` later than `earlier`).
    pub fn since(&self, earlier: &WheaCounts) -> WheaCounts {
        WheaCounts {
            total: self.total.saturating_sub(earlier.total),
            corrected: self.corrected.saturating_sub(earlier.corrected),
        }
    }
}

/// Live subscription state. Every field is a Win32/wevtapi handle, so all access is funnelled
/// through the owning `Mutex` in `WheaMonitor`.
struct Subscription {
    /// Auto-reset event wevtapi signals when matching events are queued.
    signal: HANDLE,
    /// The subscription itself; `EvtNext` pulls from this.
    subscription: EVT_HANDLE,
    /// Publisher metadata for `EvtFormatMessage`. `None` if the provider manifest could not be
    /// opened — counts still work, only the human-readable text is lost.
    metadata: Option<EVT_HANDLE>,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        // SAFETY: each handle was produced by the matching Win32/wevtapi call in `start()` and is
        // closed exactly once — `Subscription` is not `Clone` and lives in a `Mutex<Option<_>>`
        // that is only ever `take()`n or dropped.
        unsafe {
            if let Some(metadata) = self.metadata.take() {
                let _ = EvtClose(metadata);
            }
            let _ = EvtClose(self.subscription);
            let _ = CloseHandle(self.signal);
        }
    }
}

// SAFETY: `HANDLE` is `*mut c_void`, which is why this is not derived. Neither handle is
// thread-affine: a Win32 event object and a wevtapi subscription are process-wide kernel/service
// objects usable from any thread. `Subscription` is only ever reached through
// `WheaMonitor::sub: Mutex<Option<Subscription>>`, so at most one thread touches these handles at
// a time — which also satisfies wevtapi, whose `EvtNext` is not documented as safe for concurrent
// calls on the same result-set handle. Do not move these fields out of the `Mutex`.
unsafe impl Send for Subscription {}

/// One drained WHEA event, in the form we keep it.
struct WheaRecord {
    event_id: u32,
    level: u32,
    corrected: bool,
    /// Formatted provider message (what Event Viewer shows), whitespace-collapsed. Empty if
    /// `EvtFormatMessage` was unavailable.
    message: String,
}

impl WheaRecord {
    /// One-line form for the console and the saved report.
    fn describe(&self) -> String {
        let severity = if self.corrected { "corrected" } else { "UNCORRECTED" };
        let level = level_name(self.level);
        if self.message.is_empty() {
            format!("WHEA [{}] id={} level={} ({})", severity, self.event_id, self.level, level)
        } else {
            format!("WHEA [{}] id={} {}: {}", severity, self.event_id, level, self.message)
        }
    }
}

/// Standard Windows event levels.
fn level_name(level: u32) -> &'static str {
    match level {
        0 => "LogAlways",
        1 => "Critical",
        2 => "Error",
        3 => "Warning",
        4 => "Information",
        5 => "Verbose",
        _ => "Unknown",
    }
}

/// Subscribes to hardware WHEA events for the duration of a run and counts what arrives.
///
/// Starts inactive: call [`WheaMonitor::start`] once at run setup. If the subscription cannot be
/// created (no permission to read the System log, wevtapi unavailable) the monitor stays inactive
/// and every other method is a no-op returning zeros — WHEA monitoring is an addition to error
/// detection, never a precondition for testing.
pub struct WheaMonitor {
    sub: Mutex<Option<Subscription>>,
    total: AtomicU64,
    corrected: AtomicU64,
    active: AtomicBool,
    /// Set once we have logged an unexpected drain error, so a persistent failure cannot spam the
    /// log every poll.
    drain_error_logged: AtomicBool,
    /// Descriptions kept for the final report, capped at `MAX_RECORDED`.
    recorded: Mutex<Vec<String>>,
    /// Descriptions not yet shown on the console. The reporting thread takes these at its own
    /// display interval so a burst of events cannot flood the progress line.
    pending: Mutex<Vec<String>>,
}

impl Default for WheaMonitor {
    fn default() -> Self {
        Self::new()
    }
}

impl WheaMonitor {
    /// Creates an inactive monitor. No Win32 calls happen until [`start`](Self::start).
    pub fn new() -> Self {
        Self {
            sub: Mutex::new(None),
            total: AtomicU64::new(0),
            corrected: AtomicU64::new(0),
            active: AtomicBool::new(false),
            drain_error_logged: AtomicBool::new(false),
            recorded: Mutex::new(Vec::new()),
            pending: Mutex::new(Vec::new()),
        }
    }

    /// Subscribes to future WHEA events. Idempotent; safe to call when already active.
    ///
    /// Returns `Err` with a human-readable reason if monitoring is unavailable. Callers should
    /// report that and carry on — a failure here must never abort a test run.
    pub fn start(&self) -> Result<(), String> {
        let mut guard = self.sub.lock().map_err(|_| "WHEA monitor lock poisoned".to_string())?;
        if guard.is_some() {
            return Ok(());
        }

        match open_subscription() {
            Ok(subscription) => {
                *guard = Some(subscription);
                self.active.store(true, Ordering::Relaxed);
                log::info!("WHEA monitoring active (provider {}, channel {})", WHEA_PROVIDER, WHEA_CHANNEL);
                Ok(())
            }
            Err(reason) => {
                log::warn!("WHEA monitoring unavailable: {}", reason);
                Err(reason)
            }
        }
    }

    /// True once [`start`](Self::start) has succeeded.
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Relaxed)
    }

    /// Drains anything wevtapi has queued and folds it into the counters.
    ///
    /// Cheap to call often: when nothing has happened this is a single zero-timeout wait on a
    /// local event handle and no wevtapi round-trip at all. Safe from multiple threads — the
    /// handles are serialised by the internal `Mutex`, and a contended call simply returns
    /// (someone else is already draining).
    pub fn poll(&self) {
        if !self.is_active() {
            return;
        }

        // try_lock, not lock: the coordinator calls this at test boundaries while the reporting
        // thread calls it on its own cadence. If the other one is mid-drain there is nothing to
        // wait for — its work updates the same counters we would have updated.
        let Ok(mut guard) = self.sub.try_lock() else {
            return;
        };
        let Some(subscription) = guard.as_mut() else {
            return;
        };

        let mut rounds = 0;
        loop {
            // SAFETY: `signal` is a live auto-reset event owned by `subscription`; a zero timeout
            // makes this a non-blocking check that never waits on a pinned core.
            let signalled = unsafe { WaitForSingleObject(subscription.signal, 0) } == WAIT_OBJECT_0;
            if !signalled {
                break;
            }

            self.drain(subscription);

            rounds += 1;
            if rounds >= MAX_DRAIN_ROUNDS {
                // wevtapi keeps re-signalling (events arriving as fast as we read them). Leave the
                // rest for the next poll rather than spinning here.
                break;
            }
        }
    }

    /// Pulls every queued event out of the subscription and records it.
    fn drain(&self, subscription: &mut Subscription) {
        for _ in 0..MAX_BATCHES_PER_DRAIN {
            let mut handles = [0isize; BATCH];
            let mut returned = 0u32;

            // SAFETY: `handles` and `returned` are locals passed directly as call arguments, so
            // they outlive the call (see the FFI lifetime rule in CLAUDE.md). A zero timeout means
            // "return what is queued now".
            let result = unsafe { EvtNext(subscription.subscription, &mut handles, 0, 0, &mut returned) };

            if let Err(e) = result {
                // Both of these just mean "nothing queued", which is the normal exit from a drain.
                let empty = is_win32_error(&e, ERROR_NO_MORE_ITEMS) || is_win32_error(&e, ERROR_TIMEOUT);
                if !empty && !self.drain_error_logged.swap(true, Ordering::Relaxed) {
                    log::warn!("WHEA drain failed, counts may be incomplete: {}", e);
                }
                return;
            }

            let count = (returned as usize).min(BATCH);
            for &raw in &handles[..count] {
                let event = EVT_HANDLE(raw);
                let record = read_record(event, subscription.metadata);
                self.record(record);

                // SAFETY: `event` came from `EvtNext` above, is used only in this iteration, and
                // is closed exactly once here.
                unsafe {
                    let _ = EvtClose(event);
                }
            }

            if count < BATCH {
                return;
            }
        }
    }

    /// Folds one event into the counters and the description caches.
    fn record(&self, record: WheaRecord) {
        let seq = self.total.fetch_add(1, Ordering::Relaxed) + 1;
        if record.corrected {
            self.corrected.fetch_add(1, Ordering::Relaxed);
        }

        // Past the cap, keep counting but stop describing. A run this far gone has already answered
        // the question, and an unbounded description list would let a flapping link flood both the
        // console and the log file.
        if seq > MAX_RECORDED as u64 {
            if seq == MAX_RECORDED as u64 + 1 {
                log::warn!(
                    "WHEA: over {} events this run; further descriptions suppressed (counts continue)",
                    MAX_RECORDED
                );
            }
            return;
        }

        let description = record.describe();

        // Straight to the log file as well as the console queue — a run left unattended overnight
        // should still be diagnosable from ./logs.
        if record.corrected {
            log::warn!("{}", description);
        } else {
            log::error!("{}", description);
        }

        if let Ok(mut recorded) = self.recorded.lock() {
            recorded.push(description.clone());
        }
        if let Ok(mut pending) = self.pending.lock() {
            pending.push(description);
        }
    }

    /// Cumulative counts for the run so far.
    pub fn counts(&self) -> WheaCounts {
        WheaCounts {
            total: self.total.load(Ordering::Relaxed),
            corrected: self.corrected.load(Ordering::Relaxed),
        }
    }

    /// Removes and returns descriptions not yet shown on the console.
    pub fn take_pending(&self) -> Vec<String> {
        self.pending.lock().map(|mut p| std::mem::take(&mut *p)).unwrap_or_default()
    }

    /// Every description kept for the final report (capped at `MAX_RECORDED`).
    pub fn recorded(&self) -> Vec<String> {
        self.recorded.lock().map(|r| r.clone()).unwrap_or_default()
    }

    /// Drains a last time and closes the subscription.
    ///
    /// Call this at end of run, *before* reading the final [`counts`](Self::counts) — the final
    /// drain can raise them. Not required for cleanup: dropping the monitor closes the handles
    /// either way (see the note below the impl), so the early-return paths in the runner leak
    /// nothing; they just skip this last drain, and report nothing anyway.
    pub fn stop(&self) {
        if !self.is_active() {
            return;
        }
        self.poll();
        self.active.store(false, Ordering::Relaxed);
        if let Ok(mut guard) = self.sub.lock() {
            *guard = None; // Subscription::drop closes the handles
        }
    }
}

// No `Drop` impl needed: dropping the `Mutex<Option<Subscription>>` runs `Subscription::drop`,
// which closes both handles. `stop()` exists only to do that earlier, and to take a final drain.

/// Creates the signal event, the subscription, and (best effort) the publisher metadata handle.
fn open_subscription() -> Result<Subscription, String> {
    // LIFETIME: these UTF-16 buffers must outlive the `EvtSubscribe`/`EvtOpenPublisherMetadata`
    // calls that read through the `PCWSTR`s below. They are function-scope locals and the pointers
    // are passed *directly* as call arguments, never stored — the safe form documented in
    // CLAUDE.md's FFI rule.
    let channel = to_wide(WHEA_CHANNEL);
    let query = to_wide(WHEA_QUERY);
    let provider = to_wide(WHEA_PROVIDER);

    // SAFETY: all arguments are locals or null; the returned handles are owned by the
    // `Subscription` built below, which closes them in `Drop`.
    unsafe {
        // Auto-reset (manual_reset = false): the wait consumes the signal, then we drain until the
        // queue is empty. Events arriving mid-drain re-signal it, which the retry loop in `poll`
        // picks up, so nothing is lost.
        let signal = CreateEventW(None, false, false, PCWSTR::null())
            .map_err(|e| format!("CreateEventW failed: {}", e))?;

        let subscription = match EvtSubscribe(
            None,                                 // local session
            Some(signal),                         // signal-event form, not the callback form
            PCWSTR::from_raw(channel.as_ptr()),   // channel: System
            PCWSTR::from_raw(query.as_ptr()),     // XPath filter on the WHEA provider
            None,                                 // no bookmark
            None,                                 // no callback context
            None,                                 // no callback
            EvtSubscribeToFutureEvents.0,         // only events from now on
        ) {
            Ok(handle) => handle,
            Err(e) => {
                let _ = CloseHandle(signal);
                // Access denied here means the process cannot read the System log — say so rather
                // than dumping a raw code, since it is actionable.
                let reason = if is_win32_error(&e, ERROR_ACCESS_DENIED) {
                    "access denied reading the System event log (run elevated, or join Event Log Readers)".to_string()
                } else {
                    format!("EvtSubscribe failed: {}", e)
                };
                return Err(reason);
            }
        };

        // Best effort: without this we still count events, we just cannot name their source.
        let metadata = EvtOpenPublisherMetadata(None, PCWSTR::from_raw(provider.as_ptr()), PCWSTR::null(), 0, 0)
            .inspect_err(|e| log::debug!("WHEA publisher metadata unavailable, messages will be omitted: {}", e))
            .ok();

        Ok(Subscription { signal, subscription, metadata })
    }
}

/// Reads one event into a `WheaRecord`. Never fails: a WHEA event we cannot parse still counts.
fn read_record(event: EVT_HANDLE, metadata: Option<EVT_HANDLE>) -> WheaRecord {
    let xml = render_xml(event).unwrap_or_default();
    let event_id = element_u32(&xml, "EventID").unwrap_or(0);
    // Default to level 2 (Error) when the field is missing: treating an unparsable hardware error
    // as uncorrected is the safe direction for a stability verdict.
    let level = element_u32(&xml, "Level").unwrap_or(2);

    let message = metadata.and_then(|m| format_message(m, event)).unwrap_or_default();

    if !xml.is_empty() {
        if message.is_empty() {
            // No formatted text available, so the XML is the only record of *what* failed — that
            // has to reach the log at a level the user actually sees.
            log::warn!("WHEA raw event XML (no formatted message available): {}", xml);
        } else {
            log::debug!("WHEA raw event XML: {}", xml);
        }
    }

    WheaRecord { event_id, level, corrected: level >= 3, message }
}

/// Renders an event as XML. Two-phase: probe for the size, then fill.
fn render_xml(event: EVT_HANDLE) -> Option<String> {
    let mut used = 0u32;
    let mut properties = 0u32;

    // SAFETY: probe call — a null buffer with size 0 is the documented way to learn the required
    // byte count; it is expected to fail with ERROR_INSUFFICIENT_BUFFER, so the result is ignored
    // and `used` is what we act on.
    unsafe {
        let _ = EvtRender(None, event, EvtRenderEventXml.0, 0, None, &mut used, &mut properties);
    }
    if used == 0 {
        return None;
    }

    // ALIGNMENT: u16-backed, not u8 — wevtapi writes UTF-16 through a `PWSTR`, which needs 2-byte
    // alignment. `Vec<u8>` only guarantees 1 (the same trap fixed in cpu_topology.rs, TODO #66).
    // `used` is in BYTES for XML rendering; the extra element leaves room for the terminator.
    let mut buffer: Vec<u16> = vec![0; (used as usize).div_ceil(2) + 1];
    let capacity_bytes = (buffer.len() * std::mem::size_of::<u16>()) as u32;

    // SAFETY: the buffer pointer is passed directly as a call argument and the `Vec` outlives the
    // call; `capacity_bytes` is derived from that same `Vec`, so wevtapi cannot overrun it.
    unsafe {
        EvtRender(
            None,
            event,
            EvtRenderEventXml.0,
            capacity_bytes,
            Some(buffer.as_mut_ptr().cast()),
            &mut used,
            &mut properties,
        )
        .ok()?;
    }

    let chars = (used as usize / std::mem::size_of::<u16>()).min(buffer.len());
    let text = String::from_utf16_lossy(&buffer[..chars]);
    Some(text.trim_end_matches('\0').to_string())
}

/// Formats the provider's human-readable message — the same text Event Viewer shows, which is
/// where the error *source* ("Reported by component: Memory", a PCIe device path, …) lives.
/// Collapsed to a single line and truncated for the console.
fn format_message(metadata: EVT_HANDLE, event: EVT_HANDLE) -> Option<String> {
    const MAX_CHARS: usize = 4096;
    let mut used = 0u32;
    let mut buffer: Vec<u16> = vec![0; 512];

    // Two attempts: the first may only be telling us how big the buffer needs to be.
    for _ in 0..2 {
        // SAFETY: `buffer` and `used` are locals passed directly as call arguments. Buffer length
        // is in CHARACTERS for this API (unlike EvtRender's bytes), and the slice carries its own
        // length, so wevtapi cannot overrun it.
        let result = unsafe {
            EvtFormatMessage(
                Some(metadata),
                Some(event),
                0,    // message id unused when formatting an event
                None, // no substitution values
                EvtFormatMessageEvent.0,
                Some(buffer.as_mut_slice()),
                &mut used,
            )
        };

        // The "insert" errors are NOT failures for our purposes: wevtapi could not substitute one
        // of the event's data values into the message template (common for WHEA, whose payload is
        // a binary error record), but it still wrote everything it could resolve. Rejecting these
        // would throw away the component/source text for exactly the events we care about most.
        let partial = result.as_ref().err().is_some_and(|e| {
            is_win32_error(e, ERROR_EVT_UNRESOLVED_VALUE_INSERT)
                || is_win32_error(e, ERROR_EVT_UNRESOLVED_PARAMETER_INSERT)
                || is_win32_error(e, ERROR_EVT_MAX_INSERTS_REACHED)
        });

        if result.is_ok() || (partial && used > 0) {
            let chars = (used as usize).min(buffer.len());
            let text = String::from_utf16_lossy(&buffer[..chars]);
            let collapsed = collapse_whitespace(text.trim_end_matches('\0'));
            return if collapsed.is_empty() { None } else { Some(collapsed) };
        }

        // Grow once if that is all that was wrong. `used` is a character count here.
        let needs_more = result
            .as_ref()
            .err()
            .is_some_and(|e| is_win32_error(e, ERROR_INSUFFICIENT_BUFFER));
        if !needs_more || used as usize <= buffer.len() || used as usize > MAX_CHARS {
            return None;
        }
        buffer = vec![0; used as usize];
    }
    None
}

/// Collapses all whitespace runs to single spaces and truncates. WHEA messages are multi-line;
/// the progress line is not.
fn collapse_whitespace(text: &str) -> String {
    const MAX_LEN: usize = 220;
    let mut out = String::with_capacity(text.len().min(MAX_LEN + 1));
    let mut last_was_space = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            if !last_was_space && !out.is_empty() {
                out.push(' ');
            }
            last_was_space = true;
        } else {
            out.push(ch);
            last_was_space = false;
        }
        if out.len() >= MAX_LEN {
            out.push('…');
            break;
        }
    }
    out.trim_end().to_string()
}

/// Reads `<Tag>123</Tag>` out of rendered event XML.
///
/// A deliberate shortcut over a real XML parser: the `System` section of the event schema is
/// fixed-shape, these two elements carry plain integers, and this runs at most a handful of times
/// per run (a system producing enough WHEA events for parse cost to matter has already failed the
/// test). Attribute-carrying forms such as `<EventID Qualifiers='0'>47</EventID>` are handled by
/// scanning to the closing `>` of the opening tag rather than assuming it ends the tag name.
fn element_u32(xml: &str, tag: &str) -> Option<u32> {
    let open = format!("<{}", tag);
    let mut search_from = 0usize;

    while let Some(rel) = xml[search_from..].find(&open) {
        let tag_start = search_from + rel;
        let after_name = tag_start + open.len();
        let next = xml.as_bytes().get(after_name)?;
        // Reject a prefix match: `<Level` must be followed by `>` or whitespace, not by another
        // name character (so looking for `Level` never matches `<LevelName>`).
        if *next != b'>' && !next.is_ascii_whitespace() {
            search_from = after_name;
            continue;
        }

        let content_start = tag_start + xml[tag_start..].find('>')? + 1;
        let content_end = content_start + xml[content_start..].find('<')?;
        return xml[content_start..content_end].trim().parse::<u32>().ok();
    }
    None
}

/// NUL-terminated UTF-16, for `PCWSTR` arguments.
fn to_wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "<Event xmlns='http://schemas.microsoft.com/win/2004/08/events/event'>\
        <System><Provider Name='Microsoft-Windows-WHEA-Logger' Guid='{c26c4f3c}'/>\
        <EventID Qualifiers='0'>47</EventID><Version>0</Version><Level>3</Level>\
        <Task>0</Task><Opcode>0</Opcode></System></Event>";

    #[test]
    fn parses_event_id_with_attributes() {
        assert_eq!(element_u32(SAMPLE, "EventID"), Some(47));
    }

    #[test]
    fn parses_level() {
        assert_eq!(element_u32(SAMPLE, "Level"), Some(3));
    }

    #[test]
    fn ignores_prefix_matches() {
        // `<Version>` must not satisfy a search for `<Ver`, and `<Level>` must win over a
        // hypothetical `<LevelName>` appearing first.
        let xml = "<System><LevelName>bogus</LevelName><Level>1</Level></System>";
        assert_eq!(element_u32(xml, "Level"), Some(1));
    }

    #[test]
    fn missing_tag_is_none() {
        assert_eq!(element_u32(SAMPLE, "Keywords"), None);
    }

    #[test]
    fn counts_arithmetic() {
        let earlier = WheaCounts { total: 4, corrected: 3 };
        let later = WheaCounts { total: 10, corrected: 5 };
        let delta = later.since(&earlier);
        assert_eq!(delta.total, 6);
        assert_eq!(delta.corrected, 2);
        assert_eq!(later.uncorrected(), 5);
        assert_eq!(later.corrected_percent(), Some(50.0));
        assert_eq!(WheaCounts::default().corrected_percent(), None);
    }

    #[test]
    fn collapses_multiline_messages() {
        let text = "A corrected hardware error has occurred.\r\n\r\nReported by component: Memory\n";
        assert_eq!(
            collapse_whitespace(text),
            "A corrected hardware error has occurred. Reported by component: Memory"
        );
    }

    #[test]
    fn inactive_monitor_is_inert() {
        let monitor = WheaMonitor::new();
        assert!(!monitor.is_active());
        monitor.poll(); // must not touch Win32
        assert_eq!(monitor.counts(), WheaCounts::default());
        assert!(monitor.take_pending().is_empty());
    }
}
