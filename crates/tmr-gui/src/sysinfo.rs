//! What the header's system panel shows (TODO 7): the engine's own detection of the machine, as the session's setup reports it
//! (`Setup::SystemInfo`, TODO 29): the CPU from CPUID, the caches as the run uses them, and the memory modules SMBIOS lists. Plain Rust:
//! the window lays it out.

use tmr::frontend::{MemoryModule, SystemInfoReport};

/// The machine under test, as the system panel takes it.
pub struct Machine {
    pub cpu: String,
    pub physical_cores: usize,
    pub logical_cpus: usize,
    /// Per core: L1 data, L1 instruction and L2; the L3, one instance's (shared by the cores of a CCD, or of the whole CPU); and the cache
    /// line. What `detection` found, or its fallbacks.
    pub l1_data: usize,
    pub l1_instruction: usize,
    pub l2: usize,
    pub l3: usize,
    pub cache_line: usize,
    pub detection: String,
    pub memory_modules: Vec<MemoryModule>,
}

impl Machine {
    /// From setup's report of the system, and its memory modules
    pub fn of(report: &SystemInfoReport, memory_modules: Vec<MemoryModule>) -> Self {
        let (cpu, cache) = (&report.cpu_info, &report.cache_info);
        Machine {
            cpu: cpu.brand.clone(),
            physical_cores: cpu.physical_cores,
            logical_cpus: cpu.logical_cores,
            l1_data: cache.per_core_l1d as usize,
            l1_instruction: cache.per_core_l1i as usize,
            l2: cache.per_core_l2 as usize,
            l3: cache.l3_total as usize,
            cache_line: cache.cache_line_size as usize,
            detection: cache.detection_method.clone(),
            memory_modules,
        }
    }
}

/// A memory module's line in the DIMM list: its locator, its maker and part number, its capacity and its speed. A field SMBIOS left empty
/// (a VM's often are) shows as `–`.
#[derive(Clone, Debug, PartialEq)]
pub struct Dimm {
    pub locator: String,
    pub part: String,
    pub capacity: String,
    pub speed: String,
}

/// The system panel's lines.
#[derive(Clone, Debug, PartialEq)]
pub struct SystemPanel {
    pub cpu: String,
    /// The CPU line's tooltip: its cores and logical CPUs
    pub cpu_detail: String,
    pub cache_line: String,
    /// The cache line's tooltip: each level and where the sizes came from
    pub cache_detail: String,
    pub dimms: Vec<Dimm>,
    /// The DIMM list's title: `2 × DDR5 · 64 GiB total`
    pub dimm_title: String,
}

impl SystemPanel {
    pub fn of(machine: &Machine) -> Self {
        let modules = &machine.memory_modules;
        let total: u64 = modules.iter().map(|m| m.capacity_bytes).sum();
        let types: Vec<&str> = modules.iter().filter_map(|m| memory_type(m.memory_type)).collect();
        let kind = match types.first() {
            Some(kind) if types.iter().all(|t| t == kind) => format!(" × {kind}"),
            _ => String::new(),
        };
        SystemPanel {
            cpu: machine.cpu.clone(),
            cpu_detail: format!("{}\n{} cores, {} logical CPUs", machine.cpu, machine.physical_cores, machine.logical_cpus),
            cache_line: cache_line(machine),
            cache_detail: cache_detail(machine),
            dimms: modules.iter().map(dimm).collect(),
            dimm_title: if modules.is_empty() {
                "SMBIOS lists no memory modules".to_string()
            } else {
                format!("{}{kind} · {} total", modules.len(), size(total as usize))
            },
        }
    }
}

/// `L1 48 KiB · L2 1 MiB per core · L3 32 MiB`; a level not found is left out
fn cache_line(m: &Machine) -> String {
    let per_core: Vec<String> = [("L1", m.l1_data), ("L2", m.l2)]
        .into_iter()
        .filter(|&(_, bytes)| bytes > 0)
        .map(|(level, bytes)| format!("{level} {}", size(bytes)))
        .collect();
    let mut line = per_core.join(" · ");
    if !line.is_empty() {
        line.push_str(" per core");
    }
    if m.l3 > 0 {
        if !line.is_empty() {
            line.push_str(" · ");
        }
        line += &format!("L3 {}", size(m.l3));
    }
    if line.is_empty() { "caches not found".to_string() } else { line }
}

/// Every level, the line size and where they came from, a line each
fn cache_detail(m: &Machine) -> String {
    let mut lines: Vec<String> = [("L1 data", m.l1_data), ("L1 instruction", m.l1_instruction), ("L2", m.l2)]
        .into_iter()
        .filter(|&(_, bytes)| bytes > 0)
        .map(|(level, bytes)| format!("{level}: {} per core", size(bytes)))
        .collect();
    if m.l3 > 0 {
        lines.push(format!("L3: {}, shared", size(m.l3)));
    }
    if m.cache_line > 0 {
        lines.push(format!("{}-byte lines", m.cache_line));
    }
    lines.push(format!("from {}", m.detection));
    lines.join("\n")
}

fn dimm(m: &MemoryModule) -> Dimm {
    let or_dash = |text: &str| if text.trim().is_empty() { "–".to_string() } else { text.trim().to_string() };
    let part = format!("{} {}", m.manufacturer.trim(), m.part_number.trim());
    Dimm {
        locator: or_dash(&m.locator),
        part: or_dash(&part),
        capacity: if m.capacity_bytes > 0 { size(m.capacity_bytes as usize) } else { "–".to_string() },
        speed: if m.speed_mts > 0 { m.speed_mts.to_string() } else { "–".to_string() },
    }
}

/// The SMBIOS memory type's name (Type 17 offset 0x12), for the ones a DDR5 tester meets
fn memory_type(code: u8) -> Option<&'static str> {
    match code {
        0x1A => Some("DDR4"),
        0x1E => Some("LPDDR4"),
        0x22 => Some("DDR5"),
        0x23 => Some("LPDDR5"),
        _ => None,
    }
}

/// 49152 -> `48 KiB`, 2097152 -> `2 MiB`, 67 GB -> `62.4 GiB`
pub fn size(bytes: usize) -> String {
    const KIB: usize = 1 << 10;
    const MIB: usize = 1 << 20;
    const GIB: usize = 1 << 30;
    let (unit, name) = if bytes >= GIB {
        (GIB, "GiB")
    } else if bytes >= MIB {
        (MIB, "MiB")
    } else {
        (KIB, "KiB")
    };
    if bytes.is_multiple_of(unit) { format!("{} {name}", bytes / unit) } else { format!("{:.1} {name}", bytes as f64 / unit as f64) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn machine(modules: Vec<MemoryModule>) -> Machine {
        Machine {
            cpu: "AMD Ryzen 5 8600G w/ Radeon 760M Graphics".to_string(),
            physical_cores: 6,
            logical_cpus: 12,
            l1_data: 32 << 10,
            l1_instruction: 32 << 10,
            l2: 1 << 20,
            l3: 16 << 20,
            cache_line: 64,
            detection: "raw_cpuid leaf 4".to_string(),
            memory_modules: modules,
        }
    }

    fn module(locator: &str, gib: u64) -> MemoryModule {
        MemoryModule {
            locator: locator.to_string(),
            manufacturer: "G.Skill".to_string(),
            part_number: "F5-6000J3040G32G ".to_string(),
            speed_mts: 6000,
            memory_type: 0x22,
            capacity_bytes: gib << 30,
            ..Default::default()
        }
    }

    /// A desktop's: the caches in one line with every level in its tooltip, and each DIMM
    #[test]
    fn a_desktop_shows_its_caches_and_dimms() {
        let panel = SystemPanel::of(&machine(vec![module("DIMM_A2", 32), module("DIMM_B2", 32)]));
        assert_eq!(panel.cache_line, "L1 32 KiB · L2 1 MiB per core · L3 16 MiB");
        assert_eq!(
            panel.cache_detail,
            "L1 data: 32 KiB per core\nL1 instruction: 32 KiB per core\nL2: 1 MiB per core\nL3: 16 MiB, shared\n64-byte lines\nfrom raw_cpuid leaf 4"
        );
        assert_eq!(panel.cpu_detail, "AMD Ryzen 5 8600G w/ Radeon 760M Graphics\n6 cores, 12 logical CPUs");
        assert_eq!(panel.dimm_title, "2 × DDR5 · 64 GiB total");
        let dimm = Dimm {
            locator: "DIMM_A2".to_string(),
            part: "G.Skill F5-6000J3040G32G".to_string(),
            capacity: "32 GiB".to_string(),
            speed: "6000".to_string(),
        };
        assert_eq!(panel.dimms[0], dimm);
    }

    /// A VM's: a module SMBIOS says little about, or none at all
    #[test]
    fn a_vm_shows_what_smbios_gives() {
        let blank = MemoryModule { capacity_bytes: 64 << 30, ..Default::default() };
        let panel = SystemPanel::of(&machine(vec![blank]));
        assert_eq!(panel.dimm_title, "1 · 64 GiB total");
        assert_eq!(panel.dimms[0], Dimm {
            locator: "–".to_string(),
            part: "–".to_string(),
            capacity: "64 GiB".to_string(),
            speed: "–".to_string()
        });
        assert_eq!(SystemPanel::of(&machine(Vec::new())).dimm_title, "SMBIOS lists no memory modules");
        let none = Machine { l1_data: 0, l1_instruction: 0, l2: 0, l3: 0, ..machine(Vec::new()) };
        assert_eq!(cache_line(&none), "caches not found");
        assert_eq!([49152, 2 << 20, 67_000_000_000].map(size), ["48 KiB", "2 MiB", "62.4 GiB"]);
    }
}
