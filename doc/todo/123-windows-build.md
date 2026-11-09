# TODO 123: Windows' version and build with every run, and the commit method chosen by it

## Why (the owner, 2026-11-09)

The stitched allocator commits by free-then-allocate because, on build 26100, a `VirtualFree` after a refused large-page
`MEM_REPLACE_PLACEHOLDER` bugchecks 0x139 (TMR-APP CLAUDE.md, the settled decisions). Microsoft's fix is expected in an update (the
owner was told: Patch Tuesday, 2026-11-10). An update raises the revision (UBR), not the build: 26100.4349, then 26100.4652. So TMR
needs the build and revision with every run, both to choose the method by it and to tell runs on different updates apart.

## Decided (the owner, 2026-11-09)

- **Recorded, never fingerprinted**: Windows' version goes to the console, the log and the results, and never into the machine's
  fingerprint (`machine_id`, `MachineIdentity`), so a Windows update doesn't make a calibration stale.
- **The console**: `🪟 OS: Windows Server 2025 Datacenter 24H2, build 26100.33650` ("OS", not "Windows", which the name repeats).
- **The commit method shown** on the allocation's start line: `(commits: free-then-allocate)`.

## Built (2026-11-09)

- `src/windows_version.rs`: read once from `HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion` (`ProductName`, `DisplayVersion`,
  `CurrentBuildNumber`, `UBR`, `InstallationType`). A Windows 11 client's `ProductName` still says Windows 10, so a client from build
  22000 on is named 11. WMI's caption names it right but is far slower to ask.
- `Setup::SystemInfo.os`: the console's line after the system and its modules, the window's System tab's `OS:` line.
- Results: `identity.windows` (name, display_version, build, revision; outside `machine_id`); `run_config.memory.commit_method`.
  `--compare-results` notes a different build or revision (a ⚠️, not invalidating), only between two runs that both recorded it.
- `stitched::commit_method_for`: `Replace` on a build and revision `REPLACE_FIXED` lists, else free-then-allocate, and why (the log
  file has the reason: `stitched: commits by free-then-allocate: no fix for the refused-replace bugcheck is known for build ...`). The
  table is empty: nothing is verified.

## Next: verifying the fix

1. Find the update and its revision (Microsoft's release notes, the KB), and whether its notes name the bugcheck.
2. Verify on a machine that can afford a 0x139: the US VM (Server 2025, build 26100), resized as needed, never this dev box. Install
   the update, then reproduce the refused replace: a large-page `MEM_REPLACE_PLACEHOLDER` refused (memory short of the page size), then
   `VirtualFree` of that placeholder. Before the fix it bugchecks; after, it returns. The probe exists: `../stitch-test/` (not in
   git), its narrowing repro `trigger=` built but never run (its FINDINGS.md: E5 bugchecked Server 2025 26100.33438 with
   `0x139_e_INVALID_REFERENCE_COUNT_nt!KiFastFailDispatch`, from `MiDeletePartialVad`). Each run the owner's go-ahead.
3. Only then a row in `REPLACE_FIXED` (the build, the first fixed revision), and a check that Replace lays out as free-then-allocate
   does on that machine (`commit_methods_lay_out_the_same` is the fake's).
4. Other builds (Server 2022's 20348, Windows 11's 22000-26099): each a row of its own when its fix is verified; until then
   free-then-allocate.
