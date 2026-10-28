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
  `logs\` there. `current_exe()` gives the path the exe was started by, which can be a
  symlink or a short (8.3) name, so it is resolved first (`src/home.rs`).
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
  `--scope user`. But `--scope` asks the same of the dependencies, and neither runtime offers both
  (the 2026-10-26 manifest review): `--scope machine` fails where the Windows App Runtime is
  missing, `--scope user` where the VC++ runtime is. A plain `winget install Petz.TMR` installs
  them; the guide says to install them first for all users (0.1.1).
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

## 0.1.1: the first on crates.io and winget (the owner, 2026-10-28)

0.1.0 is on GitHub Releases only: its guide's winget advice was wrong, and the owner has app changes
for the next release, so 0.1.1 is the first version on crates.io and winget (0.1.0 isn't rebuilt:
it was public, and its published checksums would stop matching). Ready: release.yml's crates-io
and winget jobs (20b52f0, off until set up), the `release` environment (only `v*.*.*` tags deploy
to it, no reviewers; created 2026-10-28), the reviewed manifest in `dist/winget/` and
`tools/winget_manifest.py`. In order:

1. The version: `[workspace.package] version` and the three `=` pins in `[workspace.dependencies]`
   to 0.1.1, `TMR_BLESS=1 cargo test --workspace` for the two goldens' version lines, the checks;
   push; CI green; tag `v0.1.1`. release.yml checks the packages and uploads; its crates-io job is
   skipped (no `PUBLISH_CRATES`) and its winget job only says there's no token.
2. The owner tries the release on a desktop (Windows 11): the installer, for all users and for one;
   a run unelevated; an upgrade; an uninstall. Then `python tools/winget_manifest.py 0.1.1`, and
   with local manifests on (`winget settings --enable LocalManifestFiles`, elevated) a plain
   `winget install --manifest <its folder>` (no `--scope`: that installs both runtimes).
3. crates.io by hand (Publishing, above): an account with a verified email, a token
   (`publish-new`, `publish-update`, a few days), `cargo login`, then from a fresh clone of
   `v0.1.1` the dry run and `cargo +nightly publish --workspace`.
4. Each crate's trusted publisher (crate > Settings > Trusted Publishing > GitHub): owner `jlpetz`,
   repository `Test-Memory-R`, workflow `release.yml`, environment `release`; then the repository
   variable `PUBLISH_CRATES` = `true`. After the first release it publishes: "Require trusted
   publishing" on each crate, and the step 3 token revoked.
5. winget by hand: fork microsoft/winget-pkgs (jlpetz), a branch with the three files in
   `manifests/p/Petz/TMR/0.1.1/` and nothing else, the PR `New package: Petz.TMR version 0.1.1`
   with the template's boxes and a note for the validator: tmr.exe with no arguments starts a
   memory test that runs until stopped, so its executable check should run `tmr.exe --version`
   (exits 0, writes no file); the installer is unsigned; an uninstall leaves the user's configs,
   results and logs. The owner posts the CLA agreement on it. Four weeks or more.
6. Once it's merged: the bot account `petz-io` (made 2026-10-28, two-factor on; never a
   collaborator anywhere) has forked winget-pkgs (`petz-io/winget-pkgs`, master only); its classic
   PAT, `workflow` (which GitHub makes take all of `repo`: it reaches only the bot's own fork),
   with an expiry and a renewal reminder, becomes the `release` environment's secret `WINGET_TOKEN`
   (`gh secret set WINGET_TOKEN --env release --repo jlpetz/Test-Memory-R`). Its first pull request asks it to sign the CLA
   too. From then on each tag's winget job syncs the fork and opens the version's PR (Komac
   2.17.1: a newer Inno Setup in release.yml needs a newer Komac, checked with `komac update
   --dry-run` first).

A re-run of a release run is safe: the upload replaces no asset, the crates-io job leaves out what
crates.io has, and Komac opens no second PR for a version (one in any state stops it, even a
closed one: resubmit by hand, `--skip-pr-check`).

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
its lib name (`[lib] name = "tmr"`, `tmr_cli`, `tmr_gui`), so no code changes. The owner took it
(2026-10-25).

**The baseline check, first.** `cargo install` doesn't read the package's `.cargo/config.toml`, so
a crates.io build compiles at the default x86-64 level, and the kernels that take their 256-bit
width from the x86-64-v3 baseline come out as pairs of 128-bit instructions: slower, and not TMR
as tested, with no warning. A `RUSTFLAGS` in the repo does the same today (it replaces the config's
flags). So the engine stops the build: a `compile_error!` at its root unless the whole x86-64-v3
set is on (AVX2 alone would let `-C target-feature=+avx2` past without BMI, FMA, LZCNT, MOVBE,
POPCNT, XSAVE),
naming the fix (`RUSTFLAGS="-C target-cpu=x86-64-v3" cargo +nightly install test-memory-r`). Docs
builds are exempt: rustdoc never gets `build.rustflags`, and docs.rs builds a dependent's docs
with the engine as a plain dependency, so the engine's build script exempts a docs.rs build
(`DOCS_RS`) rather than each dependent setting `rustc-args`. CLFLUSHOPT stays the startup gate's:
it isn't in any baseline; the gate moves into the engine, so a frontend built on it has it too.

**Each crate's package (built in stage 2, 2b41455).**
- Names, version 0.1.0 and the exact pins (`=0.1.0`) between the four: the Decided list above.
- The engine is the root package, so an `include` list (`src/`, `build.rs`, its crates.io README,
  the licences and `testdata/`); each member carries copies of the licence texts and a README.
- docs.rs: `default-target = "x86_64-pc-windows-msvc"`, `targets = []`.
- The app's release profile doesn't travel with its package, so tmr.exe's release build stops
  without `panic = "abort"` (the part the code can see); its README gives the
  `CARGO_PROFILE_RELEASE_*` settings for `cargo install`.

**Publishing (the owner).** From a fresh clone of the tag in a new folder (`git clone --branch
v0.1.0 https://github.com/jlpetz/Test-Memory-R tmr-publish`), at its root: with `--manifest-path`
from elsewhere `.cargo/config.toml` is left out, and the baseline check stops the build. A fresh
folder matters: `cargo publish --workspace` checks each member against the members it packaged,
from a local registry built out of `target\package\*.crate` and unpacked under
`%USERPROFILE%\.cargo\registry\src\-<hash>\`, and cargo never rebuilds a registry crate it
built before. In the working tree an earlier package's `.crate` files or build made the CLI check
against the engine as it was the day before (2026-10-26); clearing them took `target\package`,
those `-<hash>` folders (never crates.io's `index.crates.io-*`) and a new `--target-dir`. Then
`cargo +nightly publish --workspace --dry-run`, `cargo login` with a crates.io token (an account
with a verified email), and `cargo +nightly publish --workspace`, which publishes the engine, the
CLI, the window and the app in that order, waiting for each (four new crates is within crates.io's
burst of five). Checked 2026-10-26 at dd106e4: the dry run packages and verifies all four. A
version can never be replaced, only yanked; a crate another depends on can't be deleted, so once
the app is out the others stay. Trusted publishing (`release.yml`, no stored token) can follow once
the crates exist.

## winget

- A manifest per release in `microsoft/winget-pkgs` (a pull request), `Petz.TMR`, schema 1.12:
  three files (version, defaultLocale, installer). `InstallerType: inno`, the release's
  `tmr-<version>-setup.exe` and its SHA-256 (`SHA256SUMS.txt` in the release), listed twice, `Scope:
  machine` with `InstallerSwitches: Custom: /ALLUSERS` and `Scope: user` with `/CURRENTUSER`, and
  `ElevationRequirement: elevatesSelf` (Inno asks UAC itself for all users). winget's silent
  switches for Inno are its defaults. `Commands: [tmr]`, `Moniker: tmr`, `MinimumOSVersion:
  10.0.20348.0`, `ProductCode` the installer's AppId (`{A1BC7167-67F4-461D-BE00-B57FBAF0B53C}_is1`).
- `Dependencies: PackageDependencies`: `Microsoft.VCRedist.2015+.x64`, and
  `Microsoft.WindowsAppRuntime.2` with `MinimumVersion` 2.4 (Reactor 0.100's minimum; not
  `Microsoft.WindowsAppRuntime.2.0`, an older 2.0 release).
- The first: `wingetcreate new <the installer's URL>`, then the fields above by hand, `winget
  validate`, a test in Windows Sandbox (`Tools\SandboxTest.ps1` in winget-pkgs), the pull request,
  and the Microsoft CLA. Its checks run the installed exe: `tmr --version` exits 0 and leaves no file.
- Each later release: `wingetcreate update Petz.TMR --version <v> --urls <url>` (it keeps the
  dependencies and scopes), or a job in `release.yml` (`needs: build`, winget-releaser or Komac, a
  classic PAT as a secret), which can only start once the first version is merged.
- The first submission is Microsoft's to approve: their checks install the package on a test
  machine and scan it, then a moderator merges it. That is slow for a new package: of the
  new-package pull requests opened in September 2026, 483 had merged by 2026-10-25 (median 27.5
  days) and 725 were still open, so four weeks at best. Updates to a package merge in hours.

## The release (stage 4)

`release.yml`, on a `v*.*.*` tag: refuses a tag that isn't the workspace's version, runs CI's clippy
and tests (CI doesn't run on tags), builds with `--locked`, stages the installer's files (`tmr.exe`;
`docs\`: `dist/docs/guide.md`, the licences, `THIRD-PARTY-NOTICES.html` from `cargo about` with
`dist/about.toml` and `about.hbs`; `configs\`: `dist/configs/README.txt` and the two demo configs,
which `tmr --create-demo-configs` writes with `TMR_HOME` on the stage), builds `dist/tmr.iss` with
Inno Setup 7.1 (a 64-bit setup, `SetupArchitecture=x64`, the owner 2026-10-26; the runners have only
6.7, so the job takes 7.1.0's release, pinned by its SHA-256), and uploads the installer, `tmr.exe`,
`tmr.pdb`, the notices and `SHA256SUMS.txt`. A tag with a `-` is a pre-release. For all users,
`{app}\configs`, `results`, `logs` and a `{}` `tmr-cfg.json` (`dist/tmr-cfg.json`, which TMR reads
as no settings) get `users-modify`; `{app}` and `tmr.exe` keep Program Files' ACL.

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
