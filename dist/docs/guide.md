# TMR user guide

TMR (Test Memory R) tests whether your memory, and its overclock, is stable. It remakes TM5
(TestMem5) for today's Windows and hardware, and runs TM5's `.cfg` files. This guide is for TMR
0.1, its first public release: it works, but it is young, and its options and files may still
change between versions. The project's page has the details:
<https://github.com/jlpetz/Test-Memory-R>.

> **An unstable overclock can freeze, crash or restart your PC while TMR runs**: that is what a
> memory test is for. Save your work and close other programs first.

## Installing

- **The installer** (`tmr-<version>-setup.exe`, from
  [GitHub Releases](https://github.com/jlpetz/Test-Memory-R/releases)) installs for all users, in
  `C:\Program Files\TMR`, after a UAC prompt; or choose "Install for me only" on its first page, in
  `%LOCALAPPDATA%\Programs\TMR`, with no prompt. It adds TMR to the Start menu and `tmr` to the PATH,
  and Settings > Apps uninstalls it.
- **winget**, once TMR's package is accepted there: `winget install Petz.TMR`. winget installs for
  you only unless you ask for all users (`--scope machine`), and brings the two runtimes below.
- **The bare `tmr.exe`**, from the same releases: put it in a folder of its own that you can write
  to. TMR keeps its files beside it.

TMR needs:

- **Windows 11, or Server 2022 or later.** The window needs Windows 11 or Server 2025; the console
  runs on Server 2022 too.
- **A CPU from 2015 or later**: Intel 6th gen Core (Skylake) or AMD Ryzen (Zen) and later. TMR
  checks at startup, and says so on an older one.
- **The Microsoft Visual C++ Redistributable (x64)**, which most PCs have:
  `winget install Microsoft.VCRedist.2015+.x64`. Without it Windows won't start `tmr.exe`.
- **The Windows App Runtime 2.4 or later**, for the window only: TMR offers to download it when
  it's missing, or `winget install Microsoft.WindowsAppRuntime.2`.

TMR bundles neither runtime, on purpose: each is shared by many programs, so your PC keeps one
copy of each up to date.

## Large pages, once

TMR tests fastest on large (2 MB) and huge (1 GB) pages, which need the "Lock pages in memory"
privilege. Grant it once, from a console run as administrator:

```
tmr --setup-large-pages
```

Then sign out of Windows and back in, or restart it: a privilege reaches your programs only at
your next sign-in. Without it TMR warns, and asks before it runs on regular 4 KB pages, which are
slower.

## Running a test

- **Double-click TMR** (or its Start menu entry) for its window. Type any settings in its box, and
  press Start.
- **From a console**, type `tmr`. On its own it runs the stability plan, 3 cycles of it; press
  Ctrl+C to stop it sooner, and it stops at its next checkpoint and writes its report. Some
  settings:

  | Setting | What it does |
  |---|---|
  | `plan=quick` | the stability tests once; `plan=all` runs every test (`tmr --help` lists the plans) |
  | `cycles=10`, `duration=3600` | stop after 10 cycles, or after an hour |
  | `memory=10%-from-available` | leave 10% of the free memory for Windows, and test the rest (the default) |
  | `config=my_tm5.cfg` | run a TM5 config, or a TMR `.json` one |
  | `errors=halt` | stop at the first error |

  `tmr --help` explains every setting.

The run ends with a verdict: no errors, or the errors found and where. A script can read it from
the exit code: 0 passed, 1 failed or found errors, 2 stopped before testing, 3 interrupted.

## TM5 configs

TMR loads TM5's `.cfg` files and runs the equivalent tests: `tmr config=path\to\file.cfg`. Put the
ones you use in TMR's `configs\` folder (below) and run them by name, `tmr config=file.cfg`: a
config that isn't in the folder you're in is looked for there. `configs\` also has two examples
of TMR's own `.json` format.

## Where TMR keeps its files

Beside `tmr.exe`, in its folder:

| | What it holds |
|---|---|
| `configs\` | your configs, and the examples |
| `results\` | one `TMR_<date>_<time>.json` a run, saved as the run goes, so a crash keeps the cycles saved so far |
| `logs\` | one `TMR_<date>_<time>.log` a run, the same name as its results file |
| `tmr-cfg.json` | what TMR's calibrations (`tmr --calibrate-all`) measured, and of which system |

The run's last lines say where its results file went. `tmr --compare-results <one> <other>`
compares two results files, by their names in `results\` or by their paths.

A results file describes the system it came from, so runs can be told apart: the CPU, the BIOS,
the memory modules (their part numbers and serial numbers) and the motherboard's system UUID.
Bear that in mind before you post one.

To keep the files somewhere else, set the `TMR_HOME` environment variable to that folder
(`setx TMR_HOME D:\tmr`, then open a new console).

## Uninstalling

Settings > Apps > TMR (Test Memory R), or `winget uninstall Petz.TMR`. Your configs, results, logs
and `tmr-cfg.json` stay in TMR's folder; delete it when you no longer want them.

## Help

Report a problem at <https://github.com/jlpetz/Test-Memory-R/issues>, with the run's results
file and its log, your Windows version and your CPU.
