# Setting up a development machine

What a new Windows machine needs to build, test and check TMR as CI does, and what isn't in the
repository (the owner, 2026-11-03: if this machine is lost, check the code out and resume quickly).

## In one go

```
git clone https://github.com/jlpetz/Test-Memory-R.git
cd Test-Memory-R
powershell -ExecutionPolicy Bypass -File tools\setup_dev.ps1 -Check   # what's there and what's missing
powershell -ExecutionPolicy Bypass -File tools\setup_dev.ps1          # installs what's missing
python tools\ci_local.py                                              # CI's checks, here
```

`setup_dev.ps1` installs only what is missing, from winget and crates.io; `-Window`, `-Installer` and
`-Debugger` add the optional ones below. Run it from an elevated prompt the first time: the C++ build
tools' installer asks for it.

## What it installs, and why

| Tool | For | From |
|---|---|---|
| Git, the GitHub CLI | the repository; CI's runs and the releases (`gh run watch`) | winget `Git.Git`, `GitHub.cli` |
| Python 3 | `tools\*.py` (the standard library only) | winget `Python.Python.3.13`, or any Python 3 already there |
| The C++ build tools | the MSVC linker and the Windows SDK | any Visual Studio 2022 or later with the C++ tools; else winget `Microsoft.VisualStudio.BuildTools` with the VCTools workload |
| Windows SDK 10.0.28000.2957 | the same SDK libraries locally and in CI; its directory is named 10.0.28000.0 | Microsoft's installer, with the version and checksum in `tools/windows_sdk.json`; `setup_dev.ps1` installs it when missing |
| rustup, the nightly with rustfmt and clippy, as the default toolchain | the engine uses nightly features (`portable_simd`, the CLFLUSHOPT and AMX intrinsics); the nightly isn't pinned (CLAUDE.md), so CI builds with each new one | winget `Rustlang.Rustup` |
| cargo-nextest, cargo-audit | CI's tests (a process a test) and its advisory check | `cargo install --locked` |
| cargo-about | release.yml's third-party notices | `cargo install --locked` |
| Optional, `-Window`: the Windows App Runtime 2 | running the window (not building it) | winget `Microsoft.WindowsAppRuntime.2` |
| Optional, `-Installer`: Inno Setup 7, at 7.1.0 | building `dist\tmr.iss`, as release.yml pins it | winget `JRSoftware.InnoSetup.7` |
| Optional, `-Debugger`: WinDbg | crash dumps | winget `Microsoft.WinDbg` |

Windows Server 2022 doesn't ship winget: install App Installer first, or the list above by hand.

## Then

- **Before a push:** `python tools\ci_local.py` runs CI's cargo steps with its commands, read from
  `.github/workflows/Rust-nightly-build.yml` (`--release`: release.yml's). What only CI does (the
  installer, the checksums, the upload, the crates.io and winget jobs) runs on a tag, in GitHub.
- **Building for development testing:** `python tools/build_release.py --locked` builds locally and
  records the compiler, SDK, settings and artifact hashes in `target/release/tmr.build.json`; it
  does not wait for CI. Then `.\target\release\tmr.exe`. A built
  exe keeps its configs, results and logs beside itself; `$env:TMR_HOME = "$PWD"` keeps them in the
  repository's `results\` and `logs\`, which git ignores.
- **Large pages:** `tmr.exe --setup-large-pages` from an elevated prompt once, then sign out and back in.

## Not in the repository

The repository holds the code and its documentation. These live beside it or in the user profile, so
a lost machine loses them unless they are kept somewhere else:

- `CLAUDE.md` and `AGENTS.md` (gitignored), the coding agents' instructions, and `..\CLAUDE.md`, the
  workspace's;
- `.claude\` (gitignored): the agents' local settings and the project's skills;
- `..\TODO.md`, `..\TODO_ARCHIVE.md` and `..\todo_check.py`, the task list (its detail files are in
  `doc\todo\`, in git);
- `..\TM5\` and `..\TM5_Source\`, the reference TM5 sources and configs, and the probe crates
  `..\*-test*\`;
- `..\baselines\<date>\`, the hardware campaigns' results and logs, kept to compare a later build against (they
  carry machine IDs, so they stay out of git), with `..\hw-pass\`, the kit that runs a campaign (its `README.md`:
  "Comparing a later run"), and `..\builds\<commit>[-dirty]-<exe-hash>\`, the EXE, PDB and manifest of
  each passing `tools/ci_local.py` run (older build directories remain where they were);
- the agents' memory, in the user profile (`%USERPROFILE%\.claude\projects\<the repository's path>\memory\`).

No secret is in the repository: the release's tokens are GitHub secrets.
