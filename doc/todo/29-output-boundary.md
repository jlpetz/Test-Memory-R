# TODO 29. [TMR-APP] Console → Log → Results output boundary (absorbed #67 item 3)

> Full record, moved verbatim out of `TODO.md` on 2026-09-28 (open item).
> The short entry in `TODO.md` holds the current status; this file holds the reasoning.

**Priority**: Low (dev convenience, not testing-critical) — but see the #7 coupling below, which
may promote it: if the GUI is built, the engine/output split stops being a convenience.
**Status**: Parked (2026-06-01). **Scope widened 2026-09-09** — #67 item 3 (where the
results/log boundary should sit) was folded in here rather than left open under #67, since it
cannot be decided without deciding this.

**Problem**: Console output and `logs/*.log` don't match. Two independent streams:
1. `log::` macros → `setup_logging()` custom formatter (main.rs:1452) writes them TWICE —
   colored to stdout AND uncolored `[ts LEVEL module]` to file.
2. `println!`/`print!` — every banner, table, per-test report, console renderer
   (`reporting/renderers/console.rs`) — go ONLY to stdout, never to the file.
So the log file is missing exactly the high-value flat output (config tables, per-test
reports, allocation summary, final summary).

**Desired**: console stays FLAT (no `[ts LEVEL module]` prefix — width budget is tight);
the FILE captures both streams as an exact transcript (minus colors, minus progress ticker).

**Recommended approach — stdout tee (Option A)**: wrap process stdout in a writer that forks
every byte to terminal + file; strip ANSI on the file path; drop progress-ticker lines (the
`\r\x1b[K` carriage-return updates). Delete the `log::`-to-file half of `setup_logging` (the
tee already captures the console line). Self-maintaining — future `println!`s captured for
free. ~40-60 lines, localized. (Option B = route all `println!` through a `log::`-style
`out!` macro: more "correct" but scatter-edits dozens of sites across 4+ files, high
drift risk. Option C = dual-sink only the console renderer: partial, misses bare `println!`s.)

**Note**: `results/*.json` is SEPARATE from logs — it's structured finished-test data
(per-test `throughput_mib_s`, `duration_ms`, `bytes_processed`, `errors`). That's the right
source for perf comparison/parity baselines; the log transcript is purely for dev/debug
readability. Decision (2026-06-01): use results JSON for the #19 Part A.2 parity baseline;
defer this log fix as not testing-critical.

---

#### Absorbed from #67 item 3 (2026-09-09): where does the results/log line actually sit?

Original intent was **log = exactly what the console showed**, **results = test performance data for
comparison**. #67 items 1/2 proved the split leaks: run identity and resolved config are neither
console chatter nor test performance, yet a comparison is worthless without them. They now live in
`results/*.json`, which was the pragmatic call — but the boundary was never actually decided.

Options to weigh (still not decided):
- **(a) Two files, run identity/config as structured fields in results.** What ships today. Log stays
  the human transcript; the duplication is intentional (one is prose, one is data).
- **(b) Single JSON with a `log` section embedding the transcript.** One artifact to attach to a bug
  report, no risk of pairing the wrong log with the wrong result. Cost: result files get large and
  stop being cheap to diff.
- **(c) Two files, cross-referenced.** ✅ **Partly done 2026-09-09**: both filenames are now built
  from one captured instant (`run_context::run_start` / `run_file_stem`), so a run's log and result
  share a stem, and the log header names its paired result file. A shared run UUID was *not* added —
  the stem is sufficient today and a UUID would be a second identity to keep in sync.

**Why this can't be settled independently — two couplings:**

1. **This item (#29) itself.** Option (a)'s premise is that the log *is* a faithful console
   transcript. Today it is not (see above: every `println!` bypasses the file). If that is never
   fixed, (a) is built on a false claim and (b) gets more attractive.

2. **#7 (GUI).** Raised by the user 2026-09-09 and the stronger of the two constraints. A GUI forces
   the app to be split into an **engine/library** and a **presentation layer**, because the GUI must
   show everything the CLI shows with no gaps. Today output shape and output content are tangled:
   `println!` *is* the reporting layer. So the real question is not "one file or two" but **what the
   engine emits as structured events**, with console text, log file, results JSON and GUI all being
   renderers over that one stream. Decide that and (a)/(b)/(c) mostly answers itself — decide the
   file layout first and it will likely have to be redone when the GUI lands.

**Requirement the final solution must satisfy (added 2026-09-21): a single owner for the console.**
Two threads write to stdout today — the coordinator (banners, per-test reports, final summary) and
the `progress_reporter` thread (`runner.rs:718` spawns it; the 500 ms ticker line). They are
coordinated only by an **advisory flag**, `pause_progress_output` (`progress.rs:39`, set at
`runner.rs:1060`, cleared at `1225`), and that flag is **check-then-act**: the reporter tests it at
`progress.rs:260` and then does its whole print block (line-clear, pending WHEA lines, the status
line, `flush()`) through to `progress.rs:363`. If the coordinator sets the flag anywhere inside that
window, the reporter is already committed and its line lands in the middle of the report. Windows
console writes go through conhost IPC and take milliseconds, so the window is milliseconds wide, not
nanoseconds — which is why the user sees this occasionally rather than never. `AtomicBool` is not the
problem and a mutex *around the flag* would fix nothing: atomics make an operation indivisible,
whereas "I am printing until I am done" is an **interval**, which only a held lock can express.

Deliberately **not** fixed on its own (decided 2026-09-21): the tactical fix is a console `Mutex<()>`
held across each print, which would also let `pause_progress_output` be deleted outright (net less
state) — but it buys only a cosmetic guarantee, and an engine/renderer split makes the whole area
moot by giving the console exactly one writer. So it is folded in here as an acceptance criterion
instead: **whatever #29/#7 produces must make interleaved console output structurally impossible, not
merely unlikely.** A structured event stream with one renderer owning the terminal does that for free.

Second, smaller thing to carry over: the pause is a bare `store(true)` … 165 lines … `store(false)`
pair. There is no early return in that range today (verified 2026-09-21), but a future `?` inside it
would silently disable progress output for the remainder of the run, with nothing in the log saying
so. A guard scope, or the single-writer design, removes that class rather than relying on the range
staying return-free.

**Sequencing**: do not settle the boundary before #7's engine/presentation split is scoped. The
cheap, unblocked parts of item 3 (timestamp alignment) are already done and are independent of the
outcome.
