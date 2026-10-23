//! Which frontend tmr.exe runs (TODO 29 S6), decided before anything prints, from how it was launched. `--cli` and `--gui` decide. Any
//! other argument is a command line, so the CLI (`bench_ab.py`, `tmr.exe memory=...`). With none, a double-click opens the window, and a
//! shell or a remote session (WinRM) runs the CLI. `decide` is a pure function of the facts `probe` reads, so its table is tested.
//!
//! The console tells a double-click from a shell. On build 26100 and later the GUI build's manifest (`tmr.manifest`,
//! `consoleAllocationPolicy` detached) gives a tmr.exe launched with no console to inherit no console at all. Older builds ignore it and
//! open one, which no other process shares (`GetConsoleProcessList` counts 1), so tmr.exe leaves it before the window opens. A shell shares
//! its console with tmr.exe, which counts 2 or more. A remote session's output is a pipe, and its console may have no window, so redirected
//! output decides for the CLI first.

use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{FILE_TYPE, FILE_TYPE_DISK, FILE_TYPE_PIPE, GetFileType};
use windows::Win32::System::Console::{
    AllocConsole, FreeConsole, GetConsoleProcessList, GetConsoleWindow, GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE,
    STD_OUTPUT_HANDLE, SetStdHandle,
};

/// The frontend a launch runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Cli,
    Gui,
}

/// Takes `--gui` and `--cli` out of `args`, and says which was asked for: the last of them given decides, and `None` without either. The
/// session never sees them.
pub fn take_mode(args: &mut Vec<String>) -> Option<Mode> {
    let mut asked = None;
    args.retain(|arg| match arg.as_str() {
        "--gui" => {
            asked = Some(Mode::Gui);
            false
        }
        "--cli" => {
            asked = Some(Mode::Cli);
            false
        }
        _ => true,
    });
    asked
}

/// The first Windows build the window runs on: Windows 11 and Server 2025 (TODO 7). Below it the Windows App Runtime's bootstrap has no
/// `TryCreatePackageDependency` (build.rs).
pub const FIRST_GUI_BUILD: u32 = 22000;

/// How tmr.exe was launched, as `decide` reads it.
#[derive(Clone, Copy, Debug)]
pub struct Facts {
    /// `--cli` or `--gui`, the last given (`take_mode`)
    pub asked: Option<Mode>,
    /// Any argument besides those: a session's parameters, or a one-shot command
    pub arguments: bool,
    /// Standard output is a file or a pipe: `tmr.exe > out.txt`, or a remote session's. No standard output at all, which a double-click has
    /// under the manifest, isn't redirection.
    pub stdout_redirected: bool,
    /// The process has a console with a window (`GetConsoleWindow`)
    pub console_window: bool,
    /// The processes attached to the console, this one counted (`GetConsoleProcessList`): 1 for a console Windows opened for tmr.exe alone,
    /// more for a shell's, 0 with none
    pub console_processes: u32,
    /// The Windows build, `None` if it couldn't be read
    pub build: Option<u32>,
    /// This tmr.exe has the window: it was built with the `gui` feature
    pub gui_built: bool,
}

/// What a launch does, from `decide`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Launch {
    pub mode: Mode,
    /// Leave the console before the window opens: Windows opened it for tmr.exe alone, and it would stay up, empty, behind the window
    pub free_console: bool,
    /// Open a console before the CLI runs, which has none to show on: a launch with parameters and no console to inherit (a shortcut,
    /// Win+R), which the manifest left without one. Windows opened one for it before the GUI build had the manifest.
    pub alloc_console: bool,
    /// Why the window doesn't open, for the console
    pub note: Option<Note>,
}

/// Why the window doesn't open, when `--gui` asked for it or the launch looked like a double-click.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Note {
    /// `--gui`, in a tmr.exe built without the window. A double-click on that build gets no note: the CLI is what it has always run.
    NotBuilt,
    /// Windows is older than `FIRST_GUI_BUILD`, or its build couldn't be read
    OldWindows(Option<u32>),
}

impl std::fmt::Display for Note {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Note::NotBuilt => {
                write!(f, "Note: this tmr.exe was built without the window (cargo build --features tmr-app/gui): running the CLI")
            }
            Note::OldWindows(build) => write!(
                f,
                "Note: the window needs Windows build {FIRST_GUI_BUILD} or later (Windows 11, Server 2025), and this is {}: \
                 running the CLI",
                build.map_or("unknown".to_string(), |build| build.to_string())
            ),
        }
    }
}

/// Which frontend runs, and what to do about the console first. `--cli`/`--gui` decide; any other argument means the CLI; with none,
/// redirected output means the CLI (WinRM), no console the window (a double-click, from build 26100 with the manifest), a console of this
/// process alone the window after leaving it (a double-click below 26100), and a shared one the CLI (a shell). The window opens only in a
/// build that has it, from `FIRST_GUI_BUILD`; otherwise the CLI runs, with a note.
pub fn decide(facts: &Facts) -> Launch {
    let wanted = facts.asked.unwrap_or(if facts.arguments || facts.stdout_redirected {
        Mode::Cli
    } else if !facts.console_window || facts.console_processes == 1 {
        Mode::Gui
    } else {
        Mode::Cli
    });
    let opens = facts.gui_built && facts.build.is_some_and(|build| build >= FIRST_GUI_BUILD);
    let mode = if wanted == Mode::Gui && opens { Mode::Gui } else { Mode::Cli };
    let note = match (wanted, mode) {
        (Mode::Gui, Mode::Cli) if !facts.gui_built => (facts.asked == Some(Mode::Gui)).then_some(Note::NotBuilt),
        (Mode::Gui, Mode::Cli) => Some(Note::OldWindows(facts.build)),
        _ => None,
    };
    Launch {
        mode,
        free_console: mode == Mode::Gui && facts.console_window && facts.console_processes == 1,
        alloc_console: mode == Mode::Cli && facts.gui_built && !facts.console_window && !facts.stdout_redirected,
        note,
    }
}

/// This launch's facts: what the command line asked for (`asked`, and whether it has other `arguments`), the console and the Windows build.
pub fn probe(asked: Option<Mode>, arguments: bool) -> Facts {
    // SAFETY: none of them takes a pointer but `GetConsoleProcessList`, which writes at most the slice's length into it; with no console it
    // fails, and counts 0. No standard output at all (a double-click under the manifest) is an error from `GetStdHandle`, so not
    // redirected.
    let (stdout_redirected, console_window, console_processes) = unsafe {
        (
            GetStdHandle(STD_OUTPUT_HANDLE).is_ok_and(|out| redirected(GetFileType(out))),
            !GetConsoleWindow().is_invalid(),
            GetConsoleProcessList(&mut [0; 2]),
        )
    };
    Facts {
        asked,
        arguments,
        stdout_redirected,
        console_window,
        console_processes,
        build: windows_build(),
        gui_built: cfg!(feature = "gui"),
    }
}

/// Standard output of this type is redirected: a file or a pipe (`tmr.exe > out.txt`, a remote session's). A character device isn't, a
/// console's or NUL's (`> NUL` from a shell, which has the shell's console), and nor is a type Windows can't tell or a remote one.
fn redirected(file_type: FILE_TYPE) -> bool {
    matches!(file_type, FILE_TYPE_DISK | FILE_TYPE_PIPE)
}

/// The Windows build, from `RtlGetVersion`, which unlike `GetVersionEx` doesn't take the answer from the exe's manifest. `None` if it
/// fails.
fn windows_build() -> Option<u32> {
    use windows::Win32::System::SystemInformation::OSVERSIONINFOW;
    // ntdll's: the `windows` crate has it only behind a WDK feature the engine doesn't take
    #[link(name = "ntdll", kind = "raw-dylib")]
    unsafe extern "system" {
        fn RtlGetVersion(info: *mut OSVERSIONINFOW) -> i32;
    }
    let mut info = OSVERSIONINFOW { dwOSVersionInfoSize: size_of::<OSVERSIONINFOW>() as u32, ..Default::default() };
    // SAFETY: `info` is an OSVERSIONINFOW with its size set, valid for the call, which only writes it
    (unsafe { RtlGetVersion(&mut info) } == 0).then_some(info.dwBuildNumber)
}

/// Leaves the console (`Launch::free_console`) and empties the standard handles it leaves behind: a closed handle's value is given to the
/// next file opened, where a stray print would land. A print to an empty handle writes nothing (std takes it as no output).
pub fn free_console() {
    // SAFETY: no pointers; leaving a console this process alone has, and setting handles to null
    unsafe {
        let _ = FreeConsole();
        for handle in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            let _ = SetStdHandle(handle, HANDLE::default());
        }
    }
}

/// Opens a console for the CLI (`Launch::alloc_console`), which becomes its standard handles.
pub fn alloc_console() {
    // SAFETY: no arguments; a process that has a console already is refused, and keeps it
    let _ = unsafe { AllocConsole() };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `--gui` and `--cli` pick the frontend, the last one given deciding, and the session never sees them
    #[test]
    fn the_mode_flags_are_taken_out_of_the_arguments() {
        let take = |args: &[&str]| {
            let mut args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
            let asked = take_mode(&mut args);
            (asked, args)
        };
        let args = |args: &[&str]| args.iter().map(|a| a.to_string()).collect::<Vec<_>>();
        assert_eq!(take(&["tmr", "memory=50%"]), (None, args(&["tmr", "memory=50%"])));
        assert_eq!(take(&["tmr", "--gui", "memory=50%"]), (Some(Mode::Gui), args(&["tmr", "memory=50%"])));
        assert_eq!(take(&["tmr", "--gui", "cycles=1", "--cli"]), (Some(Mode::Cli), args(&["tmr", "cycles=1"])));
        assert_eq!(take(&["tmr", "--cli", "--gui"]), (Some(Mode::Gui), args(&["tmr"])));
    }

    /// Only a file or a pipe is redirected output: a console or NUL (a character device) isn't, and nor is a type that can't be told
    #[test]
    fn redirected_output_is_a_file_or_a_pipe() {
        use windows::Win32::Storage::FileSystem::{FILE_TYPE_CHAR, FILE_TYPE_REMOTE, FILE_TYPE_UNKNOWN};
        for (file_type, expected) in
            [(FILE_TYPE_DISK, true), (FILE_TYPE_PIPE, true), (FILE_TYPE_CHAR, false), (FILE_TYPE_UNKNOWN, false), (FILE_TYPE_REMOTE, false)]
        {
            assert_eq!(redirected(file_type), expected, "{file_type:?}");
        }
    }

    /// The launches: a double-click on build 26100 with the manifest (no console), one below it (a console of tmr.exe's own), a shell, a
    /// redirected shell, WinRM (a pipe, a console with no window), and a shortcut with parameters on 26100 (no console)
    fn double_click() -> Facts {
        Facts {
            asked: None,
            arguments: false,
            stdout_redirected: false,
            console_window: false,
            console_processes: 0,
            build: Some(26100),
            gui_built: true,
        }
    }
    fn own_console() -> Facts {
        Facts { console_window: true, console_processes: 1, build: Some(22631), ..double_click() }
    }
    fn shell() -> Facts {
        Facts { console_window: true, console_processes: 2, ..double_click() }
    }

    fn launch(mode: Mode, free_console: bool, alloc_console: bool, note: Option<Note>) -> Launch {
        Launch { mode, free_console, alloc_console, note }
    }

    /// The decision table (TODO 29 S6): the flags decide, then any other argument, then redirected output, then the console
    #[test]
    fn a_launch_picks_its_frontend() {
        let (cli, gui) = (launch(Mode::Cli, false, false, None), launch(Mode::Gui, false, false, None));
        let winrm = Facts { stdout_redirected: true, console_window: false, console_processes: 1, ..double_click() };
        let cases = [
            ("a double-click, 26100 with the manifest", double_click(), gui),
            ("a double-click below 26100: leaves its console", own_console(), launch(Mode::Gui, true, false, None)),
            ("a shell", shell(), cli),
            ("a shell, redirected", Facts { stdout_redirected: true, ..shell() }, cli),
            ("WinRM: a pipe and a console of no window", winrm, cli),
            ("a shell, with parameters", Facts { arguments: true, ..shell() }, cli),
            ("own console, with parameters", Facts { arguments: true, ..own_console() }, cli),
            (
                "no console, with parameters: one opened for the CLI",
                Facts { arguments: true, ..double_click() },
                launch(Mode::Cli, false, true, None),
            ),
            ("no console, redirected: the output has a place", Facts { arguments: true, stdout_redirected: true, ..double_click() }, cli),
            ("--gui from a shell", Facts { asked: Some(Mode::Gui), arguments: true, ..shell() }, gui),
            ("--gui, redirected", Facts { asked: Some(Mode::Gui), stdout_redirected: true, ..shell() }, gui),
            (
                "--gui in a console of its own: leaves it",
                Facts { asked: Some(Mode::Gui), ..own_console() },
                launch(Mode::Gui, true, false, None),
            ),
            (
                "--cli on a double-click: a console opened for it",
                Facts { asked: Some(Mode::Cli), ..double_click() },
                launch(Mode::Cli, false, true, None),
            ),
            ("--cli in a console of its own", Facts { asked: Some(Mode::Cli), ..own_console() }, cli),
        ];
        for (case, facts, expected) in cases {
            assert_eq!(decide(&facts), expected, "{case}");
        }
    }

    /// Below build 22000, an unknown build, or a tmr.exe without the window: the CLI, in the console it has, with the note when the window
    /// was asked for or the launch looked like a double-click, but none for a double-click on a build without the window
    #[test]
    fn without_the_window_a_launch_runs_the_cli() {
        let server_2022 = |facts: Facts| Facts { build: Some(20348), ..facts };
        let without = |facts: Facts| Facts { gui_built: false, ..facts };
        let old = |build| launch(Mode::Cli, false, false, Some(Note::OldWindows(build)));
        let cases = [
            ("a double-click on Server 2022", server_2022(own_console()), old(Some(20348))),
            ("--gui on Server 2022", server_2022(Facts { asked: Some(Mode::Gui), ..shell() }), old(Some(20348))),
            ("a shell on Server 2022", server_2022(shell()), launch(Mode::Cli, false, false, None)),
            ("an unknown build", Facts { build: None, ..own_console() }, old(None)),
            (
                "--gui without the window",
                without(Facts { asked: Some(Mode::Gui), ..shell() }),
                launch(Mode::Cli, false, false, Some(Note::NotBuilt)),
            ),
            ("a double-click without the window", without(own_console()), launch(Mode::Cli, false, false, None)),
            (
                "no console without the window: as launched",
                without(Facts { arguments: true, ..double_click() }),
                launch(Mode::Cli, false, false, None),
            ),
        ];
        for (case, facts, expected) in cases {
            assert_eq!(decide(&facts), expected, "{case}");
        }
        assert_eq!(
            Note::OldWindows(Some(20348)).to_string(),
            "Note: the window needs Windows build 22000 or later (Windows 11, Server 2025), and this is 20348: running the CLI"
        );
        assert_eq!(
            Note::OldWindows(None).to_string(),
            "Note: the window needs Windows build 22000 or later (Windows 11, Server 2025), and this is unknown: running the CLI"
        );
        assert_eq!(
            Note::NotBuilt.to_string(),
            "Note: this tmr.exe was built without the window (cargo build --features tmr-app/gui): running the CLI"
        );
    }

    /// The manifest asks only for the detached console, and doesn't carry the marker Reactor reads for a self-contained Windows App
    /// Runtime: with it, Reactor would skip the bootstrap of the installed one and look for the runtime's DLLs beside the exe
    #[test]
    fn the_manifest_detaches_the_console_and_leaves_reactor_its_bootstrap() {
        let manifest = include_str!("../tmr.manifest");
        assert!(manifest.contains(
            r#"<consoleAllocationPolicy xmlns="http://schemas.microsoft.com/SMI/2024/WindowsSettings">detached</consoleAllocationPolicy>"#
        ));
        assert!(!manifest.contains("windows-reactor-self-contained"));
        // The only setting
        assert_eq!(manifest.matches("<windowsSettings>").count(), 1);
        assert!(!manifest.contains("<dependency") && !manifest.contains("<trustInfo") && !manifest.contains("dpiAware"));
    }
}
