//! Where a click's help goes when the window can't open (TODO 121): the console Windows opened for tmr.exe closes as it exits, so the help
//! goes to a shell that stays, at its prompt in tmr.exe's folder, as `tmr` typed into a shell leaves you at that shell's prompt. A new
//! Windows Terminal window when it is installed, which shows the help itself and draws TMR's emoji; otherwise this console, after the help
//! and the classic console's warning (tmr-cli), the shell taking it over as tmr.exe exits. The shell is the newest PowerShell found
//! (`newest_shell`), Command Prompt only when there is none. Either way the folder starts the shell's PATH, so `tmr` runs as typed:
//! PowerShell doesn't run a program from the current folder without `.\`, and a bare tmr.exe isn't on the PATH as the installer's is.

use std::ffi::OsStr;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A shell to leave at its prompt
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Shell {
    /// PowerShell: `pwsh.exe` (6 and later) or Windows PowerShell 5.1's `powershell.exe`
    PowerShell(PathBuf),
    /// Command Prompt, only when no PowerShell is found
    Cmd(PathBuf),
}

/// The newest PowerShell: `pwsh` on the PATH, what typing `pwsh` gives (so the one you chose, of several), then the newest release the
/// PowerShell installer recorded (`installed_powershells`, previews left out), then Windows PowerShell 5.1, which every Windows has.
/// Command Prompt only when none is found. No version is named: every PowerShell since 6.0 is `pwsh.exe`, so a later one is found the
/// same way, and one that weren't would leave 5.1, not nothing.
pub fn newest_shell() -> Shell {
    let path = std::env::var_os("PATH").unwrap_or_default();
    if let Some(pwsh) = on_path(&path, "pwsh.exe") {
        return Shell::PowerShell(pwsh);
    }
    if let Some(pwsh) = newest(&installed_powershells()) {
        return Shell::PowerShell(pwsh);
    }
    let windows = std::env::var_os("SystemRoot").map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
    let windows_powershell = windows.join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
    if exists(&windows_powershell) {
        return Shell::PowerShell(windows_powershell);
    }
    Shell::Cmd(std::env::var_os("ComSpec").map_or_else(|| windows.join(r"System32\cmd.exe"), PathBuf::from))
}

/// Windows Terminal, when it is installed: `wt.exe` on the PATH, the alias its package puts in WindowsApps
pub fn windows_terminal() -> Option<PathBuf> {
    on_path(&std::env::var_os("PATH").unwrap_or_default(), "wt.exe")
}

/// Opens a new Windows Terminal window (`wt`) in `exe`'s folder, running `powershell`, which shows `exe --help` and stays at its prompt.
/// Returns once Windows Terminal has started; tmr.exe exits after, and its own console closes.
pub fn open_terminal(wt: &Path, powershell: &Path, exe: &Path) -> std::io::Result<()> {
    let dir = folder(exe);
    // `-w new`: a window of its own, not a tab in one already open. The command is encoded: Windows Terminal takes a `;` as its own
    // separator, which base64 has none of.
    Command::new(wt).args(["-w", "new", "-d"]).arg(dir).arg(powershell).args(powershell_args(dir, Some(exe))).spawn().map(drop)
}

/// Starts `shell` in this console, at its prompt in `exe`'s folder; tmr.exe exits after, and the console stays with the shell. Returns
/// once the shell has started.
pub fn stay(shell: &Shell, exe: &Path) -> std::io::Result<()> {
    let dir = folder(exe);
    let mut command = match shell {
        Shell::PowerShell(powershell) => {
            let mut command = Command::new(powershell);
            command.args(powershell_args(dir, None));
            command
        }
        Shell::Cmd(cmd) => {
            let mut command = Command::new(cmd);
            // As cmd parses it, not as a C program's arguments are quoted
            command.arg("/k").raw_arg(format!("set \"PATH={};%PATH%\"", dir.display()));
            command
        }
    };
    command.current_dir(dir).spawn().map(drop)
}

/// The folder `exe` is in, which the shell starts in and its PATH starts with
fn folder(exe: &Path) -> &Path {
    exe.parent().unwrap_or(Path::new("."))
}

/// PowerShell's arguments: no banner, stay at the prompt, and the command, `dir` first on the PATH and, with `help`, that exe's help. The
/// command is encoded (`-EncodedCommand`, base64 of its UTF-16), so no quoting of a path can break it, and nothing between, Windows
/// Terminal's own parsing included, can split it.
fn powershell_args(dir: &Path, help: Option<&Path>) -> [String; 4] {
    let quote = |path: &Path| format!("'{}'", path.display().to_string().replace('\'', "''"));
    let mut command = format!("$env:Path = {} + ';' + $env:Path", quote(dir));
    if let Some(exe) = help {
        command.push_str(&format!("; & {} --help", quote(exe)));
    }
    ["-NoLogo".to_string(), "-NoExit".to_string(), "-EncodedCommand".to_string(), encoded(&command)]
}

/// A command as `-EncodedCommand` takes it: base64 of its UTF-16LE
fn encoded(command: &str) -> String {
    base64(&command.encode_utf16().flat_map(u16::to_le_bytes).collect::<Vec<u8>>())
}

/// Standard base64, with its padding
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let byte = |i: usize| u32::from(chunk.get(i).copied().unwrap_or(0));
        let triple = (byte(0) << 16) | (byte(1) << 8) | byte(2);
        for i in 0..4 {
            out.push(if i <= chunk.len() { char::from(ALPHABET[(triple >> (18 - 6 * i) & 63) as usize]) } else { '=' });
        }
    }
    out
}

/// `name` in the first folder of `path` (a PATH) that has it
fn on_path(path: &OsStr, name: &str) -> Option<PathBuf> {
    std::env::split_paths(path).map(|dir| dir.join(name)).find(|file| exists(file))
}

/// `file` is there, and not a folder. Not followed: an app's alias in WindowsApps (wt.exe, a Store PowerShell's pwsh.exe) is a reparse
/// point only a launch can follow.
fn exists(file: &Path) -> bool {
    std::fs::symlink_metadata(file).is_ok_and(|metadata| !metadata.is_dir())
}

/// The newest release in `installed` (its version, its pwsh.exe): a preview's or release candidate's version (`7.7.0-preview.3`) isn't a
/// release, and nor is what isn't a version
fn newest(installed: &[(String, PathBuf)]) -> Option<PathBuf> {
    installed
        .iter()
        .filter_map(|(version, pwsh)| Some((release(version)?, pwsh)))
        .max_by(|a, b| a.0.cmp(&b.0))
        .map(|(_, pwsh)| pwsh.clone())
}

/// A release's version as numbers, compared part by part (7.10.0 after 7.6.9); none for a preview's or what isn't a version
fn release(version: &str) -> Option<Vec<u32>> {
    version.trim().split('.').map(|part| part.parse().ok()).collect()
}

/// The PowerShells the PowerShell installer recorded, each its version and pwsh.exe: a key a release under
/// `HKLM\SOFTWARE\Microsoft\PowerShellCore\InstalledVersions`, with `SemanticVersion` and `InstallLocation`. Those whose pwsh.exe has
/// gone are left out. A Store or winget-MSIX PowerShell records none, but its alias is on the PATH.
fn installed_powershells() -> Vec<(String, PathBuf)> {
    use windows::Win32::System::Registry::{HKEY, HKEY_LOCAL_MACHINE, KEY_READ, RegCloseKey, RegEnumKeyExW, RegOpenKeyExW};
    use windows::core::{PWSTR, w};

    let mut key = HKEY(std::ptr::null_mut());
    // SAFETY: a constant key name, and `key` a local the call writes, which outlives it
    if unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, w!(r"SOFTWARE\Microsoft\PowerShellCore\InstalledVersions"), None, KEY_READ, &mut key) }
        .is_err()
    {
        return Vec::new();
    }
    let mut found = Vec::new();
    // A key a release, which an installer of each major version adds: never many
    for index in 0..64 {
        let mut name = [0u16; 128];
        let mut length = name.len() as u32;
        // SAFETY: `name` is a local buffer and `length` its size in characters, which the call writes no more than (the name and its NUL)
        if unsafe { RegEnumKeyExW(key, index, Some(PWSTR(name.as_mut_ptr())), &mut length, None, None, None, None) }.is_err() {
            break;
        }
        // The release's key name, its NUL kept for the reads below
        let Some(release) = name.get(..length as usize + 1) else { continue };
        if let (Some(version), Some(location)) = (string(key, release, w!("SemanticVersion")), string(key, release, w!("InstallLocation")))
        {
            found.push((version, PathBuf::from(location).join("pwsh.exe")));
        }
    }
    // SAFETY: the key opened above, closed once
    let _ = unsafe { RegCloseKey(key) };
    found.retain(|(_, pwsh)| exists(pwsh));
    found
}

/// The string `value` of `key`'s subkey `subkey` (its name and NUL), if it is one
fn string(key: windows::Win32::System::Registry::HKEY, subkey: &[u16], value: windows::core::PCWSTR) -> Option<String> {
    use windows::Win32::System::Registry::{RRF_RT_REG_SZ, RegGetValueW};
    let mut data = [0u16; 512];
    let mut size = (data.len() * 2) as u32;
    // SAFETY: `subkey` ends in its NUL and `value` is a constant; `data` is a local buffer of `size` bytes, which the call writes no more
    // than, and `size` a local it writes the length to
    let read = unsafe {
        RegGetValueW(
            key,
            windows::core::PCWSTR(subkey.as_ptr()),
            value,
            RRF_RT_REG_SZ,
            None,
            Some(data.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    if read.is_err() {
        return None;
    }
    // `size` counts bytes, the NUL included
    let characters = (size as usize / 2).saturating_sub(1).min(data.len());
    Some(String::from_utf16_lossy(&data[..characters]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 4648's vectors, and the example of PowerShell's own help for `-EncodedCommand`
    #[test]
    fn a_command_is_encoded_as_powershell_takes_it() {
        for (bytes, expected) in [
            (&b""[..], ""),
            (b"f", "Zg=="),
            (b"fo", "Zm8="),
            (b"foo", "Zm9v"),
            (b"foob", "Zm9vYg=="),
            (b"fooba", "Zm9vYmE="),
            (b"foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(bytes), expected);
        }
        assert_eq!(encoded(r#"dir "c:\program files" "#), "ZABpAHIAIAAiAGMAOgBcAHAAcgBvAGcAcgBhAG0AIABmAGkAbABlAHMAIgAgAA==");
    }

    /// The newest release wins, part by part; previews and what isn't a version are left out
    #[test]
    fn the_newest_powershell_release_is_picked() {
        let installed = |versions: &[&str]| -> Vec<(String, PathBuf)> {
            versions.iter().map(|version| (version.to_string(), PathBuf::from(format!(r"C:\PS\{version}\pwsh.exe")))).collect()
        };
        let pick = |versions: &[&str]| newest(&installed(versions)).map(|pwsh| pwsh.display().to_string());
        assert_eq!(pick(&["7.6.9"]).as_deref(), Some(r"C:\PS\7.6.9\pwsh.exe"));
        assert_eq!(pick(&["7.6.9", "7.10.0"]).as_deref(), Some(r"C:\PS\7.10.0\pwsh.exe"), "7.10 after 7.6, not before it as text");
        assert_eq!(pick(&["7.6.9", "8.0.0", "7.7.0"]).as_deref(), Some(r"C:\PS\8.0.0\pwsh.exe"), "a later major, found the same way");
        assert_eq!(pick(&["7.6.9", "7.7.0-preview.3"]).as_deref(), Some(r"C:\PS\7.6.9\pwsh.exe"), "a preview isn't a release");
        assert_eq!(pick(&["7.7.0-rc.1", "not a version"]), None);
        assert_eq!(pick(&[]), None);
    }

    /// A PATH's first folder that has the file, a link not followed
    #[test]
    fn a_program_is_found_on_the_path() {
        let root = std::env::temp_dir().join(format!("tmr-shell-test-{}", std::process::id()));
        let (a, b) = (root.join("a"), root.join("b"));
        std::fs::create_dir_all(&a).expect("a test folder");
        std::fs::create_dir_all(b.join("pwsh.exe")).expect("a folder named as the program");
        std::fs::write(a.join("pwsh.exe"), b"").expect("a test file");
        let path = std::env::join_paths([b.clone(), a.clone()]).expect("a PATH");
        assert_eq!(on_path(&path, "pwsh.exe"), Some(a.join("pwsh.exe")), "a folder of the program's name is passed over");
        assert_eq!(on_path(&path, "wt.exe"), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The command PowerShell runs: the folder first on the PATH, then, in Windows Terminal, the help; a quote in a path doubled
    #[test]
    fn powershell_is_told_the_folder_and_the_help() {
        let decode = |args: &[String; 4]| {
            assert_eq!(args[..3], ["-NoLogo", "-NoExit", "-EncodedCommand"]);
            args[3].clone()
        };
        let dir = Path::new(r"C:\Users\O'Brien\TMR");
        assert_eq!(decode(&powershell_args(dir, None)), encoded(r"$env:Path = 'C:\Users\O''Brien\TMR' + ';' + $env:Path"));
        assert_eq!(
            decode(&powershell_args(dir, Some(&dir.join("tmr.exe")))),
            encoded(r"$env:Path = 'C:\Users\O''Brien\TMR' + ';' + $env:Path; & 'C:\Users\O''Brien\TMR\tmr.exe' --help")
        );
    }
}
