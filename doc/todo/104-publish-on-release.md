# TODO 104: publish on release, crates.io pages and a winget package

Raised 2026-10-24 by the owner: at the first tagged release, publish every crate on crates.io and
put TMR in winget. The release itself stays GitHub Releases' (`release.yml`: `tmr.exe` with both
frontends, and `tmr.pdb`); crates.io and winget point at it.

## Decided (the owner, 2026-10-25)

A survey of the repo, the packages, winget and the installers (2026-10-25, each part checked by a
second agent) came first; the owner's calls on it:

- **0.1.0** for all four crates, released together: the API isn't stable, so no 1.x. The window's
  title and the console's banner read different crates' versions, so they stay equal.
- **Names**: `test-memory-r` (the app, bin `tmr`), `test-memory-r-engine`, `test-memory-r-cli`,
  `test-memory-r-gui`, lib names kept. winget: `Petz.TMR` (no `Petz` publisher in winget-pkgs on
  2026-10-25). Both are permanent once used.
- **Files beside the real exe**, not the current folder: `configs\`, `results\`, `logs\` and
  `tmr-cfg.json` (and the window's `colours.json`). `TMR_HOME` points them elsewhere: for `cargo
  run`, whose exe is under `target\` (`cargo clean` would take the results with it), the scripts
  and the test hosts. The current folder was wherever TMR started: the shell's, System32 for an
  Explorer-style launch or an elevated prompt, Documents over WinRM; and even `--help` made
  `logs\` there. Through winget's `Links` symlink, `current_exe()` gives the link's path, so the
  path is resolved first.
- **A light installer, Inno Setup**, which gives TMR its own folder with `docs\` and `configs\`
  in it: all users by default (UAC), "install for me only" offered on its first page and by
  `/CURRENTUSER`. For all users it's `C:\Program Files\TMR`, which users can't write, and TMR
  runs its tests unelevated, so the installer makes `configs\`, `results\`, `logs\` and
  `tmr-cfg.json` writable by users, never the exe's folder or the exe (anyone able to replace it
  could have an admin run their program). An upgrade never overwrites a config; an uninstall leaves
  what TMR and the user made. The bare `tmr.exe` stays a release asset, for TM5-style use.
- **winget** takes the installer (`InstallerType: inno`), listed twice, `Scope: machine` and
  `Scope: user`. winget's own preference (`installBehavior.preferences.scope`) defaults to `user`
  (winget-cli `UserSettings.h`), so a winget user gets a per-user install unless they set
  `machine` or pass `--scope machine`; the installer then asks UAC itself, so no elevated terminal
  is needed (that is a portable package's limit). Listing machine only would force it, and break
  `--scope user`.
- **No bundled runtimes** (the owner: each system keeps its own up to date), so no `+crt-static`:
  winget brings `Microsoft.VCRedist.2015+.x64` and `Microsoft.WindowsAppRuntime.2` (2.4+) as
  dependencies, which it installs for every installer type; the installer checks for them and says
  where to get them.
- **Results files keep the hardware IDs** (the SMBIOS system UUID, the DIMMs' serials): the UUID
  tells a user which system a file came from. The README says what a file holds.
- **Unsigned for 0.1.0.** winget installs carry a trusted mark, so SmartScreen doesn't stop them;
  a browser download gets "Run anyway". Signing (SignPath's free OSS program once there's a
  release, or a certificate) is for later; individual Artifact Signing is US and Canada only.

Found by the survey and to fix in 0.1.0: the window crate includes `../../tmr-app/tmr.ico` (it
can't package); LICENSE-APACHE held only the notice; the engine would ship `research/`, `doc/`,
`tools/` and `.mcp.json`; workspace profiles don't travel into a member's package, so a `cargo
install` of the app would get no LTO and `panic = unwind`; `--version` must exit and leave no file
behind (winget's checks run it, and fail a package whose uninstall leaves files); the privilege
check exported the security policy into the current folder.

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
flags). So the engine stops the build: a `compile_error!` at its root unless the whole x86-64-v3
set is on (AVX2 alone would let `-C target-feature=+avx2` past without BMI, FMA, LZCNT, MOVBE),
naming the fix (`RUSTFLAGS="-C target-cpu=x86-64-v3" cargo +nightly install test-memory-r`). Docs
builds are exempt: rustdoc never gets `build.rustflags`, and docs.rs builds a dependent's docs
with the engine as a plain dependency, so the engine's build script exempts a docs.rs build
(`DOCS_RS`) rather than each dependent setting `rustc-args`. CLFLUSHOPT stays the startup gate's:
it isn't in any baseline; the gate moves into the engine, so a frontend built on it has it too.

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
- Each later release: a new manifest version (`wingetcreate update` with the new URL), or a GitHub
  Action in `release.yml` that submits it on each tag.
- The first submission is Microsoft's to approve: their checks install the package on a test
  machine and scan it, then a moderator merges it. That is slow for a new package: of the
  new-package pull requests opened in September 2026, 483 had merged by 2026-10-25 (median 27.5
  days) and 725 were still open, so four weeks at best. Updates to a package merge in hours.

## An installer? (weighed 2026-10-24; Inno Setup chosen 2026-10-25, above)

A portable package (the bare `tmr.exe`) puts `tmr` on the PATH, which serves a console user, but
makes no Start menu shortcut, which a double-click user wants. The options, smallest first:

- **Portable only**: no work; the shortcut is the user's to make.
- **An MSI**, with `cargo-wix` (the Rust ecosystem's: it writes a WiX source from `Cargo.toml` and
  builds `tmr-<version>-x86_64.msi`): an entry in Settings > Apps and a clean uninstall; winget
  takes it as `InstallerType: wix`. But its release uses WiX v3, which is out of support, installs
  per machine only (Program Files), makes no Start menu shortcut without a hand-edited template,
  and its workspace support is an open issue.
- **Inno Setup**: a small script, a common choice for Windows tools; winget's `inno` type.
- **MSIX**: Windows' modern package, but it must be signed with a certificate the PC trusts (a
  paid one, or the Store's), and a packaged app runs with restrictions (a virtualised file system,
  among them) that TMR's large-page setup would need checking against. The least suited.

Code signing is its own question, whichever the package: an unsigned download gets SmartScreen's
"Windows protected your PC" until it builds a reputation, and a signed one does too until its
certificate has one (Microsoft's SmartScreen page); Smart App Control blocks unsigned apps however
they arrive.
