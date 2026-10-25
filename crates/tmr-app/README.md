# test-memory-r

**TMR (Test Memory R)** is a DDR5 memory tester for Windows that remakes TM5 (TestMem5) and runs its
configs: `tmr.exe`, with a console and a window. See the
[project's page](https://github.com/jlpetz/Test-Memory-R) for what it tests and how.

**To test memory, install TMR** from [GitHub Releases](https://github.com/jlpetz/Test-Memory-R/releases):
an installer (for all users, or for you only), or the bare `tmr.exe`; or with winget,
`winget install Petz.TMR`, once its package is accepted. Those are the builds TMR is tested as.

The crates (`test-memory-r-engine`, `test-memory-r-cli`, `test-memory-r-gui` and this one) are
published so TMR's releases and its dependencies can be tracked, and to build on. `cargo install`
builds `tmr.exe` from them, but builds it as TMR's releases are only with the baseline and the
release profile they're built with, which a package can't carry (a build without them stops and says
so):

```powershell
$env:RUSTFLAGS = "-C target-cpu=x86-64-v3"
$env:CARGO_PROFILE_RELEASE_PANIC = "abort"
$env:CARGO_PROFILE_RELEASE_LTO = "true"
$env:CARGO_PROFILE_RELEASE_CODEGEN_UNITS = "1"
$env:CARGO_PROFILE_RELEASE_DEBUG = "line-tables-only"
cargo +nightly install test-memory-r --locked
```

That needs nightly Rust and Microsoft's C++ build tools. Running TMR needs the Visual C++
Redistributable (2015 or later, x64), and for the window, Windows 11 or Server 2025 and the Windows
App Runtime 2.4 or later. TMR keeps its configs, results and logs beside `tmr.exe`, which for `cargo
install` is `.cargo\bin`; set `TMR_HOME` to keep them elsewhere.

Licensed under either of [MIT](https://github.com/jlpetz/Test-Memory-R/blob/main/LICENSE-MIT) or
[Apache-2.0](https://github.com/jlpetz/Test-Memory-R/blob/main/LICENSE-APACHE), at your option.
