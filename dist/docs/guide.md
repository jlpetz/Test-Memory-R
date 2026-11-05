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
- **winget**, once TMR's package is accepted there: `winget install Petz.TMR`. winget installs it
  for you only, and brings the two runtimes below with it. For all users, install the runtimes
  first, `winget install Microsoft.VCRedist.2015+.x64 Microsoft.WindowsAppRuntime.2`, then
  `winget install Petz.TMR --scope machine`: with `--scope`, winget asks the same scope of each
  package TMR depends on, and neither runtime offers both.
- **The bare `tmr.exe`**, from the same releases: put it in a folder of its own that you can write
  to. TMR keeps its files beside it.

TMR needs:

- **Windows 11, or Server 2022 or later.** The window needs Windows 11 or Server 2025; the console
  runs on Server 2022 too.
- **A CPU from 2015 or later**: Intel 6th gen Core (Skylake) or AMD Ryzen (Zen) and later. TMR
  checks at startup, and says so on an older one.
- **The Microsoft Visual C++ Redistributable (x64)**, which most PCs have:
  `winget install Microsoft.VCRedist.2015+.x64`. Without it Windows won't start `tmr.exe`. TMR
  takes it from Windows' `System32` folder only, where the Redistributable puts it: a copy of its
  DLL beside `tmr.exe` isn't used.
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

That grants it to you, the account you're signed in as. To grant it to a group you're in, or to
another account, name it after `Group:` or `User:`:

```
tmr --setup-large-pages Group:Administrators
tmr --setup-large-pages User:.\alex
tmr --setup-large-pages "Group:Remote Desktop Users"
```

The prefix says which you mean, as a user and a group can share a name. A name alone is looked
for on this PC and in the domain it's in; `.\alex` is this PC's own account `alex`, and
`CORP\witches` names the domain's (`Group:CORP\witches`). Put a name with spaces in quotes. The
English names of Windows' own groups (`Administrators`, `Users`, `Everyone` and the like) work on
Windows in any language.

Run as administrator, TMR asks too, when its account doesn't have the privilege: whom to grant it
to, your account first, then the groups you're in, the broad ones last and warned of (Users, and
the groups Windows puts everyone who signs in a certain way in, such as Everyone). A broad pick
asks again: any program of any of those accounts could then lock memory away from Windows.

Then sign out of Windows and back in, or restart it: a privilege reaches your programs only at
your next sign-in (Microsoft: a restart isn't needed). From then on TMR has it in every run, run as
administrator or not, whether you sign in as an administrator or as a standard account. On a PC in a domain, a domain Group Policy that sets this
privilege replaces one granted on the PC.

Without it TMR warns and asks first, whenever `maxpage=` allows large pages (as it does unless you
change it): it runs on regular 4 KB pages, which are slower, only when you say so this time, or
when you give `minpage=regular maxpage=regular`, which asks nothing. Not run as administrator, it
asks whether to run on them this time or quit, to be started as administrator. No answer (an
empty line, or the window's question closed) quits, running nothing. `tmr --remove-large-pages`
takes the privilege away again (from you, or from the `Group:` or `User:` named).

## Running a test

- **Double-click TMR** (or its Start menu entry) for its window. Type any settings in its box, or
  pick a config with Load config, and press Start; once the run is over, Run again starts another.
  A finished step's row opens its report, and the final summary opens itself as the run ends. The
  Log tab has what the console would show; Plan, System, Threads and Memory what it prints as it
  starts; Compare compares two runs' results files. The window keeps its settings in
  `window.json` beside tmr.exe: the chart shown or hidden, the theme, whether a run starts as it
  opens (`tmr --gui --start` does that once), whether serial numbers are kept, the last run's
  parameters, its size and colours. Settings, in its `•••` menu, sets the theme, starting on open
  and the serial numbers. The line above the box, the header's last, says before a run what
  cache calibration it finds; the first run on a machine measures one before its tests (about 2
  minutes), or Calibrate the caches and the seal, in the same menu, measures it now, with the
  seal's fastest width.
- **From a console**, type `tmr`. On its own it runs the stability plan, 3 cycles of it; press
  Ctrl+C to stop it sooner, and it stops at its next checkpoint and writes its report. Some
  settings:

  | Setting | What it does |
  |---|---|
  | `plan=quick` | the stability tests once; `plan=all` runs every test (`tmr --help` lists the plans) |
  | `cycles=10`, `duration=3600` | stop after 10 cycles, or after an hour |
  | `memory=10%-from-available` | leave 10% of the free memory for Windows, and test the rest (the default) |
  | `cpus=50%` | test on half the CPUs: the cores Windows prefers first, spread evenly over the CPU's L3 caches |
  | `skip-cores=1` | leave the core Windows ranks lowest free, for Windows (the default; `skip-cores=0` uses every core) |
  | `config=my_tm5.cfg` | run a TM5 config, or a TMR `.json` one |
  | `errors=halt` | stop at the first error |
  | `calibrate=recheck` | probe the stored cache calibration before the tests (about 15 s) and measure it again if the cache has changed; `calibrate=always` measures every run, `calibrate=off` takes none (by default the first run on a machine measures one) |
  | `--no-serials` | leave the memory modules' serial numbers and the system's UUID out of the output, the log and the results file |

  `tmr --help` explains every setting.

The run ends with a verdict: no errors, or the errors found and where. A script can read it from
the exit code: 0 passed, 1 failed or found errors, 2 stopped before testing, 3 interrupted.

## Latency under load

Two tests measure what a memory setting does to a read's latency while the other cores keep the
memory busy, which the other latency tests, measured idle, don't see. They are in no plan yet, so
run them by name: `tmr test=Lat-Loaded-DRAM,Lat-Turnaround-DRAM`.

- **Lat-Loaded-DRAM** (about 10 s a cycle) times a read idle, while every other core writes to
  memory as fast as it can, and while they read as fast as they can. On a Ryzen 7 5700X, raising
  tWR moved the second of these, and no other test.
- **Lat-Turnaround-DRAM** (about 50 s) does the same, then holds the other cores to 25%, 50% and
  75% of the lower of those two speeds, writing and then reading at the same speed. Its
  turnaround at each level is the latency under writes less that under reads: what writes cost a
  read beyond the time they take. There, raising tRDWR moved the turnaround at 50% and 75%, and no
  other test.

Each figure is a median, with the lowest and highest in brackets: a change is real when it clears
them. A level's share is of each run's own speed, which a faster setting raises, so to compare two
settings at the same load, give the second run the `load-levels=` the first one's report prints.
`load-pairs=` and `load-window-ms=` set how many windows they take and how long; `tmr --help`
explains each. Each thread tests 1 GiB of its memory (more on a CPU with a large L3), and the
figures compare only between runs on the same page size (large pages, which TMR takes when it
can).

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
compares two results files, by their names in `results\` or by their paths. It matches the two
runs' tests by name (a TM5 config's by test number too): it lists the tests only one of them ran
and those that ran with other settings, then how each test both ran changed.

`tmr --show-results <file>` shows a run's end again, from its results file (found the same way):
a line saying how the run ended (it finished, was stopped, reached its time limit or halted on
errors, with its verdict; or it never finished, killed or crashed, and its figures go up to its
last saved cycle), which run it was, then the final summary as the console showed it. A file from
another version of TMR is shown in this version's layout, and TMR says so above it.

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
