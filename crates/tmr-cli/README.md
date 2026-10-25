# test-memory-r-cli

The console of **TMR (Test Memory R)**, a DDR5 memory tester for Windows that remakes TM5 (TestMem5)
and runs its configs: a session on the console, with its setup's report, the live progress line, the
end-of-run summary, its log file and Ctrl+C. In code it is `tmr_cli`; `tmr.exe` calls
`tmr_cli::main`.

**To test memory, install TMR** from [GitHub Releases](https://github.com/jlpetz/Test-Memory-R/releases):
an installer, or the bare `tmr.exe`. The crates (`test-memory-r-engine`, this one,
`test-memory-r-gui` and `test-memory-r`) are published so TMR's releases and its dependencies can be
tracked, and to build on.

Building it needs Windows, nightly Rust and the x86-64-v3 baseline
(`RUSTFLAGS="-C target-cpu=x86-64-v3"`), as the engine's page says. TMR is pre-1.0: its API can
change with each minor version.

Licensed under either of [MIT](https://github.com/jlpetz/Test-Memory-R/blob/main/LICENSE-MIT) or
[Apache-2.0](https://github.com/jlpetz/Test-Memory-R/blob/main/LICENSE-APACHE), at your option.
