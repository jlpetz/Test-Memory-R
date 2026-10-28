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
