# TODO 109: the window: each step's report, the CPU, Memory and Log tabs, files and compare, the chart

Raised 2026-10-28 by the owner, after the window's chart was seen to flatline during "pending",
"Resealing · pending" and "Checking seal · pending", and with the console's reports the window
lacks (the CPU and thread layout, the memory setup and each thread's allocation, the final summary,
each test's thread report). The aim (the owner): stay as plain as TM5's window, a memory tester
first, with the console's information one click away rather than all on screen.

## What the window has

Every report the console prints already reaches the window as an event (`src/frontend.rs`):
`Setup::CpuTopology`, `Setup::Selection`, `Run::MemoryPlan`, `Run::Allocated`,
`Run::CpuPerformance`, `Run::FinalSummary`, and `Run::StepFinished`, whose `StepReport` carries
each thread's time, bytes and errors (`threads`) and a latency step's latencies. The window drops
several of them (`model.rs`, the `=> {}` arms). So this is the window's work: no engine change,
nothing near the tests, but for item 1's seal figures.

## The plan (the owner, 2026-10-28)

1. **The chart** (`chart.rs`, `model.rs` `Series`):
   - No data is a gap, not 0: a sample is a test's speed, the seal's, or none.
   - The seal's work between steps (`thread_pool::seal_before_step`: resealing a step's memory, or
     checking it, before the step's test) publishes its own figures, in counters of its own, so the
     ticker shows `Resealing · 3.20 GiB @ 41,000 MiB/s` rather than `pending`, and the chart draws
     it in the seal's colour. The cycle-wide stages (`Sealing memory`, `Final seal check`) already
     publish. The seal checks inside a test's chunks stay out (a gap until the test publishes).
   - A labelled line where a stage begins: `C1 T5 Mem-MovingInv256`, `C1 Sealing memory`; a label
     only where it fits.
2. **Each step's report:** a step row opens its full report (its config line, each thread's figures)
   in a panel beside the list; the final summary arrives as a row of its own and opens itself.
3. **Tabs: Run | Log | System | Threads | Memory** (the owner, 2026-10-28: the log first after Run,
   a tab, not a docked pane, then the console's startup sections). Run is today's view, and the one
   the window opens on. Log: the run's log lines, live (the console's report tables join the log
   with TODO 29's transcript; until then the section tabs draw them). System: the CPU, its cache
   tiers and calibration, the TSC, the memory modules. Threads: the core topology, which thread is
   on which CPU. Memory: the memory plan, the page sizes, each thread's allocation.
4. **A command bar:** Start; Load config (the Open dialog for `.cfg`/`.json`, from the home's
   `configs\`, writing `config=<path>` into the parameters); Compare (two results files, from
   `results\`, shown as `--compare-results` shows them; a side-by-side by step is TODO 72's later
   goal); and under `…`: open the log, its folder, the results file, its folder, About.
5. **About:** the larger logo, the version and build (commit, compiler), the licence, the GitHub link.

## windows-reactor 0.100.0 (2026-09-03), and what its next release fixes

Built on 0.100.0 now (the owner, 2026-10-28), living with its bugs until Microsoft releases the
~100 Reactor commits on windows-rs' master since. A published crate can only depend on published
crates, so the window stays on a crates.io release. In 0.100.0:
- No file dialog in Reactor (none on master either): the `windows` crate's IFileOpenDialog, owned
  by the window (its handle from the UI thread, as Reactor exposes none).
- TextBlock has no font family: the Log tab can't take Cascadia Mono until the next release, so its
  columns don't line up. Then: Cascadia Mono, falling back to Consolas; emoji from Segoe UI Emoji,
  as in Windows Terminal (Terminal gives an emoji two cells; XAML its own width).
- ListView crashes when an item with content is moved and selected in one update (#5222): the step
  list is a panel of rows, only ever appended to.
- NavigationView can clear its selection on a stale event (#5210): the tabs are a SelectorBar or
  Pivot.
- SplitView echoes a false close (#5213): no SplitView.
- Windows App SDK 2.6.4 on master (#5228): if the next Reactor release needs it, the winget
  dependency's and the README's minimum (2.4) rise with it.

## Order

The chart first, then each step's report, the tabs, the command bar, About: each a commit the owner
checks in the window (Claude doesn't run tmr.exe).

## Built (2026-10-29, for the owner's check in the window)

All five, with tests and clippy clean; none seen running yet.
- 08b76c6, 881400c: the chart's gaps, the seal's figures and colour, the stage lines, and a dash for
  a reading alone between gaps.
- 4c9f999: the 8 tests that published only at a cycle's end (stuck-bit, refresh, cache-bust,
  stride, block-move, moving inversions, modulo-X, sequence) publish between chunks, as random
  torture did; a quick run's cycle can be the whole test. Asm: only those families changed, no
  new stack access; speed: every test within its runs' spread (the commit has the table).
- 76d5016: a row opens its step's report beside the list; the final summary's row opens itself.
  The tables are the engine's (`report.rs` renders `Reporter` into blocks), the console's own.
- f223b8b: the tabs. System, Threads and Memory come from setup's and the run's events
  (`sections.rs`); the Log tab takes every record the log file gets.
- 3943981: Load config, Compare, the `…` menu and About (`dialogs.rs`: IFileOpenDialog and
  ShellExecute, each on a thread and in an apartment of its own).

Where it departs from the plan, for the owner to accept or change:
- Start and Stop stay beside the parameters box, with Load config, which writes into it; Compare
  and `…` sit at the right of the tabs.
- While a report is open the list shows only the cycle, step and label (the label red for a step
  with errors): beside a report at 860 px the eight columns don't fit.
- A row opens on the press (Border's pointer event, no Button: its padding insets the stripes), so
  it has no hover or keyboard focus.
- The log tab draws the newest 500 lines and keeps 10,000; the step reports kept are the newest
  200, as many as the rows drawn.
- The memory modules' table has no serial number (a window is shared as a screenshot).
- Compare takes both files in one dialog (Ctrl for the second), the older the baseline.

## The second round (the owner, 2026-10-31)

The owner's ten points on the first round, and the chart's flat start, which a capture of the
window on the dev box showed: Mem-SimpleV2-512_A flat at 38.5K for about 3 s, then climbing to
60K, its row 62,626, all in the test's colour while the ticker said "Checking seal".
- The flat and the climb were the live figure's clock: a test's bytes over its wall time, which
  holds the seal's work around its chunks and its setup. A sealed test's first cycle checks,
  fills and reseals every chunk (`check_seal`, `reseal`: the first cycle only), so it ran flat and
  low, and the rest climbed as that cost averaged away; CPU boost and branch predictors warm in
  milliseconds. Fixed in ab89d94: a worker publishes its test's own time, as the report divides.
- ab89d94, the errors in one form everywhere (`ErrorCounts`): `4 errors (2 test + 2 seal) + 2
  WHEA (1C/1UC)`, WHEA never in the total; a step's failing bits on a line of their own.
- 3ed7a95: the command bar at the top (the box at most 640 px wide); one run button (Start, Stop
  in red, Stopping…, Finishing…, Run again); Run again closes the window and starts tmr.exe with
  `--gui --start` and the box's parameters (the engine's run settings are once a process); the
  Plan tab; the run's work by thread at the end of the summary's report, the setup tabs fixed
  once the suite starts; the Compare tab in place of the dialog, its steps a table.
- 3a8f4b8: `window.json` (chart, theme, start_on_open, parameters, size, colours, which were
  `colours.json`); the chart's ▾. The installer ships an empty one, writable by users.
- ea672a7: the console's output in the log file and the Log tab (TODO 29's transcript).
- Not done: point 4, the Log tab following its last line. Reactor has no scroll offset, scroll-to
  or anchoring (0.100.0, nor its master, 2026-10-31), so the lists stay newest first; a feature
  request for the ScrollViewer's anchor ratio is the owner's to raise. Point 10, the report
  beside the list at 1080p: the owner is thinking on it (the options: beside when the window is
  wide, in the list's place otherwise; always in its place; below it).

Not seen in the window yet: the dev box's session wasn't drawing when the final build ran (a
locked or minimised session draws nothing, so the captures were black). The run itself worked:
its log has the console's text between the records, and window.json its parameters and size.
