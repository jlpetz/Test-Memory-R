# test-memory-r-engine

The engine of **TMR (Test Memory R)**, a DDR5 memory tester for Windows that remakes TM5 (TestMem5)
and runs its configs: the tests and their SIMD kernels, the memory they test (large and huge pages,
per NUMA node), a session's setup and run, and the results files. In code it is `tmr`.

**To test memory, install TMR** from [GitHub Releases](https://github.com/jlpetz/Test-Memory-R/releases):
an installer, or the bare `tmr.exe`. This crate and its siblings, `test-memory-r-cli` (the console),
`test-memory-r-gui` (the window) and `test-memory-r` (`tmr.exe`), are published so TMR's releases and
its dependencies can be tracked, and to build on: another frontend runs a session through
`tmr::session::run` and the `tmr::frontend::Frontend` it reports to.

```toml
[dependencies]
tmr = { package = "test-memory-r-engine", version = "0.1" }
```

Building it needs:

- **Windows** (`x86_64-pc-windows-msvc`) and **nightly Rust**: the engine uses `portable_simd` and
  other unstable features.
- **The x86-64-v3 baseline**, `RUSTFLAGS="-C target-cpu=x86-64-v3"`: the kernels take their 256-bit
  width from it, so a build without it stops. The repository's `.cargo/config.toml` sets it.

At run time the CPU needs AVX2 and CLFLUSHOPT, an Intel Skylake (2015) or AMD Zen (2017) or newer
CPU: `tmr::cpu_check` says whether it has them, and a session refuses to start without them.

TMR is pre-1.0: its API can change with each minor version.

Licensed under either of [MIT](https://github.com/jlpetz/Test-Memory-R/blob/main/LICENSE-MIT) or
[Apache-2.0](https://github.com/jlpetz/Test-Memory-R/blob/main/LICENSE-APACHE), at your option.
