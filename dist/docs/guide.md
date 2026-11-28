# TMR user guide

TMR (Test Memory R) tests whether your memory, and its overclock, is stable. It is inspired by TM5
(TestMem5), built for today's Windows and hardware, and runs TM5's `.cfg` files. This guide is for TMR
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
  first (the two commands below), then `winget install Petz.TMR --scope machine`: with `--scope`,
  winget asks the same scope of each package TMR depends on, and neither runtime offers both.
- **The bare `tmr.exe`**, from the same releases: put it in a folder of its own that you can write
  to. TMR keeps its files beside it ([run as administrator, elsewhere](#where-tmr-keeps-its-files)).

TMR needs:

- **Windows 11, or Server 2022 or later.** The window needs Windows 11 or Server 2025; the console
  runs on Server 2022 too.
- **A CPU from 2015 or later**: Intel 6th gen Core (Skylake) or AMD Ryzen (Zen) and later. TMR
  checks at startup, and says so on an older one.
- **The Microsoft Visual C++ Redistributable (x64)**, which most PCs have (the first command
  below). Without it Windows won't start `tmr.exe`. TMR takes it from Windows' `System32` folder
  only, where the Redistributable puts it: a copy of its DLL beside `tmr.exe` isn't used.
- **The Windows App Runtime 2.4 or later**, for the window only: TMR offers to download it when
  it's missing, or use the second command below.

On a new PC, these install both runtimes: the console needs the first, the window both.

```
winget install --id Microsoft.VCRedist.2015+.x64 --exact --source winget
winget install --id Microsoft.WindowsAppRuntime.2 --exact --source winget
```

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
when you give `minpage=reg-page maxpage=reg-page`, which asks nothing. Not run as administrator, it
asks whether to run on them this time or quit, to be started as administrator. No answer (an
empty line, or the window's question closed) quits, running nothing. `tmr --remove-large-pages`
takes the privilege away again (from you, or from the `Group:` or `User:` named).

### Running as Administrator

Once the privilege is granted and you have signed in again, run TMR normally: it needs
administrator rights only to grant or remove the privilege. Started as administrator anyway (Run
as administrator, an elevated console, or an account that is always elevated, such as the
built-in Administrator), TMR still runs, with two differences:

- **It keeps its files in a home of its own**, `%ProgramFiles%\TMR Admin` unless `TMR_HOME` names
  another, not beside `tmr.exe`: its own configs, results, logs, calibrations and window settings
  (see [Where TMR keeps its files](#where-tmr-keeps-its-files)).
- **It asks before it tests**, after the large-page check: "Run memory tests as Administrator
  using <folder>?" Only yes runs the tests; anything else exits, having run nothing. In the
  window, **Settings → Allow administrator runs without asking** stops the question for your
  account, in that home, from when you switch it on. On the console, `--allow-admin` does the
  same for one run, for example `tmr --allow-admin plan=quick`, and a run with no console to
  answer from (a script, a scheduled task, a remote session) needs it: without it, the run exits
  with status 2. A config can't set it.

`--setup-large-pages` and `--remove-large-pages` never ask, and granting the privilege ends the run
so you can sign in again.

## Running a test

After installation and the large-page setup, these commands cover the usual starting points:

| Command | What it runs |
|---|---|
| `tmr plan=quick` | the stability tests once |
| `tmr plan=stability` | the stability tests for three cycles |
| `tmr config=my_tm5.cfg` | the tests and cycles from a config; see [Configurations](#configurations) |

The built-in plans leave 10% of available memory free by default and test the rest. `memory=`
changes that; a config can supply its own allocation settings. The first run on a machine also
measures the cache calibration, which takes about two minutes before testing starts.

A run tests every thread's whole share or none. When Windows can't find enough 1 GB and 2 MB pages
(they fragment as Windows runs), TMR finishes each thread on 4 KB pages, up to 5% of its share,
locked in RAM as the large pages are. A thread short by more stops the run before testing, saying
by how much and what to change: a bigger reserve (`memory=`), fewer threads (`cpus=`), a restart,
or another `minpage=`:

| `minpage=` | 4 KB pages |
|---|---|
| `reg-lock` (the default) | where the large pages run short, locked in RAM, up to 5% of a thread's share |
| `reg-page` | whatever the large pages leave, pageable: Windows may page them out under pressure |
| `large` | none: a thread the large pages leave short stops the run |

The Memory report's 4 KB column says which kind a run took.

- **Double-click TMR** (or its Start menu entry) for its window. Type any settings in its box, or
  pick a config with Load config, and press Start; once the run is over, Run again starts another.
  A finished step's row opens its report, and the final summary opens itself as the run ends. The
  Log tab has what the console would show; Plan, System, Threads and Memory what it prints as it
  starts; Compare compares two runs' results files.
  The window saves its preferences in `settings\window.json` in [TMR's home](#where-tmr-keeps-its-files).
  Settings controls the theme, language, automatic start, optional sounds and whether to include
  hardware identifiers. The log and results stay in English. If the window was run as
  administrator, Settings also controls whether to ask before testing. TMR remembers the last
  parameters and the window position and size, including whether it was maximized.
  The menu beside Start provides file actions, calibration, Settings and About.
  Settings can check GitHub for a newer release and open its page.
  When a previous run did not finish, the window can show the last saved progress, errors and
  readings. Dismiss that message, then choose Start when ready. The line above the parameters shows whether this machine has a cache calibration.
  The first run measures one when needed, and the menu can measure it separately.
- **From a console**, type `tmr plan=stability` for the stability plan, 3 cycles of it: any
  setting starts a run, and `tmr` alone lists every setting and starts nothing. Press Ctrl+C to
  stop a run sooner, and it stops at its next checkpoint and writes its report. In the classic
  Windows console, which draws TMR's emoji as boxes, a WARNING at the run's start and end says
  so, and how to get Windows Terminal, which draws them. Some settings:

  | Setting | What it does |
  |---|---|
  | `plan=quick` | the stability tests once; `plan=all` runs every test (`tmr --help` lists the plans) |
  | `cycles=10`, `duration=3600` | stop after 10 cycles, or after an hour |
  | `memory=10%-from-available` | leave 10% of the free memory for Windows, and test the rest (the default) |
  | `cpus=50%` | test on half the CPUs: the cores Windows prefers first, spread evenly over the CPU's L3 caches |
  | `skip-cores=1` | leave the core Windows ranks lowest free, for Windows (the default; `skip-cores=0` uses every core) |
  | `config=my_tm5.cfg` | run a TM5 config, or a TMR `.json` one |
  | `record=compact` | a smaller results file for long runs: per-thread rows only where errors were, unindented; `record=off` keeps no results file and no log file |
  | `errors=halt` | stop at the first error |
  | `errors=halt:100` | go on past errors, and stop once the run has found 100 (about: it is counted twice a second) |
  | `calibrate=recheck` | probe the stored cache calibration before the tests (about 15 s) and measure it again if the cache has changed; `calibrate=always` measures every run, `calibrate=off` takes none (by default the first run on a machine measures one) |
  | `--serials` | keep the memory modules' serial numbers, the system's UUID and the machine's fingerprints in the output, the log and the results file (left out by default) |

  `tmr --help` explains every setting.

## Reading a result

The console and window report the same three error sources:

| Source | What it means |
|---|---|
| Test | the test read data that differed from what it expected |
| Seal | a check found changed data left sealed between tests or chunks; see [the seal](tests/seal-and-bench.md) |
| WHEA | Windows reported a hardware error during the run; its event details identify the reported component |

Any of these fails the step or run. A failed check marked **"not reproduced on rescan"** still
counts: the first check saw a mismatch, even though a later read did not. Error counts can
include repeated observations of the same word; they are not a count of distinct damaged cells.
The [stability test guide](tests/stability.md#how-they-check) explains the checks and rescans.

A completed run with no errors passed the work it performed. An interrupted or crashed run has
only partial results, even if no errors were recorded. `errors=halt` stops on the first failed
check before TMR overwrites the failing chunk again.

A script can read the exit code: 0 passed, 1 failed or found errors, 2 stopped before testing,
3 interrupted. Errors still give code 1 if the run was also interrupted.

## What each test does

The [test reference](tests/README.md) lists the plans and every test, with its defaults. Each
family has a guide explaining its purpose, how it runs and how to read its report:

| Guide | What it covers |
|---|---|
| [Stability tests](tests/stability.md) | data patterns, moving inversions, strides, random accesses and block moves |
| [Refresh and exchange](tests/refresh-and-exchange.md) | data left waiting, data passed between cores and shared-line traffic |
| [The seal and Bench tests](tests/seal-and-bench.md) | checking data between steps and measuring pattern generation and verification |
| [AMX tile test](tests/amx.md) | the experimental matrix-unit test on CPUs with AMX |
| [Latency](tests/latency.md) | access latency in each cache tier and DRAM |
| [Latency under load](tests/loaded-latency.md) | read latency while other cores read or write |
| [Bandwidth](tests/bandwidth.md) | read, write and copy throughput at each memory tier |

Use `plan=stability` or `plan=quick` to look for errors. `plan=perf` measures performance; its
latency and bandwidth tests do not verify data. Its summary opens on a table of each cache tier's
and the memory's bandwidth and latency, and one of the memory's latency under load:
[reading the summary](tests/README.md#reading-the-summary). See [choosing tests yourself](tests/README.md#choosing-tests-yourself)
to run individual tests or compare SIMD widths.

## Latency under load

Two tests measure what a memory setting does to a read's latency while the other cores keep the
memory busy, which the other latency tests, unloaded, don't see. How to read their table, what the
turnaround's sign means and why their chart steps: [tests/loaded-latency.md](tests/loaded-latency.md).
`plan=perf` runs them after those; on their own, run them by name:
`tmr test=Lat-Loaded-DRAM,Lat-Turnaround-DRAM`.

- **Lat-Loaded-DRAM** (about 10 s a cycle) times a read idle, while every other core writes to
  memory as fast as it can, and while they read as fast as they can. Its two loaded rows swing 10-15%
  between boots, so they show only large changes: on a Ryzen 7 5700X (DDR4, two DIMMs a channel)
  raising tRRD_L from 4 to 6 lifted reads under flat-out reads to 356-392 ns, above all 15 runs at
  other settings (302-350). Compare several runs.
- **Lat-Turnaround-DRAM** (about 50 s) does the same, then holds the other cores to 25%, 50% and
  75% of the lower of those two speeds, writing and then reading at the same speed. Its
  turnaround at each level is the latency under writes less that under reads: what writes cost a
  read beyond the time they take. It holds within about 0.3 ns from boot to boot there, and raising
  tRDWR from 8 to 12 moved it 12 ns at 50% and 24 ns at 75%, and no other test.

Each figure is a median (a turnaround's taken so that a slow drift, as the memory warms, cancels),
with the lowest and highest in brackets: a change is real when it clears them. A level's share is
of each run's own speed, which a faster setting raises, so to compare two settings at the same
load, give the second run the `load-levels=` the first one's report prints.
`load-pairs=` and `load-window-ms=` set how many windows they take and how long; `tmr --help`
explains each. Each thread tests 1 GiB of its memory (more on a CPU with a large L3), and the
figures compare only between runs on the same page size (large pages, which TMR takes when it
can). They also compare only at the same `cpus=` and `skip-cores=`, as any bandwidth figure does:
those set how many cores load, which the report shows.

## Optional timings and temperatures

TMR can read memory timings and DIMM temperatures from ZenTimings and LibreHardwareMonitor.
Install the tools from the window's **Timings tools** menu item or run `tmr --get-readings`
from a standard-user session. TMR checks the downloaded archives against pinned SHA-256 hashes.
They are unpacked under `%LOCALAPPDATA%\TMR\companions`, or under `companions` in an explicit
`TMR_HOME`. Use `tmr --get-readings --launch` to start the tools and handle their Windows prompts.

- In ZenTimings, enable **Live Snapshot** and auto refresh. TMR checks the snapshot beside the
  downloaded copy. Set `TMR_TIMINGS` to the JSON file for a different installation.
- In LibreHardwareMonitor, enable its web server on this PC on port `8085`.

When a source is available, a run stores its startup sample in the optional `external` field of the
results JSON. Existing results remain readable. Compare shows changes in the primary timings.
The window refreshes its readout during a run. Timing changes and temperatures are informational.

## Configurations

`config=` loads either a TM5 `.cfg` or a TMR `.json` file. A relative path is looked for in the
current folder, then in TMR's `configs\` folder. Put the configs you use there and run them by
name:

```
tmr config=my_tm5.cfg
tmr config=my_tm5.cfg cycles=1 errors=halt
tmr config=my_tm5.cfg --startup-debug
```

Explicit command-line settings override the config. The last command validates the settings
and prints the startup plan without allocating test memory or running tests. A config supplies
its own test list, so `config=` cannot be combined with `plan=`.

### Importing a TM5 config

TMR keeps the test numbers, sequence and repeated entries, and translates the timing and chunk
settings. There are deliberate differences:

- **Memory coverage.** Every imported test covers the full allocation. TM5's Testing Window
  sizes chunks; it does not limit the amount of memory the test covers in TMR.
- **Rounding.** Each thread's share rounds down to the config's Lock Memory Granularity (at
  least 16 MB, in steps of 16 MB), as TM5 does, so the config's Reserved Memory for Windows is
  kept. TMR's own plans round up to whole GB, which can take part of the reserve. `blkround=` and
  `blkroundtarget=` override either.
- **2 MB pages** are asked for 64 MB at a time (TMR's own plans: 128 MB), so when they run short
  the threads end closer together. TM5 has no such setting; `largechunk=` overrides it.
- **Test 0.** An enabled RefreshStable test 0 becomes the seal around the tests, even when 0 is
  absent from Test Sequence. A 0 in the sequence also adds an explicit Seal-Check step.
- **Patterns.** SimpleTest modes 0, 1 and 2 become native pattern programs:

  | TM5 mode | TMR program |
  |---|---|
  | 0 | `ident, switch, random, switch` |
  | 1 | `linepair, switch, random, switch` |
  | 2 | `lines, switch, random, switch` |

  Each family runs as a pattern/inverse pair. The pairs advance through the program over write
  rounds and suite cycles, with new keys as the run progresses. The original Pattern Param0/1
  values are reported, but do not feed the native generators.

The startup report shows the pattern substitutions; the log has the original fields and the
effective settings. `patterns=legacy` selects TMR's older numbered adaptations for comparison,
not exact TM5 reproduction.

Otherwise TMR reads a `.cfg` as TM5 does:

- **Tests and Test Sequence.** Only `[Test0]` to `[Test(Tests-1)]` are read (Tests is 6 when
  missing), and a Test Sequence entry at or above Tests is skipped. Without a Test Sequence, TM5's
  default one runs: `6,12,14,2,15,10,5,1,4,3,0,13,9,7,8,1,11`. Anything but a digit separates the
  numbers (a letter is refused as a typo); the first 127 characters and the first 32 numbers count,
  and a number keeps its lowest byte, as TM5 stores it, so 258 is test 2.
- **Missing values.** A missing key, an empty number and a missing section take TM5's default:
  Cycles 3, Tests 6, Channels 2, Testing Window Size 1024 MB, Lock Memory Granularity 16 MB. An
  empty text value stays empty, as TM5 0.12.3 reads it: an empty `Function=` on an enabled test is
  refused, as any function TMR doesn't know is, and an empty `Test Sequence=` runs every enabled
  test once.
- **Numbers.** Hex is written with `0x`, an `h` after it, or a letter A-F in it; anything else is
  decimal, and a fraction is dropped. Pattern Param0 and Param1 written as digits only are decimal.
  The import report keeps the original Pattern Param0/1 spelling. Native programs supply their own
  data. With `patterns=legacy`, the generator uses the low 32 bits and warns about wider values.
  For example, 1usmus_v3's 9-digit hex values use their last 8 digits. Quotes around a value and a
  `;` comment after it are dropped.
- **Odd values.** A SimpleTest Pattern Mode TMR doesn't have runs as TM5 runs it, by its lowest
  byte (1 or 2, else 0); TMR's own mode numbers (10, 11, 13, 15-17, 20, 21, 30-33) keep their
  meaning. A negative Parameter runs as 0, and a negative Test Block Size as TM5's shift to bytes
  in 32 bits leaves it: -1 to -2048 the window, -3584 512 MB, -4096 1 MB. Interleave Type 0 on two
  or more channels walks each strided SimpleTest in 128-byte visits, as TM5's 128-bit mode does;
  `channels=` and `pattern-mode=` on the command line pick that walk again (one channel is type
  1's walk, and a numbered mode visits one line). A disabled test's Function isn't checked,
  Test0's included.
- **Wider ranges.** TMR takes any Channels from 1, a Lock Memory Granularity of 1-1024 MB and a
  Tests of 1-16, where TM5 takes 1-3, 1-512 MB and 2-16.

A crash can damage a `.cfg` with no visible sign, so TMR warns, naming the file and the line,
about NUL bytes (what a crash leaves), a line that is neither a `[section]` nor a `key=value`, a
key or section TM5 doesn't read, a key before the first section, a header without its `]`, a
repeated section or key, text that isn't UTF-8, a missing section, a `[TestN]` at or above Tests,
a number truncated to 32 bits, a negative Parameter or Test Block Size, an unusual Test Sequence
separator or a number of 256 or more in it, and a Tests, Channels or Lock Memory Granularity
outside what TMR takes. These are warnings, not errors: the file loads, read as TM5 reads it. The
first of a repeated key or section counts, a header without its `]` still starts its section, and
NULs and spaces around a name or a value are ignored. A value that isn't a number in TM5's form
stops the run, and so does a negative Cores, Cycles or the like: TM5 checks such a number without
its `-` and then stores it negated, so `Cycles=-3` would run 253 cycles.

### TMR JSON configs

The installer puts `demo_comprehensive_test.json` and `demo_tm5_compatible.json` in `examples\`,
beside `tmr.exe`. They show TMR's system and per-test settings, and make starting points for your
own plans. Run one with `tmr config=demo_tm5_compatible.json`: a run started normally looks for a
name in the current folder, then in its home's `configs\`, then in `examples\`. Run as
administrator it doesn't look in `examples\`, so give it the path. `tmr --create-demo-configs`
writes copies to edit into the home's `configs\`. Use `--startup-debug` to inspect an edited config
before running it.

For access-order comparisons, the installer also includes
[`access_geometry.json`](../configs/access_geometry.json). It runs SimpleTest and Mem-Sequence
with several spacings and groups of 1, 2, 4, 8 and 16 adjacent lines.
The [access geometry guide](tests/access-geometry.md) explains what changes, how to run it,
and how to add these per-test settings to your own config.

## Where TMR keeps its files

TMR keeps its files in one folder, its home. Which folder that is depends on how TMR was started:

| Started | Its home |
|---|---|
| normally | the folder `tmr.exe` is in; or `TMR_HOME`, when it names a folder |
| as administrator | `%ProgramFiles%\TMR Admin`; or `TMR_HOME`, when it names a folder only administrators can change |

`TMR_HOME` is an environment variable (`setx TMR_HOME D:\tmr`, then open a new console). Run as
administrator, TMR takes it only when it is a full path (`C:\...`, not a relative one or a share)
on a local drive, only administrators can change that folder, and nobody else can move or replace
a folder above it. A folder in your user profile (Documents, Downloads, a git checkout there)
doesn't qualify: your own account can change it. A folder in `C:\Program Files`, such as
`C:\Program Files\TMR Dev`, does. TMR makes the home's four folders, and `TMR Admin` itself, with
permissions only administrators and Windows itself can change, and checks them at every start. If
they are no longer as TMR made them, it stops and says so: close TMR and rename the folder to keep
what is in it (or point `TMR_HOME` at a new one), and the next start makes a new home.
`--setup-large-pages` and `--remove-large-pages` still run meanwhile, with no log file.

Run as administrator, TMR starts with none of the files of its normal runs: its own calibrations,
results and logs, and window settings for each Windows account. A config from elsewhere still
runs, given its path. A damaged settings file there is reported and left as it is, not set aside
as `<name>.unreadable` and replaced.

In its home:

| | What it holds |
|---|---|
| `configs\` | your configs and any editable demos you generate |
| `results\` | one `TMR_<date>_<time>.json` a run, saved at the end of setup and as the run goes, so a crash keeps the cycles saved so far |
| `logs\` | one `TMR_<date>_<time>.log` a run, the same name as its results file |
| `settings\` | `tmr-cfg.json`, what TMR's calibrations (`tmr --calibrate-all`) measured, and of which system (`tmr --show-calibration` shows it, and whether it is this machine's); `window.json`, the window's settings |

TMR writes each file in `settings\` whole: a crash or a power cut while it saves leaves the last
save as it was. After each save it writes a copy of that save beside it, `tmr-cfg.json.bak` and
`window.json.bak`. If a file is damaged all the same (edited by hand and broken, say), TMR uses its
`.bak` copy instead, and keeps the damaged file as `<name>.unreadable` for you to look at or delete.
For `tmr-cfg.json` it says so as a run starts, among the console's setup lines and on the window's
System tab (`tmr --show-calibration` names the copy too); for `window.json`, in the window's Errors
list as the window opens. On a share or a drive where TMR can't make a new file in `settings\`, it
writes each save in place, so a crash while it saves can still damage the file, and it can make no
`.bak` there, only rewrite one already there: with none, a damaged file's settings are lost, and
TMR starts from its defaults.

A file you delete is not brought back from its copy: TMR starts again from its defaults. The 0.1.2
release and earlier kept `tmr-cfg.json` and `window.json` beside `tmr.exe`. The next run started
normally copies each into `settings\`, when that has neither the file nor its `.bak`, and leaves
the old one in place. A run as administrator copies none: it has settings of its own.

The run's last lines say where its results file went. `tmr --compare-results <one> <other>`
compares two results files, by their names in `results\` or by their paths. It matches the two
runs' tests by name (a TM5 config's by test number too): it lists the tests only one of them ran
and those that ran with other settings, then how each test both ran changed.

`tmr --show-results <file>` shows a run's end again, from its results file (found the same way):
a line saying how the run ended (it finished, was stopped, reached its time limit or halted on
errors, with its verdict; or it never finished, killed or crashed, and its figures go up to its
last saved cycle), which run it was, then the final summary as the console showed it. A run that
crashed in its first cycle leaves the file saved at the end of setup: it has no cycle, so no
figures, and its result is "no test ran". A file from another version of TMR is shown in this
version's layout, and TMR says so above it.

A results file describes the system it came from, so runs can be told apart: the CPU, the BIOS
and the memory modules' part numbers and speeds. A run leaves out the DIMMs' serial numbers, the
motherboard's system UUID and the two fingerprints TMR makes of the machine and its memory, unless
it is given `--serials` (`tmr plan=stability --serials`), which keeps them in the output, log and
results; in the window, Settings has the same switch, off by default. Files from the 0.1.2 release
or earlier have them: it kept them by default. A file that left them out has
`(left out: --serials keeps it)` for its `machine_id` and `memory_id`.

To share a run's files, `tmr --sanitize <results file or log>` (in the window, Sanitize a results
file and its log, in the `•••` menu) writes copies of both beside them, `<name>.sanitized.json` and
`<name>.sanitized.log`, without the serials, the UUID and the fingerprints, nor your user folder and
OneDrive organisation in paths, the PC in a share's path (`\\PC\share`), the Windows account and PC
names, or SIDs: each is `<removed>`. Either file names the pair, by its path or its name in
`results\` or `logs\`; the other must be there (or in the same folder) with the same run ID, or
nothing is written. Read the copies before posting them: a name TMR didn't write, in a config's name
or a folder outside `C:\Users\`, is still there.

That is the one to use when you have both files. To share one file alone, or one whose partner is
gone or from another run (a log from before TMR recorded run IDs), `tmr --sanitize-single <results
file or log>` (Sanitize one file, in the same menu) writes a copy of that file only, without looking
for its partner or checking a run ID. It removes the same things as far as that file shows them, and
says what it couldn't. A results file alone can't know the PC and user names only its log gives (the
Windows account, a hardware error event's PC), so one in the results file outside `C:\Users\` stays,
such as a config in a folder named for you (`D:\<your name>\`). A log alone finds the serials, the
UUID and the fingerprints by TMR's own lines for them (each DIMM's, the system's, a calibration's
identity check) rather than from a results file, so one only a line of another kind has, as in an
older build's log, stays.

## Uninstalling

Settings > Apps > TMR (Test Memory R), or `winget uninstall Petz.TMR`. Your configs, results, logs
and settings stay in TMR's folder, and so do a `TMR_HOME` folder and `%ProgramFiles%\TMR Admin`,
when TMR was run as administrator; delete them when you no longer want them.

## Help

Report a problem at <https://github.com/jlpetz/Test-Memory-R/issues>, with the run's results
file and its log, your Windows version and your CPU.
