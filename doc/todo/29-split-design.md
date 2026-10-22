# TODO 29/7/72: the engine/presentation split, the design (2026-10-22)

> The design the coordinator implements stage by stage. Produced on 2026-10-22 by an inventory of every output and input site (six readers), two designs (smallest change; cleanest boundary) and a judge, read-only, then checked by the coordinator. TODO 29, 72 and 7 link here. Update this file when a stage changes the plan.

## Decisions taken while the owner was away (2026-10-22), for review

- **The three log records labelled `tmr::cli`** (cli.rs:581, 1512, 1513; the panic record): no parity helper. When their code moves (S3/S4), the label becomes the new module path; that is an intended, one-line-each console change, recorded in the goldens.
- **The Ctrl+C wording** 'without the final report or results file' (cli.rs:964, 975) becomes untrue when S1 saves the results after every cycle: S1 corrects it. The other stale text (the skip count at cli.rs:1603, bold escapes in redirected output, ticker frames in WinRM captures) is left for the owner.
- **Per-thread rows** are stored per step per cycle; the size test (S1) decides pretty vs compact JSON. Pretty stays unless a long run's final file is large.
- **GUI thread priority:** Normal for v1 (workers stay High); measured with the owner later.
- **An elevated launch's silent secedit grant** (privileges.rs:398-409) is unchanged by the split; noted for the owner.
- **TODO 29's log transcript** (the log file holding everything the console showed) comes after the split.
- **GUI v1** takes the exe's own arguments; the args box, AppConfig's `last_config`/`ui_mode`/`auto_start` and 'run again' (a relaunch, one run per process) are S6 or later. Release builds stay CLI-only until the owner turns the GUI on. The S3 preview is `tmr --gui ...` from a shell.
- **CLAUDE.md's command lines** (`-p tmr-app`, `--workspace`) are updated when the workspace lands (S3), and the owner told.

## Why: the judge's choice

A synthesis, with design A as the base. A wins on the points that decide weight and churn, and I checked those against the code:

- **The engine stays at the repo root as crate `tmr`.** I built a scratch workspace outside the repo with this shape: a lib-only root package with `autobins = false`, `crates/tmr-app` holding `[[bin]] name = "tmr"`, and `default-members` set. Plain `cargo run` from the root runs the bin, `cargo build` writes `target/debug/tmr.exe`, and `cargo run --features tmr-app/gui` works. A `[lints.clippy]` deny on the root package fails a planted `println!` and `process::exit` in the engine and leaves the bin alone.
  - The asm lands under the cargo build dir layout `target/release/build/<pkg>/<hash>/out/` (I looked at the current `target/release/build/tmr/...`). So `tools/hot_loop_spills.py:213`'s glob `build/tmr*/**/tmr.s` still matches with a `tmr-app` package, and its `3tmr` symbol filter (`:97`, `:145`) holds while the lib keeps the name `tmr`.
  - B instead moves 48k lines into `crates/engine`. That breaks every `src/` path in TODO and docs and collides with TODO 28's work in progress.
- **The engine keeps today's monitor thread.** That is `progress_reporter` (progress.rs:394-437), whose 500 ms `whea.poll()` (progress.rs:403) must survive in both modes. It calls `Frontend::tick` instead of drawing. B moves the poll into `recv_timeout` loops at the result channel (runner.rs:1349, 1424, 1838): that is more code near the workers, and it leaves gaps where the coordinator isn't waiting.
- **env_logger stays.** Its format prints `record.module_path()` (cli.rs:1474, 1491) and relies on `WriteStyle::Never` stripping escapes when redirected (cli.rs:1504-1508). B's replacement logger would have to reproduce both, byte for byte.

**Taken from B:**
- a `#[cfg(test)]` shim in console.rs plus a test logger. Without them the stage-0 capture misses `print_above` lines and log records: console.rs:41 writes through `io::stdout().lock()`, which bypasses `set_output_capture`;
- a plan-only golden;
- an asm fingerprint tool;
- a `partial` marker per cycle;
- an `--compare-results` warning on files saved mid-run;
- a module-level `#[expect]` for each module not yet converted, so the converted run path can't regress;
- a guard that fails when `hot_loop_spills.py` finds zero hot loops;
- the explicit note that the CLI renders synchronously and must stay that way.

**Changed from both:**
- **The GUI comes right after the run path,** not last. It first ships as a `--gui`-from-a-shell preview over the run events. That tests the event vocabulary before the 1,500-line setup move, and the user asked to get cracking on the real GUI.
- **The calibration save is a closure the CLI passes to the one-shot command,** not a method on the frontend trait. The GUI never calls calibration, and a default answer is exactly the silent-yes bug at cli.rs:1195-1197.
- **Three log records need a parity shim.** cli.rs:581, 1512 and 1513 print on every run with module path `tmr::cli`, and neither design noticed that moving them changes console bytes. `log::RecordBuilder::module_path_static` exists in log 0.4.34 (`lib.rs:1107`), so a 6-line helper keeps the bytes; the owner can drop it.

## How it works

**How the messages work.**

- **Today:** the engine prints its own text with `println!` at about 150 quiet moments: during setup, between test steps, at a cycle's start and end, and at the run's end.
- **After the change:** at exactly those moments it calls one small interface, `Frontend`, and hands over a plain Rust value instead of text, for example "step 7 finished: these bytes, these errors, these per-thread rows".
- **What the call costs depends on who listens:**
  - **CLI:** the call formats the same text with the same format strings and writes it before returning, on the same thread. There is no message queue, no extra thread and nothing serialised, so the console keeps today's bytes and today's order against log lines, by construction.
  - **GUI:** the call drops the value into a standard channel. A Reactor background waiter (the demo's pattern, gui-test/reactor/src/main.rs:82-101 and 189-197) collects everything queued every 500 ms and redraws once.
- **The fast numbers never travel at all.** Progress, bytes, MiB/s and live error counts are already shared atomics plus one mutex (`ProgressTracker`, `TestProgress`). The frontend reads them with a new `ProgressTracker::snapshot()` on its own timer:
  - the CLI on the engine's existing 500 ms monitor thread, the one that must stay anyway for the WHEA poll at progress.rs:403;
  - the GUI from its waiter at 2 Hz.
- **The one question during a run** is the large-page restart (cli.rs:803-851, stdin at 817). It becomes a blocking method the frontend answers. The calibration save (cli.rs:1188-1199) becomes a closure the CLI hands to the one-shot command.
- **Stop** stays the `SHUTDOWN_REQUESTED` flag (runner.rs:184).
- **Kernels, hot loops, `WorkResult`, `TestProgress` and the worker loop do not change.** Every call sits where a `println!` sits today.

**Order of work.** Characterisation goldens come first. Then TODO 72's cheap slice: save after every cycle, record the build and the left-out tests, per-thread rows. It doesn't depend on events and TODO 100 wants it first. Then:
1. the run path behind the trait;
2. the workspace and a first GUI over the run events;
3. the setup path;
4. the CLI crate cut and lints;
5. the launch model and the rest of the GUI.

Every stage ends green on `cargo test` and on `clippy` with zero warnings, with the kernel asm unchanged.

## Crate layout

Repo root: `C:/Users/Administrator/Downloads/TMR/TMR-APP`. It becomes a workspace whose root package is the engine. I checked the shape in a scratch workspace outside the repo, then deleted it.

**Root `Cargo.toml`, package `tmr` (the engine, lib only, `autobins = false`):**
- `[workspace] members = [".", "crates/tmr-app", "crates/tmr-cli", "crates/tmr-gui"]`.
- `default-members` = all but `tmr-gui`, so plain `cargo build`, `cargo test` and `cargo run` never compile Reactor.
- The profiles stay here (Cargo.toml: `lto=true`, `codegen-units=1`, `panic=abort`, `strip`), and so does `.cargo/config.toml` (x86-64-v3).
- `src/` stays where it is. So every `src/...` path in TODO.md, `doc/todo/*.md` and CLAUDE.md stays valid, and so does `config.rs:2085`'s `CARGO_MANIFEST_DIR` `test_configs` path.
- Log records keep their module paths `tmr::runner` etc. (cli.rs:1474), and v0 symbols keep `3tmr`.
- New `build.rs` for the build identity.

**Engine keeps:**
- everything that computes;
- `reporting/{models, converters, formatters}`, plus `Reporter` and the `Renderer` trait. These are presentation-neutral `TableData` with no ANSI. The converters read engine state (converters.rs:554, 110), so they must stay.

**Engine gains:**
- `frontend.rs`: the trait, `Event`/`Setup`/`Run`, `StepReport`, `ProgressSnapshot`, `RestartChoice`.
- `session.rs`: `cli::run`'s body, cli.rs:129-1119.
- `commands.rs`: the one-shots, writing to a caller's `&mut dyn io::Write`.
- `logfile.rs`: the file half of `setup_logging` (cli.rs:1433-1448, 1477-1495) plus `FILE_ONLY_TARGET` (console.rs:115).
- `build_info.rs`: `pub const BUILD`.

**Engine `lib.rs`:**
- `#![cfg_attr(not(test), deny(clippy::print_stdout, clippy::print_stderr, clippy::exit))]`. It is a crate attribute rather than a `[lints]` table, because engine tests print (31 sites in calibration.rs, config.rs, smbios.rs and tsc.rs test code).
- A narrow pub API: `session`, `frontend`, `commands`, `logfile`, `BUILD`, `request_stop`, the payload re-exports. Everything else stays private, so `dead_code` still checks it (lib.rs:6-9).

**`crates/tmr-app` (package `tmr-app`, `[[bin]] name = "tmr"`):**
- `main.rs`: the panic hook and VEH (cli.rs:30-87, 133-154), the CPU gate (cli.rs:98-115), and the mode pick, which strips `--cli`/`--gui`.
- `[features] gui = ["dep:tmr-gui"]` on this package, so toggling it relinks only the bin.
- A `build.rs` that adds the manifest and delay-load only when `CARGO_FEATURE_GUI` is set.
- The output is still `target/release/tmr.exe`, the path release.yml relies on.

**`crates/tmr-cli` (lib):**
- `console.rs`, `table.rs`, `reporting/renderers/console.rs` (`ConsoleRenderer`).
- `render/` (`CliFrontend`).
- `prompts.rs`.
- `setup_logging`'s console half, keeping env_logger.
- Ctrl+C.
- `pub fn main(args) -> i32`.
- env_logger and ctrlc move here.

**`crates/tmr-gui` (lib, optional):**
- `windows-reactor =0.100.0`; no windows-canvas, no nightly gates.
- Adapted from `../gui-test/reactor/src/{main.rs, chart.rs, palette.rs}`.

**Graph:** `tmr-app` → `tmr-cli` → `tmr`; `tmr-app` → `tmr-gui`? → `tmr`. The engine names neither frontend, and the frontends don't name each other.

**Commands after the split:**
- dev loop: `cargo clippy --workspace --exclude tmr-gui --all-targets` and `cargo test --workspace --exclude tmr-gui`; full `--workspace` before a release;
- asm: `cargo rustc --release -p tmr-app --bin tmr -- --emit asm`;
- CI (`Rust-nightly-build.yml:37, 48`) adds `--workspace`;
- release.yml stays CLI-only until the owner turns the GUI on.

## The frontend interface

**Engine side (`src/frontend.rs`):**
```rust
pub trait Frontend: Send + Sync {
    /// A low-rate fact, on the engine thread that printed it before. The CLI must render
    /// before returning: console order against log records depends on it.
    fn event(&self, event: Event);
    /// Every 500 ms while a suite runs, from the engine's monitor thread, after its WHEA poll.
    fn tick(&self, _progress: &ProgressTracker) {}
    /// The suite is over: the monitor thread's last call.
    fn ticks_done(&self, _progress: &ProgressTracker) {}
    /// Large pages are configured but need a restart (cli.rs:803-851). Blocks until answered.
    fn large_page_restart(&self) -> RestartChoice; // Exit | Continue4K
}
pub enum Event { Setup(Setup), Run(Run) }
pub fn session::run(args: Vec<String>, fe: Arc<dyn Frontend>) -> crate::Result<SessionEnd>;
pub fn request_stop(); // SHUTDOWN_REQUESTED.store(true), runner.rs:184
impl ProgressTracker {
    pub fn snapshot(&self) -> Option<ProgressSnapshot>;
    pub fn take_whea_descriptions(&self) -> Vec<String>;
}
pub fn commands::calibrate(.., out: &mut dyn io::Write, confirm: &mut dyn FnMut(&str) -> bool);
```

**Why `Send + Sync` and `&self`.** Two engine threads call it: the session/coordinator thread (events) and the monitor thread (tick). `Arc` is cloned once, into the monitor at runner.rs:1032. Events move by value, so the GUI keeps them with no clone, and `StepReport` is boxed so the enum stays small.

**`session::run` takes `args`,** not `std::env::args` (cli.rs:170). The CLI and the GUI therefore accept the same parameters, and the run records the args it was given; TestRunOverrides carries them in place of run_context.rs:764.

**CLI (`CliFrontend`).**
- `event()` formats with the moved format strings into a String and writes it before returning. There is no channel and no thread.
- Each site keeps its write primitive:
  - plain `print!` before the suite and after it (the ticker is down for good after progress.rs:428);
  - `console::print_above` for the seed, cycle and seal lines (runner.rs:1134, 1151, 1852);
  - a new `console::print_block` for the step report: one TICKER lock, erase, write, flush, no redraw until the next draw. That is today's `hold()`/`release()` bytes (console.rs:98-110), but no log line can land inside the report, which meets TODO 29's criterion.
- The `match` over `Event` has no wildcard arm, so a new event fails to compile until the CLI handles it.
- `tick()` is today's 2 s draw:
  - a `Mutex<Option<Instant>>`, set at the first tick;
  - snapshot → `render` → `console::draw_ticker(text, || p.take_whea_descriptions())`;
  - `last_draw` advances only when it drew, as progress.rs:422-424.
- `ticks_done()` is `hold()` plus printing the pending WHEA lines.
- `large_page_restart()` prints the A/B block and reads stdin (`prompts.rs` over `BufRead`/`Write`, so tests can script it).

**GUI (`GuiFrontend`).**
- `event()` is an unbounded `mpsc::Sender::send`: about 25 setup events, then steps+4 per cycle.
- `tick` and `ticks_done` are no-ops; WHEA is still polled by the engine's monitor.
- `large_page_restart()` sends `Feed::Ask(reply: SyncSender<RestartChoice>)`, blocks on `recv()`, and the UI answers with a ContentDialog. If the window is gone, the answer is Continue4K, as an empty answer is today.

**Stop.**
- The CLI's ctrlc handler is moved verbatim (cli.rs:958-979) and installed once per process when Setup::Starting renders; a second `set_handler` aborts (cli.rs:979).
- The GUI's Stop button calls `request_stop()`.
- No new checks anywhere.

**Progress.**
- `snapshot()` takes the Position lock once and sums the workers' `TestProgress` atomics via `live_figures` (progress.rs:360-382), exactly what `render` reads today. One small String (the test name) is cloned per read.
- It returns: runtime, time_limit, cycle, cycle_limit, cycle_elapsed, tests_done, tests_per_cycle, finished_errors, live_errors, WheaCounts, test{number, name, elapsed, time_limit, finished, publishes} and live{published, bytes, bytes_per_sec, stage}.
- `render`, `suite_line`, `test_stamp`, `hms` and `percent` (progress.rs:262-392) move verbatim to the CLI as functions of the snapshot.

**One-shot commands** (help, demo configs, compare, topology debug/show, setup-large-pages, calibrate) live in `tmr::commands` and write to the caller's writer: `println!` becomes `writeln!(out)`, byte for byte. They are not on the run path, and the GUI doesn't offer them in v1.

**Log records are not part of the trait.**
- The log facade stays the engine's diagnostic channel, and each frontend installs a logger.
- The CLI keeps env_logger and LogSink (cli.rs:1452-1509).
- The GUI installs a `log::Log` that writes the same file line through `tmr::logfile` and `try_send`s Warn+ records into a bounded `sync_channel(4096)` with a dropped count. So the kernels' cold error lines (tests.rs:3770, 3800, 3818, 3858) reach the GUI with no kernel change, and a worker never blocks.

## Keeping the console output, and proving it

**How the bytes stay the same:**

1. **Synchronous at the same spot.** `CliFrontend` writes inside `event()`, on the emitting thread, where today's println group ran. Log records still go through the unchanged env_logger and LogSink path, also synchronously, so their interleaving is today's.
   - **Conversion rule:** one event per run of consecutive prints with no engine call between them, emitted where that run printed; an event never spans a log call.
   - Hence the splits: StartupHeader/StartupParams around cli.rs:581, Cache/SealCalibration around `valid_seal_calibration`'s logs, and LeftOut before runner.rs:778. Two more, found in S2's review: Channels/TestPlan, because building the plan logs (its extents and chunks, at debug level), and NoTestMatches/Matchable, because collecting the list builds the suite (each auto-dispatch test's width, at debug level).
   - frontend.rs documents that the CLI must not become asynchronous: runner.rs:1783's 'Halting test suite' would then overtake its step report.
2. **Verbatim moves.** Each CLI arm is the old print group cut and pasted with the same format strings, written into a String.
   - `TableBuilder::print` (table.rs:41-107) becomes `write_to(&mut impl fmt::Write)`, which `print()` calls.
   - `ConsoleRenderer` buffers.
   - Reporter's three bare printlns (reporting/mod.rs:103, 155, 180) go through a new `Renderer::render_text`.
   - The shared wording stays in the engine, untouched: `WheaCounts::split_suffix`, `FailingBits::brief`, `OpTally::line`, `Stage::label`, the formatters.
3. **Streams and colour kept.**
   - stderr stays stderr for cli.rs:123, 436, 763, 110-112, 144 and 965, and runner.rs:4816.
   - Log colour still follows `is_terminal` (cli.rs:1504).
   - ConsoleRenderer's hard-wired bold (renderers/console.rs:21) is kept.
4. **The three moved log records keep `tmr::cli`.** cli.rs:581, 1512 and 1513 print on every run with `record.module_path()` (cli.rs:1474), and the panic record at 145 too. A 6-line helper builds them with `log::Record::builder().module_path_static(Some("tmr::cli"))` (log-0.4.34/src/lib.rs:1107); env_logger still applies its level filter. The owner may drop the helper in a one-line commit.
5. **The ticker.** Same text, the 500 ms poll and 2 s draw, the same erase sequences (console.rs:47-55, 81-93). Frame timing may shift by at most one 500 ms tick; nothing parses frames.
6. **Versions.** The literal '1.0.0'/'v1.0.0' becomes `env!("CARGO_PKG_VERSION")` = 1.0.0, the same bytes (cli.rs:167, 422, 581, 1512; params.rs:496; config.rs:16; results.rs:335).
7. **'💾 Results saved: <path>'** prints once, after the final save; per-cycle saves are silent (bench_ab.py:54, 76 use `re.search`).
8. **Kept on purpose, each the owner's call as its own commit:**
   - the stale skip count (cli.rs:1603);
   - ~~the Ctrl+C wording 'without the final report or results file' (cli.rs:964, 975)~~: corrected in S1, per the decisions above, to '…the results file keeps only the cycles saved so far';
   - ANSI bold in redirected output;
   - ticker frames in WinRM captures.

**Proof:**
- **S0, on today's code:** exact-byte goldens of each Reporter method from fixed models.
- **A masked fake `execute_test_cycle`** over two heap Regions (runner.rs:4826-4895) on `ThreadPool::new(…, false, ThreadPriority::Normal, 2, None)` (thread_pool.rs:110). It uses `TestRunResult::new(Default::default())`, a never-started WHEA monitor (inert per whea.rs:821-827) and a fixed seed (pattern.rs:631). The plan is a sealed Mem- step, a Bench-Init/Verify pair with one injected flip and a latency step; times, rates and hh:mm:ss are masked, and labels, counts, glyphs and line order stay exact.
- **A plan-only golden** of `run_tests_with_layout_and_timing_filtered` with a literal EnhancedMemoryLayout. It returns at runner.rs:947-951 before WHEA and allocation; the host-dependent left-out line is masked.
- **The capture itself:**
  - `std::io::set_output_capture` (nightly stdio.rs:1105-1111); libtest propagates it to spawned threads (test/src/lib.rs:142-148);
  - a `#[cfg(test)]` shim routes console.rs's writes (console.rs:41) through `print!` so `print_above` lines are captured;
  - a once-installed test logger formats records like cli.rs:1458-1475, so log lines and their order are in the golden.
- **S2:** the same goldens pass byte-identical through `CliFrontend`; per-event goldens with fixed payloads; an order test with a RecordingFrontend plus the test logger.
- **Setup can't run in-process** (secedit via privileges.rs:404, 737-786; process::exit). So the owner runs the old and new exe, diffed by a new tools/console_diff.py (strips ANSI and ticker frames, masks times, rates, seeds and stems):
  - `--startup-debug memory=50% cputype=cores cpus=100% skip-cores=0`;
  - `--help`, `--version`, an unknown flag, a bad value;
  - one short run.
  He does this at S2, S4 and S5.

## The GUI frontend

**Preview (S3), to validate the vocabulary early.**
- `tmr --gui …` from a shell only. The feature is on, and the build must be at least 22000 by `RtlGetVersion`; otherwise the CLI runs, with a one-line note.
- main is the Reactor UI thread (`App::run_component`, STA; TMR initialises COM nowhere).
- Start spawns a named `tmr-engine` thread calling `tmr::cli::run_session(args, Arc<GuiFrontend>)`. That is today's `cli::run` with the frontend injected, returning the status instead of exiting at cli.rs:1116.
- Setup text, its log lines and the large-page prompt still go to the inherited console. The window shows the run.
- `build.rs` delay-loads `api-ms-win-appmodel-runtime-l1-1-5.dll` plus `delayimp.lib`, only with `CARGO_FEATURE_GUI`, so the CLI still starts on 20348.

**Window (the owner's layout, TODO 7 detail).**
- **Header:** status badge (Ready / Running / Stopping / Passed / Failed / Interrupted / Not run, from Run::SuiteEnded and the thread's end); runtime timer top right (snapshot); system info top right, filled after Start so the lazily initialised detection logs (cache.rs:206-216, smbios.rs:392) land in the run's log as today: CPU, RAM, caches, and the DIMM drop-down from `MemoryModule` (smbios.rs:37-51).
- **Info line:** threads, GiB, page mix and plan, from Run::Allocated and Run::TestPlan.
- **Position:** cycle n/L, test n/N with name and stage, and the rounded bar with the % inside, from the snapshot.
- **Chart at the top:** the shapes chart from chart.rs; `bytes_per_sec` each 500 ms, 120 samples; error marks when the snapshot's errors rise.
- **Results table:** one keyed row per Run::StepFinished: cycle·step, label, MiB/s (`summary.throughput_mib_s`), errors in the palette's red when above 0, seal errors, WHEA.
- **Errors list:** steps with errors, seal-stage errors, WHEA descriptions drained by the GUI from the Arc<ProgressTracker>.
- **Start/Stop:** Stop calls `request_stop()`.
- **Colours:** palette.rs and colours.json.

**Feed.** One `spawn_background` waiter at a time:
1. `recv_timeout(500 ms)` on the Receiver, which moves into the closure and back out in the Message, not the demo's `Arc<Mutex<Receiver>>` at main.rs:57;
2. `try_iter`;
3. `progress.snapshot()` once SuiteStarted handed over the Arc;
4. return `Message::Feed{events, snapshot, rx}`, re-armed in update.

A timeout also returns, which is the window's 2 Hz clock (Reactor 0.100.0 has no public timer and LocalSender isn't Send).

**Completion (S6).**
- **Launch model,** in tmr-app main before anything prints:
  - `--cli`/`--gui` decide;
  - any other argument means CLI, so bench_ab.py (`bench_ab.py:69`) and `tmr.exe memory=…` stay CLI;
  - with no args: redirected stdout (GetFileType not FILE_TYPE_CHAR) means CLI (WinRM); `GetConsoleWindow()==NULL` means GUI (double-click on 26100 with the detached manifest); `GetConsoleProcessList()==1` means FreeConsole, then GUI; otherwise CLI;
  - `tmr.manifest` carries only `consoleAllocationPolicy=detached`, embedded with `/MANIFEST:EMBED` and `/MANIFESTINPUT:{CARGO_MANIFEST_DIR}/tmr.manifest` (forward slashes), only with the feature;
  - the CPU gate and hooks run before either frontend.
- **Setup in the window:** the GUI moves to `tmr::session::run`.
  - Setup events fill the info line and system panel.
  - The restart question is a ContentDialog (Exit / continue on 4 KiB).
  - The GUI logger (via `tmr::logfile`) feeds the errors list with the kernels' bad-word lines, plus an 'N lines dropped' line.
  - An args TextBox in the CLI's `key=value` syntax, prefilled from the exe's args minus `--gui`, so both frontends take the same parameters.
  - The panic hook, VEH and CPU gate also show a MessageBoxW in GUI mode.
  - The log file opens at Start, so RUN_START (run_context.rs:51-54) names the run.
- **One run per process.** These are first-set-wins OnceLocks: simd.rs:163, pattern.rs:627-635 and 684-688, seal.rs:85-91, tests.rs:469-489, cpu_topology.rs:112-144. SHUTDOWN_REQUESTED is never reset; env_logger `.init()` and ctrlc are once per process. So 'Run again' relaunches the exe with the same args.
- **Closing mid-run:** Reactor 0.100.0 can't veto a close. After `App::run` returns, main calls `request_stop()` and joins the engine thread for up to 30 s, so the current step ends and the final save runs. The cycles already finished are in the per-cycle file anyway.

## Results file and log

**Per-cycle save (S1, no console change).**
- In the cycle loop right after `execute_test_cycle` returns (runner.rs:1157), after the first cycle, after one that stopped early, and after any other once a minute has passed since the last save (`RESULTS_SAVE_INTERVAL`; S1 found that saving after every cycle writes what grows with the square of the cycle count, hundreds of GB over a night of short cycles: doc/todo/72), the engine:
  1. locks `test_run_result`;
  2. calls `finalize(suite_start.elapsed())`, which is idempotent: it reassigns every overall field from `self.cycles` (results.rs:455-487; per_step_averages replaced at 569);
  3. calls `set_whea_totals(progress.whea.counts(), is_active())`;
  4. sets `in_progress = true`;
  5. calls `save_atomic`: writes `results/<stem>.json.tmp` and flushes it to the disk (so a system that goes down keeps a whole file), then `fs::rename` over the target.
- A failed rename (a viewer holding the file without share-delete) falls back to writing the file in place plus a `log::warn`, retried at the next save; when that is refused too, the save stays in the `.tmp`, which the warning names. It is never fatal.
- The final save in `display_and_save_results` (runner.rs:4746-4775) uses `save_atomic` too, with `in_progress = false`, outcome and status, then the one 'Results saved:' line.
- **New `#[serde(default)]` fields:**
  - `OverallStats.in_progress: bool` (default false, so old files read as finished);
  - `outcome: Option<String>` (completed / time_limit / interrupted / halted);
  - `status: Option<String>` (passed / failed / not_run / interrupted). Neither is in OverallStats today (results.rs:186-215);
  - `CycleResult.partial: bool`: the step loop stopped before the final check; add_cycle at runner.rs:1812-1819 gains the flag.
- `--compare-results` warns when either file is `in_progress`.

**Build identity.**
- A root `build.rs` sets `TMR_COMMIT` (`git rev-parse HEAD`, 'unknown' without git), `TMR_DIRTY` (non-empty `git status --porcelain --untracked-files=no`) and `TMR_RUSTC` (`$RUSTC -V`; the nightly is unpinned).
- It reruns on `.git/HEAD`, `.git/logs/HEAD`, `.git/index`, `src`, `build.rs` and `Cargo.toml`.
- `pub const BUILD: BuildInfo{version: env!("CARGO_PKG_VERSION"), commit, dirty, rustc, profile}`.
- `metadata.tmr_version` comes from BUILD.version (replacing results.rs:335), plus a `#[serde(default)] build` record.
- The log file header (file only, cli.rs:1439-1447) gains a '# build:' line.

**Tests left out.** runner.rs:770's `Vec<(name, reason)>` goes into RunConfigInputs (run_context.rs:534-565), then into `RunConfigSnapshot.tests_left_out: Vec<{name, reason}>`, skipped when empty. It is captured at runner.rs:1077-1103.

**Per-thread data.**
- In the result loop (runner.rs:1424-1489), next to the CpuStatRow push, one `ThreadStepRecord` per WorkResult (thread_pool.rs:59), keyed by thread:
  - thread, cpu and node from `get_cpu_assignments` (thread_pool.rs:189);
  - core from `cpu_topology::get_physical_core_for_cpu` (cpu_topology.rs:199);
  - bytes, ms (timed, else elapsed), errors;
  - alarms, not_reproduced, seal_errors (skipped when 0);
  - operations and cycles;
  - latency{min, p50, p99, spread} when present.
- A row whose CPU isn't found is still recorded with `cpu: None`; today it is dropped (1428-1434).
- The records go into TestSummary.threads, then `TestResult.threads`, skipped when empty. Both also gain `operations` and `timed_ms`, which the console shows (runner.rs:1628-1633) and the JSON lacks.
- With the CPU, core and node stored, the CPU Performance Summary can later be replayed off-box (TODO 72 D).
- **Size, estimated:** about 300 B per record pretty-printed, so 32×16×50 is about 7.7 MB final and about 200 MB written over the run. An `#[ignore]` test builds that synthetic TestRunResult and prints its bytes and `to_string_pretty` time; run it once and record the numbers in doc/todo/72 before the per-thread rows ship. Fallbacks: compact JSON, or per-test aggregates.

**Log.**
- The content is unchanged: `logs/<stem>.log`, one '[ts.mmm LEVEL module] msg' line per record (cli.rs:1477-1495).
- The file half moves to `tmr::logfile` in S4, so the CLI's env_logger and the GUI's logger write identical files.
- TODO 29's transcript gap (printed text never reaches the log) is not closed here, but becomes cheap: every CLI event is now rendered into a String first, so a tee of those Strings into the log under the console lock, ANSI stripped and ticker left out, is a small follow-up.
- The results/log boundary stays option (c): two files sharing one stem (run_context.rs:51-59).

## Run-time weight

**Kernels, hot loops and workers:** no change to any kernel, hot loop, WorkResult, TestProgress, Rescan or worker loop, and no new branch or check where a test runs. Every call sits at a point that prints today.

**CLI mode:**
- An event costs one virtual call and a struct move. It formats the same text println! formats now, into one String per group or report, written once.
- There is no channel and no new thread: the monitor thread replaces progress_reporter one for one, with the same 500 ms poll and the WHEA drain kept in the engine.
- The snapshot every 500 ms does what render() does now: one Position lock, four relaxed loads per worker, one name clone.

**GUI mode:**
- About 25 setup events per run and steps+4 per cycle, each one std mpsc send with no serialisation.
- The snapshot is read at 2 Hz on Reactor's thread pool.
- Log records go through a bounded try_send that never blocks a worker.
- Threads added: the UI thread, one engine thread, and one pooled waiter at a time. Measured redraw cost is 4-5% of a core (TODO 7 detail).

**Per step:**
- Added: one Box<StepReport>, one TestSummary clone, N small ThreadStepRecords.
- Removed: one full clone and sort of every thread's raw latency samples (runner.rs:1573-1594 duplicated 1717-1745).

**Per cycle:** at most one atomic save a minute, measured in S1 at about 37 ms for an 8.6 MB file (32 threads x 16 steps x 50 cycles; doc/todo/72 has the other shapes).

**Binary size:** the CLI-only exe doesn't grow beyond the moved code. The GUI build adds about 1.6 MB (+37%) and about 17 s per build, only with the feature (reactor inventory).

## Event vocabulary

### Setup::TestFilter / Setup::QuickTest
- **Fields:** pattern: String / none
- **Emitted at:** S4, session.rs, where cli.rs:201-209 and 321-325 print, once per test= argument
- **Replaces:** println at cli.rs:204-207, 322-324

### Setup::ParamNotes
- **Fields:** Vec<ParamNote>: MinPage(String), MaxPage(String), Block(&'static str, String), Topology(&'static str), SkipCores(usize, unresolved as today), PinningOff
- **Emitted at:** S4, end of build_config_from_validated_params (cli.rs:1576-1607), only without config=, before the header as today
- **Replaces:** println at cli.rs:1579, 1583, 1588, 1598, 1603 and 1606. The stale value at 1603 is kept for parity; the owner decides whether to fix it

### Setup::StartupHeader / Setup::StartupParams
- **Fields:** Header: command_line: Vec<String> (the injected args), release: bool. Params: rust_log, rust_backtrace: Option<String>, explicit: Vec<String>, defaults: Vec<String>
- **Emitted at:** S4, two events because the log::info at cli.rs:581 sits between them: cli.rs:570-580, then 583-638
- **Replaces:** println at cli.rs:571-572, 580, 585, 589, 634, 637

### Setup::CpuType / Setup::CpuFilter / Setup::CpuCount / Setup::Selection
- **Fields:** CpuType{cputype, avoid_smt}. CpuFilter{lo, hi}. CpuCount{percent, available, threads}. Selection{error_mode, threads, available, cputype, cpu_list, pinned, cpus_to_skip, avoid_smt_doubling, cycles, duration_secs, simd: String}
- **Emitted at:** S4, each where it prints now (cli.rs:649-653, 661-663, 741, 767-799); topology and selection calls between them may log
- **Replaces:** println and print! at cli.rs:650/652, 662, 741, 768-799, the trailing blank line included

### Setup::LargePageCheck, then Frontend::large_page_restart() -> RestartChoice{Exit, Continue4K}
- **Fields:** none / blocking request
- **Emitted at:** S4, cli.rs:801, then cli.rs:803 when check_restart_needed() (privileges.rs:737-747) is true. Blocks the session thread, which is idle then
- **Replaces:** cli.rs:801 and 804-851 including stdin at 817. The CLI prints the A/B text and the branch text inside its implementation; an empty answer or EOF is Continue4K as today (846-851). Exit becomes SessionEnd::RestartChosen (exit 0, as the `return Ok` at 831); Continue4K sets minpage=regular (835, 847)

### Setup::LargePageDiagnostic / Setup::LargePageStatus
- **Fields:** Diagnostic{report: String}, the prose privileges.rs:536-579 already returns, untouched here. Status{usable: bool, min_page: String, min_needs_large: bool}
- **Emitted at:** S4, cli.rs:853-857, then 859-882 after check_large_page_privilege (which logs at privileges.rs:84-135)
- **Replaces:** println at cli.rs:856, 861-878, 882

### Setup::CacheCalibration / Setup::SealCalibration
- **Fields:** Cache: Loaded{extended, at} | None | Stale | Disabled. Seal: Loaded{bits, at} | Stale | None (load_seal_calibration, cli.rs:1204-1215, returns a status instead of the line)
- **Emitted at:** S4, cli.rs:896-921; two events because valid_seal_calibration can log between the lines
- **Replaces:** println at cli.rs:904, 907, 911, 914, 917, 919; its test (cli.rs:1638-1659) splits into an engine status test and a CLI text test

### Setup::CpuTopology / Setup::MemoryStatus / Setup::SystemInfo
- **Fields:** CpuTopology{report: CpuTopologyReport, smt_p_cores: Option<usize>, e_cores_skipped_first: bool}. MemoryStatus(Option<{total_phys, avail_phys, total_page_file, avail_page_file}>). SystemInfo{report: SystemInfoReport, memory_modules: Vec<MemoryModule>}
- **Emitted at:** S4, cli.rs:925 (display_cpu_topology cpu_topology.rs:649-818 split into an engine builder and a CLI render), 929 (runner.rs:4777-4822, GlobalMemoryStatusEx stays in the engine), 931-943
- **Replaces:** the Reporter tables, println at cpu_topology.rs:812, 816 and cli.rs:926, 943; the eprintln at runner.rs:4816 (kept on stderr)

### Setup::Starting / Setup::RealtimeOnEveryCpu / Setup::Plan
- **Fields:** none / none / {name, about: &'static str, is_default}
- **Emitted at:** S4, cli.rs:950-952, 995-1000, 1066-1070. The CLI installs its Ctrl+C handler (cli.rs:958-979, moved verbatim, once per process) when it renders Starting, which is today's place
- **Replaces:** println at cli.rs:950-952, 998, 1069

### Run::MemoryPlan / Run::NotRun / Run::Matchable / Run::LeftOut / Run::Channels / Run::TestPlan
- **Fields:** MemoryPlan(ConsolidatedMemoryReport). NotRun: Config(String) | Plan(String) | NoTestMatches{filter, plan} | NothingLeftToRun | Allocation(String). Matchable{tests: Vec<String>}. LeftOut{tests: Vec<(String, String)>}. Channels{channels, cache_line}. TestPlan(TestConfigurationReport)
- **Emitted at:** S2. runner.rs:705 (layout.rs:31-50; its log::info at 52-59 stays after the event). The NotRun sites at 719, 730, 744-752, 781, 802, 885, 894, 919 and 1001, each before its existing return; NoTestMatches before the list is collected, then Matchable with it (collecting it logs). 770-777 before the per-test log::info at 778. 950 and 1028 (print_test_plan 655-667): Channels before the plan is built, then TestPlan (building it logs)
- **Replaces:** the Reporter render and println at layout.rs:49; the '❌' println at those sites (their FILE_ONLY log records stay); println at runner.rs:776; println at 659 and reporting/mod.rs:103

### Run::WheaMonitoring / Run::AllocationStarting / Run::Allocated
- **Fields:** Result<(), String> / {min, max: PageSizeLevel} / AllocationReport
- **Emitted at:** S2, runner.rs:960-963, 967-994, 1012-1020
- **Replaces:** println at 961-962, 968, 987-993; report_allocation with its blank lines (reporting/mod.rs:180)

### Run::SuiteStarted
- **Fields:** progress: Arc<ProgressTracker> (from S3, with its first reader), seed: u64, transfer_bits: Option<u32>, transfer_source: &'static str
- **Emitted at:** S2, runner.rs:1125-1135, after begin_suite and the log::info at 1130
- **Replaces:** print_above at runner.rs:1134. The GUI keeps the Arc for its timer; the CLI ignores it

### Run::CycleStarted / Run::SealStage
- **Fields:** {cycle: u32, limit: Option<u32>} / {op: SealOp, kernel: &'static str, bytes, errors, bits: FailingBits, duration}
- **Emitted at:** S2, runner.rs:1150-1154; run_seal_stage at 1852, before its log::error at 1855
- **Replaces:** print_above at runner.rs:1151 and 1852

### Run::StepFinished(Box<StepReport>)
- **Fields:** cycle; summary: TestSummary (progress.rs:74-124, gaining threads: Vec<ThreadStepRecord>, operations, timed_ms in S1); wall: Duration; timed_secs; ops_per_sec; cycles: Option<{completed, limit, by_time}>; sealed: bool; config: StepConfigLine{parameter, mode, extent, chunk, reps} (Strings from the engine's DefaultFormatter, as runner.rs:1641-1644 does now); threads: ThreadTimingReport; latency: Option<LatencyTestSummaryReport>; stamp: Option<ProgressSnapshot>. P50 and Spread come from summary.latency_p50_ns and latency_spread: the same arithmetic (runner.rs:1573-1594 duplicates 1717-1745), so one clone and sort of the raw samples goes
- **Emitted at:** S2, at the point of today's report (runner.rs:1551-1708). TestSummary is built first and complete_test (now 1672) moves ahead of the event; nothing printed before 1672 reads the tracker, so the stamp is still taken after it, as test_stamp does (progress.rs:310-318, finished test so live errors = 0)
- **Replaces:** hold() at 1551 through release() at 1708: the lines at 1616-1635 and 1663-1668, the thread-timing and latency tables (and '🔍 Latency' at reporting/mod.rs:155), the stamp and the blank line, written as one console::print_block

### Run::CycleFinished
- **Fields:** cycle: u32, partial: bool
- **Emitted at:** S3 (planned for S2; it waits for its first reader), in the cycle loop after execute_test_cycle (runner.rs:1157) and S1's per-cycle save, partial cycles too
- **Replaces:** nothing on the console (there's none today); it is for the GUI

### Frontend::tick / Frontend::ticks_done (not events)
- **Fields:** &ProgressTracker; the frontend calls snapshot() and take_whea_descriptions() itself
- **Emitted at:** S2, the engine's monitor thread: today's progress_reporter (spawned at runner.rs:1031-1034). tick every 500 ms after whea.poll (progress.rs:403); ticks_done once after finish() (progress.rs:427-436)
- **Replaces:** the ticker draw (render progress.rs:262-301, draw_ticker at 422), the final hold() at 428 and the pending WHEA lines at 434-436

### Run::LateWhea / Run::CpuPerformance / Run::SuiteEnded
- **Fields:** Vec<String> / {by_thread, by_cpu, by_core} reports (the aggregation at runner.rs:4598-4744 stays in the engine) / {outcome: RunOutcome, status: RunStatus, memory_errors, whea: WheaCounts}
- **Emitted at:** S2, runner.rs:1209-1213, 1216-1225, 1243-1261
- **Replaces:** println at 1212; the header at 4609 and the three tables; the verdict lines at 1246-1261

### Run::FinalSummary / Run::ResultsSaved
- **Fields:** FinalTestSummaryReport (converters.rs:594) / path: String
- **Emitted at:** S2, display_and_save_results runner.rs:4757-4766, then after the final save at 4771-4772; a failed save stays a log::error (4773)
- **Replaces:** report_final_summary and '💾 Results saved: {path}' (runner.rs:4772), printed once, so bench_ab.py:54 and 76 keep matching

### SessionEnd (session::run's return value)
- **Fields:** Ran{status, elapsed, plan_only} | RestartChosen | Stopped(UnknownFlags(Vec<String>) | Invalid(String) | MemoryOverride{spec, error} | CpuSelection(String)) | Command; Err(e) stays the ? path
- **Emitted at:** S4, where cli.rs exits or returns today: 264, 437, 511, 764, 831, 351-427, 1094-1117
- **Replaces:** the run-path process::exit sites (cli.rs:124, 264, 356, 363, 373, 437, 511, 764, 1116, 1366, 1393). 113 and 969 stay in the bin/CLI. Each text keeps its stream: stdout at 256-263 and 510; stderr for 'Error: ' plus the FILE_ONLY record (122-123), 436 and 763. The verdict (1096-1112) and exit codes move to the CLI

## Stages

### S0 Characterise today's output (tests and tools only) (M (~700 lines of tests and fixtures, two ~80-line tools))

**Work.**
- `#![cfg_attr(test, feature(internal_output_capture))]` in lib.rs.
- A capture helper with goldens in `testdata/console/` (`TMR_BLESS=1` rewrites).
- A `#[cfg(test)]` shim in console.rs routing its writes through `print!`.
- A once-installed test logger.
- Goldens:
  - one per Reporter method, from fixed models (the converters' fixtures at converters.rs:716-729; final summary via add_cycle and finalize, as results.rs:896-912);
  - the masked fake execute_test_cycle (two heap Regions, fixed seed, sealed step, Init/Verify pair with an injected flip, latency step);
  - the plan-only golden.
- New `tools/asm_fingerprint.py`: hash each post-LTO function of hot_loop_spills.py's MODULES, with labels and crate hashes normalised.
- New `tools/console_diff.py`.
- Record the spill and fingerprint baselines at 25ff780.

**Gate.**
- `cargo test --release` green (275 existing tests plus the new ones).
- clippy --all-targets: 0 warnings.
- Goldens read by eye against a current console run.
- The owner saves 'before' captures for the console_diff A/B.

### S1 TODO 72's cheap slice and build identity (no console change) (M (~350 lines))

**Work.**
- build.rs and build_info.rs.
- results.rs: BUILD.version, `build`, save_atomic, in_progress, outcome, status, partial.
- runner.rs: the per-cycle save after 1157; the final save's outcome and status; ThreadStepRecord in the result loop; operations and timed_ms; left_out into RunConfigInputs.
- run_context.rs: command_line from passed-in args instead of env::args (764); cli.rs passes its raw argv, so the value is unchanged.
- The version literals become BUILD.version.
- '# build:' in the log header.
- The compare warning.

**Gate.**
- cargo test and clippy clean.
- New tests:
  - an existing small results file committed as a fixture still loads (serde defaults);
  - the new fields round-trip;
  - save_atomic replaces an existing file in a temp dir;
  - leave-out (runner.rs:5111-5145) lands in the snapshot;
  - the #[ignore] size test, run once, numbers into doc/todo/72.
- S0 goldens unchanged; asm fingerprint of the kernel modules identical. In S1 three functions read as changed with no instruction changed (a COMDAT number, a closure's index, log records' line numbers inlined into the worker loop), so asm_fingerprint.py normalises the first two and lists a function changed only in its source lines apart, as not changed; its baseline at 25ff780 was saved again.
- The owner retakes a bench_ab.py reference, because build.rs changes the crate metadata and the .text order.

### S2 The run path behind Frontend (still one crate) (L (~1,200 lines touched, mostly moved))

**Work.**
- frontend.rs (pub(crate) for now).
- `src/cli_render/` with CliFrontend: the step report (runner.rs:1616-1705), the seed/cycle/seal lines, WHEA and Stage 1 (961-993), NotRun and LeftOut (719-920), the CPU-performance header (4609), the verdict (1245-1261), 'Results saved' (4772), and the ticker render (progress.rs:262-392), all verbatim.
- `ProgressTracker::snapshot` and `take_whea_descriptions`.
- progress_reporter becomes `monitor(progress, fe)`.
- TableBuilder::write_to, the buffered ConsoleRenderer, Renderer::render_text.
- `console::print_block` replaces hold/release at 1551/1708.
- print_layout becomes MemoryPlan; the CPU summary becomes an engine fn returning the three reports.
- run_tests_with_layout_and_timing_filtered and CycleContext take `&dyn Frontend`; cli.rs builds `Arc<CliFrontend>`.
- Crate-level deny on, with `#![expect(clippy::print_stdout, reason = "TODO 29 S4")]` on each module that still prints.
- As built (2026-10-23), against the plan above:
  - `Run::CycleFinished` and `SuiteStarted`'s `Arc<ProgressTracker>` wait for S3: with no reader before the GUI, `dead_code` flags them. `large_page_restart` waits for S4, with its caller and prompts.rs.
  - The expects sit on the printing functions where a module also holds run-path code (runner.rs's `print_current_memory_status`, results.rs's compare, table.rs's `print`, cpu_topology.rs, calibration.rs, config.rs, params.rs, buffer.rs's drop), so the run path around them stays denied; module-level only on cli.rs, and on cli_render (reason S5: it moves to tmr-cli).
  - The run's inputs are one `runner::RunSetup`, the frontend apart: a `Frontend` parameter would be an eighth (clippy's `too_many_arguments`).
  - `StepReport` carries the cycle limit and whether a time limit stopped a thread, and the three repetition counts; the passes and the ops/s come from its `TestSummary`, as the report computed them. `SealStage`'s kernel is a `String`, as `SealKernel::name` returns it.
  - `AllocationResult::print_summary`, the plan's fallback when its table failed to render, is gone: a render into a String can't fail.
  - Two events split in review, by the conversion rule: Channels/TestPlan and NoTestMatches/Matchable (see above). Under `RUST_LOG=debug` the records their builds log land where they did; the order test sees them through `golden::capture_with_debug`.
  - `ProgressSnapshot`'s test has `finished`, which its `live_errors` is derived from. The ticker's 2 s clock is `cli_render::draw_when_due`, unit-tested; a draw now stamps the tick's time, not the time after the draw.

**Gate.**
- S0 goldens byte-identical through CliFrontend; per-event goldens; the order test; capture bridge kept until S5.
- In S2 the order test covers the seal's errors after its line (a fake cycle with a bit flipped after its last step), but not LeftOut before its records: the CPU check is fixed to this machine's, which lacks nothing here. Two things only run live: LeftOut's order (seen on a CPU without AVX-512, daddy-petz), and `ticks_done`'s take-down of the ticker, without which a stray ticker frame would sit above the verdict.
- clippy 0; fingerprint and spill table identical. In S2 one cold function read as changed: `Option<BusBeat>::map_or` instantiated for the run's closures had two instances (the seed line's and the log record's), and has one, the log record's, its hash one of the baseline's two; the seed line's formatting moved to the CLI.
- The owner's console_diff A/B of one short run is clean, with no ticker frame above the verdict, and bench_ab.py against S1 is inside noise.

### S3 Workspace and the GUI preview over run events (L (~1,300 lines, about 1,000 adapted from the demo))

**Work.**
- Root becomes `[workspace]`, lib-only (`autobins = false`).
- `git mv src/main.rs` to `crates/tmr-app/src/main.rs`, with the hooks and CPU gate moved there (the panic record keeps `tmr::cli` through the helper) and `--gui`/`--cli` stripping.
- `cli::main(args)`; `pub fn cli::run_session(args, fe) -> Result<RunStatus>` (no exit at 1116).
- `pub mod frontend` and the payload re-exports.
- From S2, with their first reader: `Run::CycleFinished{cycle, partial}` after the per-cycle save, partial cycles too, and `SuiteStarted`'s `progress: Arc<ProgressTracker>`.
- `crates/tmr-gui`: GuiFrontend, the waiter, window v1, Start/Stop.
- tmr-app's build.rs: delay-load only with the feature, plus the RtlGetVersion gate.
- hot_loop_spills.py: `-p tmr-app` (docstring at lines 31-32 and the message at 215) and the zero-hot-loops guard.
- CI uses --workspace.
- The CLAUDE.md command lines are the owner's to update.

**Gate.**
- `cargo build --release` gives target/release/tmr.exe; `cargo run` from the root still runs it.
- Builds with and without `--features tmr-app/gui`. dumpbin: without the feature, no delay-import directory; with it, only the appmodel DLL is delay-loaded, and the subsystem stays CUI.
- cargo test (workspace minus gui) and clippy --workspace: 0.
- Fingerprint identical with the feature on and off.
- New bench_ab reference.
- The owner: `tmr --gui memory=… cputype=cores cpus=100% skip-cores=0` from a shell shows the run, and one run's per-step MiB/s, errors and WHEA equal the console's and the JSON's (TODO 7 Next).

### S4 Setup path, prompt, one-shot commands (L (~1,500 lines touched, mostly moved))

**Work.**
- session.rs takes cli.rs::run (129-1119) verbatim, with Setup events in place of prints, SessionEnd in place of the exits, and `fe.large_page_restart()` at 817.
- build_config_from_validated_params and load_seal_calibration (typed status) move with it.
- display_cpu_topology splits; print_current_memory_status becomes memory_status().
- commands.rs: help (params.rs:495-668), demo configs, compare, topology debug/show, setup-large-pages, calibrate. They take `out` and `confirm`.
- buffer.rs:70's eprintln becomes log::error.
- logfile.rs.
- The GUI moves to session::run and shows the Setup facts.
- The matching `#[expect]`s come off.

**Gate.**
- Per-event goldens for every Setup event and SessionEnd text.
- Scripted prompt tests: A, B, empty, EOF, junk; Enter, y, n, EOF.
- The split load_seal_calibration test.
- cargo test and clippy clean; fingerprint identical.
- The owner's console_diff A/B: --startup-debug with the standard args, --help, --version, an unknown flag, a bad value, a short run.

### S5 The tmr-cli crate cut and lints (M (moves, ~80 visibility edits))

**Work.**
- git mv into crates/tmr-cli: console.rs, table.rs, renderers/console.rs, cli_render/, prompts.rs and the small cli.rs. env_logger and ctrlc move there.
- The engine drops `pub mod cli`; no `#[expect]`s remain.
- The Reporter goldens move to tmr-cli; the set_output_capture bridge is removed, replaced by writer goldens.
- tools/README updated.

**Gate.**
- clippy --workspace --all-targets: 0.
- A planted println!/process::exit in the engine fails clippy; `grep -rn "stdin()\|stdout()\|stderr()" src/` finds nothing outside tests.
- Fingerprint and spill table identical.
- The owner's short-run console_diff is clean.

### S6 Launch model and GUI completion (M (~600 lines))

**Work.**
- The detached manifest and mode detection.
- The GUI logger and errors list.
- The ContentDialog restart prompt.
- The args TextBox.
- Close as request_stop plus a bounded join.
- MessageBoxW in the hooks.
- The UI-priority choice from the owner.
- release.yml stays CLI-only.

**Gate.**
- Both builds; dumpbin shows RT_MANIFEST only with the feature.
- clippy --workspace (feature on): 0.
- Fingerprint identical with the feature on and off.
- The owner: a double-click opens the window with no console (26100); a shell launch and a redirected launch stay CLI; the CLI starts on Server 2022 (20348).

## Risks

- Setup parity can't be captured in-process: cli.rs::run runs secedit (privileges.rs:404, 737-786) and calls process::exit. It rests on verbatim moves, per-event goldens and the owner's A/B at S4.
- Ordering depends on the conversion rule, followed by hand. The order test covers coordinator records in the fake cycle; worker and setup records are covered only by the A/B transcripts.
- `set_output_capture` is perma-unstable (stdio.rs:1105-1111) and the nightly is unpinned. A bump between S0 and S5 could break the bridge; pin the toolchain for that window if needed.
- The S3 preview still exits the process on setup errors and reads the prompt from the inherited console. It must be launched with `--gui` from a shell until S4/S6.
- Every Cargo.toml or build.rs change re-hashes the crate and reorders .text (reactor finding). Kernel instructions stay identical but loop alignment moves. Retake bench_ab references after S1 and S3, and with the GUI feature on.
- hot_loop_spills.py silently reports zero hot loops if the lib name or the build command drifts (`:97`, `:145`, `:213`). The S3 guard and the S0 fingerprint catch it; the fingerprint tool is new itself, so cross-check its first diff by hand.
- GUI starvation: workers default to High priority on every selected CPU (runner.rs:4458-4459) while the UI thread is Normal.
- The GUI has never been launched. Unverified: WinUI 3 in a console-subsystem exe with the detached manifest, the double-click on 26100, the delay-loaded start on 20348, and Reactor 0.100.0's preview limits (no timer, no close veto).
- Per-cycle saves rewrite the whole file, which grows with every cycle; S1 measured it and limits the saves to one a minute (doc/todo/72). A rename can fail on a sharing violation (warn, retry at the next save).
- Error storms: rescans per step are unbounded (tests.rs:3624-3629 caps only per rescan), so the GUI's bounded log channel drops lines. The file keeps every line, and the window says how many it dropped.
- Visibility widens: the payload types become pub and drop out of dead_code (lib.rs:6-9). Keep the API to session, frontend, commands, logfile, BUILD, request_stop and the re-exports.
- An elevated launch, now possibly a double-click, still grants SeLockMemoryPrivilege through secedit without asking (privileges.rs:398-409). Behaviour is unchanged by this split.
- TODO 28's mixed steps will need a work-item key on WorkResult (thread_pool.rs:59) and ThreadStepRecord. Today the coordinator takes thread_count results off one channel (runner.rs:1424), so the key is additive later.
- CLAUDE.md's asm and clippy command lines go stale at S3. They are owner-controlled, so the coordinator should ask before editing them.

## Rejected

- B's move of src/ into crates/engine (a ~48k-line git mv). It breaks every src/ path in TODO and docs and conflicts with TODO 28 in flight. The root-engine layout is verified: `cargo run`, `cargo build` and the asm glob all still work.
- B's IO-free tmr-text crate. It is needed only for a GUI-mode log transcript, which is out of this scope. tmr-gui can reuse tmr-cli's renderer later if wanted.
- B's custom logger replacing env_logger. It has to reproduce env_logger's escape stripping on redirected output (cli.rs:1504-1508) byte for byte, for no gain.
- B's move of the 500 ms WHEA poll into `recv_timeout` result waits (runner.rs:1349, 1424, 1838). It adds code at the worker result channel and leaves the coordinator's non-waiting stretches unpolled. The engine's existing monitor thread is kept instead.
- B's `&mut dyn Frontend` with no Sync. It is incompatible with the engine's monitor thread calling tick.
- B's results as a fold over events (Recorder) now. It restructures add_cycle before TODO 100 needs the cheap slice. The fold is TODO 72's 'better version', for later with replay.
- A's `Frontend::confirm` on the trait. Only the one-shot calibration asks, so a closure passed to that command keeps the trait to what a run needs. A default answer would repeat the silent-yes bug at cli.rs:1195-1197.
- A's GUI as the last stage. It moves to right after the run path, so the vocabulary is proven before the 1,500-line setup move.
- Structured MemoryError events from Rescan::bad (tests.rs:3763). That code sits next to the kernels' cold paths, and Rescan::word is inline(always) (tests.rs:3731). The log facade already carries those lines.
- Progress as events, like the demo's per-tick Progress (gui-test/reactor/src/engine.rs). The frontends read snapshot() on their own timers instead.
- A typed WheaRecord in the pending queue (whea.rs:258) and Commit error codes. Later; the strings are enough for v1.
- B's Arc<str> test name. It saves two small allocations a second, which isn't worth the churn.
- Extracting the thread-selection arithmetic into pure functions before the move (B and the inventory). Verbatim moves change only the prints, so the untested arithmetic is not rewritten.
- clippy disallowed-methods for stdin/stdout/stderr. A root clippy.toml would also apply to tmr-cli, which needs a shadow file; a grep in the S5 gate is lighter.
- Multiple runs per GUI process, async/tokio, and in-process serialisation of events. Excluded by the OnceLocks and by the owner's constraints.

## Open for the owner

- Three log lines print on every run labelled `tmr::cli` (cli.rs:581, 1512, 1513). Do we keep that label through the small parity helper, or accept a one-time change to the new module names?
- Text kept as it is on purpose. Fix each of these in its own commit, or keep it?
- 'Skipping first 1 CPU(s)' with skip-cores=0 (cli.rs:1603);
- ~~the Ctrl+C wording 'without the final report or results file', untrue once results save every cycle (cli.rs:964, 975)~~: corrected in S1;
- bold escape codes in redirected output (renderers/console.rs:21);
- ticker frames in WinRM captures.
- After the size test: store per-thread rows for every step of every cycle in pretty JSON, compact JSON, or per-test averages only?
- GUI responsiveness under cpus=100% at High worker priority: raise the window thread's priority during a run, or leave one core free in GUI mode?
- An elevated launch silently grants the large-page privilege through secedit (privileges.rs:398-409). Should it become a question, now that a double-click can trigger it?
- TODO 29's log transcript, which makes the log file hold everything the console showed: next after the split, or later? It changes what logs/*.log contains.
- GUI v1 input: is a single args box in the CLI's key=value syntax enough? Should it save to AppConfig.last_config, and should ui_mode/auto_start (app_config.rs:197-205) be wired or deleted?
- 'Run again' relaunches the exe (one run per process). Acceptable for v1?
- When should release builds include the GUI (`--features tmr-app/gui`: +1.6 MB, Windows App Runtime 2.x needed for the window)?
- For the preview stage, is `tmr --gui …` from a shell (setup text and the large-page prompt in the console) acceptable before the double-click launch exists?
- May the coordinator update CLAUDE.md's asm and clippy command lines (`-p tmr-app`, `--workspace`) when the workspace lands?

## Weakest parts

- Setup-path byte identity is proven only by verbatim moves plus the owner's A/B transcripts. Branches the dev box never takes (elevated without the privilege, hybrid CPUs, the restart prompt) are covered by neither.
- The S3 preview runs through a transitional `cli::run_session` that still exits the process on setup errors and reads stdin, so it validates the run vocabulary but not setup. Some of its glue is rewritten in S4.
- The S0 bridge relies on a perma-unstable std API and a cfg(test) shim in console.rs. The plan-only golden needs host-dependent lines masked, and only a run on the dev box will show whether its masking is enough.
- The module-path parity helper is a deliberate hack that keeps three log lines labelled `tmr::cli` from code that no longer lives there.
- asm_fingerprint.py is new and unproven. Its normalisation could hide a real change or flag noise; hot_loop_spills.py and a hand check of the first diff are the backstop.
- The per-cycle save cost and file size are estimates until the ignored test runs, and the fallback (compact or aggregates) is a schema choice made after S1 ships.
- Nothing about the GUI has been run: the detached-console manifest, the delay-load on 20348, ContentDialog from a blocked engine thread, and UI starvation at High worker priority are all unverified.
- The ordering rule (an event never spans a log call) is enforced by review and one coordinator-only order test, not by the compiler.
