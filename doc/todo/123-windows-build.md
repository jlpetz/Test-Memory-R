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
- `stitched::commit_method_for`: `Replace` on a line, build and revision `REPLACE_FIXED` lists, else free-then-allocate, and why (the
  log file has the reason: `stitched: commits by free-then-allocate: no fix for the refused-replace bugcheck is known for server build
  26100.33650`). The table is empty: every Windows, server and client, is taken as having the bugcheck (the owner).
- **Keyed by line as well as build**: a server's revisions run apart from a client's of the same build. This dev box's Server 2025 read
  26100.33650 while the owner's list had Windows 11 24H2 at 26100.9725, so a client's row keyed by build alone would have taken the
  unfixed server for fixed (33650 past any 9xxx) and picked Replace. `WindowsVersion::server` (from `InstallationType`) is in the
  results too.

## The current revisions (the owner's list, 2026-11-09, before the fix)

| Windows | Build.revision | Update | Date |
|---|---|---|---|
| Windows 11 26H2 | 26300.9818 | 2026-10 D (preview) | Oct 28, 2026 |
| Windows 11 26H1 (new hardware only) | 28000.3365 | 2026-10 D (preview) | Oct 28, 2026 |
| Windows 11 25H2 | 26200.9818 | 2026-10 D (preview) | Oct 28, 2026 |
| Windows 11 24H2 | 26100.9725 | 2026-10 OOB | Oct 26, 2026 |
| Windows 11 23H2 | 22631.7654 | 2026-10 OOB | Oct 26, 2026 |
| Windows 10 22H2 (ESU; TMR doesn't support 10) | 19045.7811 | 2026-10 OOB | Oct 26, 2026 |
| Server 2025 (this dev box, read) | 26100.33650 | | 2026-11-09 |

Server 2022 (20348) isn't listed there: it hadn't been checked yet.

## The fix: CVE-2026-119965, Patch Tuesday 2026-11-10

Microsoft's fix shipped 2026-11-10 as a security update, Elevation of Privilege, Important, for the vulnerability TMR reported:
<https://msrc.microsoft.com/update-guide/vulnerability/CVE-2026-119965>. Its affected-product list, read from MSRC's own API
(`api.msrc.microsoft.com/sug/v2.0/.../affectedProduct`, 2026-11-11), gives the exact fixed build for each Windows line, which is a
stronger source than an announcement: it is Microsoft's own confirmation of the specific issue reported, not a general release note.
The owner's call (2026-11-11): bake these in directly, rather than waiting on an on-box repro, which stays optional (below).

Two KBs cover several lines — not one superseding the other, but two parallel servicing mechanisms shipped the same day, each
confirmed (checked on their own KB pages, 2026-11-11) to fix this CVE: a **Hotpatch** update (applies the fix live, no reboot, needs
baseline KB5127648 already installed — a narrower, opt-in track) at the lower revision, and the regular monthly **cumulative update
(LCU)** everyone else gets at a higher one. Both are in the CVE's own per-build list, so the lower number is still the correct floor:
a Hotpatch box settles at it, an LCU box passes through it on the way to the higher one, and the two tracks' numbers don't interleave,
so there's no in-between revision that's unpatched but reads as fixed. 23H2 and Server 2022 have no Hotpatch track, hence one KB each.

| Line | Build | Fixed from | Hotpatch KB (used) | LCU KB (same fix, the default path) |
|---|---|---|---|---|
| Windows 11 24H2, client | 26100 | 9911 | KB5132043 | KB5132061 (9974) |
| Windows 11 25H2, client | 26200 | 9911 | KB5132043 | KB5132061 (9974) |
| Windows 11 26H2, client | 26300 | 9911 | KB5132043 | KB5132061 (9974) |
| Windows 11 23H2, client | 22631 | 7711 | KB5131087 | — (no Hotpatch track for this build) |
| Windows Server 2025, server | 26100 | 33730 | KB5131077 | KB5131080 (33822) |
| Windows Server 2022, server | 20348 | 5870 | KB5131090 | — (no Hotpatch track for this build) |

This dev box's 26100.33650 (above) sits 80 revisions short of the server fix (33730): still unpatched, by the owner's choice (patching
paused on purpose, so the unfixed state could still be read here). `REPLACE_FIXED` now carries all six rows (`src/memory/stitched.rs`,
2026-11-11); `the_real_fixed_builds_match_cve_2026_119965` pins each against the CVE's own numbers, one revision above and below.

Not in the CVE, so still free-then-allocate at any revision: build 28000 (Windows 11's next feature update, 26H1, new hardware only),
and any build before 23H2 (client) or Server 2022 (server) that the CVE doesn't name — absence here reads as "not proven fixed," not
"not affected."

Checked live on this box (2026-11-11, a disposable test, not kept): `WindowsVersion::detect()` read 26100.33650/server as above, and
`commit_method_for` returned `FreeThenAllocate ("server build 26100.33650 predates the bugcheck's fix (26100.33730)")` — the wiring
end to end, on the one build this box can prove, without needing the on-box repro below.

## Optional: an on-box repro, not run

The probe for one still exists and wasn't run: `../stitch-test/` (not in git), its narrowing repro `trigger=` built but never
triggered (its FINDINGS.md: E5 bugchecked Server 2025 26100.33438 with `0x139_e_INVALID_REFERENCE_COUNT_nt!KiFastFailDispatch`, from
`MiDeletePartialVad`). It would deliberately bugcheck whichever machine runs it (never this dev box), so it stays the owner's own call
to run, not something to do from here. With the CVE's own fixed-build numbers now in `REPLACE_FIXED`, it would only be for extra
confidence, not for the table to be trustworthy: a crash-then-patch-then-clean-run cycle on the US VM, if the owner wants it before
trusting `Replace` on a freshly patched box.

## TODO 124: no override; a visible warning instead (the owner, 2026-11-11)

Asked whether TMR should let someone force the commit method (`commitmethod=`, say): no such parameter exists, and none is wanted.
The only use for one would be forcing `Replace` on a build `REPLACE_FIXED` doesn't clear — which is just choosing to reproduce the
bugcheck, no safer gated version of that makes sense. Decided instead: keep auto-selection as built, `FreeThenAllocate` the default
when the build is below its line's fixed revision or isn't detected, and add a visible warning for that case (today's
`stitched: commits by free-then-allocate: ...` line is `log::info!`, not shown on the console in a release build by default). Not
built yet: the owner will crash a box with `../stitch-test/`'s `trigger=` manually first, before patching, then again after, to
confirm the fix independently of the CVE reading before anything in the app leans on the table being right. Open when it's time:
whether the warning covers both "build unknown" and "build known but unpatched" (the common, currently-universal case) or only the
unknown case, since the latter is the normal, safe, no-slower-in-practice state for everyone until they patch, not obviously warning-
worthy on every run.
