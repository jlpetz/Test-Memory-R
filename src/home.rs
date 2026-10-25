//! TMR's home (TODO 104): the one folder that holds what it reads and writes of its own, `configs\`, `results\`, `logs\`, `tmr-cfg.json`
//! and the window's `colours.json`. It is the real exe's folder (the owner, 2026-10-25), so an unzipped tmr.exe keeps them beside it as
//! TM5 did, and so does an installed one: for all users, in Program Files, the installer makes `configs\`, `results\`, `logs\` and
//! `tmr-cfg.json` writable by users (`dist/tmr.iss`). `TMR_HOME` puts the home elsewhere: for `cargo run`, whose exe is under `target\`,
//! which `cargo clean` empties, and for scripts.
//!
//! Not the current folder, as it was: that is wherever TMR started, the shell's folder, System32 for an Explorer-style launch or an
//! elevated prompt, Documents over WinRM. A path the user types (`config=`, `--compare-results`) still means the current folder first.

use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The environment variable that names another home
pub const HOME_VAR: &str = "TMR_HOME";

/// TMR's home, worked out once: `TMR_HOME` when it is set (a relative one from the current folder), else the folder of the real exe.
/// `current_exe` gives the path the exe was started by, which can be a symlink or a short (8.3) name, so it is resolved first, and taken
/// as it is when it can't be. The current folder only when even `current_exe` fails.
pub fn dir() -> &'static Path {
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        if let Some(home) = from_var() {
            return std::path::absolute(&home).unwrap_or(home);
        }
        std::env::current_exe()
            .ok()
            .map(|exe| std::fs::canonicalize(&exe).unwrap_or(exe))
            .and_then(|exe| exe.parent().map(without_verbatim_prefix))
            .unwrap_or_else(|| std::path::absolute(".").unwrap_or_else(|_| PathBuf::from(".")))
    })
}

/// `TMR_HOME`, when it is set and not empty
fn from_var() -> Option<PathBuf> {
    std::env::var_os(HOME_VAR).filter(|home| !home.is_empty()).map(PathBuf::from)
}

/// The configs the user keeps, which a relative `config=` is looked for in after the current folder
pub fn configs() -> PathBuf {
    dir().join("configs")
}

/// The run's results files
pub fn results() -> PathBuf {
    dir().join("results")
}

/// The run's log files
pub fn logs() -> PathBuf {
    dir().join("logs")
}

/// Makes the home's folders, and checks TMR can write in the two a run writes to, `results\` and `logs\`: a session calls it before it
/// allocates, so a home TMR can't write to (a tmr.exe copied into Program Files) stops it then, not at the end of a run with no results.
/// `configs\` is only the user's: a run never writes there, so one that can't be made stops nothing.
pub fn prepare() -> Result<(), String> {
    if let Err(e) = std::fs::create_dir_all(configs()) {
        log::debug!("Couldn't make {}: {e}", configs().display());
    }
    for folder in [results(), logs()] {
        std::fs::create_dir_all(&folder).and_then(|()| probe(&folder)).map_err(|e| cant_write(&folder, &e))?;
    }
    Ok(())
}

/// Whether TMR can save `path` in its home: write the file when it is there (the installer's `tmr-cfg.json`, writable by users in a
/// folder that isn't), or make a new one in its folder when it isn't. Opening an existing file to write it, without truncating, changes
/// nothing; a calibration checks it before it measures, rather than finding out after.
pub(crate) fn can_save(path: &Path) -> Result<(), String> {
    let checked =
        if path.exists() { std::fs::OpenOptions::new().write(true).open(path).map(drop) } else { path.parent().map_or(Ok(()), probe) };
    checked.map_err(|e| cant_write(path, &e))
}

/// Writes a new file in `folder` and deletes it. A name of this process's own, and only a new file: in an install for all users these
/// folders are writable by every user, and TMR may run elevated, so it never opens a file someone else put there, a link among them.
/// Deleted by its handle when it closes, not by its name.
fn probe(folder: &Path) -> std::io::Result<()> {
    use windows::Win32::Foundation::GENERIC_WRITE;
    use windows::Win32::Storage::FileSystem::{DELETE, FILE_FLAG_DELETE_ON_CLOSE};
    let name = format!(".tmr-write-check-{}", std::process::id());
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .access_mode(GENERIC_WRITE.0 | DELETE.0)
        .custom_flags(FILE_FLAG_DELETE_ON_CLOSE.0)
        .open(folder.join(name))
        .map(drop)
}

fn cant_write(path: &Path, e: &std::io::Error) -> String {
    match from_var() {
        Some(home) => format!(
            "TMR can't write to {} ({e}). {HOME_VAR} is {}: set it to a folder you can write to, or unset it to keep TMR's files beside \
             tmr.exe.",
            path.display(),
            home.display()
        ),
        None => format!(
            "TMR can't write to {} ({e}). It keeps its files beside tmr.exe: move tmr.exe to a folder you can write to, install it with \
             its installer, or set {HOME_VAR} to such a folder.",
            path.display()
        ),
    }
}

/// The file `path` names, and where it was looked for when it isn't found: as typed, from the current folder; then, when `path` is
/// relative, in `folder` (the home's `configs\` or `results\`), and in the home, as TMR shows its own files (`results\TMR_….json`).
pub(crate) fn find(path: &str, folder: &Path) -> Result<PathBuf, String> {
    let typed = Path::new(path);
    if typed.exists() {
        return Ok(typed.to_path_buf());
    }
    if typed.is_relative() {
        for place in [folder, dir()] {
            let found = place.join(typed);
            if found.exists() {
                return Ok(found);
            }
        }
        return Err(format!("looked in the current folder, {} and {}", folder.display(), dir().display()));
    }
    Err("looked where it says".to_string())
}

/// `path` as Explorer shows it: `canonicalize` gives a drive path as `\\?\C:\...` and a share as `\\?\UNC\server\share\...`.
fn without_verbatim_prefix(path: &Path) -> PathBuf {
    let Some(text) = path.to_str() else { return path.to_path_buf() };
    if let Some(share) = text.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{share}"))
    } else if let Some(drive) = text.strip_prefix(r"\\?\").filter(|rest| rest.as_bytes().get(1) == Some(&b':')) {
        PathBuf::from(drive)
    } else {
        path.to_path_buf()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A canonical path loses the `\\?\` prefix a user never typed, a drive's and a share's; any other path is kept as it is.
    #[test]
    fn a_home_reads_as_explorer_shows_it() {
        for (canonical, shown) in [
            (r"\\?\C:\Program Files\TMR", r"C:\Program Files\TMR"),
            (r"\\?\UNC\server\share\tmr", r"\\server\share\tmr"),
            (r"C:\tools", r"C:\tools"),
            (r"\\?\Volume{0e5b}\tmr", r"\\?\Volume{0e5b}\tmr"),
        ] {
            assert_eq!(without_verbatim_prefix(Path::new(canonical)), PathBuf::from(shown), "{canonical}");
        }
    }

    /// A file named as typed is found from the current folder first; a relative name not there, in the folder given; an absolute one
    /// only where it says. One found nowhere says where it was looked for.
    #[test]
    fn a_typed_path_is_found_here_then_in_the_home() {
        let dir = crate::results::TempDir::new("home-find");
        let folder = Path::new(&dir.file("configs")).to_path_buf();
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("mine.cfg"), b"").unwrap();
        assert_eq!(find("mine.cfg", &folder), Ok(folder.join("mine.cfg")), "in the folder");
        assert_eq!(find("Cargo.toml", &folder), Ok(PathBuf::from("Cargo.toml")), "here first");
        assert!(find("absent.cfg", &folder).unwrap_err().contains(&folder.display().to_string()), "says where it looked");
        let absolute = folder.join("mine.cfg").display().to_string();
        assert_eq!(find(&absolute, &folder), Ok(PathBuf::from(&absolute)));
        assert!(find(&folder.join("absent.cfg").display().to_string(), &folder).is_err());
    }

    /// The write check leaves nothing behind, and a file already there in an existing one's place doesn't stop it.
    #[test]
    fn the_write_check_leaves_no_file() {
        let dir = crate::results::TempDir::new("home-probe");
        let folder = Path::new(&dir.file("results")).to_path_buf();
        std::fs::create_dir_all(&folder).unwrap();
        probe(&folder).unwrap();
        probe(&folder).unwrap();
        assert_eq!(std::fs::read_dir(&folder).unwrap().count(), 0, "the probe's file is gone");
        let saved = folder.join("tmr-cfg.json");
        assert!(can_save(&saved).is_ok(), "a new file in a folder TMR can write");
        std::fs::write(&saved, b"{}").unwrap();
        assert!(can_save(&saved).is_ok(), "the file there");
        assert_eq!(std::fs::read(&saved).unwrap(), b"{}", "untouched by the check");
    }
}
