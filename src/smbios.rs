//! SMBIOS table parser for system identity and memory module detection.
//!
//! Reads raw SMBIOS data via `GetSystemFirmwareTable(RSMB)` and parses:
//! - Type 0: BIOS Information (vendor, version)
//! - Type 1: System Information (UUID, manufacturer, product)
//! - Type 17: Memory Device (per-DIMM serial, part, manufacturer, speed, capacity)

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::OnceLock;
use serde::{Deserialize, Serialize};

// ============================================================================
// Public data structures
// ============================================================================

/// BIOS information from SMBIOS Type 0
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BiosInfo {
    pub vendor: String,
    pub version: String,
    /// BIOS release date in ISO format (YYYY-MM-DD), converted from SMBIOS MM/DD/YYYY
    pub release_date: String,
}

/// System identity from SMBIOS Type 1
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SystemIdentity {
    pub manufacturer: String,
    pub product_name: String,
    /// System UUID (motherboard/VM instance identity)
    pub uuid: String,
}

/// Per-DIMM memory module info from SMBIOS Type 17
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MemoryModule {
    pub locator: String,
    pub manufacturer: String,
    pub serial_number: String,
    pub part_number: String,
    /// Configured speed in MT/s
    pub speed_mts: u32,
    /// Module capacity in bytes
    pub capacity_bytes: u64,
    /// Human-readable capacity (e.g. "64.0 GB")
    #[serde(default)]
    pub capacity_human: String,
}

/// All SMBIOS-derived system information + supplementary CPU/BIOS data from registry
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SmbiosData {
    pub bios: BiosInfo,
    pub system: SystemIdentity,
    pub memory_modules: Vec<MemoryModule>,
    /// CPU microcode revision (e.g. "0x010003E0") — from registry, not SMBIOS
    pub cpu_microcode: String,
    /// CPU base frequency in MHz from registry ~MHz — fallback when CPUID leaf 0x16 returns 0
    pub cpu_base_mhz: u32,
}

// ============================================================================
// Detection
// ============================================================================

impl SmbiosData {
    /// Read and parse SMBIOS tables from firmware, plus CPU registry data. Returns Default on failure.
    pub fn detect() -> Self {
        let mut data = match read_smbios_raw() {
            Some(raw) => parse_smbios(&raw),
            None => {
                log::warn!("Failed to read SMBIOS tables — using defaults");
                Self::default()
            }
        };

        // Supplement with CPU data from registry (microcode revision, base frequency)
        let (microcode, base_mhz) = read_cpu_registry_info();
        data.cpu_microcode = microcode;
        data.cpu_base_mhz = base_mhz;

        data
    }

    /// Generate a hash of memory module configuration.
    /// Detects DIMM swaps, XMP changes, capacity changes.
    pub fn memory_fingerprint(&self) -> String {
        let mut hasher = DefaultHasher::new();
        self.memory_modules.len().hash(&mut hasher);
        for module in &self.memory_modules {
            module.manufacturer.hash(&mut hasher);
            module.serial_number.hash(&mut hasher);
            module.part_number.hash(&mut hasher);
            module.speed_mts.hash(&mut hasher);
            module.capacity_bytes.hash(&mut hasher);
        }
        format!("{:016x}", hasher.finish())
    }

    /// Total memory capacity across all modules
    pub fn total_memory_bytes(&self) -> u64 {
        self.memory_modules.iter().map(|m| m.capacity_bytes).sum()
    }
}

static SMBIOS: OnceLock<SmbiosData> = OnceLock::new();

/// The machine's SMBIOS data, read from firmware on first use. The tables cannot change while we
/// run, and both the calibration identity check and the result file's identity need them, so they
/// are read once rather than once per caller.
pub fn get_smbios() -> &'static SmbiosData {
    SMBIOS.get_or_init(SmbiosData::detect)
}

/// Format capacity in bytes to human-readable string (e.g. "64.0 GB")
pub fn format_capacity(bytes: u64) -> String {
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.1} KB", b / KB)
    } else {
        format!("{} B", bytes)
    }
}

// ============================================================================
// Raw SMBIOS reading via GetSystemFirmwareTable
// ============================================================================

/// Read raw SMBIOS data from firmware table
fn read_smbios_raw() -> Option<Vec<u8>> {
    use windows::Win32::System::SystemInformation::{GetSystemFirmwareTable, RSMB};

    // First call: get required buffer size
    let size = unsafe { GetSystemFirmwareTable(RSMB, 0, None) };
    if size == 0 {
        log::warn!("GetSystemFirmwareTable returned 0 size");
        return None;
    }

    // Second call: read the data
    let mut buffer = vec![0u8; size as usize];
    let written = unsafe { GetSystemFirmwareTable(RSMB, 0, Some(&mut buffer)) };
    if written == 0 {
        log::warn!("GetSystemFirmwareTable failed to read data");
        return None;
    }

    buffer.truncate(written as usize);
    log::debug!("Read {} bytes of SMBIOS data", buffer.len());
    Some(buffer)
}

// ============================================================================
// CPU registry data (microcode, base frequency)
// ============================================================================

/// Read CPU microcode revision and base MHz from Windows registry.
/// Returns (microcode_string, base_mhz). Falls back to empty/0 on failure.
fn read_cpu_registry_info() -> (String, u32) {
    use windows::Win32::System::Registry::*;
    use windows::core::PCSTR;

    let mut microcode = String::new();
    let mut base_mhz: u32 = 0;

    let subkey = b"HARDWARE\\DESCRIPTION\\System\\CentralProcessor\\0\0";

    let mut hkey = HKEY::default();
    let result = unsafe {
        RegOpenKeyExA(
            HKEY_LOCAL_MACHINE,
            PCSTR::from_raw(subkey.as_ptr()),
            Some(0),
            KEY_READ,
            &mut hkey,
        )
    };

    if result.is_err() {
        log::debug!("Failed to open CPU registry key: {:?}", result);
        return (microcode, base_mhz);
    }

    // Read ~MHz (DWORD)
    let mhz_name = b"~MHz\0";
    let mut mhz_value: u32 = 0;
    let mut mhz_size = std::mem::size_of::<u32>() as u32;
    let result = unsafe {
        RegQueryValueExA(
            hkey,
            PCSTR::from_raw(mhz_name.as_ptr()),
            None,
            None,
            Some(&mut mhz_value as *mut u32 as *mut u8),
            Some(&mut mhz_size),
        )
    };
    if result.is_ok() {
        base_mhz = mhz_value;
        log::debug!("Registry CPU ~MHz: {}", base_mhz);
    }

    // Read "Update Revision" (REG_BINARY, 8 bytes — microcode revision)
    let rev_name = b"Update Revision\0";
    let mut rev_buf = [0u8; 8];
    let mut rev_size = 8u32;
    let result = unsafe {
        RegQueryValueExA(
            hkey,
            PCSTR::from_raw(rev_name.as_ptr()),
            None,
            None,
            Some(rev_buf.as_mut_ptr()),
            Some(&mut rev_size),
        )
    };
    if result.is_ok() && rev_size >= 4 {
        // Microcode revision is stored as a little-endian value.
        // The high DWORD (bytes 4-7) contains the actual revision.
        let revision = if rev_size >= 8 {
            u32::from_le_bytes([rev_buf[4], rev_buf[5], rev_buf[6], rev_buf[7]])
        } else {
            u32::from_le_bytes([rev_buf[0], rev_buf[1], rev_buf[2], rev_buf[3]])
        };
        microcode = format!("0x{:08X}", revision);
        log::debug!("Registry CPU microcode: {} (raw bytes: {:02X?})", microcode, &rev_buf[..rev_size as usize]);
    }

    let _ = unsafe { RegCloseKey(hkey) };

    (microcode, base_mhz)
}

// ============================================================================
// SMBIOS table parsing
// ============================================================================

/// Raw SMBIOS firmware table header (8 bytes before the structure table)
/// Layout: Used (1), Major (1), Minor (1), Revision (1), Length (4)
const SMBIOS_HEADER_SIZE: usize = 8;

/// Parse raw SMBIOS firmware table data
fn parse_smbios(raw: &[u8]) -> SmbiosData {
    let mut data = SmbiosData::default();
    let mut found_type0 = false;
    let mut found_type1 = false;
    let mut type16_capacity_bytes: u64 = 0;

    if raw.len() < SMBIOS_HEADER_SIZE {
        log::warn!("SMBIOS data too short ({} bytes)", raw.len());
        return data;
    }

    // Skip the firmware table header to get to the SMBIOS structure table
    let table = &raw[SMBIOS_HEADER_SIZE..];
    let mut offset = 0;

    while offset + 4 <= table.len() {
        let struct_type = table[offset];
        let struct_len = table[offset + 1] as usize;

        if struct_len < 4 {
            break; // Invalid structure
        }

        // End-of-table marker (Type 127)
        if struct_type == 127 {
            break;
        }

        // Ensure we have enough data for the formatted section
        if offset + struct_len > table.len() {
            break;
        }

        // Extract the formatted (fixed-length) part
        let formatted = &table[offset..offset + struct_len];

        // Parse the string table (follows the formatted section)
        let string_start = offset + struct_len;
        let (strings, next_offset) = parse_string_table(table, string_start);

        let end_preview = (next_offset + 8).min(table.len());
        log::debug!("SMBIOS: Type {} at offset {}, formatted_len={}, strings={:?}, next_offset={}, bytes_at_next={:02X?}",
            struct_type, offset, struct_len, strings, next_offset,
            &table[next_offset..end_preview]);

        match struct_type {
            0 if !found_type0 => {
                found_type0 = true;
                parse_type0_bios(formatted, &strings, &mut data.bios);
            }
            1 if !found_type1 => {
                found_type1 = true;
                parse_type1_system(formatted, &strings, &mut data.system);
            }
            16 => {
                // Type 16: Physical Memory Array — fallback for when Type 17 is absent
                if formatted.len() >= 0x0F {
                    // Offset 0x07: Maximum Capacity in KB (u32), 0x80000000 means use extended
                    let max_cap_kb = u32::from_le_bytes([
                        formatted[0x07], formatted[0x08], formatted[0x09], formatted[0x0A],
                    ]);
                    if max_cap_kb == 0x80000000 && formatted.len() >= 0x17 {
                        // Extended Maximum Capacity at offset 0x0F (u64, in bytes)
                        type16_capacity_bytes = u64::from_le_bytes([
                            formatted[0x0F], formatted[0x10], formatted[0x11], formatted[0x12],
                            formatted[0x13], formatted[0x14], formatted[0x15], formatted[0x16],
                        ]);
                    } else if max_cap_kb != 0x80000000 {
                        type16_capacity_bytes = (max_cap_kb as u64) * 1024;
                    }
                    // Offset 0x0D: Number of Memory Devices (u16)
                    let device_count = u16::from_le_bytes([formatted[0x0D], formatted[0x0E]]);
                    log::debug!("SMBIOS Type 16: capacity={} bytes, devices={}",
                        type16_capacity_bytes, device_count);
                }
            }
            17 => {
                if struct_len >= 0x0E {
                    let size_field = u16::from_le_bytes([formatted[0x0C], formatted[0x0D]]);
                    log::debug!("SMBIOS Type 17: len={}, size_field=0x{:04X}, bytes={:02X?}",
                        struct_len, size_field, &formatted[..struct_len]);
                }
                if let Some(module) = parse_type17_memory(formatted, &strings) {
                    data.memory_modules.push(module);
                } else {
                    log::debug!("SMBIOS Type 17: skipped (empty slot or short struct, len={})", struct_len);
                }
            }
            _ => {} // Skip other types / duplicate Type 0/1
        }

        offset = next_offset;
    }

    // Fix up Type 17 modules that have capacity=0 but Type 16 has the real capacity.
    // This happens on VMs (e.g. EC2) with buggy firmware: Type 17 says size=0x7FFF
    // (use extended) but extended size is 0. Type 16 has the correct total.
    if !data.memory_modules.is_empty() && type16_capacity_bytes > 0 {
        let modules_needing_fixup: Vec<usize> = data.memory_modules.iter()
            .enumerate()
            .filter(|(_, m)| m.capacity_bytes == 0)
            .map(|(i, _)| i)
            .collect();
        if !modules_needing_fixup.is_empty() {
            let per_module = type16_capacity_bytes / modules_needing_fixup.len() as u64;
            for idx in modules_needing_fixup {
                data.memory_modules[idx].capacity_bytes = per_module;
                data.memory_modules[idx].capacity_human = format_capacity(per_module);
            }
            log::debug!("SMBIOS: Fixed up {} module(s) with capacity from Type 16 ({})",
                data.memory_modules.len(), format_capacity(type16_capacity_bytes));
        }
    }

    // Set capacity_human for any modules that have it unset
    for module in &mut data.memory_modules {
        if module.capacity_human.is_empty() && module.capacity_bytes > 0 {
            module.capacity_human = format_capacity(module.capacity_bytes);
        }
    }

    let bios_str = if data.bios.vendor.is_empty() && data.bios.version.is_empty() {
        "(not available)".to_string()
    } else {
        format!("{} {}", data.bios.vendor, data.bios.version).trim().to_string()
    };
    let sys_str = if data.system.manufacturer.is_empty() && data.system.product_name.is_empty() {
        "(not available)".to_string()
    } else {
        format!("{} {}", data.system.manufacturer, data.system.product_name).trim().to_string()
    };
    let uuid_str = if data.system.uuid.is_empty() { "(none)" } else { &data.system.uuid };
    let mem_str = if data.memory_modules.is_empty() {
        "none detected".to_string()
    } else {
        let total_gb = data.total_memory_bytes() as f64 / (1024.0 * 1024.0 * 1024.0);
        format!("{} module(s), {:.1} GB total", data.memory_modules.len(), total_gb)
    };
    log::info!("SMBIOS: BIOS={}, System={}, UUID={}, Memory={}",
        bios_str, sys_str, uuid_str, mem_str);

    data
}

/// Parse the null-terminated string table after a structure's formatted section.
/// Returns (vector of strings, offset of next structure).
fn parse_string_table(table: &[u8], start: usize) -> (Vec<String>, usize) {
    let mut strings = Vec::new();
    let mut pos = start;

    // SMBIOS string table format:
    //   - Each string is null-terminated (0x00)
    //   - After the last string, an additional null terminates the set (double-null)
    //   - If NO strings: two consecutive null bytes (0x00, 0x00)
    //
    // The next structure begins immediately after the double-null.
    loop {
        if pos >= table.len() {
            break;
        }

        // Find the end of this string
        let str_start = pos;
        while pos < table.len() && table[pos] != 0 {
            pos += 1;
        }

        if pos == str_start {
            // Hit a null without any preceding string content — end of string table.
            pos += 1; // Skip this null
            break;
        }

        let s = String::from_utf8_lossy(&table[str_start..pos]).trim().to_string();
        strings.push(s);
        pos += 1; // Skip the null terminator after this string
    }

    // When there are NO strings, the string area is 0x00 0x00 (double-null).
    // The loop above only consumed the first null. Consume the second.
    // With strings present this is a no-op: the loop already ate "laststr\0" + "\0".
    if strings.is_empty() && pos < table.len() && table[pos] == 0 {
        pos += 1;
    }

    (strings, pos)
}

/// Get a string by 1-based index from the string table
fn get_string(strings: &[String], index: u8) -> String {
    if index == 0 {
        return String::new();
    }
    strings
        .get((index - 1) as usize)
        .cloned()
        .unwrap_or_default()
}

/// Parse Type 0: BIOS Information
fn parse_type0_bios(formatted: &[u8], strings: &[String], bios: &mut BiosInfo) {
    // Offset 0x04: Vendor (string index)
    // Offset 0x05: BIOS Version (string index)
    // Offset 0x08: BIOS Release Date (string index, typically MM/DD/YYYY)
    if formatted.len() >= 6 {
        bios.vendor = get_string(strings, formatted[0x04]);
        bios.version = get_string(strings, formatted[0x05]);
    }
    if formatted.len() >= 9 {
        let raw_date = get_string(strings, formatted[0x08]);
        bios.release_date = convert_bios_date_to_iso(&raw_date);
    }
}

/// Convert BIOS date from MM/DD/YYYY to ISO YYYY-MM-DD.
/// Falls back to the raw string if parsing fails.
fn convert_bios_date_to_iso(raw: &str) -> String {
    // SMBIOS spec says format is MM/DD/YYYY
    let parts: Vec<&str> = raw.split('/').collect();
    if parts.len() == 3 {
        let month = parts[0];
        let day = parts[1];
        let year = parts[2];
        if !month.is_empty() && !day.is_empty() && year.len() == 4 {
            return format!("{}-{:0>2}-{:0>2}", year, month, day);
        }
    }
    raw.to_string()
}

/// Parse Type 1: System Information
fn parse_type1_system(formatted: &[u8], strings: &[String], system: &mut SystemIdentity) {
    // Offset 0x04: Manufacturer (string index)
    // Offset 0x05: Product Name (string index)
    // Offset 0x08-0x17: UUID (16 bytes)
    if formatted.len() >= 8 {
        system.manufacturer = get_string(strings, formatted[0x04]);
        system.product_name = get_string(strings, formatted[0x05]);
    }

    if formatted.len() >= 0x18 {
        let uuid = &formatted[0x08..0x18];
        // UUID format per SMBIOS spec: first 3 fields are little-endian
        system.uuid = format!(
            "{:02X}{:02X}{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
            uuid[3], uuid[2], uuid[1], uuid[0],
            uuid[5], uuid[4],
            uuid[7], uuid[6],
            uuid[8], uuid[9],
            uuid[10], uuid[11], uuid[12], uuid[13], uuid[14], uuid[15],
        );
    }
}

/// Parse Type 17: Memory Device — returns None for genuinely empty slots
fn parse_type17_memory(formatted: &[u8], strings: &[String]) -> Option<MemoryModule> {
    // Need at least enough bytes for the size field
    if formatted.len() < 0x0E {
        return None;
    }

    // Offset 0x0C: Size (u16) — 0 means empty slot, 0xFFFF means unknown
    let size_field = u16::from_le_bytes([formatted[0x0C], formatted[0x0D]]);
    if size_field == 0 {
        return None; // Genuinely empty DIMM slot
    }

    // Size: bit 15 indicates KB (1) vs MB (0), 0x7FFF means use extended, 0xFFFF means unknown
    let capacity_bytes = if size_field == 0xFFFF {
        0 // Unknown size — still parse the module for speed/strings
    } else if size_field == 0x7FFF {
        // Extended size at offset 0x1C (u32, in MB) — for modules > 32GB
        // Some VMs set 0x7FFF but leave extended size as 0 (buggy firmware)
        if formatted.len() >= 0x20 {
            let ext_size_mb = u32::from_le_bytes([
                formatted[0x1C], formatted[0x1D], formatted[0x1E], formatted[0x1F],
            ]);
            (ext_size_mb as u64) * 1024 * 1024
        } else {
            0
        }
    } else if size_field & 0x8000 != 0 {
        // Size in KB
        ((size_field & 0x7FFF) as u64) * 1024
    } else {
        // Size in MB
        (size_field as u64) * 1024 * 1024
    };

    // Offset 0x15: Speed in MT/s (u16) — guard for short structs
    let speed_mts = if formatted.len() >= 0x17 {
        u16::from_le_bytes([formatted[0x15], formatted[0x16]])
    } else {
        0
    };

    // String references (all guarded for short structs)
    // 0x10: Device Locator
    // 0x17: Manufacturer
    // 0x18: Serial Number
    // 0x19: Asset Tag
    // 0x1A: Part Number
    let locator = if formatted.len() > 0x10 {
        get_string(strings, formatted[0x10])
    } else {
        String::new()
    };
    let manufacturer = if formatted.len() > 0x17 {
        get_string(strings, formatted[0x17])
    } else {
        String::new()
    };
    let serial_number = if formatted.len() > 0x18 {
        get_string(strings, formatted[0x18])
    } else {
        String::new()
    };
    let part_number = if formatted.len() > 0x1A {
        get_string(strings, formatted[0x1A])
    } else {
        String::new()
    };

    Some(MemoryModule {
        locator,
        manufacturer,
        serial_number,
        part_number,
        speed_mts: speed_mts as u32,
        capacity_human: format_capacity(capacity_bytes),
        capacity_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_smbios() {
        // Integration test: should succeed on any Windows machine
        let data = SmbiosData::detect();
        // UUID should be non-empty on real hardware or VMs
        assert!(!data.system.uuid.is_empty() || data.system.manufacturer.is_empty(),
            "Expected UUID or empty SMBIOS on this system");
        println!("BIOS: {} {}", data.bios.vendor, data.bios.version);
        println!("System: {} {} UUID={}", data.system.manufacturer, data.system.product_name, data.system.uuid);
        println!("Memory modules: {}", data.memory_modules.len());
        for m in &data.memory_modules {
            println!("  {} {} {} {} {}MT/s {:.1}GB",
                m.locator, m.manufacturer, m.serial_number, m.part_number,
                m.speed_mts, m.capacity_bytes as f64 / (1024.0 * 1024.0 * 1024.0));
        }
        println!("Memory fingerprint: {}", data.memory_fingerprint());
    }

    #[test]
    fn test_memory_fingerprint_deterministic() {
        let data = SmbiosData::detect();
        let fp1 = data.memory_fingerprint();
        let fp2 = data.memory_fingerprint();
        assert_eq!(fp1, fp2, "Fingerprint should be deterministic");
    }
}
