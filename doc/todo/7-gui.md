# TODO 7. [TMR-APP] GUI Implementation

> The entry's original text, moved verbatim out of `TODO.md` on 2026-10-22, then that day's
> research, measurements and decisions. The short entry in `TODO.md` holds the current status.

## The original entry (2026-09-09 to 2026-09-27)

Options: egui/eframe, Tauri, Win32/WinUI, Slint. MVP: memory/CPU selection, progress, errors.

**Prerequisite, identified 2026-09-09: an engine/presentation split, and it is the hard part.**
Today the reporting layer *is* `println!` — output shape and output content are the same code
(`reporting/renderers/console.rs`, plus bare `println!` scattered through `main.rs`/`runner.rs`).
A GUI cannot reuse that, so it would either re-derive every number itself (guaranteeing it drifts
out of sync with the CLI) or scrape text. Both are how a GUI ends up showing *less* than the
console does.

What the split means concretely: the engine emits **structured events** (allocation resolved,
test started, cycle completed, error found, progress tick) and console text / `logs/*.log` /
`results/*.json` / the GUI all become renderers over that one stream. That is the same question
#29 is now parked on, so **#7 and #29 must be decided together** — see "Absorbed from #67 item 3"
under #29. Deciding the file layout first would likely mean redoing it when the GUI lands.

**Constraint from `TMR-APP/CLAUDE.md`**: TMR is "a testing tool, not a dashboard" — favour
throughput over real-time gauges. So the event stream must stay off the hot path: emitted on the
existing progress timers, never inline with memory operations. A GUI that polls structured events
is fine; a GUI that makes the test loop check whether anything is watching is not.

**GUI-era settings nothing reads (parked 2026-09-27, from #71):** `AppConfig`'s `last_config`,
`ui_mode` and `auto_start` (`app_config.rs` ~106-114) are saved and loaded but never acted on. Wire
them when the GUI is built, or delete them if its design has no use for them.

## 2026-10-22: purpose, framework, launch model

**Purpose (the owner).** A basic GUI now, to prove the engine/presentation split (#29, #72): a GUI
that renders the same engine output as the console shows the split is real. The full GUI comes
later. It is for the owner and for users.

**Framework: windows-reactor (WinUI 3), the owner's call.** The same mock TMR screen was built in
egui 0.37, windows-reactor 0.100.0 and Slint 1.18 (`../gui-test/`, not in git;
`show-side-by-side.ps1` opens them). Measured on the dev box (Server 2025, no GPU, so WARP software
rendering), CPU as % of one core, 20 s windows, a 2 Hz event feed, windows half the screen each:

| | egui (wgpu on WARP) | Reactor | dwm (compositor) |
|---|---|---|---|
| Both running | 34.6, 31.3 | 4.4, 5.4 | 0.6-1.1 |
| Alone (the other suspended) | 29.8, 27.5 | 3.7 | 0.2-0.5 |
| At a third of the screen | 16-18 | 5.7-6.3 | — |

egui redraws the whole window on every update, twice per `request_repaint` by design
(`egui-0.37.0/src/context.rs:141-144`), and on every mouse move: the owner saw 5-30% spikes just
moving the mouse over it, against near zero for Reactor. On a GPU most of egui's cost would move to
the GPU (not measured), but on the APU desktops the GPU shares the memory under test. Slint was
ruled out by the owner (look).

Costs accepted with Reactor:
- A preview API. The only crates.io release is 0.100.0 (2026-09-03); master has rewritten its
  internals since and its docs describe API the release lacks. Pin it and expect migrations.
- The Windows App Runtime 2.x (about 116 MB) must be installed; Reactor's own bootstrap shows a
  dialog with the download link when it's missing. Self-contained instead means about 56 MiB of
  DLLs beside the exe.
- The GUI needs Windows 11 (22000+) or Server 2025. Reactor 0.100.0 imports the Windows 11-only
  `TryCreatePackageDependency` the normal way (confirmed in the demo exe's import table), so below
  22000 the loader refuses the exe before `main()` (windows-rs issues 4866, 4924). Guard:
  delay-load `api-ms-win-appmodel-runtime-l1-1-5.dll` (linker flag) so the CLI still starts there,
  and check the build before opening the GUI. Server 2022 (the metal box) is CLI only. Not yet
  tested on 20348.
- Reactor doesn't need TMR's `windows` crate moved to 0.100: the two `windows-core` versions link
  side by side (checked). Keep that migration apart; it lands on the allocator code.

**Launch model: one exe (the owner).** The engine is its own crate whose output is the event
stream; the CLI and the GUI are separate crates rendering it, so the compiler enforces the split
inside one exe. The exe opens the GUI on a desktop launch and the CLI from a shell or with
redirected output (WinRM runs), `--cli`/`--gui` override. A console-subsystem exe with the
`consoleAllocationPolicy=detached` manifest opens no console on a double-click from Windows 11
24H2 / Server 2025 (build 26100); older builds flash a console briefly. TMR's panic hook and
vectored exception handler (`cli.rs:133-154`) log a crash before the process ends; the GUI shows
the reason the same way. If crash isolation ever matters, the same exe can run the engine as its
own child, with no second file.

## The owner's design notes for the GUI (2026-10-22, from the demos)

1. **Colours are a setting.** Some users will want egui's grey text, others Reactor's brighter
   white (the owner's pick). The owner prefers egui's darker, richer red for error bullets and for a
   non-zero error count in the results table. Reactor takes any colour (`Brush` from `Color`); it
   has only 8 theme brushes, so the rest are set per light/dark, as the demo already does.
2. **Progress bar:** egui's wider, rounded bar with the % inside, not WinUI's thin bar. Reactor
   0.100.0's `ProgressBar` has no height or rounding, so build it (a rounded `Border`, an inner one
   sized to the fraction, the % centred over it). **Timer** on the right, as in Reactor.
3. **Chart:** the hover readout is optional; drop it for now. Later: temperature and errors on
   the same chart, to see whether DIMM temperature brings errors. Error marks come from the error
   events. DIMM temperature is a data question: DDR5 DIMMs report it through the SPD hub's sensor
   on SMBus, which user mode can't read without a driver (TMR has none, a settled decision whose
   revival trigger is ring-0 observability). Without one: HWiNFO's shared memory when the user runs
   it. Decide when we get there.
4. **Chart placement:** at the top (egui's) or the bottom, never between the results and the
   errors list.
5. **System info, top right:** CPU make and model, total RAM, DIMM count, cache sizes, and a
   drop-down listing each DIMM with what TMR detects (`smbios.rs` `MemoryModule`: locator,
   manufacturer, part number, capacity, MT/s; caches from `tests::get_cache_info`). Reactor has
   `Expander`, `ComboBox` and `DropDownButton`.
6. **Timer font:** egui's, which is Hack (egui bundles `Hack-Regular.ttf`, MIT plus Bitstream Vera
   licence). Reactor 0.100.0's `TextBlock` has no font setting (only size, weight and colour); master
   has added `FontFamily`, so the next release should have it. Until then: wait for it, use master,
   or draw the timer with windows-canvas (which in 0.100.0 wakes every frame). Whether an unpackaged
   WinUI 3 app can load a bundled font file is still to check.
