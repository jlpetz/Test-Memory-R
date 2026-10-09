// Add this to your runner.rs file to centralize topology detection
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::OnceLock;
use raw_cpuid::CpuId;

use windows::Win32::System::SystemInformation::{
    GetLogicalProcessorInformationEx,
    RelationNumaNode, RelationNumaNodeEx,
    GROUP_AFFINITY,
    SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
};

// Global cached topology - computed only once
static CPU_TOPOLOGY: OnceLock<Vec<CpuTopologyInfo>> = OnceLock::new();

#[derive(Debug, Clone)]
pub struct CpuTopologyInfo {
    pub logical_id: usize,
    pub physical_core_id: usize,
    pub numa_node: u32,
    pub is_hyperthreaded: bool,
    pub core_type: CoreType,              // New field
    pub threads_on_this_core: usize,      // New field
    pub efficiency_class: u8,             // New field (raw Windows value)
}
// Clean enum design for core types
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CoreType {
    Performance(u8),  // Performance tier (0 = highest)
    Efficiency(u8),   // Efficiency tier (0 = most efficient)
    Unknown,
}

impl CoreType {
    pub fn display_name(&self) -> String {
        match self {
            CoreType::Performance(0) => "P-core".to_string(),
            CoreType::Performance(tier) => format!("P-core{}", tier),
            CoreType::Efficiency(0) => "E-core".to_string(),
            CoreType::Efficiency(tier) => format!("E-core{}", tier),
            CoreType::Unknown => "Unknown".to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct EnhancedCpuInfo {
    pub logical_processor_index: u32,
    pub scheduling_class: u8,
}


// Modern NUMA information structure
#[derive(Debug, Clone)]
pub struct NumaTopology {
    pub cpu_to_node: HashMap<u32, u32>,
}

/// `topology=`: which core-type detector types the cores. Each reads one signal and stops at "no
/// hit" (every core the same type); Auto runs them as a cascade (`detect_cpu_topology_auto`). All
/// type the same core list (`detect_cpu_topology`), so picking one alone shows what its signal says
/// on this CPU, for debugging it outside Auto.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TopologyDetectionMethod {
    /// `windows`: Windows' efficiency class (Intel's P-, E- and LP E-cores).
    WindowsApi,
    /// `windowsv2`: Windows' core priority, the scheduling class. A hybrid that reports nothing else
    /// (a Ryzen 8500G); on a uniform CPU it is only the preferred cores.
    WindowsApiV2,
    /// `cpuid`: each core's own L3 size. Not in Auto: a 7950X3D's CCD without the 3D V-Cache would
    /// read as E-cores, though both are full cores.
    CpuidBased,
    /// `amd`: AMD's CPUID 0x80000026 core type (Zen 5 with Zen 5c).
    AmdCpuid,
    /// `auto`, the default: efficiency class, then AMD's core type, then (only with
    /// `core-priority=hybrid`) the core priority.
    Auto,
}

impl TopologyDetectionMethod {
    pub const ALL: [TopologyDetectionMethod; 5] = [Self::WindowsApi, Self::WindowsApiV2, Self::CpuidBased, Self::AmdCpuid, Self::Auto];

    /// The `topology=` value.
    pub fn name(self) -> &'static str {
        match self {
            Self::WindowsApi => "windows",
            Self::WindowsApiV2 => "windowsv2",
            Self::CpuidBased => "cpuid",
            Self::AmdCpuid => "amd",
            Self::Auto => "auto",
        }
    }

    /// From a `topology=` value (with the old aliases windowsapi and v2).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "windows" | "windowsapi" => Some(Self::WindowsApi),
            "windowsv2" | "v2" => Some(Self::WindowsApiV2),
            "cpuid" => Some(Self::CpuidBased),
            "amd" => Some(Self::AmdCpuid),
            "auto" => Some(Self::Auto),
            _ => None,
        }
    }
}

// Global configuration for detection method
static TOPOLOGY_METHOD: OnceLock<TopologyDetectionMethod> = OnceLock::new();

// Set the detection method (call this early in main.rs)
pub fn set_topology_detection_method(method: TopologyDetectionMethod) {
    TOPOLOGY_METHOD.set(method).ok();
}

/// `core-priority=`: what Auto makes of Windows' core priority (the scheduling class) when
/// nothing else tells the cores apart (TODO 96).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CorePriority {
    /// The default: a priority ranking alone types nothing. On most CPUs it is only the preferred
    /// cores (a Ryzen 5 8600G ranks 2 of its 6 identical cores above the rest).
    #[default]
    Equal,
    /// The higher-priority cores are P-cores and the rest E-cores, for a hybrid that reports no
    /// core types: a Ryzen 5 8500G's Zen 4 and Zen 4c cores show only as that ranking.
    Hybrid,
}

impl CorePriority {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_lowercase().as_str() {
            "equal" => Ok(CorePriority::Equal),
            "hybrid" => Ok(CorePriority::Hybrid),
            other => Err(format!("core-priority '{other}': use equal or hybrid")),
        }
    }
}

static CORE_PRIORITY: OnceLock<CorePriority> = OnceLock::new();

/// Set before the topology is first detected; later calls change nothing.
pub fn set_core_priority(priority: CorePriority) {
    CORE_PRIORITY.set(priority).ok();
}

// Main wrapper function - all other functions should call this
pub fn get_cpu_topology() -> &'static Vec<CpuTopologyInfo> {
    CPU_TOPOLOGY.get_or_init(|| {
        let method = TOPOLOGY_METHOD.get().copied().unwrap_or(TopologyDetectionMethod::Auto);
        
        log::info!("Detecting CPU topology using method: {:?}", method);
        let start = std::time::Instant::now();

        let topology = detect_cpu_topology_with(method);

        let elapsed = start.elapsed();
        log::info!("CPU topology detection completed in {:?}", elapsed);
        
        // Log summary
        let physical_cores = topology.iter()
            .map(|cpu| cpu.physical_core_id)
            .collect::<std::collections::HashSet<_>>()
            .len();
        let p_cores = topology.iter()
            .filter(|cpu| matches!(cpu.core_type, CoreType::Performance(_)))
            .map(|cpu| cpu.physical_core_id)
            .collect::<std::collections::HashSet<_>>()
            .len();
        let e_cores = topology.iter()
            .filter(|cpu| matches!(cpu.core_type, CoreType::Efficiency(_)))
            .map(|cpu| cpu.physical_core_id)
            .collect::<std::collections::HashSet<_>>()
            .len();
            
        log::info!("Topology: {} physical cores ({} P-cores, {} E-cores, {} Unknown)",
                  physical_cores, p_cores, e_cores, physical_cores - p_cores - e_cores);
        
        topology
    })
}

/// Physical cores across every processor group. `num_cpus::get_physical()` sees only the calling
/// thread's group on Windows (64 CPUs), so on a 192-CPU m8a.48xlarge TMR started 64 threads.
pub fn physical_core_count() -> usize {
    get_cpu_topology().iter().map(|cpu| cpu.physical_core_id).collect::<HashSet<_>>().len()
}

/// Logical CPUs across every processor group, in TMR's numbering (group * 64 + bit), ascending.
pub fn logical_cpu_ids() -> Vec<usize> {
    let mut ids: Vec<usize> = get_cpu_topology().iter().map(|cpu| cpu.logical_id).collect();
    ids.sort_unstable();
    ids
}

/// Physical-core id for a logical CPU, from the real detected topology (the same source the
/// CPU Topology table uses). Replaces the old `cpu_id / 2` approximation, which hardcoded
/// 2-way SMT and mislabeled cores on non-SMT (e.g. AMD EPYC) or non-2-way-SMT parts.
/// Falls back to the logical id (a sane 1:1 default) if the CPU isn't found.
pub fn get_physical_core_for_cpu(cpu_id: usize) -> usize {
    get_cpu_topology()
        .iter()
        .find(|cpu| cpu.logical_id == cpu_id)
        .map(|cpu| cpu.physical_core_id)
        .unwrap_or(cpu_id)
}


pub fn get_numa_node_for_cpu(cpu_id: usize) -> u32 {
    static NUMA_TOPOLOGY: OnceLock<NumaTopology> = OnceLock::new();
    
    let topology = NUMA_TOPOLOGY.get_or_init(|| {
        discover_numa_topology().unwrap_or_else(|e| {
            log::warn!("Failed to discover NUMA topology: {}", e);
            NumaTopology {
                cpu_to_node: HashMap::new(),
            }
        })
    });
    
    topology.cpu_to_node.get(&(cpu_id as u32)).copied().unwrap_or(0)
}

pub fn is_hybrid_cpu(topology: &[CpuTopologyInfo]) -> bool {
    // Note: This now takes topology as a parameter instead of detecting it
    let efficiency_classes: std::collections::HashSet<_> = topology
        .iter()
        .map(|cpu| cpu.efficiency_class)
        .collect();
    
    // Count cores by type
    let mut p_cores = 0;
    let mut e_cores = 0;
    let mut unknown_cores = 0;
    
    let mut counted_physical = std::collections::HashSet::new();
    for cpu in topology {
        if !counted_physical.contains(&cpu.physical_core_id) {
            counted_physical.insert(cpu.physical_core_id);
            match cpu.core_type {
				CoreType::Performance(_) => p_cores += 1,
				CoreType::Efficiency(_) => e_cores += 1,
				CoreType::Unknown => unknown_cores += 1,
            }
        }
    }
    
    log::debug!("Core type distribution: P-cores={}, E-cores={}, Unknown={}", 
              p_cores, e_cores, unknown_cores);
    log::debug!("Efficiency classes found: {:?}", efficiency_classes);
    
    // Consider it hybrid if:
    // 1. We have more than one efficiency class (excluding 0xFF which means "not specified")
    // 2. OR we have both P and E cores identified
    let has_multiple_classes = efficiency_classes.len() > 1 && 
                              !efficiency_classes.iter().all(|&c| c == 0xFF);
    let has_both_core_types = p_cores > 0 && e_cores > 0;
    
    log::debug!("is_hybrid_cpu: multiple_classes={}, both_types={}", 
              has_multiple_classes, has_both_core_types);
    
    has_multiple_classes || has_both_core_types
}

/// The cores from `detect_cpu_topology`, typed by `method`: Auto's cascade, or one detector alone,
/// every core the same type when its signal doesn't tell them apart.
pub fn detect_cpu_topology_with(method: TopologyDetectionMethod) -> Vec<CpuTopologyInfo> {
    use TopologyDetectionMethod::*;
    if method == Auto {
        return detect_cpu_topology_auto();
    }
    let mut topology = detect_cpu_topology();
    let (signal, types) = match method {
        WindowsApi => ("the efficiency class", types_from_efficiency(&efficiency_classes(&topology))),
        WindowsApiV2 => ("Windows' core priority", types_from_priority(&core_priorities(&topology))),
        CpuidBased => ("the L3 size", types_from_l3(&topology)),
        AmdCpuid => ("AMD's CPUID 0x80000026 core type", amd_cpuid_core_types(&topology).map(|amd| types_from_amd(&amd))),
        Auto => unreachable!("handled above"),
    };
    match &types {
        Some(_) => log::info!("Core types from {signal} alone (topology={})", method.name()),
        None => log::info!("topology={}: {signal} doesn't tell the cores apart, so every core is the same type", method.name()),
    }
    for cpu in &mut topology {
        cpu.core_type = match &types {
            Some(types) => types.get(&cpu.physical_core_id).copied().unwrap_or(CoreType::Unknown),
            None => CoreType::Performance(0),
        };
    }
    topology
}

/// `topology=auto`, the default: the detectors as a cascade, stopping at the first that tells the
/// cores apart (`classify_core_types`): the efficiency class, then AMD's core type, then, only
/// with `core-priority=hybrid`, Windows' core priority; else every core the same type, with a
/// warning when the priority alone tells them apart.
pub fn detect_cpu_topology_auto() -> Vec<CpuTopologyInfo> {
    let mut topology = detect_cpu_topology();
    let efficiency = efficiency_classes(&topology);
    let amd = if classes_vary(&efficiency) { None } else { amd_cpuid_core_types(&topology) };
    let priority = core_priorities(&topology);
    let hybrid = CORE_PRIORITY.get().copied().unwrap_or_default() == CorePriority::Hybrid;
    let types = classify_core_types(&efficiency, amd.as_ref(), hybrid.then_some(&priority));
    let ranked_only = ranked_only(&efficiency, amd.as_ref(), &priority);
    let source = if classes_vary(&efficiency) {
        "efficiency class"
    } else if amd.is_some() {
        "AMD CPUID 0x80000026 core type"
    } else if hybrid && ranked_only.is_some() {
        "Windows' core priority (core-priority=hybrid)"
    } else {
        "none: every core the same type"
    };
    log::info!("Core types from {source}");
    match (hybrid, ranked_only) {
        (false, Some(top)) => log::warn!(
            "Every core is typed the same: neither Windows nor the CPU reports core types, but Windows \
             gives {} a higher priority. On most CPUs that is only the preferred cores. On a hybrid that \
             doesn't report its types (a Ryzen 8500G's Zen 4 and Zen 4c cores, for one), add \
             core-priority=hybrid to make those P-cores and the rest E-cores.", core_list(&top)),
        (true, None) if !classes_vary(&efficiency) && amd.is_none() => log::warn!(
            "core-priority=hybrid, but Windows gives every core the same priority: every core is typed the same"),
        _ => {}
    }
    for cpu in &mut topology {
        cpu.core_type = types.get(&cpu.physical_core_id).copied().unwrap_or(CoreType::Unknown);
    }
    topology
}

/// Each physical core's efficiency class, as Windows reports it.
fn efficiency_classes(topology: &[CpuTopologyInfo]) -> HashMap<usize, u8> {
    topology.iter().map(|cpu| (cpu.physical_core_id, cpu.efficiency_class)).collect()
}

/// Each physical core's priority (Windows' scheduling class), the highest of its logical CPUs.
/// Empty if Windows doesn't give the CPU sets.
fn core_priorities(topology: &[CpuTopologyInfo]) -> HashMap<usize, u8> {
    let Ok(sets) = get_system_cpu_set_information() else { return HashMap::new() };
    let by_cpu: HashMap<usize, u8> = sets.iter().map(|set| (set.logical_processor_index as usize, set.scheduling_class)).collect();
    let mut priority: HashMap<usize, u8> = HashMap::new();
    for cpu in topology {
        if let Some(&class) = by_cpu.get(&cpu.logical_id) {
            let core = priority.entry(cpu.physical_core_id).or_insert(class);
            *core = (*core).max(class);
        }
    }
    priority
}

/// The higher-priority cores, ascending, when the priority ranking is the only thing that tells the
/// cores apart: efficiency classes uniform, no AMD core types, and the priorities differ.
fn ranked_only(efficiency: &HashMap<usize, u8>, amd: Option<&HashMap<usize, u8>>, priority: &HashMap<usize, u8>) -> Option<Vec<usize>> {
    if classes_vary(efficiency) || amd.is_some() || !classes_vary(priority) {
        return None;
    }
    let top = priority.values().copied().max()?;
    let mut cores: Vec<usize> = priority.iter().filter(|&(_, &class)| class == top).map(|(&core, _)| core).collect();
    cores.sort_unstable();
    Some(cores)
}

/// "core 3", "cores 0 and 4", "cores 0, 2 and 4".
fn core_list(cores: &[usize]) -> String {
    match cores {
        [one] => format!("core {one}"),
        [rest @ .., last] => format!("cores {} and {last}", rest.iter().map(|c| c.to_string()).collect::<Vec<_>>().join(", ")),
        [] => "no core".to_string(),
    }
}

/// Whether the classes Windows reports differ between cores (0xFF, "not specified", left out):
/// the efficiency classes, or the scheduling classes in `core_priorities`.
fn classes_vary(classes_by_core: &HashMap<usize, u8>) -> bool {
    classes_by_core.values().filter(|&&class| class != 0xFF).collect::<HashSet<_>>().len() > 1
}

/// Types from per-core classes that vary: the highest class is the performance cores, each lower
/// one an efficiency tier, the lowest tier 0; 0xFF ("not specified") is Unknown.
fn types_by_class(classes_by_core: &HashMap<usize, u8>) -> HashMap<usize, CoreType> {
    let mut classes: Vec<u8> = classes_by_core.values().copied().filter(|&class| class != 0xFF).collect();
    classes.sort_unstable();
    classes.dedup();
    let top = classes.last().copied();
    classes_by_core.iter().map(|(&core, &class)| {
        let core_type = if class == 0xFF {
            CoreType::Unknown
        } else if Some(class) == top {
            CoreType::Performance(0)
        } else {
            CoreType::Efficiency(classes.iter().position(|&c| c == class).unwrap_or(0) as u8)
        };
        (core, core_type)
    }).collect()
}

/// Each physical core's type. From the efficiency class Windows reports when it varies (Intel's
/// P- and E-cores, and any CPU Windows itself treats as hybrid): the highest class is the
/// performance cores, each lower one an efficiency tier, the lowest tier 0. Else from AMD's CPUID
/// core type (`amd`: 0 performance, 1 efficiency), given only when the CPU says its cores differ.
/// Else, under `core-priority=hybrid` (`priority` given), from Windows' core priority the same way
/// as the efficiency class. Else every core is a performance core.
///
/// A priority ranking is not a core type by default. On a uniform AMD part it is the preferred-core
/// (CPPC) ranking, and reading it as types made a Ryzen 5 8600G "2 P-cores + 4 E-cores" and a
/// Ryzen 7 5700X "2 + 6". A Ryzen 5 8500G's ranking looks the same and is its real Zen 4 and
/// Zen 4c cores (cores 0 and 4 sustain 5.2 GHz, the rest 3.5), hence the opt-in.
pub fn classify_core_types(efficiency: &HashMap<usize, u8>, amd: Option<&HashMap<usize, u8>>,
                           priority: Option<&HashMap<usize, u8>>) -> HashMap<usize, CoreType> {
    let hit = types_from_efficiency(efficiency)
        .or_else(|| amd.map(types_from_amd))
        .or_else(|| priority.and_then(types_from_priority));
    efficiency.keys().map(|&core| {
        let core_type = match &hit {
            Some(types) => types.get(&core).copied().unwrap_or(CoreType::Unknown),
            None => CoreType::Performance(0),
        };
        (core, core_type)
    }).collect()
}

/// The efficiency-class detector (`topology=windows`, Auto's first step): None when every core
/// has the same class.
fn types_from_efficiency(classes: &HashMap<usize, u8>) -> Option<HashMap<usize, CoreType>> {
    classes_vary(classes).then(|| types_by_class(classes))
}

/// The core-priority detector (`topology=windowsv2`, Auto's last step under
/// `core-priority=hybrid`): Windows' scheduling classes ranked as efficiency classes are. None
/// when every core has the same priority.
fn types_from_priority(priority: &HashMap<usize, u8>) -> Option<HashMap<usize, CoreType>> {
    classes_vary(priority).then(|| types_by_class(priority))
}

/// The AMD detector (`topology=amd`, Auto's second step): `amd_cpuid_core_types`'s 0 is
/// performance, 1 efficiency.
fn types_from_amd(amd: &HashMap<usize, u8>) -> HashMap<usize, CoreType> {
    amd.iter().map(|(&core, &t)| {
        let core_type = match t {
            0 => CoreType::Performance(0),
            1 => CoreType::Efficiency(0),
            _ => CoreType::Unknown,
        };
        (core, core_type)
    }).collect()
}

/// The L3 detector (`topology=cpuid`): the cores with the most L3 are P-cores, the rest E.
/// None when every core has the same L3, or a core can't be read.
fn types_from_l3(topology: &[CpuTopologyInfo]) -> Option<HashMap<usize, CoreType>> {
    let sizes = l3_sizes(topology)?;
    let mut distinct: Vec<usize> = sizes.values().flatten().copied().collect::<HashSet<_>>().into_iter().collect();
    distinct.sort_unstable();
    log::info!("L3 per core: {:?} MiB", distinct.iter().map(|s| *s as f64 / (1 << 20) as f64).collect::<Vec<_>>());
    types_by_l3(&sizes)
}

/// `types_from_l3`'s rule on per-core L3 sizes; a core that lists no L3 is Unknown.
fn types_by_l3(sizes: &HashMap<usize, Option<usize>>) -> Option<HashMap<usize, CoreType>> {
    let distinct: HashSet<usize> = sizes.values().flatten().copied().collect();
    let most = *distinct.iter().max()?;
    (distinct.len() > 1).then(|| sizes.iter().map(|(&core, &l3)| {
        let core_type = match l3 {
            Some(l3) if l3 == most => CoreType::Performance(0),
            Some(_) => CoreType::Efficiency(0),
            None => CoreType::Unknown,
        };
        (core, core_type)
    }).collect())
}

/// AMD's type for each physical core, from CPUID leaf 0x80000026 (Extended CPU Topology): EAX bit 30
/// (HeterogeneousCores) says the cores differ, and EBX bits 31:28 are each core's type, 0 for
/// performance and 1 for efficiency (Zen 5 with Zen 5c; whether Zen 4c parts set it is unchecked).
/// None unless the CPU is AMD, has the leaf, and sets that bit. Each core is read on its own CPU
/// (`read_on_each_core`).
fn amd_cpuid_core_types(topology: &[CpuTopologyInfo]) -> Option<HashMap<usize, u8>> {
    use std::arch::x86_64::{__cpuid, __cpuid_count};

    if CpuId::new().get_vendor_info().is_none_or(|vendor| vendor.as_str() != "AuthenticAMD") {
        return None;
    }
    // Note: __cpuid and __cpuid_count are safe in edition 2024
    if __cpuid(0x8000_0000).eax < 0x8000_0026 || __cpuid_count(0x8000_0026, 0).eax & (1 << 30) == 0 {
        return None;
    }
    read_on_each_core(topology, "core type", || (__cpuid_count(0x8000_0026, 0).ebx >> 28) as u8)
}

/// `read()` once for each physical core, on that core's first logical CPU, from a short-lived
/// thread pinned there by processor group: above 64 CPUs too, and without touching the caller's
/// affinity. None if a pin fails.
fn read_on_each_core<T: Send>(topology: &[CpuTopologyInfo], what: &str, read: impl Fn() -> T + Sync) -> Option<HashMap<usize, T>> {
    use windows::Win32::System::SystemInformation::GROUP_AFFINITY;
    use windows::Win32::System::Threading::{GetCurrentThread, SetThreadGroupAffinity};

    let mut first_cpu: HashMap<usize, usize> = HashMap::new();
    for cpu in topology {
        first_cpu.entry(cpu.physical_core_id).or_insert(cpu.logical_id);
    }
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let mut values = HashMap::new();
            for (&core, &logical) in &first_cpu {
                let target = GROUP_AFFINITY { Mask: 1usize << (logical % 64), Group: (logical / 64) as u16, Reserved: [0; 3] };
                // SAFETY: this thread's own pseudo-handle, and a GROUP_AFFINITY that outlives the call
                if !unsafe { SetThreadGroupAffinity(GetCurrentThread(), &target, None) }.as_bool() {
                    log::warn!("Could not pin to logical CPU {logical} to read its {what}");
                    return None;
                }
                values.insert(core, read());
            }
            Some(values)
        }).join().ok().flatten()
    })
}

/// Each physical core's L3 in bytes, from its own CPUID cache parameters: ways x partitions x
/// line size x sets, as `cache.rs` sizes it (raw_cpuid gives the counts, not the fields' "minus
/// one" encoding; adding 1 to each read a 16 MiB L3 as 34.5 MiB). None for a core that lists no L3.
fn l3_sizes(topology: &[CpuTopologyInfo]) -> Option<HashMap<usize, Option<usize>>> {
    read_on_each_core(topology, "L3 size", || {
        CpuId::new().get_cache_parameters()?
            .find(|cache| cache.level() == 3)
            .map(|c| c.associativity() * c.physical_line_partitions() * c.coherency_line_size() * c.sets())
    })
}

pub fn detect_cpu_topology() -> Vec<CpuTopologyInfo> {
    let mut topology = Vec::new();
    
    unsafe {
        use windows::Win32::System::SystemInformation::{
            GetLogicalProcessorInformationEx, RelationProcessorCore,
            SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
        };
        
        // First get the buffer size
        let mut buffer_size = 0u32;
        let _ = GetLogicalProcessorInformationEx(
            RelationProcessorCore,
            None,
            &mut buffer_size,
        );
        
        if buffer_size == 0 {
            log::warn!("GetLogicalProcessorInformationEx returned 0 buffer size");
            // Fallback for single core or error
            for i in 0..num_cpus::get() {
                topology.push(CpuTopologyInfo {
                    logical_id: i,
                    physical_core_id: i,
                    numa_node: get_numa_node_for_cpu(i),
                    is_hyperthreaded: false,
                    core_type: CoreType::Unknown,
                    threads_on_this_core: 1,
                    efficiency_class: 0xFF,
                });
            }
            return topology;
        }
        
        // ALIGNMENT: u64-backed, not u8 — `SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX` contains
        // `KAFFINITY` (u64) so it needs 8-byte alignment, which `Vec<u8>` does not guarantee.
        // See the same note in `memory/privileges.rs`. The walk below is still in BYTES, so it
        // takes an explicit `*const u8` base — do not call `.add(offset)` on the `*const u64`.
        let mut buffer = vec![0u64; (buffer_size as usize).div_ceil(std::mem::size_of::<u64>())];

        if GetLogicalProcessorInformationEx(
            RelationProcessorCore,
            Some(buffer.as_mut_ptr() as *mut SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX),
            &mut buffer_size,
        ).is_ok() {
            let base = buffer.as_ptr() as *const u8;
            let mut offset = 0;
            let mut physical_core_id = 0;

            log::debug!("Processing CPU topology from Windows API...");

            while offset < buffer_size as usize {
                let info = &*(base.add(offset) as *const SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX);
                
                if info.Relationship == RelationProcessorCore {
                    let core_info = &info.Anonymous.Processor;
                    let group_count = core_info.GroupCount;
                    let efficiency_class = core_info.EfficiencyClass;
                    
                    log::debug!("Physical Core {}: EfficiencyClass = {}", physical_core_id, efficiency_class);
                    
                    // The cores only; a detector types them (`detect_cpu_topology_with`). Typing
                    // class 0 as E-cores here made every uniform CPU read as all E-cores.
                    let core_type = CoreType::Unknown;
                    
                    let mut logical_processors = Vec::new();
                    
                    // Collect all logical processors for this physical core
                    for group_mask in &core_info.GroupMask[..group_count as usize] {
                        let group = group_mask.Group;
                        let mask = group_mask.Mask;
                        
                        for bit in 0..64 {
                            if (mask & (1u64 << bit) as usize) != 0 {
                                let logical_id = (group as usize * 64) + bit;
                                logical_processors.push(logical_id);
                            }
                        }
                    }
                    
                    // Create topology entries for each logical processor
                    let threads_on_this_core = logical_processors.len();
                    let is_hyperthreaded = threads_on_this_core > 1;
                    
                    log::debug!("  Physical Core {} has {} logical processors, type: {:?}", 
                              physical_core_id, threads_on_this_core, core_type);
                    
                    for logical_id in logical_processors {
                        topology.push(CpuTopologyInfo {
                            logical_id,
                            physical_core_id,
                            numa_node: get_numa_node_for_cpu(logical_id),
                            is_hyperthreaded,
                            core_type,
                            threads_on_this_core,
                            efficiency_class,
                        });
                    }
                    
                    physical_core_id += 1;
                }
                
                offset += info.Size as usize;
            }
        }
    }
    
    // Sort by logical ID for consistent display
    topology.sort_by_key(|cpu| cpu.logical_id);
    
    // Log summary of efficiency classes found
    let efficiency_classes: std::collections::HashSet<_> = topology
        .iter()
        .map(|cpu| cpu.efficiency_class)
        .collect();
    log::info!("Detected efficiency classes: {:?}", efficiency_classes);
    
    topology
}


pub fn display_cpu_topology(cpu_list: &[usize], cpus_to_skip: usize, avoid_smt: bool, pinned: bool) {
    use crate::reporting::{create_console_reporter, models::{CpuTopologyReport, CpuTopologyEntry, TopologySummary}};

    let topology = get_cpu_topology();
    let is_hybrid = is_hybrid_cpu(topology);

    // Build set of physical cores that have assigned CPUs (for SMT exclusion marking)
    let assigned_physical_cores: std::collections::HashSet<usize> = if avoid_smt {
        cpu_list.iter()
            .filter_map(|&cpu_id| {
                topology.iter()
                    .find(|c| c.logical_id == cpu_id)
                    .map(|c| c.physical_core_id)
            })
            .collect()
    } else {
        std::collections::HashSet::new()
    };
    
    // First, identify which physical cores are being skipped
    let mut skipped_physical_cores = std::collections::HashSet::new();
    if cpus_to_skip > 0 {
        // Group by physical core, prioritizing E-cores for skipping
        let mut cores_by_type: std::collections::HashMap<&str, Vec<usize>> = std::collections::HashMap::new();
        let mut cores_map: std::collections::HashMap<usize, (Vec<usize>, CoreType)> = std::collections::HashMap::new();
        
        for cpu in topology {
            cores_map.entry(cpu.physical_core_id)
                .or_insert_with(|| (Vec::new(), cpu.core_type))
                .0.push(cpu.logical_id);
        }
        
        // Separate cores by type
        for (core_id, (_, core_type)) in &cores_map {
            let type_key = match core_type {
                CoreType::Performance(_) => "performance",
                CoreType::Efficiency(_) => "efficiency", 
                CoreType::Unknown => "unknown",
            };
            cores_by_type.entry(type_key)
                .or_default()
                .push(*core_id);
        }
        
        // Sort core IDs
        for cores in cores_by_type.values_mut() {
            cores.sort();
        }
        
        // Skip E-cores first, then P-cores
        let mut remaining_to_skip = cpus_to_skip;
        
        // Skip efficiency cores first
        if let Some(e_cores) = cores_by_type.get("efficiency") {
            for &core_id in e_cores.iter().take(remaining_to_skip) {
                skipped_physical_cores.insert(core_id);
                remaining_to_skip = remaining_to_skip.saturating_sub(1);
            }
        }
        
        // Then skip performance cores if needed
        if remaining_to_skip > 0
            && let Some(p_cores) = cores_by_type.get("performance") {
                for &core_id in p_cores.iter().take(remaining_to_skip) {
                    skipped_physical_cores.insert(core_id);
                }
            }
    }
    
    let mut skipped_count = 0;
    let mut assigned_count = 0;
    let mut available_count = 0;
    let mut smt_excluded_count = 0;
    let mut p_core_count = 0;
    let mut e_core_count = 0;

    // Count unique physical cores by type
    let mut counted_cores = std::collections::HashSet::new();
    let mut cpu_entries = Vec::new();

    for cpu_info in topology {
        if !counted_cores.contains(&cpu_info.physical_core_id) {
            counted_cores.insert(cpu_info.physical_core_id);
            match cpu_info.core_type {
                CoreType::Performance(_) => p_core_count += 1,
                CoreType::Efficiency(_) => e_core_count += 1,
                CoreType::Unknown => {},
            }
        }

        // Determine status based on physical core
        let (status, thread_id) = if skipped_physical_cores.contains(&cpu_info.physical_core_id) {
            skipped_count += 1;
            ("Skipped", None)
        } else if let Some(thread_idx) = cpu_list.iter().position(|&cpu| cpu == cpu_info.logical_id) {
            assigned_count += 1;
            ("Assigned", Some(thread_idx))
        } else if avoid_smt && assigned_physical_cores.contains(&cpu_info.physical_core_id) {
            // This CPU is an SMT sibling of an assigned core, excluded due to cputype=cores
            smt_excluded_count += 1;
            ("SMT Excluded", None)
        } else {
            available_count += 1;
            ("Available", None)
        };
        
        cpu_entries.push(CpuTopologyEntry {
            logical_cpu: cpu_info.logical_id,
            physical_core: cpu_info.physical_core_id,
            core_type: cpu_info.core_type.display_name().to_string(),
            threads_on_core: cpu_info.threads_on_this_core,
            numa_node: cpu_info.numa_node,
            is_hyperthreaded: cpu_info.is_hyperthreaded,
            status: status.to_string(),
            thread_id,
        });
    }
    
    // Calculate summary data
    let total_logical = topology.len();
    let total_physical = topology.iter()
        .map(|cpu| cpu.physical_core_id)
        .collect::<std::collections::HashSet<_>>()
        .len();
    
    let performance_logical = topology.iter().filter(|c| matches!(c.core_type, CoreType::Performance(_))).count();
    let efficiency_logical = topology.iter().filter(|c| matches!(c.core_type, CoreType::Efficiency(_))).count();
    
    let summary = TopologySummary {
        is_hybrid,
        total_logical,
        total_physical,
        performance_cores: p_core_count,
        efficiency_cores: e_core_count,
        performance_logical,
        efficiency_logical,
        assigned_count,
        available_count,
        skipped_count,
        smt_excluded_count,
    };
    
    let report = CpuTopologyReport {
        cpus: cpu_entries,
        summary,
        pinned,
    };
    
    let mut reporter = create_console_reporter();
    if let Err(e) = reporter.report_cpu_topology(&report) {
        log::error!("Failed to display CPU topology table: {}", e);
        return;
    }
    
    // Additional SMT usage information for hybrid CPUs
    if is_hybrid {
        let p_cores_with_smt = topology.iter()
            .filter(|t| matches!(t.core_type, CoreType::Performance(_)) && t.is_hyperthreaded)
            .map(|t| t.physical_core_id)
            .collect::<std::collections::HashSet<_>>()
            .len();
        
        if p_cores_with_smt > 0 {
            println!("  SMT/Hyperthreading: Enabled on {} P-cores", p_cores_with_smt);
        }
        
        if cpus_to_skip > 0 {
            println!("    Note: E-cores were prioritized for skipping");
        }
    }
}

pub fn get_system_cpu_set_information() -> Result<Vec<EnhancedCpuInfo>, String> {
    unsafe {
        use windows::Win32::System::SystemInformation::{
            GetSystemCpuSetInformation, SYSTEM_CPU_SET_INFORMATION,
        };
        
        let mut buffer_length = 0u32;
        
        // First call to get required buffer size
        let _ = GetSystemCpuSetInformation(
            None,
            0,
            &mut buffer_length,
            None,
            None,
        );
        
        if buffer_length == 0 {
            return Err("Failed to get buffer size for CPU set information".to_string());
        }
        
        // Allocate buffer. ALIGNMENT: u64-backed (see `memory/privileges.rs`); the walk below
        // stays in BYTES via an explicit `*const u8` base.
        let mut buffer = vec![0u64; (buffer_length as usize).div_ceil(std::mem::size_of::<u64>())];

        // Second call to get actual data
        let status = GetSystemCpuSetInformation(
            Some(buffer.as_mut_ptr() as *mut SYSTEM_CPU_SET_INFORMATION),
            buffer_length,
            &mut buffer_length,
            None,
            None,
        );

        if !status.as_bool() {
            return Err("GetSystemCpuSetInformation failed".to_string());
        }

        // Parse the buffer
        let base = buffer.as_ptr() as *const u8;
        let mut cpu_infos = Vec::new();
        let mut offset = 0usize;

        while offset < buffer_length as usize {
            let info = &*(base.add(offset) as *const SYSTEM_CPU_SET_INFORMATION);

            // Per logical CPU; a CPU's physical core comes from `detect_cpu_topology`, not from
            // here (dividing by 2 assumed 2-way SMT, wrong on CPUs without it)
            cpu_infos.push(EnhancedCpuInfo {
                // Group-relative in the API; TMR numbers CPUs group * 64 + index
                logical_processor_index: info.Anonymous.CpuSet.Group as u32 * 64 + info.Anonymous.CpuSet.LogicalProcessorIndex as u32,
                scheduling_class: info.Anonymous.CpuSet.Anonymous2.SchedulingClass,
            });
            
            offset += info.Size as usize;
        }
        
        Ok(cpu_infos)
    }
}

// Modern NUMA topology discovery
pub fn discover_numa_topology() -> Result<NumaTopology, String> {
    unsafe {
        // RelationNumaNodeEx: each node with every processor group it spans (GroupCount masks).
        // RelationNumaNode gives only a node's primary group, so on a node over 64 CPUs (an
        // m8a.48xlarge: 96 per node, group 1 split between both) the rest defaulted to node 0.
        let mut relation = RelationNumaNodeEx;
        let mut buffer_size = 0u32;

        let _ = GetLogicalProcessorInformationEx(
            relation,
            None,
            &mut buffer_size,
        );
        if buffer_size == 0 {
            // Before RelationNumaNodeEx existed: primary groups only
            relation = RelationNumaNode;
            let _ = GetLogicalProcessorInformationEx(relation, None, &mut buffer_size);
        }
        
        if buffer_size == 0 {
            return Ok(NumaTopology {
                cpu_to_node: (0..num_cpus::get() as u32).map(|cpu| (cpu, 0)).collect(),
            });
        }
        
        // ALIGNMENT: u64-backed (see `memory/privileges.rs`); the walk below stays in BYTES
        // via an explicit `*const u8` base.
        let mut buffer = vec![0u64; (buffer_size as usize).div_ceil(std::mem::size_of::<u64>())];

        GetLogicalProcessorInformationEx(
            relation,
            Some(buffer.as_mut_ptr() as *mut SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX),
            &mut buffer_size,
        ).map_err(|e| format!("Failed to get NUMA topology: {:?}", e))?;

        let base = buffer.as_ptr() as *const u8;
        let mut cpu_to_node = HashMap::new();
        let mut offset = 0;

        while offset < buffer_size as usize {
            let info = &*(base.add(offset) as *const SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX);
            
            // Both requests return their nodes as RelationNumaNode records
            if info.Relationship == RelationNumaNode {
                let numa_info = &info.Anonymous.NumaNode;
                let node_id = numa_info.NodeNumber;
                // GroupCount masks follow in the record (0 from the older request: one mask)
                let masks = &raw const numa_info.Anonymous.GroupMasks as *const GROUP_AFFINITY;
                for i in 0..(numa_info.GroupCount as usize).max(1) {
                    let group_affinity = &*masks.add(i);
                    for bit in 0..64 {
                        if (group_affinity.Mask & (1usize << bit)) != 0 {
                            cpu_to_node.insert(group_affinity.Group as u32 * 64 + bit, node_id);
                        }
                    }
                }
            }
            
            offset += info.Size as usize;
        }
        
        Ok(NumaTopology { cpu_to_node })
    }
}

/// `--debug-topology`: every detector alone, then Auto's cascade, each core's type under each,
/// and the signals they read, so a platform's detectors can be checked one by one.
pub fn debug_topology_detection() {
    println!("\n=== CPU Topology Detection Debug ===");
    let mut cores: Vec<usize> = Vec::new();
    for method in TopologyDetectionMethod::ALL {
        let topology = detect_cpu_topology_with(method);
        let mut by_core: HashMap<usize, (CoreType, Vec<usize>)> = HashMap::new();
        for cpu in &topology {
            by_core.entry(cpu.physical_core_id).or_insert((cpu.core_type, Vec::new())).1.push(cpu.logical_id);
        }
        let count = |pred: fn(&CoreType) -> bool| by_core.values().filter(|(t, _)| pred(t)).count();
        println!("\n--- topology={} ---", method.name());
        println!("  {} logical CPUs, {} cores: P={}, E={}, Unknown={}", topology.len(), by_core.len(),
                 count(|t| matches!(t, CoreType::Performance(_))), count(|t| matches!(t, CoreType::Efficiency(_))),
                 count(|t| matches!(t, CoreType::Unknown)));
        cores = by_core.keys().copied().collect();
        cores.sort_unstable();
        for core in &cores {
            let (core_type, cpus) = &by_core[core];
            println!("    Core {core}: {} -> CPUs {cpus:?}", core_type.display_name());
        }
    }

    // The signals the detectors read, per core
    let topology = detect_cpu_topology();
    let (efficiency, priority) = (efficiency_classes(&topology), core_priorities(&topology));
    let l3 = l3_sizes(&topology).unwrap_or_default();
    println!("\n--- Signals per core ---");
    println!("  {:>5} {:>17} {:>14} {:>9}", "Core", "Efficiency class", "Core priority", "L3 MiB");
    for core in &cores {
        let l3 = l3.get(core).copied().flatten().map_or("-".to_string(), |b| format!("{:.1}", b as f64 / (1 << 20) as f64));
        println!("  {:>5} {:>17} {:>14} {:>9}", core, efficiency.get(core).map_or("-".to_string(), |c| c.to_string()),
                 priority.get(core).map_or("-".to_string(), |c| c.to_string()), l3);
    }
    println!("\n=== End Debug ===");
}


pub fn show_complete_topology_mapping() {
    let topology = get_cpu_topology();
    
    println!("\n=== Complete CPU Topology Mapping ===");
    println!("Physical → Type      → Sched → NUMA → Logical CPUs");
    println!("-------------------------------------------------------");
    
    // Group by physical core
    let mut cores_map: HashMap<usize, (CoreType, Vec<usize>)> = HashMap::new();
    for cpu in topology {
        cores_map.entry(cpu.physical_core_id)
            .or_insert((cpu.core_type, Vec::new()))
            .1.push(cpu.logical_id);
    }
    
    // Scheduling classes by CPU (a core's first CPU stands for it)
    let mut cpu_sched_map: HashMap<usize, u8> = HashMap::new();
    if let Ok(cpu_sets) = get_system_cpu_set_information() {
        for cpu_set in &cpu_sets {
            cpu_sched_map.insert(cpu_set.logical_processor_index as usize, cpu_set.scheduling_class);
        }
    }
    
    // Sort by physical core ID
    let mut sorted_cores: Vec<_> = cores_map.into_iter().collect();
    sorted_cores.sort_by_key(|(id, _)| *id);
    
    for (phys_core, (core_type, mut logicals)) in sorted_cores {
        logicals.sort();
        let logical_str = logicals.iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join(",");
        
        let sched_class = cpu_sched_map.get(&logicals[0])
            .map(|s| s.to_string())
            .unwrap_or_else(|| "?".to_string());
        
        let numa_node = get_numa_node_for_cpu(logicals[0]);
        
        println!("Core {} → {:9} →   {}   →  {}  → [{}]", 
                phys_core, 
                core_type.display_name(),
                sched_class,
                numa_node,
                logical_str);
    }
    
    println!("\nLegend:");
    println!("  Sched: Scheduling class (higher = performance priority)");
    println!("  NUMA: Non-Uniform Memory Access node");
}
#[cfg(test)]
mod tests {
    use super::{classify_core_types, core_list, detect_cpu_topology, detect_cpu_topology_with, l3_sizes, ranked_only, types_by_l3,
                CorePriority, CoreType, TopologyDetectionMethod};
    use std::collections::HashMap;

    fn by_core(values: &[u8]) -> HashMap<usize, u8> {
        values.iter().copied().enumerate().collect()
    }

    fn types(efficiency: &[u8], amd: Option<&[u8]>) -> Vec<CoreType> {
        types_ranked(efficiency, amd, None)
    }

    /// As `types`, with `core-priority=hybrid`'s per-core priorities when `priority` is given.
    fn types_ranked(efficiency: &[u8], amd: Option<&[u8]>, priority: Option<&[u8]>) -> Vec<CoreType> {
        let (amd, priority) = (amd.map(by_core), priority.map(by_core));
        let types = classify_core_types(&by_core(efficiency), amd.as_ref(), priority.as_ref());
        (0..efficiency.len()).map(|core| types[&core]).collect()
    }

    /// TODO 96: under `core-priority=hybrid` Windows' core priority types the cores when nothing
    /// else does, as the efficiency class would; anything the OS or CPU reports wins over it.
    /// Under `equal` the ranking only earns the warning, which names the higher-priority cores.
    #[test]
    fn core_priority_types_cores_only_when_asked() {
        use CoreType::{Efficiency as E, Performance as P};
        // The 8500G: cores 0 and 4 (Zen 4) rank above the four Zen 4c cores
        let r8500g = [1, 0, 0, 0, 1, 0];
        assert_eq!(types_ranked(&[0; 6], None, Some(&r8500g)), [P(0), E(0), E(0), E(0), P(0), E(0)]);
        // Equal priorities type nothing, even when asked
        assert_eq!(types_ranked(&[0; 6], None, Some(&[0; 6])), [P(0); 6]);
        // The efficiency class and AMD's core types win
        assert_eq!(types_ranked(&[1, 0], None, Some(&[0, 1])), [P(0), E(0)]);
        assert_eq!(types_ranked(&[0, 0], Some(&[1, 0]), Some(&[1, 0])), [E(0), P(0)]);

        let (e, p) = (by_core(&[0; 6]), by_core(&r8500g));
        assert_eq!(ranked_only(&e, None, &p), Some(vec![0, 4]));
        assert_eq!(ranked_only(&e, None, &by_core(&[0; 6])), None, "no ranking, no warning");
        assert_eq!(ranked_only(&by_core(&[1, 0, 0, 0, 0, 0]), None, &p), None, "the efficiency class types the cores");
        assert_eq!(ranked_only(&e, Some(&by_core(&[0; 6])), &p), None, "AMD's CPUID types the cores");
        assert_eq!(core_list(&[0, 4]), "cores 0 and 4");
        assert_eq!(core_list(&[0, 2, 4]), "cores 0, 2 and 4");
        assert_eq!(core_list(&[3]), "core 3");

        assert_eq!(CorePriority::parse("Hybrid"), Ok(CorePriority::Hybrid));
        assert_eq!(CorePriority::parse("equal"), Ok(CorePriority::Equal));
        assert!(CorePriority::parse("on").is_err());
        assert_eq!(CorePriority::default(), CorePriority::Equal);
    }

    /// TODO 96: a core's type comes from the efficiency class when it varies, else from AMD's
    /// CPUID core type, else every core is a performance core; never from a scheduling class
    /// unless `core-priority=hybrid` asks (above).
    #[test]
    fn core_types_come_from_efficiency_class_or_amd_cpuid() {
        use CoreType::{Efficiency as E, Performance as P};
        // Uniform: the 8600G, the 5700X, an EPYC VM all report efficiency class 0 on every core
        assert_eq!(types(&[0; 6], None), [P(0); 6]);
        // Intel hybrid: P-cores 1, E-cores 0
        assert_eq!(types(&[1, 1, 0, 0, 0, 0], None), [P(0), P(0), E(0), E(0), E(0), E(0)]);
        // Three classes (Meteor Lake's LP E-cores): the lowest is efficiency tier 0
        assert_eq!(types(&[2, 1, 0], None), [P(0), E(1), E(0)]);
        // "Not specified" is Unknown, and doesn't make the classes vary
        assert_eq!(types(&[0xFF, 1, 0], None), [CoreType::Unknown, P(0), E(0)]);
        assert_eq!(types(&[0xFF, 0, 0], None), [P(0); 3]);
        // AMD heterogeneous (Zen 5 + Zen 5c), efficiency classes uniform
        assert_eq!(types(&[0; 4], Some(&[0, 0, 1, 1])), [P(0), P(0), E(0), E(0)]);
        // Varying efficiency classes win over AMD's core types
        assert_eq!(types(&[1, 0], Some(&[0, 0])), [P(0), E(0)]);
    }

    /// TODO 96: on the machine running the tests every detector, alone and as Auto's cascade, gives
    /// every core a type, one entry per logical CPU in every processor group.
    #[test]
    fn detection_types_every_core_here() {
        use windows::Win32::System::Threading::{GetActiveProcessorCount, ALL_PROCESSOR_GROUPS};
        // SAFETY: a plain query
        let cpus = unsafe { GetActiveProcessorCount(ALL_PROCESSOR_GROUPS) } as usize;
        for method in TopologyDetectionMethod::ALL {
            let topology = detect_cpu_topology_with(method);
            assert_eq!(topology.len(), cpus, "topology={}", method.name());
            assert!(topology.iter().all(|cpu| cpu.core_type != CoreType::Unknown), "topology={}: {topology:?}", method.name());
            assert_eq!(TopologyDetectionMethod::parse(method.name()), Some(method));
        }
        assert_eq!(TopologyDetectionMethod::parse("v2"), Some(TopologyDetectionMethod::WindowsApiV2));
        assert_eq!(TopologyDetectionMethod::parse("l3"), None);
    }

    /// TODO 96: the L3 detector's rule. A 7950X3D-like split types the larger L3's cores P (the
    /// reason it isn't in Auto's cascade); one size types nothing; an unread core is Unknown.
    #[test]
    fn l3_sizes_type_cores_only_when_they_differ() {
        use CoreType::{Efficiency as E, Performance as P};
        let mib = |n: usize| Some(n << 20);
        let x3d: HashMap<usize, Option<usize>> = (0..4).map(|c| (c, mib(if c < 2 { 96 } else { 32 }))).collect();
        let types = types_by_l3(&x3d).expect("two sizes");
        assert_eq!((0..4).map(|c| types[&c]).collect::<Vec<_>>(), [P(0), P(0), E(0), E(0)]);
        assert_eq!(types_by_l3(&(0..4).map(|c| (c, mib(16))).collect()), None);
        let partial: HashMap<usize, Option<usize>> = [(0, mib(16)), (1, mib(8)), (2, None)].into();
        assert_eq!(types_by_l3(&partial).expect("two sizes")[&2], CoreType::Unknown);
    }

    /// TODO 96: each core's own L3, as the L3 detector reads it, is the size the cache detection
    /// (`cache.rs`) reports. The detector once added 1 to each of its four factors and read the
    /// 8500G's 16 MiB as 34.5 MiB.
    #[test]
    fn per_core_l3_is_what_the_cache_detection_reports() {
        let topology = detect_cpu_topology();
        let sizes = l3_sizes(&topology).expect("pinned to every core");
        let reported = crate::cache::CacheInfo::detect().l3_cache;
        assert!(reported > 0, "the cache detection found no L3");
        assert!(sizes.values().all(|&s| s == Some(reported)), "per core {sizes:?}, cache.rs {reported}");
    }
}
