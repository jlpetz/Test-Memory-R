# TODO 114: an elevated tmr.exe loaded DLLs from its own folder first

Found 2026-11-03 by TODO 113's design review. The owner, 2026-11-03: "make the linker strict, so the base library DLLs come from their own
location; I don't want to package them."

## The hole

tmr.exe's static imports (`dumpbin /dependents`) are KnownDLLs (kernel32, advapi32, oleaut32, ole32, user32, shell32), API sets
(`api-ms-win-*`, which the loader maps to the DLL that implements them), ntdll, and three more: VCRUNTIME140.dll, wevtapi.dll and
bcryptprimitives.dll. For those three the loader's standard search starts in the exe's folder. The bare exe in Downloads, or the
installer's per-user install (`%LOCALAPPDATA%\Programs\TMR`), is in a folder any program of its user can write, unelevated; run as
administrator, which TODO 113 makes the way to large pages for an administrator under UAC, tmr.exe ran a DLL put there, elevated, before
`main`.

Measured before the fix: a copy of System32's version.dll beside the exe, named wevtapi.dll or VCRUNTIME140.dll, failed the start with
0xC0000139 (the entry point not found: the loader took the decoy). Named bcryptprimitives.dll, or bcrypt.dll (wevtapi's import), it
didn't, on the dev box: another system DLL had loaded the real one from System32 first, which nothing promises.

## The fix

`crates/tmr-app/build.rs` links the bin with `/DEPENDENTLOADFLAG:0x800` (LOAD_LIBRARY_SEARCH_SYSTEM32), in every build: the linker writes
it to the exe's load configuration (DependentLoadFlags), and from build 14393 (Windows 10 1607) the loader takes the exe's static imports
from System32 only. So vcruntime140.dll must be the Visual C++ Redistributable's, in System32: a copy beside tmr.exe (an app-local CRT) is
never used. The README and the guide say so.

Not taken: `+crt-static`, which removes vcruntime140.dll altogether. The owner keeps the VCRedist dependency and packages nothing.

What the flag doesn't reach, and why that's fine:
- The delay-load helper (MSVC's `delayhlp.cpp`) calls `LoadLibraryEx(name, NULL, 0)`, ignoring the flag. Its one DLL,
  `api-ms-win-appmodel-runtime-l1-1-5.dll`, is an API set, never a file looked for, and the window opens only from build 22000, which
  has it.
- The window's Windows App Runtime. Reactor's bootstrap calls the OS's dynamic-dependency API (`TryCreatePackageDependency`,
  `AddPackageDependency`; no Bootstrap DLL), which adds the framework package to the process, and its DLLs load from the package's folder
  under `C:\Program Files\WindowsApps`.
- Loads by name at run time. TMR's own code makes none. Two in its dependencies would look in the exe's folder first, and neither is
  reached: `windows-core`'s reg-free activation, which loads `<namespace>.dll` (`LOAD_LIBRARY_SEARCH_DEFAULT_DIRS`, the exe's folder
  among them) only for a WinRT class that isn't registered, where every class the window activates is (Windows' own, or the runtime
  package's once Reactor adds it); and std's backtrace symbolizer, which loads `dbghelp.dll` (`LoadLibraryA`) to print a backtrace,
  which TMR's panic hook never does.

## Checked (2026-11-03, the dev box, build 26100, MSVC 14.51)

- `dumpbin /loadconfig`: Dependent Load Flag 0000 before, 0800 after, in the default build and the `--no-default-features` one.
  `/dependents`: the same imports as before; a delay-import directory with the window only.
- The four decoys beside the fixed exe: `--cli --version` starts and exits 0 with each, in both builds. Again in review, with a control,
  a copy of the fixed exe with its load configuration's flag patched back to 0: the control took the wevtapi.dll and VCRUNTIME140.dll
  decoys (0xC0000139), the fixed exe none, in both builds.
- `--cli --version` and `--startup-debug` (plans, allocates nothing) run in both builds.
- `--gui`: the window (WinUIDesktopWin32WindowClass, "Test Memory R (TMR) v0.1.0") in 0.3 s, as before the fix; the Windows App Runtime
  2.6.5.0's DLLs from its package's folder, wevtapi, VCRUNTIME140, bcryptprimitives and bcrypt from System32; closed, exit 2 (not run).
  The same from a copy of the exe with decoys of the runtime's DLLs beside it (Microsoft.UI.Xaml.dll, Microsoft.UI.dll, Microsoft.dll,
  Microsoft.WindowsAppRuntime.dll and .Bootstrap.dll, Microsoft.Internal.FrameworkUdk.dll, CoreMessagingXP.dll), wevtapi.dll and
  VCRUNTIME140.dll: nothing loaded from the exe's folder but the exe.

## Next

The release's check on a clean desktop: the window, and `dumpbin /loadconfig` on the released exe. A CI step that fails a release exe
without the flag would keep it from being lost.

## Checked on a desktop (2026-11-04)
The 8500G (Windows 11) started the window from `C:\Users\Public\TMR\tmr.exe` (a5de854, a folder its users can write) with the flag
in: the Windows App Runtime loaded. Closed. Left as an idea, not a task: a CI step that fails a release exe whose `dumpbin
/loadconfig` lacks the flag.
