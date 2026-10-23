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

/// A memory module, as the DIMM selector shows it: its name, the locator, or `DIMM 2` where SMBIOS left that empty, and the rest SMBIOS
/// gives, `32 GiB · 6000 MT/s · G.Skill F5-6000J3040G32G`, a field it left empty (a VM's often are) left out.
#[derive(Clone, Debug, PartialEq)]
pub struct Dimm {
    pub name: String,
    pub details: String,
}

impl Dimm {
    /// Its item in the selector's menu, which names it when it is chosen (`SystemPanel::choose`)
    pub fn choice(&self) -> String {
        if self.details.is_empty() { self.name.clone() } else { format!("{} · {}", self.name, self.details) }
    }
}

/// The system panel's memory line: the memory's total, the DIMMs' count, an item of its own, and the total's tooltip.
#[derive(Clone, Debug, PartialEq)]
pub struct MemoryLine {
    /// `64 GiB DDR5`, the modules' total and their type when they share one; with none listed, what Windows reports, if it has yet
    pub total: Option<String>,
    /// `2 DIMMs`
    pub dimms: String,
    pub about: String,
}

/// The system panel's lines: the CPU, the caches, the memory, and the DIMM chosen in its selector.
#[derive(Clone, Debug, PartialEq)]
pub struct SystemPanel {
    pub cpu: String,
    /// The CPU line's tooltip: its cores and logical CPUs
    pub cpu_detail: String,
    pub cache_line: String,
    /// The cache line's tooltip: each level and where the sizes came from
    pub cache_detail: String,
    /// The modules SMBIOS lists, in its order
    pub dimms: Vec<Dimm>,
    /// The modules' total and type, `64 GiB DDR5`, when SMBIOS gives their sizes
    installed: Option<String>,
    /// The DIMM the selector shows: the first, until another is chosen
    chosen: usize,
}

impl SystemPanel {
    pub fn of(machine: &Machine) -> Self {
        let modules = &machine.memory_modules;
        let total: u64 = modules.iter().map(|m| m.capacity_bytes).sum();
        let types: Vec<&str> = modules.iter().filter_map(|m| memory_type(m.memory_type)).collect();
        let kind = match types.first() {
            Some(kind) if types.iter().all(|t| t == kind) => format!(" {kind}"),
            _ => String::new(),
        };
        SystemPanel {
            cpu: machine.cpu.clone(),
            cpu_detail: format!("{}\n{} cores, {} logical CPUs", machine.cpu, machine.physical_cores, machine.logical_cpus),
            cache_line: cache_line(machine),
            cache_detail: cache_detail(machine),
            dimms: modules.iter().enumerate().map(|(i, module)| dimm(i, module)).collect(),
            installed: (total > 0).then(|| format!("{}{kind}", size(total as usize))),
            chosen: 0,
        }
    }

    /// The memory line, `64 GiB DDR5 · 2 DIMMs`. `ram_bytes` is what Windows reports, a little under what is installed, which the
    /// total's tooltip gives, and the total is when SMBIOS gives no sizes.
    pub fn memory_line(&self, ram_bytes: Option<u64>) -> MemoryLine {
        let windows = ram_bytes.map(|bytes| size(bytes as usize));
        let dimms = match self.dimms.len() {
            0 => "SMBIOS lists no DIMMs".to_string(),
            n => format!("{n} DIMM{}", if n == 1 { "" } else { "s" }),
        };
        let about = match (&self.installed, &windows) {
            (Some(_), Some(windows)) => {
                format!("Installed, as SMBIOS lists the modules. Windows reports {windows} (GlobalMemoryStatusEx), a little under it.")
            }
            (Some(_), None) => "Installed, as SMBIOS lists the modules".to_string(),
            (None, _) => "The physical memory Windows reports (GlobalMemoryStatusEx), a little under what is installed".to_string(),
        };
        MemoryLine { total: self.installed.clone().or(windows), dimms, about }
    }

    /// The DIMM the selector shows; `None` when SMBIOS lists none
    pub fn chosen(&self) -> Option<&Dimm> {
        self.dimms.get(self.chosen)
    }

    /// The selector's menu offers a choice: there are several
    pub fn choosable(&self) -> bool {
        self.dimms.len() > 1
    }

    /// The DIMM whose menu item is `choice` (`Dimm::choice`) is the one shown; a choice that names none changes nothing
    pub fn choose(&mut self, choice: &str) {
        if let Some(i) = self.dimms.iter().position(|dimm| dimm.choice() == choice) {
            self.chosen = i;
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

/// The `index`th module SMBIOS lists, from 0
fn dimm(index: usize, m: &MemoryModule) -> Dimm {
    let part = format!("{} {}", m.manufacturer.trim(), m.part_number.trim());
    // The size and the speed first: they are what a narrow window keeps when it cuts the line short
    let details: Vec<String> = [
        (m.capacity_bytes > 0).then(|| size(m.capacity_bytes as usize)),
        (m.speed_mts > 0).then(|| format!("{} MT/s", m.speed_mts)),
        Some(part.trim().to_string()).filter(|part| !part.is_empty()),
    ]
    .into_iter()
    .flatten()
    .collect();
    let locator = m.locator.trim();
    Dimm { name: if locator.is_empty() { format!("DIMM {}", index + 1) } else { locator.to_string() }, details: details.join(" · ") }
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

    /// What Windows reports of a desktop's 64 GiB
    const WINDOWS: Option<u64> = Some(67_000_000_000);

    fn shown(name: &str, details: &str) -> Dimm {
        Dimm { name: name.to_string(), details: details.to_string() }
    }

    /// A desktop's: the caches in one line with every level in its tooltip; the memory's total and its DIMMs, the first shown in the
    /// selector until another is chosen
    #[test]
    fn a_desktop_shows_its_caches_and_its_dimms_the_first_chosen() {
        let fast = MemoryModule {
            manufacturer: "Kingston".to_string(),
            part_number: "KF564C32-32".to_string(),
            speed_mts: 6400,
            ..module("DIMM_B2", 32)
        };
        let mut panel = SystemPanel::of(&machine(vec![module("DIMM_A2", 32), fast]));
        assert_eq!(panel.cache_line, "L1 32 KiB · L2 1 MiB per core · L3 16 MiB");
        assert_eq!(
            panel.cache_detail,
            "L1 data: 32 KiB per core\nL1 instruction: 32 KiB per core\nL2: 1 MiB per core\nL3: 16 MiB, shared\n64-byte lines\nfrom raw_cpuid leaf 4"
        );
        assert_eq!(panel.cpu_detail, "AMD Ryzen 5 8600G w/ Radeon 760M Graphics\n6 cores, 12 logical CPUs");
        assert_eq!(panel.memory_line(WINDOWS), MemoryLine {
            total: Some("64 GiB DDR5".to_string()),
            dimms: "2 DIMMs".to_string(),
            about: "Installed, as SMBIOS lists the modules. Windows reports 62.4 GiB (GlobalMemoryStatusEx), a little under it."
                .to_string(),
        });
        assert_eq!(panel.memory_line(None).about, "Installed, as SMBIOS lists the modules", "before Windows' figure");

        let (a2, b2) = (
            shown("DIMM_A2", "32 GiB · 6000 MT/s · G.Skill F5-6000J3040G32G"),
            shown("DIMM_B2", "32 GiB · 6400 MT/s · Kingston KF564C32-32"),
        );
        assert_eq!((panel.chosen(), panel.choosable()), (Some(&a2), true), "the first, before any choice");
        let choices: Vec<String> = panel.dimms.iter().map(Dimm::choice).collect();
        assert_eq!(choices, [
            "DIMM_A2 · 32 GiB · 6000 MT/s · G.Skill F5-6000J3040G32G",
            "DIMM_B2 · 32 GiB · 6400 MT/s · Kingston KF564C32-32"
        ]);
        panel.choose(&choices[1]);
        assert_eq!(panel.chosen(), Some(&b2));
        panel.choose("DIMM_C2");
        assert_eq!(panel.chosen(), Some(&b2), "a choice that names no DIMM changes nothing");
        panel.choose(&choices[0]);
        assert_eq!(panel.chosen(), Some(&a2));
    }

    /// One DIMM: shown, with nothing to choose
    #[test]
    fn one_dimm_is_shown_with_nothing_to_choose() {
        let panel = SystemPanel::of(&machine(vec![module("DIMM_A2", 32)]));
        let memory = panel.memory_line(WINDOWS);
        assert_eq!((memory.total.as_deref(), memory.dimms.as_str()), (Some("32 GiB DDR5"), "1 DIMM"));
        let a2 = shown("DIMM_A2", "32 GiB · 6000 MT/s · G.Skill F5-6000J3040G32G");
        assert_eq!((panel.chosen(), panel.choosable()), (Some(&a2), false));
    }

    /// A VM's: modules SMBIOS says little about, named by their place, or none at all, when the total is Windows'
    #[test]
    fn a_vm_shows_what_smbios_gives() {
        let blank = MemoryModule { capacity_bytes: 32 << 30, ..Default::default() };
        let panel = SystemPanel::of(&machine(vec![blank.clone(), blank]));
        assert_eq!(panel.memory_line(WINDOWS).total.as_deref(), Some("64 GiB"), "no type");
        assert_eq!(panel.dimms, [shown("DIMM 1", "32 GiB"), shown("DIMM 2", "32 GiB")]);
        assert_eq!(panel.dimms.iter().map(Dimm::choice).collect::<Vec<_>>(), ["DIMM 1 · 32 GiB", "DIMM 2 · 32 GiB"]);
        assert_eq!(shown("DIMM 1", "").choice(), "DIMM 1", "nothing more to say");

        let none = SystemPanel::of(&machine(Vec::new()));
        assert_eq!(none.memory_line(WINDOWS), MemoryLine {
            total: Some("62.4 GiB".to_string()),
            dimms: "SMBIOS lists no DIMMs".to_string(),
            about: "The physical memory Windows reports (GlobalMemoryStatusEx), a little under what is installed".to_string(),
        });
        assert_eq!(none.memory_line(None).total, None, "before Windows' figure");
        assert_eq!((none.chosen(), none.choosable()), (None, false));

        let no_caches = Machine { l1_data: 0, l1_instruction: 0, l2: 0, l3: 0, ..machine(Vec::new()) };
        assert_eq!(cache_line(&no_caches), "caches not found");
        assert_eq!([49152, 2 << 20, 67_000_000_000].map(size), ["48 KiB", "2 MiB", "62.4 GiB"]);
    }
}
