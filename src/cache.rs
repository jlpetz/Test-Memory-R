use raw_cpuid::CpuId;

use crate::constants::{KB, KB_F64, MB, MB_F64};
use crate::tsc::TscInfo;

#[derive(Debug, Clone)]
pub struct CacheInfo {
    pub l1_data_cache: usize,
    pub l1_instruction_cache: usize,
    pub l2_cache: usize,
    pub l3_cache: usize,
    pub cache_line_size: usize,
    pub total_cache: usize,
    pub detection_method: String,
    pub per_core_l1d: usize,
    pub per_core_l1i: usize,
    pub per_core_l2: usize,
    pub core_count: usize,
    pub tsc_frequency_ghz: f64, // TSC frequency (kept for backward compat)
    pub tsc_info: TscInfo,      // Full TSC detection info
    /// A VM, not the PC's own Windows above the hypervisor that VBS runs on a desktop (`vm_signs`, TODO 115)
    pub is_virtual_machine: bool,
    /// The hypervisor's vendor, whenever CPUID reports one: a VM's, or VBS's on a desktop
    pub hypervisor_name: Option<String>,
    /// What made it a VM (`vm_signs`); empty when it isn't
    pub vm_signs: Vec<&'static str>,
}

#[derive(Debug, Clone)]
pub struct SystemInfo {
    pub cpu_vendor: String,
    pub cpu_brand: String,
    pub cpu_family: u32,
    pub cpu_model: u32,
    pub cpu_stepping: u32,
    pub physical_cores: usize,
    pub logical_cores: usize,
    pub has_hyperthreading: bool,
    pub cache_info: CacheInfo,
}

impl Default for CacheInfo {
    fn default() -> Self {
        Self {
            l1_data_cache: 32 * KB,
            l1_instruction_cache: 32 * KB,
            l2_cache: 256 * KB,
            l3_cache: 8 * MB,
            cache_line_size: 64,
            total_cache: (32 + 32) * KB + 256 * KB + 8 * MB,
            detection_method: "Default fallback values".to_string(),
            per_core_l1d: 32 * KB,
            per_core_l1i: 32 * KB,
            per_core_l2: 256 * KB,
            core_count: 1,
            tsc_frequency_ghz: 0.0,
            tsc_info: TscInfo::default(),
            is_virtual_machine: false,
            hypervisor_name: None,
            vm_signs: Vec::new(),
        }
    }
}

impl CacheInfo {
    pub fn detect() -> Self {
        // Every processor group (num_cpus sees only one)
        let physical_cores = crate::cpu_topology::physical_core_count();

        // Detect TSC frequency using the new multi-method detection
        let tsc_info = TscInfo::detect();
        let tsc_frequency_ghz = tsc_info.frequency_ghz;

        // Detect if running in a VM
        let (vm_signs, hypervisor_name) = detect_hypervisor();
        let is_vm = !vm_signs.is_empty();

        // Try multiple detection methods in order of preference

        // 1. Try comprehensive raw_cpuid detection (best for modern CPUs)
        log::debug!("Attempting comprehensive raw_cpuid cache detection...");
        if let Some(mut cache_info) = detect_via_raw_cpuid_comprehensive(physical_cores) {
            cache_info.tsc_frequency_ghz = tsc_frequency_ghz;
            cache_info.tsc_info = tsc_info;
            cache_info.is_virtual_machine = is_vm;
            cache_info.hypervisor_name = hypervisor_name;
            cache_info.vm_signs = vm_signs;
            return cache_info;
        }

        // 2. Try Windows WMI (Windows only) - placeholder for future
        #[cfg(target_os = "windows")]
        {
            log::debug!("Attempting Windows WMI cache detection...");
            if let Some(mut cache_info) = detect_via_windows_wmi(physical_cores) {
                cache_info.tsc_frequency_ghz = tsc_frequency_ghz;
                cache_info.tsc_info = tsc_info;
                cache_info.is_virtual_machine = is_vm;
                cache_info.hypervisor_name = hypervisor_name;
                cache_info.vm_signs = vm_signs;
                return cache_info;
            }
        }

        // 3. Last resort: hardcoded detection for known CPUs
        log::debug!("Attempting hardcoded CPU detection...");
        if let Some(mut cache_info) = detect_via_hardcoded_database(physical_cores) {
            cache_info.tsc_frequency_ghz = tsc_frequency_ghz;
            cache_info.tsc_info = tsc_info;
            cache_info.is_virtual_machine = is_vm;
            cache_info.hypervisor_name = hypervisor_name;
            cache_info.vm_signs = vm_signs;
            return cache_info;
        }

        // Use defaults as last resort
        log::warn!("All cache detection methods failed, using defaults");
        let mut default = Self {
            core_count: physical_cores,
            tsc_frequency_ghz,
            tsc_info,
            is_virtual_machine: is_vm,
            hypervisor_name,
            vm_signs,
            ..Default::default()
        };
        default.recalculate_totals();
        default
    }

    fn recalculate_totals(&mut self) {
        self.l1_data_cache = self.per_core_l1d * self.core_count;
        self.l1_instruction_cache = self.per_core_l1i * self.core_count;
        self.l2_cache = self.per_core_l2 * self.core_count;
        self.total_cache = self.l1_data_cache + self.l1_instruction_cache + self.l2_cache + self.l3_cache;
    }

    pub fn print_info(&self) {
        log::info!("Cache Architecture Detected ({})", self.detection_method);
        log::info!("  Cores: {} physical", self.core_count);
        log::info!(
            "  L1 Data Cache: {:.1} KB total ({:.1} KB × {} cores)",
            self.l1_data_cache as f64 / KB_F64,
            self.per_core_l1d as f64 / KB_F64,
            self.core_count
        );
        log::info!(
            "  L1 Instruction Cache: {:.1} KB total ({:.1} KB × {} cores)",
            self.l1_instruction_cache as f64 / KB_F64,
            self.per_core_l1i as f64 / KB_F64,
            self.core_count
        );
        log::info!(
            "  L2 Cache: {:.1} KB total ({:.1} KB × {} cores)",
            self.l2_cache as f64 / KB_F64,
            self.per_core_l2 as f64 / KB_F64,
            self.core_count
        );
        log::info!("  L3 Cache: {:.1} MB (shared)", self.l3_cache as f64 / MB_F64);
        log::info!("  Total Cache: {:.1} MB", self.total_cache as f64 / MB_F64);
        log::info!("  Cache Line Size: {} bytes", self.cache_line_size);
        // TSC details
        log::info!("  TSC Frequency: {:.3} GHz ({})", self.tsc_info.frequency_ghz, self.tsc_info.detection_method);
        if self.tsc_info.is_invariant {
            log::info!("  TSC Type: Invariant (constant rate)");
        }
        if let Some(secondary) = self.tsc_info.secondary_frequency_ghz {
            log::info!("  TSC Validation: {:.3} GHz (secondary), confidence {:.0}%", secondary, self.tsc_info.confidence * 100.0);
        }
    }
}

impl SystemInfo {
    pub fn detect() -> Self {
        let cpuid = CpuId::new();

        let (cpu_vendor, cpu_brand) = if let Some(vendor_info) = cpuid.get_vendor_info() {
            let vendor = vendor_info.as_str().to_string();
            let brand = if let Some(brand_info) = cpuid.get_processor_brand_string() {
                brand_info.as_str().trim().to_string()
            } else {
                "Unknown".to_string()
            };
            (vendor, brand)
        } else {
            ("Unknown".to_string(), "Unknown".to_string())
        };

        let (cpu_family, cpu_model, cpu_stepping) = if let Some(feature_info) = cpuid.get_feature_info() {
            (feature_info.family_id().into(), feature_info.model_id().into(), feature_info.stepping_id().into())
        } else {
            (0, 0, 0)
        };

        // Every processor group (num_cpus sees only one)
        let physical_cores = crate::cpu_topology::physical_core_count();
        let logical_cores = crate::cpu_topology::logical_cpu_ids().len();
        let has_hyperthreading = logical_cores > physical_cores;

        let cache_info = CacheInfo::detect();

        Self { cpu_vendor, cpu_brand, cpu_family, cpu_model, cpu_stepping, physical_cores, logical_cores, has_hyperthreading, cache_info }
    }

    pub fn print_system_info(&self) {
        log::info!("System Information:");
        log::info!("  CPU: {} ({})", self.cpu_brand, self.cpu_vendor);
        log::info!("  Family: {}, Model: {}, Stepping: {}", self.cpu_family, self.cpu_model, self.cpu_stepping);
        log::info!(
            "  Cores: {} physical, {} logical{}",
            self.physical_cores,
            self.logical_cores,
            if self.has_hyperthreading { " (Hyperthreading enabled)" } else { "" }
        );

        self.cache_info.print_info();
    }

    pub fn get_cache_info(&self) -> &CacheInfo {
        &self.cache_info
    }
}

// Method 1: Comprehensive raw_cpuid detection with all methods
fn detect_via_raw_cpuid_comprehensive(physical_cores: usize) -> Option<CacheInfo> {
    let cpuid = CpuId::new();

    let mut cache_info = CacheInfo {
        per_core_l1d: 0,
        per_core_l1i: 0,
        per_core_l2: 0,
        l3_cache: 0,
        cache_line_size: 64,
        core_count: physical_cores,
        detection_method: String::new(),
        l1_data_cache: 0,
        l1_instruction_cache: 0,
        l2_cache: 0,
        total_cache: 0,
        tsc_frequency_ghz: 0.0,       // Will be set by caller
        tsc_info: TscInfo::default(), // Will be set by caller
        is_virtual_machine: false,    // Will be set by caller
        hypervisor_name: None,        // Will be set by caller
        vm_signs: Vec::new(),         // Will be set by caller
    };

    let mut found_any = false;

    // Method 1: Try deterministic cache parameters (CPUID leaf 4)
    if let Some(cache_params) = cpuid.get_cache_parameters() {
        log::debug!("raw_cpuid: Found cache parameters via leaf 4");
        for cache in cache_params {
            let size = cache.associativity() * cache.physical_line_partitions() * cache.coherency_line_size() * cache.sets();

            let cache_type = cache.cache_type();
            let level = cache.level();

            log::debug!("raw_cpuid leaf 4: L{} cache, type={:?}, size={}KB", level, cache_type, size / 1024);

            match (level, cache_type) {
                (1, raw_cpuid::CacheType::Data) => {
                    cache_info.per_core_l1d = size;
                    found_any = true;
                }
                (1, raw_cpuid::CacheType::Instruction) => {
                    cache_info.per_core_l1i = size;
                    found_any = true;
                }
                (2, raw_cpuid::CacheType::Unified) => {
                    cache_info.per_core_l2 = size;
                    found_any = true;
                }
                (3, raw_cpuid::CacheType::Unified) => {
                    cache_info.l3_cache = size;
                    found_any = true;
                }
                _ => {}
            }

            if cache.coherency_line_size() > 0 {
                cache_info.cache_line_size = cache.coherency_line_size();
            }
        }

        if found_any {
            cache_info.detection_method = "raw_cpuid leaf 4".to_string();
        }
    }

    // Method 2: Try extended topology (CPUID leaf 0x8000001D) - AMD preferred method
    if cpuid.get_vendor_info().map(|v| v.as_str() == "AuthenticAMD").unwrap_or(false) {
        log::debug!("raw_cpuid: Detected AMD CPU, trying extended topology");

        // Try AMD's extended cache topology
        // Note: __cpuid_count is safe in edition 2024
        for subleaf in 0..16 {
            let result = std::arch::x86_64::__cpuid_count(0x8000001D, subleaf);

            if result.eax == 0 {
                break; // No more cache levels
            }

            let cache_type = result.eax & 0x1F;
            let cache_level = (result.eax >> 5) & 0x7;
            let _self_init = (result.eax >> 8) & 0x1;
            let _fully_assoc = (result.eax >> 9) & 0x1;
            let num_sharing = ((result.eax >> 14) & 0xFFF) + 1;

            let line_size = (result.ebx & 0xFFF) + 1;
            let partitions = ((result.ebx >> 12) & 0x3FF) + 1;
            let ways = ((result.ebx >> 22) & 0x3FF) + 1;
            let sets = result.ecx + 1;

            let cache_size = (ways * partitions * line_size * sets) as usize;

            log::debug!(
                "raw_cpuid AMD 0x8000001D[{}]: L{} type={} size={}KB, sharing={}",
                subleaf,
                cache_level,
                cache_type,
                cache_size / KB,
                num_sharing
            );

            // Cache type: 1=Data, 2=Instruction, 3=Unified
            match (cache_level, cache_type) {
                (1, 1) => {
                    // L1 Data
                    cache_info.per_core_l1d = cache_size;
                    found_any = true;
                }
                (1, 2) => {
                    // L1 Instruction
                    cache_info.per_core_l1i = cache_size;
                    found_any = true;
                }
                (2, 3) => {
                    // L2 Unified
                    cache_info.per_core_l2 = cache_size;
                    found_any = true;
                }
                (3, 3) => {
                    // L3 Unified
                    cache_info.l3_cache = cache_size;
                    found_any = true;
                }
                _ => {}
            }

            if line_size > 0 && line_size <= 256 {
                cache_info.cache_line_size = line_size as usize;
            }
        }

        if found_any {
            cache_info.detection_method = "raw_cpuid AMD extended topology".to_string();
        }
    }

    // Method 3: Legacy AMD extended L2/L3 info (0x80000006)
    if cache_info.l3_cache == 0 {
        // Note: __cpuid is safe in edition 2024
        let result = std::arch::x86_64::__cpuid(0x80000006);

        // ECX contains L3 cache info
        let _l3_line_size = result.ecx & 0xFF;
        let _l3_assoc = (result.ecx >> 12) & 0xF;
        let l3_size_kb = (result.ecx >> 18) & 0x3FFF;

        if l3_size_kb > 0 {
            cache_info.l3_cache = (l3_size_kb as usize) * 512 * KB; // Units of 512KB
            found_any = true;
            log::debug!("raw_cpuid 0x80000006: L3 cache {}MB", cache_info.l3_cache / MB);

            if cache_info.detection_method.is_empty() {
                cache_info.detection_method = "raw_cpuid legacy AMD".to_string();
            }
        }

        // EDX contains L2 cache info if not already found
        if cache_info.per_core_l2 == 0 {
            let l2_size_kb = (result.edx >> 16) & 0xFFFF;
            if l2_size_kb > 0 {
                cache_info.per_core_l2 = (l2_size_kb as usize) * 1024;
                found_any = true;
            }
        }
    }

    if found_any {
        // Fill in reasonable defaults for missing values
        if cache_info.per_core_l1d == 0 {
            cache_info.per_core_l1d = 32 * KB;
        }
        if cache_info.per_core_l1i == 0 {
            cache_info.per_core_l1i = 32 * KB;
        }
        if cache_info.per_core_l2 == 0 {
            cache_info.per_core_l2 = 512 * KB;
        }

        cache_info.recalculate_totals();
        Some(cache_info)
    } else {
        None
    }
}

// Method 2: Windows WMI detection
#[cfg(target_os = "windows")]
fn detect_via_windows_wmi(_physical_cores: usize) -> Option<CacheInfo> {
    // For now, we'll use a simple Win32 API approach
    // Full WMI would require additional dependencies (wmi crate)

    // This is a placeholder - implementing full WMI requires the wmi crate
    log::debug!("WMI detection not implemented yet");
    None
}

#[cfg(not(target_os = "windows"))]
fn detect_via_windows_wmi(_physical_cores: usize) -> Option<CacheInfo> {
    None
}

// Method 3: Hardcoded database for known CPUs
fn detect_via_hardcoded_database(physical_cores: usize) -> Option<CacheInfo> {
    let cpuid = CpuId::new();

    let brand = cpuid.get_processor_brand_string().map(|b| b.as_str().to_lowercase()).unwrap_or_default();

    let (family, model) = cpuid.get_feature_info().map(|f| (f.family_id(), f.model_id())).unwrap_or((0, 0));

    // AMD CPU database
    let (per_core_l1d, per_core_l1i, per_core_l2, l3_cache) = if brand.contains("amd") {
        match (family, model) {
            // Zen 4 APUs (Phoenix)
            (25, 120) | (25, 124) => (32 * KB, 32 * KB, MB, 16 * MB),
            // Zen 4 Desktop (Raphael)
            (25, 97) | (25, 98) => (32 * KB, 32 * KB, MB, 32 * MB),
            // Zen 3
            (25, 33) | (25, 1) => (32 * KB, 32 * KB, 512 * KB, 32 * MB),
            // Zen 2
            (23, 113) | (23, 104) => (32 * KB, 32 * KB, 512 * KB, 16 * MB),
            _ => {
                // Try to parse from brand string
                if brand.contains("8500g") || brand.contains("8600g") || brand.contains("8700g") {
                    (32 * KB, 32 * KB, MB, 16 * MB)
                } else if brand.contains("7000") || brand.contains("9000") {
                    (32 * KB, 32 * KB, MB, 32 * MB)
                } else {
                    return None;
                }
            }
        }
    } else if brand.contains("intel") {
        // Intel database would go here
        return None;
    } else {
        return None;
    };

    let mut cache_info = CacheInfo {
        per_core_l1d,
        per_core_l1i,
        per_core_l2,
        l3_cache,
        cache_line_size: 64,
        core_count: physical_cores,
        detection_method: format!("Hardcoded database (Family {}, Model {})", family, model),
        l1_data_cache: 0,
        l1_instruction_cache: 0,
        l2_cache: 0,
        total_cache: 0,
        tsc_frequency_ghz: 0.0,       // Will be set by caller
        tsc_info: TscInfo::default(), // Will be set by caller
        is_virtual_machine: false,    // Will be set by caller
        hypervisor_name: None,        // Will be set by caller
        vm_signs: Vec::new(),         // Will be set by caller
    };

    cache_info.recalculate_totals();
    log::info!("Using hardcoded cache values for {} (Family {}, Model {})", brand, family, model);
    Some(cache_info)
}

/// The hypervisor CPUID reports, if any, and what says TMR runs in a VM above it (`vm_signs`, TODO 115): none for the PC's own Windows,
/// which every desktop with VBS runs above Hyper-V's hypervisor, so a hypervisor alone doesn't make a VM.
fn detect_hypervisor() -> (Vec<&'static str>, Option<String>) {
    let cpuid = CpuId::new();

    // CPUID.01H:ECX.bit31, the hypervisor-present bit
    if let Some(feature_info) = cpuid.get_feature_info()
        && feature_info.has_hypervisor()
    {
        let hypervisor_name = cpuid.get_hypervisor_info().map(|info| format!("{:?}", info.identify()));
        let leaves = hyperv_leaves();
        let waet = acpi_has_waet();
        let signs = vm_signs(leaves, waet);
        let name = hypervisor_name.as_deref().unwrap_or("Unknown");
        match leaves {
            Some(HyperVLeaves { privileges, recommendations }) => log::info!(
                "Hypervisor: {name}, privileges {privileges:#010x} (0x40000003 EBX), recommendations {recommendations:#010x} \
                 (0x40000004 EAX), WAET {waet}: {}",
                if signs.is_empty() { "its root partition, not a VM".to_string() } else { format!("a VM ({})", signs.join(", ")) }
            ),
            None => log::info!("Hypervisor: {name}, WAET {waet}: a VM ({})", signs.join(", ")),
        }
        return (signs, hypervisor_name);
    }

    log::debug!("No hypervisor detected (running on physical hardware)");
    (Vec::new(), None)
}

/// Hyper-V's CPUID leaves that tell its root partition from a guest (TODO 115): 0x40000003 EBX, the partition's privileges, and
/// 0x40000004 EAX, the hypervisor's recommendations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HyperVLeaves {
    pub privileges: u32,
    pub recommendations: u32,
}

/// Hyper-V's leaves, when the hypervisor is Microsoft's and has them (its top leaf is 0x40000004 or more); else none
fn hyperv_leaves() -> Option<HyperVLeaves> {
    use std::arch::x86_64::__cpuid;
    let top = __cpuid(0x4000_0000);
    let vendor: Vec<u8> = [top.ebx, top.ecx, top.edx].iter().flat_map(|word| word.to_le_bytes()).collect();
    (vendor == b"Microsoft Hv" && top.eax >= 0x4000_0004)
        .then(|| HyperVLeaves { privileges: __cpuid(0x4000_0003).ebx, recommendations: __cpuid(0x4000_0004).eax })
}

/// Whether the firmware lists ACPI's WAET (the Windows ACPI Emulated devices Table), which a VM's firmware has and a PC's doesn't
fn acpi_has_waet() -> bool {
    use windows::Win32::System::SystemInformation::{EnumSystemFirmwareTables, FIRMWARE_TABLE_PROVIDER};
    let acpi = FIRMWARE_TABLE_PROVIDER(u32::from_be_bytes(*b"ACPI"));
    // SAFETY: the size first, then a buffer that long, which Windows fills with the tables' 4-byte signatures
    let size = unsafe { EnumSystemFirmwareTables(acpi, None) } as usize;
    let mut ids = vec![0u8; size];
    // SAFETY: as above
    let got = unsafe { EnumSystemFirmwareTables(acpi, Some(&mut ids)) } as usize;
    ids.truncate(got.min(size));
    ids.as_chunks::<4>().0.iter().any(|id| id == b"WAET")
}

/// What says TMR runs in a VM (TODO 115), none for Hyper-V's root partition: the PC's own Windows, above the hypervisor VBS (or Hyper-V)
/// runs on a desktop. A VM is any hypervisor but Hyper-V's (or a Hyper-V without the leaves); under Hyper-V's, a partition without both
/// of the root's privileges (CreatePartitions, 0x40000003 EBX bit 0, and CpuManagement, bit 12: Linux's root test), or a root whose
/// hypervisor is itself in a VM (0x40000004 EAX bit 12: a VM host nested in a VM); and firmware with ACPI's WAET table, which only a
/// VM's has, whatever the hypervisor says. 2026-11-04: the dev box's EC2 VM, a guest with WAET; a laptop with VBS, both privileges, not
/// nested, no WAET.
pub(crate) fn vm_signs(hyperv: Option<HyperVLeaves>, waet: bool) -> Vec<&'static str> {
    const ROOT_PRIVILEGES: u32 = 1 | 1 << 12;
    const NESTED: u32 = 1 << 12;
    let mut signs = Vec::new();
    match hyperv {
        None => signs.push("not Hyper-V's"),
        Some(leaves) => {
            if leaves.privileges & ROOT_PRIVILEGES != ROOT_PRIVILEGES {
                signs.push("a guest partition");
            }
            if leaves.recommendations & NESTED != 0 {
                signs.push("its hypervisor nested in a VM");
            }
        }
    }
    if waet {
        signs.push("the firmware's WAET table");
    }
    signs
}

#[cfg(test)]
mod tests {
    use super::*;

    /// TODO 115: the probe's readings. The dev box's EC2 VM is a guest with WAET; a laptop with VBS is Hyper-V's root, not a VM; a VM host
    /// nested in a VM, another hypervisor, or WAET alone each make one.
    #[test]
    fn a_vm_is_a_guest_a_nested_root_or_another_hypervisor() {
        let ec2 = HyperVLeaves { privileges: 0x0033_0000, recommendations: 0x0006_0038 };
        assert_eq!(vm_signs(Some(ec2), true), ["a guest partition", "the firmware's WAET table"]);
        let laptop = HyperVLeaves { privileges: 0x002b_b9ff, recommendations: 0x0094_0d1c };
        assert!(vm_signs(Some(laptop), false).is_empty(), "VBS on a PC is no VM");
        let nested = HyperVLeaves { recommendations: laptop.recommendations | 1 << 12, ..laptop };
        assert_eq!(vm_signs(Some(nested), false), ["its hypervisor nested in a VM"]);
        assert_eq!(vm_signs(None, false), ["not Hyper-V's"]);
        assert_eq!(vm_signs(Some(laptop), true), ["the firmware's WAET table"]);
        let one_privilege = HyperVLeaves { privileges: 1, ..laptop };
        assert_eq!(vm_signs(Some(one_privilege), false), ["a guest partition"]);
    }
}
