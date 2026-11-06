# TODO 122: memory timings and temperatures with the run, from ZenTimings and LibreHardwareMonitor

## Why (the owner, 2026-11-06)

A keen overclocker wants it all in one place: the timings a run was made at, and the temperatures over the run against its errors, to
see whether heat rising in the DIMMs or the CPU lines up with the errors. TMR can't read either itself. A DIMM's temperature is a sensor
chip on the module, read over the SMBus; a CPU's is in its registers (MSRs on Intel, SMN on AMD); the running timings are the memory
controller's registers. All of it takes kernel mode, and TMR runs in user mode with no driver of its own (CLAUDE.md, the settled
decisions). Tools that ship a signed driver read it already; TMR takes it from them.

## Decided (the owner, 2026-11-06)

- **Timings from ZenTimings, once, at the run's start** (AMD). It isn't needed while the tests run.
- **Temperatures from LibreHardwareMonitor, during the run**: the CPU's and each DIMM's, on Intel and AMD, the one extra app left
  running. ZenTimings' DIMM temperatures would duplicate it (the same sensor chip on the module), unless a check shows they differ.
- **HWiNFO out**: its free version's shared memory is time-limited (12 hours, re-enabled by hand, as remembered; not verified, its site
  refuses automated reads), which an unattended soak can't have. HWMonitor has no interface known to us.

## The sources, as read in their code (2026-11-06)

### ZenTimings (irusanov/ZenTimings, GPL-3.0; v142.713 of 2026-10-06, the owner's 142.714+9e482d3)

- **No export command**: its startup (`WPF/App.xaml.cs`) takes only its driver cleanup mode and its autostart argument.
- **Live Snapshot** (`WPF/Export/LiveSnapshot.cs`, off by default): keeps a snapshot file current "so that local tools can read the
  current state without talking to the application", at most once a second, after each auto refresh. Written whole (a temp file, then
  `File.Replace`), serial numbers never in it. The folder and name are the user's (default beside ZenTimings,
  `live_snapshot_device_config.json`), and so are the sections.
- **Its schema**, `zentimings.snapshot` 1.0 (the owner's captures for TODO 117, `baselines\2026-11-05-timings\`):
  - `app`: ZenTimings', its core's and PawnIO's versions; `static`: the system, CPU, board, BIOS and AGESA, SMU, modules, SPD;
  - `config.timings.per_dct[].timings`: decoded, each channel (`CL 14, RCDRD 14, RFC 288, REFI 14029, PHYRDL 28`), in clocks unless
    named `ns`; `dct_mismatch` lists what differs between channels;
  - `config.ddr4_bios_controller`: raw codes (`ProcODT 56, RttNom 7, ClkDrvStren 15`), which its window decodes into ohms;
  - `readings`: `sampled_at`, `stale`, the power table (FCLK, UCLK, MCLK, SoC and VDDP voltages), each DIMM's `dimm_temp_c`;
  - `unavailable`: each missing value and why.
- **It takes outside work**: the snapshot export and the Live Snapshot are a contributor's (PRs 107, 108), the side-by-side channels
  another's (106); an open PR (109) adds CPU temperature to its window. If that lands, an AMD user could run ZenTimings alone.

### LibreHardwareMonitor (LibreHardwareMonitor/LibreHardwareMonitor, MPL-2.0, v0.9.6 of 2026-02-14)

- DIMM temperatures, DDR4 and DDR5 (`Hardware/Memory`: `SpdThermalSensor`, over the SMBus through RAMSPDToolkit and PawnIO); Intel's
  and AMD's CPU temperatures.
- Its "timings" are SPD's: the minimums the module advertises (`tAA`, `tRCD`, `tRP`), not the controller's. No use for an overclock.
- Read from its app's web server (its options; port 8085 by default): `/data.json` (the sensor tree), `/metrics` (Prometheus),
  `/Sensor?action=Get&id=...`. It has an optional login; set it to listen on this PC only.

## The plan

1. **Timings into the results.** At the run's start, TMR reads ZenTimings' live snapshot: a setting naming the file, else ZenTimings'
   default place. Taken when fresh (`readings.sampled_at` recent, `stale` false), else said and left out; never blocking. The snapshot
   goes into the results as it is, under its schema and version (TMR decodes nothing), with one line on the console and in the window:
   the speed, the primaries, the source. `--compare-results` names the timings two runs differ in.
2. **Temperatures over the run.** LibreHardwareMonitor's `/data.json` every 2 s, on the progress timer, never in the hot loops: the
   CPU's package (and each CCD) and each DIMM. A timeline in the results, each step's peak in its report, and the window's chart (TODO
   64's item 1). Not running: one line, and the run goes on.
3. **Errors with their times.** Today the results count a step's errors but don't time them (the log has the times). Each failed
   check's time, or at least a step's first and last, goes into the results, so the timeline lines heat up against errors.
4. **The check.** One run with both apps: ZenTimings' and LibreHardwareMonitor's DIMM temperatures side by side, expected equal within
   the sensor's resolution. If they aren't, ZenTimings' are kept too.
5. **No new crate**: an HTTP GET over a local `TcpStream`, and `serde_json` for both files.
6. **The results' timings block is source-neutral**: its source, version and schema, then the source's own data, so another source
   (Intel's, below) fits later without a new format.

## For ZenTimings: to discuss with its maintainer (the owner, on its Discord)

Facts behind them (its code, 2026-11-06): it runs as administrator (`requireAdministrator`), its autostart a logon task at the highest
privileges, the supported way to start elevated without a prompt; it has no tray. The export and the Live Snapshot are one format, the
default sections "what the main window shows" (System, Modules, Timings, AOD, APOB, BIOS controller, power table, DIMM telemetry), and
opt-in ones: `Spd`, `Registers` (the raw dump), `Sensors` (the board's sensor chip and plugins, not the CPU's own temperature), `AsusWmi`.
Decoded throughout but for the BIOS controller block, the structure's raw fields copied out.

In TMR's order of use:
1. **One snapshot on demand**: a command line that takes one reading, writes the file and exits without the window
   (`--snapshot <file>`, say), so TMR takes the timings at a run's start and nothing stays running. As it runs elevated, a way without a
   prompt each time too (a task like its autostart's, which a tool can start).
2. **A known place for the file**: today beside ZenTimings or a folder the user picks; a fixed default (`%ProgramData%\ZenTimings`, say)
   or a documented pointer lets a tool find it without asking.
3. **The schema as an interface**: whether `zentimings.snapshot` is meant for other tools, and its version bumped on a rename or a
   removal, which TMR keys on.
4. **The BIOS controller block decoded**, as the window shows it (ohms, RZQ), beside the raw codes.
5. **Open a saved snapshot to view, and two side by side** with the differences marked. TMR keeps the snapshot as it is, so it can write
   one back out for ZenTimings; ZenTimings needs to know nothing of TMR's format.
6. **CPU temperature (PR 109) in the snapshot's readings** as well as the window: an AMD user could then run ZenTimings alone.
7. **Intel**: whether he would want it in ZenTimings or a sibling, with PawnIO's `IntelMCHBAR` and Intel's datasheets; it needs testers,
   as we have no Intel desktop.
8. **Courtesy**: that TMR (open source, MIT/Apache) reads the snapshot and keeps it in its results, crediting ZenTimings; any
   attribution he wants. Reading a file's data isn't using GPL code, so the licences don't meet.

## Intel

- **No Intel ZenTimings.** Intel overclockers use board makers' closed tools (ASRock Timing Configurator, ASUS MemTweakIt) or closed
  cross-vendor ones (HWiNFO, AIDA64, CPU-Z's primaries, Intel's XTU); GitHub has nothing open (every timing tool there is AMD's).
  ZenTimings' issues have no Intel request (three mention Intel, none asks for it); its README and core are AMD's.
- **The access exists.** PawnIO's official signed modules (namazso/PawnIO.Modules, LGPL-2.1) include `IntelMCHBAR` (the memory
  controller's registers), `IntelMSR`, `IntelPCHThermal`, `SmbusI801` and `SmbusPIIX4` (the DIMMs' sensors) and `AMDFamily17`. A small
  overlay, SolidSnake1765/rtss-overlay-preset (no licence, so not to copy; October 2026), reads Intel 6th-14th gen running timings with
  `IntelMCHBAR`, decoded from Intel's public datasheets (vol. 2: 337345, 636761, 767626).
- **Routes**: Intel in ZenTimings as a contribution, asking the maintainer first (its Discussions), its name and core being AMD's; or
  TMR reading it itself through PawnIO, which reopens the settled no-driver decision (its trigger: ring-0 observability through a
  different driver), the owner's call. CPU-Z's command-line report (`-txt=`) is the fallback for the primaries.
- **Blocked either way on an Intel desktop**: a VM shows no memory controller, and each generation decodes differently.
