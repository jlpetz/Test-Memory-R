//! Optional timings tools (TODO 122): ZenTimings and LibreHardwareMonitor, downloaded from their official GitHub releases into
//! a per-user companions folder. TMR does not install their drivers and does not start them unless `--launch` is given: each asks for that
//! itself. ZenTimings is GPL-3.0 and wants an administrator; LibreHardwareMonitor is MPL-2.0. Neither is part of TMR.

use std::ffi::OsString;
use std::io::{self, Write};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{PCWSTR, w};

use crate::runner::RunStatus;

const ZEN_ZIP: &str = "ZenTimings_v142.713.zip";
const ZEN_URL: &str = "https://github.com/irusanov/ZenTimings/releases/download/v142.713/ZenTimings_v142.713.zip";
const ZEN_SHA_URL: &str = "https://github.com/irusanov/ZenTimings/releases/download/v142.713/ZenTimings_v142.713.zip.sha256";
const ZEN_SHA: &str = "c49f5a4a66a2c438d8b2bddfde5338df67ad8bed835a5f2e3ad421b21c4d76a0";

const LHM_ZIP: &str = "LibreHardwareMonitor.zip";
const LHM_URL: &str = "https://github.com/LibreHardwareMonitor/LibreHardwareMonitor/releases/download/v0.9.6/LibreHardwareMonitor.zip";
/// SHA-256 of the official GitHub release asset.
const LHM_SHA: &str = "086d9f1b5a99e643edc2cfaaac16051685b551e4c5ac0b32a57c58c0e529c001";

/// Per-user companions folder, or the companions folder in an explicit TMR_HOME.
pub fn dir() -> PathBuf {
    if std::env::var_os("TMR_HOME").is_some_and(|value| !value.is_empty()) {
        return crate::home::dir().join("companions");
    }
    std::env::var_os("LOCALAPPDATA")
        .map_or_else(|| crate::home::dir().to_path_buf(), |folder| PathBuf::from(folder).join("TMR"))
        .join("companions")
}

/// Download both tools, check the hashes, unpack them, and say how to turn on the readings TMR uses. One already unpacked is left as it is.
/// `--launch` starts the two programs; their own windows ask before a driver is installed.
pub fn get_readings(launch: bool, out: &mut dyn Write) -> io::Result<RunStatus> {
    if crate::home::elevated() {
        return Err(io::Error::other("Open TMR as a standard user to download or start the timings tools."));
    }
    let root = dir();
    std::fs::create_dir_all(&root).map_err(|e| io::Error::other(format!("{}: {e}", root.display())))?;
    writeln!(out, "Timings tools, from their official GitHub releases, into {}", root.display())?;
    writeln!(out, "TMR does not install their drivers. Each program asks before it does.")?;
    writeln!(out)?;

    let zen = present_or_fetch("ZenTimings", ZEN_URL, ZEN_ZIP, Some(ZEN_SHA_URL), ZEN_SHA, &root, out)?;
    let lhm = present_or_fetch("LibreHardwareMonitor", LHM_URL, LHM_ZIP, None, LHM_SHA, &root, out)?;
    writeln!(out)?;
    writeln!(out, "ZenTimings: {}", zen.display())?;
    writeln!(out, "  It wants an administrator. Enable Live Snapshot (export) so TMR can read CL-RCD-RP-RAS.")?;
    writeln!(out, "  The snapshot file it writes is read from beside it, or from TMR_TIMINGS.")?;
    writeln!(out, "LibreHardwareMonitor: {}", lhm.display())?;
    writeln!(out, "  Options: Remote web server, on this PC only, port 8085. TMR reads DIMM temperature from it.")?;
    writeln!(out, "  Without that server, the temperature line stays empty. The run does not need either tool.")?;

    if launch {
        writeln!(out)?;
        writeln!(out, "Starting them. Windows will ask if a program wants an administrator.")?;
        start_program(&zen, out)?;
        start_program(&lhm, out)?;
    } else {
        writeln!(out)?;
        writeln!(out, "Start them from that folder, or run tmr --get-readings --launch")?;
    }
    Ok(RunStatus::Passed)
}

/// The program already unpacked under `name`, or a fresh download checked against `expect`.
fn present_or_fetch(
    name: &str, url: &str, zip_name: &str, sha_url: Option<&str>, expect: &str, root: &Path, out: &mut dyn Write,
) -> io::Result<PathBuf> {
    let into = root.join(format!("{name}-{}", &expect[..12]));
    let executable = format!("{name}.exe");
    let exe = into.join(&executable);
    let _lock = crate::settings_file::lock(&into).map_err(io::Error::other)?;
    if exe.is_file() {
        writeln!(out, "{name} is already in {}", exe.display())?;
        return Ok(exe);
    }
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH).map_err(io::Error::other)?.as_nanos();
    let staging = root.join(format!(".{name}-{}-{nonce}", std::process::id()));
    std::fs::create_dir(&staging)?;
    let result = (|| {
        let unpacked = fetch(name, url, zip_name, sha_url, expect, &staging, out)?;
        if !unpacked.join(executable).is_file() {
            return Err(io::Error::other(format!("{name}: the verified archive did not contain the expected executable")));
        }
        std::fs::rename(unpacked, into)?;
        Ok(exe)
    })();
    if let Err(error) = std::fs::remove_dir_all(&staging) {
        log::warn!("Could not remove {}: {error}", staging.display());
    }
    result
}

fn fetch(
    name: &str, url: &str, zip_name: &str, sha_url: Option<&str>, expect: &str, root: &Path, out: &mut dyn Write,
) -> io::Result<PathBuf> {
    let zip = root.join(zip_name);
    writeln!(out, "Downloading {name}…")?;
    download(url, &zip)?;
    if let Some(sha_url) = sha_url {
        let sha_file = root.join(format!("{zip_name}.sha256"));
        download(sha_url, &sha_file)?;
        let published = std::fs::read_to_string(&sha_file).map_err(|e| io::Error::other(e.to_string()))?;
        let published = hex_token(&published).ok_or_else(|| io::Error::other("the published hash is not a SHA-256"))?;
        if !published.eq_ignore_ascii_case(expect) {
            return Err(io::Error::other(format!("{name}: the published hash is not the one TMR knows ({expect})")));
        }
    }
    let got = file_sha256(&zip)?;
    if !got.eq_ignore_ascii_case(expect) {
        let _ = std::fs::remove_file(&zip);
        return Err(io::Error::other(format!("{name}: hash {got}, expected {expect}. The file was not unpacked.")));
    }
    writeln!(out, "  hash {got}")?;
    let into = root.join("unpacked");
    std::fs::create_dir(&into)?;
    unpack(&zip, &into)?;
    Ok(into)
}

fn download(url: &str, dest: &Path) -> io::Result<()> {
    let status = Command::new(system_tool("curl.exe")?)
        .args(["--disable", "--fail", "--location", "--silent", "--show-error", "--max-time", "120", "--output"])
        .arg(dest)
        .arg(url)
        .status()
        .map_err(|e| io::Error::other(format!("curl.exe: {e}")))?;
    if status.success() { Ok(()) } else { Err(io::Error::other(format!("download failed ({status}): {url}"))) }
}

fn unpack(zip: &Path, into: &Path) -> io::Result<()> {
    let status = Command::new(system_tool("tar.exe")?)
        .args(["-xf"])
        .arg(zip)
        .args(["-C"])
        .arg(into)
        .status()
        .map_err(|e| io::Error::other(format!("tar.exe: {e}")))?;
    if status.success() { Ok(()) } else { Err(io::Error::other(format!("unpack failed ({status}): {}", zip.display()))) }
}

fn file_sha256(path: &Path) -> io::Result<String> {
    let output = Command::new(system_tool("certutil.exe")?)
        .args(["-hashfile"])
        .arg(path)
        .arg("SHA256")
        .output()
        .map_err(|e| io::Error::other(format!("certutil.exe: {e}")))?;
    if !output.status.success() {
        return Err(io::Error::other(String::from_utf8_lossy(&output.stderr).trim().to_string()));
    }
    hex_token(&String::from_utf8_lossy(&output.stdout)).ok_or_else(|| io::Error::other("certutil did not print a SHA-256"))
}

fn hex_token(text: &str) -> Option<String> {
    text.split_whitespace()
        .find(|word| word.len() == 64 && word.chars().all(|ch| ch.is_ascii_hexdigit()))
        .map(|word| word.to_ascii_lowercase())
}

fn system_tool(name: &str) -> io::Result<PathBuf> {
    let root = std::env::var_os("SystemRoot").ok_or_else(|| io::Error::other("Windows did not provide SystemRoot"))?;
    Ok(PathBuf::from(root).join("System32").join(name))
}

/// The pinned ZenTimings build writes its live snapshot beside the executable.
pub(crate) fn zen_snapshot() -> PathBuf {
    dir().join(format!("ZenTimings-{}", &ZEN_SHA[..12])).join("live_snapshot_device_config.json")
}

fn start_program(exe: &Path, out: &mut dyn Write) -> io::Result<()> {
    let wide = |value: OsString| value.encode_wide().chain(Some(0)).collect::<Vec<u16>>();
    let path = wide(exe.as_os_str().to_os_string());
    let folder = wide(exe.parent().unwrap_or(Path::new(".")).as_os_str().to_os_string());
    // SAFETY: the executable and working directory are NUL-terminated buffers valid throughout the call.
    let result = unsafe { ShellExecuteW(None, w!("open"), PCWSTR(path.as_ptr()), PCWSTR::null(), PCWSTR(folder.as_ptr()), SW_SHOWNORMAL) };
    if result.0 as isize <= 32 {
        return Err(io::Error::other(format!("Could not start {} (Windows Shell error {})", exe.display(), result.0 as isize)));
    }
    writeln!(out, "Started {}", exe.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sha256_line_is_its_hex_token() {
        assert_eq!(
            hex_token("c49f5a4a66a2c438d8b2bddfde5338df67ad8bed835a5f2e3ad421b21c4d76a0  ZenTimings_v142.713.zip").as_deref(),
            Some("c49f5a4a66a2c438d8b2bddfde5338df67ad8bed835a5f2e3ad421b21c4d76a0")
        );
        assert_eq!(
            hex_token("SHA256 hash of file:\r\n086d9f1b5a99e643edc2cfaaac16051685b551e4c5ac0b32a57c58c0e529c001\r\nCertUtil").as_deref(),
            Some("086d9f1b5a99e643edc2cfaaac16051685b551e4c5ac0b32a57c58c0e529c001")
        );
        assert!(hex_token("no hash here").is_none());
    }
}
