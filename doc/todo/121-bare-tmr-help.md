# TODO 121: `tmr` alone lists its help; a click's help goes to a shell; the classic console's warning

## Why (2026-11-06)

winget-pkgs' validation runs the installed `tmr.exe` with no arguments and its output captured. TMR then ran its default stability
plan in the console, asked about large pages (a fresh machine has no privilege), got no answer and quit with exit 2, "no test ran".
Step 08, "Installation Validation", installed both scopes and then concluded "neutral: needs to go to manual review"; a moderator's bot
labelled the PR `Validation-Executable-Error` and a moderator cleared it (microsoft/winget-pkgs 471507).

It isn't the first version's only: every version is checked so. Measured 2026-11-06:
- clean updates pass step 08 and are approved and merged by automation within about an hour (12 sampled);
- merged updates still carrying the label waited 64 to 778 hours for a person (12 sampled);
- astral-sh.ty, a console tool, came back neutral, "manual review", on each of 0.0.96, 0.0.97 and 0.0.98.

So each TMR release would wait in a moderator's queue, and the moderators would see the package again and again (the owner).

## Decided (the owner, 2026-11-06)

1. **`tmr` alone prints the help and exits 0**, in the console. A run takes a setting (`tmr plan=stability` is the defaults). The window
   is unchanged: it opens, waits for Start, keeps the last run's parameters, and starts on open only when its Settings say so. The
   console gets no "remember the last run": a bare command whose action depended on the last run would be hidden state, the terminal's
   up arrow reruns a command line, and the window has it already.
2. **A click where the window can't open** (Server 2022; a build without the window; `--cli` alone from a shortcut) runs the CLI in a
   console of tmr.exe's own, which closes as it exits, so its help would vanish. It goes to a shell that stays, at its prompt in
   tmr.exe's folder with that folder first on its PATH, as `tmr` typed into a shell leaves you at that shell's prompt. Considered and
   not taken: waiting for Enter to start (a GUI's Start without the GUI, the owner), a message box (can't start anything), a timer.
   - Windows Terminal installed (`wt.exe` on the PATH): a new window (`-w new`), which shows the help itself; tmr.exe's console closes.
   - Otherwise the same console: the help and the warning, then the shell takes the console over as tmr.exe exits.
   - The shell: the newest PowerShell, found by name, never a version: `pwsh.exe` on the PATH (what typing `pwsh` gives), then the
     newest release the installer recorded (`HKLM\SOFTWARE\Microsoft\PowerShellCore\InstalledVersions`, `SemanticVersion` and
     `InstallLocation`; previews left out), then Windows PowerShell 5.1, then Command Prompt. Every PowerShell since 6.0 is `pwsh.exe`.
3. **The classic console's WARNING**, on every console run anywhere, not blocking: at the end of the help (bare `tmr`, `--help`), and
   at a run's start and again at its end, so it is among the first and last lines. Plain text but for a yellow `WARNING:` (the console
   can't draw an emoji). Never in redirected output, which no console shows. "WARNING", not "Note" (the owner).

## Telling the terminal

The shell is the program that reads what is typed (cmd, PowerShell); the terminal is the window that draws it, and the emoji are the
terminal's: conhost's classic window draws in one font with no fallback. A program can tell that window apart, probed on Server 2025
(2026-11-06):

| Where | `GetConsoleWindow`'s class | `WT_SESSION` | Parent |
|---|---|---|---|
| Classic console (`conhost.exe powershell`) | `ConsoleWindowClass`, shown | not set | conhost |
| Windows Terminal (`wt.exe powershell`) | `PseudoConsoleWindow` | set | WindowsTerminal |

Every modern terminal (Windows Terminal, VS Code's, WezTerm) hosts a console through a pseudoconsole, so its class is the second. A
hidden classic window is a terminal's that draws the text itself (ConEmu), so the warning takes the class and the window being shown.
Which modern terminal, or its font, isn't told; nor is the shell, but by the parent process, which a wrapper (`cmd /c`) can hide.

## The code

- `crates/tmr-cli/src/lib.rs`: `bare_is_help`, `warn_if_classic`, `CLASSIC_WARNING`; `console.rs`: `is_classic`, set at `init`.
- `crates/tmr-app/src/launch.rs`: `Launch::hand_over` (the CLI, no arguments, not redirected, a console of its own); its cases in the
  decision table. `shell.rs`: `newest_shell`, `windows_terminal`, `open_terminal`, `stay`; PowerShell's command `-EncodedCommand`
  (Windows Terminal takes a `;` as its own separator, and no path's quoting can break it).
- The hand-over runs after the CPU gate: the code before it may not run on a CPU the gate refuses.

## Known edges

- A scheduled task that runs bare `tmr` in a console of its own now leaves a shell waiting there, where it ran the defaults before.
  Nothing of TMR's does that (hw-pass, `bench_ab.py` and the docs all pass settings).
- On an unsupported CPU, a Server 2022 click's FATAL line still closes with its console, as before (the gate exits before the hand-over).
- Checked on Server 2022 by a real double-click over RDP (the metal box at m8a.xlarge): see TODO 121's entry.
