# test-memory-r-gui

The window of **TMR (Test Memory R)**, a DDR5 memory tester for Windows that remakes TM5 (TestMem5)
and runs its configs: a run's setup, progress and results in WinUI 3, through
[windows-reactor](https://crates.io/crates/windows-reactor). In code it is `tmr_gui`; `tmr.exe` opens
it with `tmr_gui::main` on a double-click or `--gui`. It needs Windows 11 or Server 2025 (build 22000
or later) and the Windows App Runtime 2.4 or later.

**To test memory, install TMR** from [GitHub Releases](https://github.com/jlpetz/Test-Memory-R/releases):
an installer, or the bare `tmr.exe`. The crates (`test-memory-r-engine`, `test-memory-r-cli`, this
one and `test-memory-r`) are published so TMR's releases and its dependencies can be tracked, and to
build on.

Building it needs Windows, nightly Rust and the x86-64-v3 baseline
(`RUSTFLAGS="-C target-cpu=x86-64-v3"`), as the engine's page says. TMR is pre-1.0: its API can
change with each minor version.

Licensed under either of [MIT](https://github.com/jlpetz/Test-Memory-R/blob/main/LICENSE-MIT) or
[Apache-2.0](https://github.com/jlpetz/Test-Memory-R/blob/main/LICENSE-APACHE), at your option.
