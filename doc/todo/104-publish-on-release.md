# TODO 104: publish on release, crates.io pages and a winget package

Raised 2026-10-24 by the owner: at the first tagged release, publish every crate on crates.io and
put TMR in winget. The release itself stays GitHub Releases' (`release.yml`: `tmr.exe` with both
frontends, and `tmr.pdb`); crates.io and winget point at it.

## Why

- **crates.io** is for tracking and discovery, not installing: each page says where the app is (the
  GitHub releases), shows TMR's dependency chain (`windows-reactor` among it, so TMR is listed on
  its dependencies' pages), and lets someone build on the engine (a new frontend, say). The owner:
  all four crates, not the engine alone, so the chain shows Reactor (the window's dependency).
- **winget** is how TMR's users install on Windows. A manifest can name the two runtimes TMR needs
  as dependencies, so `winget install` brings them, and TMR still bundles neither (the README's
  Requirements: users keep shared runtimes current).

What other Rust apps do (crates.io, 2026-10-24): command-line tools publish (ripgrep, bat, fd,
starship, tokei, hyperfine, uv, ruff), beside their GitHub releases. Apps with a reusable core
publish it too, and the core is downloaded more than the app: Nushell's `nu` 456k and `nu-engine`
1.3M, Alacritty 266k and `alacritty_terminal` 2.2M, ripgrep's `grep` 6.5M and `ignore` 197M (a
dependent's builds count). Big GUI apps often don't (WezTerm and Zed aren't on crates.io; Helix
holds only a 2021 placeholder, the name-holding crates.io's policy now forbids).

## crates.io

**Names.** `tmr` and `tmr-cli` are taken (two CLI timers). Free on 2026-10-24: `test-memory-r`,
`test-memory-r-engine`, `test-memory-r-cli`, `test-memory-r-gui`, `tmr-engine`, `tmr-gui`,
`tmr-app`. The proposal, one family after the repo: the app `test-memory-r` (bin `tmr`), the engine
`test-memory-r-engine`, the console `test-memory-r-cli`, the window `test-memory-r-gui`. Each keeps
its lib name (`[lib] name = "tmr"`, `tmr_cli`, `tmr_gui`), so no code changes. The owner decides.

**The baseline check, first.** `cargo install` doesn't read the package's `.cargo/config.toml`, so
a crates.io build compiles at the default x86-64 level, and the kernels that take their 256-bit
width from the x86-64-v3 baseline come out as pairs of 128-bit instructions: slower, and not TMR
as tested, with no warning. A `RUSTFLAGS` in the repo does the same today (it replaces the config's
flags). So the engine stops the build: `#[cfg(not(target_feature = "avx2"))] compile_error!(...)`
at its root, naming the fix (`RUSTFLAGS="-C target-cpu=x86-64-v3" cargo +nightly install
test-memory-r`). CLFLUSHOPT stays the startup gate's: it isn't in any baseline.

**Each crate's package.**
- `version` beside each `path` dependency, and publish in dependency order: engine, CLI, window, app.
- The engine is the root package, so an `include` list (`src/`, `Cargo.toml`, the README and
  licences), or it would take `doc/`, `tools/` and the baselines. The app's needs `build.rs`,
  `tmr.ico` and `tmr.manifest`.
- `readme` a short crates.io page each: what the crate is, that the app is at the GitHub releases,
  that it's published for tracking and for building on; `repository`, `description`, `keywords`,
  `categories`, `license` (MIT OR Apache-2.0).
- docs.rs: `[package.metadata.docs.rs] targets = ["x86_64-pc-windows-msvc"]` (Windows-only crates).
- Versions: the workspace is 1.0.0. A library at 1.x promises a stable API, which the engine's
  isn't (the owner decides: 0.x for the libraries, or 1.x and major bumps on breaks).
- `cargo publish --dry-run` for each, then the owner publishes (`cargo login` with their token).
  A version can be yanked, never deleted.

## winget

- A manifest per release in `microsoft/winget-pkgs` (a pull request; `wingetcreate` generates
  one from the release's URL). `InstallerType: portable`: the release's bare `tmr.exe`, which
  winget puts on the PATH as `tmr`, with its SHA-256.
- `Dependencies: PackageDependencies`: `Microsoft.VCRedist.2015+.x64`, and
  `Microsoft.WindowsAppRuntime.2` with `MinimumVersion` 2.4 (Reactor 0.100's minimum; not
  `Microsoft.WindowsAppRuntime.2.0`, an older 2.0 release).
- The package identifier (`Publisher.Name`, for example `JarrodPetz.TMR`) is the owner's choice;
  it stays across releases.
- Each later release: a new manifest version (`wingetcreate update` with the new URL).
