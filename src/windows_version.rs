//! Windows' version as a run records it (TODO 123): the product, its feature version, and its build with the revision cumulative updates
//! raise (UBR, the update build revision: 33650 in 26100.33650). Read once, from `HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion`.
//!
//! It goes on the console's system block and the window's System tab (`Setup::SystemInfo`), into the log with them, and into the results
//! (`RunIdentity::windows`), and it picks how the stitched allocator commits (`stitched::commit_method_for`): Microsoft's fix for the
//! refused-replace bugcheck comes in an update, which raises the revision, not the build. Never in the machine's fingerprint (`machine_id`,
//! `MachineIdentity`): a Windows update doesn't make a calibration stale (the owner, 2026-11-09).

#![warn(clippy::undocumented_unsafe_blocks)]

use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use windows::Win32::System::Registry::{HKEY_LOCAL_MACHINE, REG_ROUTINE_FLAGS, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegGetValueW};
use windows::core::{PCWSTR, w};

/// Windows' version: `Windows Server 2025 Datacenter 24H2, build 26100.33650`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowsVersion {
    /// The product: `Windows Server 2025 Datacenter`, `Windows 11 Pro` (`product_name`)
    pub name: String,
    /// Its feature version, `24H2`; empty where Windows doesn't say
    pub display_version: String,
    pub build: u32,
    /// The revision cumulative updates raise (UBR); 0 where Windows doesn't say
    pub revision: u32,
}

impl WindowsVersion {
    /// This machine's, read once; `None` when the registry doesn't name the product and its build
    pub fn detect() -> Option<&'static WindowsVersion> {
        static VERSION: OnceLock<Option<WindowsVersion>> = OnceLock::new();
        VERSION.get_or_init(read).as_ref()
    }

    /// `Windows Server 2025 Datacenter 24H2, build 26100.33650`
    pub fn label(&self) -> String {
        let feature = if self.display_version.is_empty() { String::new() } else { format!(" {}", self.display_version) };
        format!("{}{feature}, build {}.{}", self.name, self.build, self.revision)
    }
}

/// The product's name as Windows 11 has it. The registry's `ProductName` still says Windows 10 on a Windows 11 client (Microsoft never
/// changed it), so a client from build 22000 on is named 11; a server's name is its own (`Windows Server 2025 Datacenter`).
fn product_name(registry: &str, client: bool, build: u32) -> String {
    if client && build >= 22000 { registry.replacen("Windows 10", "Windows 11", 1) } else { registry.to_string() }
}

/// The version from the registry's `CurrentVersion` key
fn read() -> Option<WindowsVersion> {
    let name = string(w!("ProductName"))?;
    let build = string(w!("CurrentBuildNumber"))?.trim().parse().ok()?;
    let client = string(w!("InstallationType")).is_some_and(|kind| kind.eq_ignore_ascii_case("Client"));
    Some(WindowsVersion {
        name: product_name(&name, client, build),
        display_version: string(w!("DisplayVersion")).unwrap_or_default(),
        build,
        revision: dword(w!("UBR")).unwrap_or(0),
    })
}

/// The `CurrentVersion` key, as `RegGetValueW` takes it
const KEY: PCWSTR = w!(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion");

/// `value`'s string, if it is one
fn string(value: PCWSTR) -> Option<String> {
    let mut data = [0u16; 256];
    let size = get(value, RRF_RT_REG_SZ, data.as_mut_ptr().cast(), size_of_val(&data))?;
    // `size` counts bytes, the NUL included
    let characters = (size / 2).saturating_sub(1).min(data.len());
    Some(String::from_utf16_lossy(&data[..characters]))
}

/// `value`'s number, if it is one
fn dword(value: PCWSTR) -> Option<u32> {
    let mut data = 0u32;
    get(value, RRF_RT_REG_DWORD, (&raw mut data).cast(), size_of::<u32>())?;
    Some(data)
}

/// Reads `value` of `KEY` into `data` (`capacity` bytes), as `flags` requires it to be; the bytes written
fn get(value: PCWSTR, flags: REG_ROUTINE_FLAGS, data: *mut core::ffi::c_void, capacity: usize) -> Option<usize> {
    let mut size = u32::try_from(capacity).ok()?;
    // SAFETY: `KEY` and `value` are NUL-terminated constants; `data` is the caller's local of `capacity` bytes, which the call writes no
    // more than, and `size` a local it writes the length to
    unsafe { RegGetValueW(HKEY_LOCAL_MACHINE, KEY, value, flags, None, Some(data), Some(&mut size)) }.ok().ok()?;
    usize::try_from(size).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Windows 11 client is named 11, which its registry doesn't; Windows 10's build and a server keep their own names
    #[test]
    fn a_windows_11_client_is_named_11() {
        assert_eq!(product_name("Windows 10 Pro", true, 26100), "Windows 11 Pro");
        assert_eq!(product_name("Windows 10 Pro", true, 22000), "Windows 11 Pro");
        assert_eq!(product_name("Windows 10 Pro", true, 19045), "Windows 10 Pro");
        assert_eq!(product_name("Windows Server 2025 Datacenter", false, 26100), "Windows Server 2025 Datacenter");
    }

    /// The label: the product, its feature version when Windows gives one, and the build with its revision
    #[test]
    fn the_label_has_the_build_and_its_revision() {
        let server =
            WindowsVersion { name: "Windows Server 2025 Datacenter".into(), display_version: "24H2".into(), build: 26100, revision: 33650 };
        assert_eq!(server.label(), "Windows Server 2025 Datacenter 24H2, build 26100.33650");
        let old = WindowsVersion { display_version: String::new(), build: 17763, revision: 0, ..server };
        assert_eq!(old.label(), "Windows Server 2025 Datacenter, build 17763.0");
    }

    /// This machine's, read from the registry: every Windows TMR runs on names its product and build
    #[test]
    fn this_machines_version_is_read() {
        let version = WindowsVersion::detect().expect("the registry names the product and its build");
        assert!(version.name.starts_with("Windows"), "{version:?}");
        assert!(version.build >= 20348, "{version:?}");
    }
}
