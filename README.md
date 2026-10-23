# TMR (Test Memory R)

**TMR** is short for **Test Memory R**, the R for Rust. It's a remake of TM5 (TestMem5 by Serj,
<https://testmem.tz.ru>), the memory tester highly acclaimed by memory overclockers. I contacted Serj,
who was kind enough to fix a broken link on his website that gave me TM5's original assembly source.
I then used Claude AI (with a little bit of Astra/Codex) to reimplement TM5 in Rust for modern Windows
and hardware, with DDR5 systems in mind.

## Key changes from TM5

- **Fixed allocations instead of a sliding window.** TM5 slides an AWE window over the memory it
  tests; TMR gives each thread its own fixed allocation, with native 64-bit addressing.
- **Huge (1 GB) and large (2 MB) pages**, locked in memory (non-pageable). Both are faster than
  regular 4 KB pages. No driver: TMR runs in user mode, and `tmr.exe --setup-large-pages` (elevated)
  grants the privilege they need once.
- **Wider SIMD:** each test comes in 128-bit (SSE4.2), 256-bit (AVX2) and 512-bit (AVX-512) widths,
  the widest the CPU has by default (`width=` picks one). An experimental tile test runs on AMX
  (Intel Xeon); ACE is next to look at.
- **TM5 configs still work:** TMR loads TM5 `.cfg` files and runs the equivalent tests.
- **TM5's test 0 becomes the seal:** all memory is sealed at the start of a cycle and checked at its
  end, and each block a checking test works on is checked before and resealed after it, so an error
  in memory no test is touching is still caught.
- **Errors say more:** a failed check is rescanned to tell a misread from a wrong stored value,
  failures are counted by bit position (one bit failing everywhere points at one data line), and
  Windows' hardware error log (WHEA) is watched for errors the tests can't see themselves.
- **Built for today's CPUs:** threads are pinned to physical cores and take their memory from their
  own NUMA node while it has room, CPUs beyond 64 (processor groups) are used, and checks can bypass the caches
  (non-temporal stores, CLFLUSHOPT) so they read DRAM.
- **Patterns aimed at the memory bus**, alongside TM5's. A switching family inverts each beat of a
  burst at the bus's own width, so every data line toggles on every transfer, the most switching
  noise the data can make: 64-bit for DDR4 and earlier, 32-bit for DDR5's subchannels, 16-bit for
  LPDDR4/5, picked from the memory type SMBIOS reports, and models of LPDDR6's 12-bit and DDR6's
  24-bit, chosen with `beat=`. A random family feeds every line every short bit sequence, which DDR5's receive
  equalisers react to. Both aim to make a marginal overclock fail sooner.
- **Built-in test plans** (`plan=`; `stability` by default), **results as JSON** saved during the
  run and comparable between runs, and a console (CLI) and a window (GUI, WinUI 3 through
  `windows-reactor`) in one `tmr.exe`.

## Supported Windows OSes

`tmr.exe` opens its window when double-clicked, and runs in the console when started from a shell,
a script or a remote session; `--gui` and `--cli` choose explicitly. Where the window can't open,
`--gui` says why and runs nothing.

| Windows | Build | Console (CLI) | Window (GUI) |
|---|---|---|---|
| Server 2022 | 20348 | Yes | No: a double-click runs the console in its own console window, and `--gui` says why and stops |
| Windows 11 21H2, 22H2, 23H2 | 22000–26099 | Yes | Yes, but a double-click flashes a console window briefly before the window opens |
| Windows 11 24H2 and later, Server 2025 | 26100 and later | Yes | Yes, fully supported |

Windows 10 and Server 2019 aren't supported: TMR isn't tested on them, and the window needs
Windows 11.

## Supported hardware

**CPU.** TMR is compiled for x86-64-v3 (AVX, AVX2, BMI1, BMI2, F16C, FMA, LZCNT, MOVBE) and also
needs CLFLUSHOPT, which pushes a cache line out to DRAM so that a check reads the memory, not a
cached copy. It checks for both at startup, and stops on a CPU without them.

| | Intel | AMD |
|---|---|---|
| Required: x86-64-v3 and CLFLUSHOPT | 6th gen Core (Skylake, 2015, e.g. i5-6600K) and later | Ryzen 1000 and EPYC 7001 (Zen, 2017) and later |
| Optional: AVX-512, for the 512-bit tests | Core X (Skylake-X), Xeon Scalable and Xeon W, 10th gen mobile (Ice Lake), 11th gen (Tiger Lake, Rocket Lake); no consumer chip since 12th gen | Ryzen 7000 and EPYC 9004 (Zen 4) and later |
| Optional: AMX, for the experimental tile test | 4th gen Xeon Scalable (Sapphire Rapids, 2023) and later | None |

Haswell and Broadwell (4th and 5th gen Core, e.g. i5-4670K) have AVX2 but not CLFLUSHOPT, so TMR
doesn't run on them, nor on Pentium, Celeron and Atom models without AVX2. A test the CPU can't run
(a 512-bit test without AVX-512, the tile test without AMX) is left out of the run, never stepped
down to a narrower width.

**Memory.** TMR reads the memory type from SMBIOS and aims its switching patterns at that type's
bus width (see Key changes):

| Memory type | Bus width the switching patterns flip at |
|---|---|
| DDR, DDR2, DDR3, DDR4 | 64-bit: one channel per DIMM |
| DDR5 | 32-bit: two subchannels per DIMM |
| LPDDR3 | 32-bit |
| LPDDR4, LPDDR5 | 16-bit |
| DDR6, LPDDR6 | 24-bit and 12-bit, models of the drafts: not detected, chosen with `beat=ddr6` or `beat=lpddr6` |

When SMBIOS reports no type TMR knows (a VM often reports none), TMR says so and the switching
patterns alternate their 64- and 32-bit forms; `beat=` sets the type.

**Caches.** TMR reads the size of each cache tier (L1, L2, L3) from the CPU. The cache bandwidth
and latency tests are sized to fit their tier, and tests aimed at DRAM size their blocks past the
L3, so they reach it.

## Requirements

- **Windows and a CPU** from the lists above.
- **The Microsoft Visual C++ runtime** (`VCRUNTIME140.dll`), for the console and the window alike.
  `tmr.exe` is built with Microsoft's C++ toolchain, which keeps part of its C runtime in that DLL
  rather than in each program, so without it Windows won't start `tmr.exe` ("VCRUNTIME140.dll was
  not found"). Most PCs have it already: games and many apps install it. Get it from
  <https://learn.microsoft.com/en-us/cpp/windows/latest-supported-vc-redist>, or:
  `winget install Microsoft.VCRedist.2015+.x64`
- **The Windows App Runtime 2** (2.4 or later), for the window only: the window's controls (WinUI 3)
  are in that runtime, which Microsoft ships apart from Windows. The console runs without it. When
  the window can't find it, TMR offers to open the download page,
  <https://learn.microsoft.com/windows/apps/windows-app-sdk/downloads>. Or:
  `winget install Microsoft.WindowsAppRuntime.2` (not `Microsoft.WindowsAppRuntime.2.0`, an older
  2.0 release).
- **Neither runtime is bundled with TMR**, on purpose: both are shared by many programs, so each PC
  should keep its one copy up to date (`winget upgrade --all`, or each package by its name above),
  and it keeps `tmr.exe` and this repository small.
- **Optional: the "Lock pages in memory" privilege**, for huge and large pages: run
  `tmr.exe --setup-large-pages` once from an elevated prompt, then start TMR again. Without it TMR
  warns, and asks before running on regular 4 KB pages, which are slower.

**Building from source** takes Rust nightly (`rustup toolchain install nightly`) and Microsoft's
C++ build tools for the linker (Visual Studio Build Tools, "Desktop development with C++").
`cargo +nightly build --release` builds both frontends into the one `target\release\tmr.exe`, as
each release is; `--no-default-features` leaves the window out.

## License

This project is licensed under either of:

- MIT License ([LICENSE-MIT](./LICENSE-MIT))
- Apache 2.0 License ([LICENSE-APACHE](./LICENSE-APACHE))

at your option.

## Credits

TMR was inspired by TM5, and wouldn't exist without it. Many thanks to Serj for his kind donation of
TM5 to the community.

- Serj's work: <https://testmem.tz.ru>
- TM5 page: <https://testmem.tz.ru/testmem5.htm>

![TestMem5 v0.12 testing a Ryzen 7 5700X with anta777's configuration](doc/images/tm5.png)
