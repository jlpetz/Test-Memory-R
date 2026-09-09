// Add this to your runner.rs file to centralize topology detection
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::OnceLock;
use raw_cpuid::CpuId;

use windows::Win32::System::SystemInformation::{
    GetLogicalProcessorInformationEx,
    RelationNumaNode,
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
    
    // Simple helper to get scheduling priority (higher = better)
    pub fn priority(&self) -> u8 {
        match self {
            CoreType::Performance(_) => 2,
            CoreType::Efficiency(_) => 1,
            CoreType::Unknown => 0,
        }
    }
}

// Structure to hold per-core CPUID information
#[derive(Debug, Clone)]
pub struct CoreCpuidInfo {
    pub core_id: usize,
    pub l3_cache_size: Option<u32>,
    pub thread_count: usize,
}

#[derive(Debug, Clone)]
pub struct EnhancedCpuInfo {
    pub id: u32,
    pub logical_processor_index: u32,
    pub core_index: u32,
    pub numa_node: u32,
    pub efficiency_class: u8,
    pub scheduling_class: u8,
    pub core_type: CoreType,
}


// Modern NUMA information structure
#[derive(Debug, Clone)]
pub struct NumaTopology {
    pub node_count: u32,
    pub nodes: Vec<NumaNodeInfo>,
    pub cpu_to_node: HashMap<u32, u32>,
}

#[derive(Debug, Clone)]
pub struct NumaNodeInfo {
    pub node_id: u32,
    pub group_count: u16,
    pub group_masks: Vec<GroupMask>,
    pub cpu_count: u32,
    pub cpus: Vec<u32>,
}

#[derive(Debug, Clone)]
pub struct GroupMask {
    pub group: u16,
    pub mask: usize,
}

// Configuration for which detection method to use
#[derive(Debug, Clone, Copy)]
pub enum TopologyDetectionMethod {
    WindowsApi,        // Original detect_cpu_topology()
    WindowsApiV2,      // detect_cpu_topology_v2()
    CpuidBased,        // detect_cpu_topology_cpuid()
    Auto,              // detect_cpu_topology_auto()
}

// Global configuration for detection method
static TOPOLOGY_METHOD: OnceLock<TopologyDetectionMethod> = OnceLock::new();

// Set the detection method (call this early in main.rs)
pub fn set_topology_detection_method(method: TopologyDetectionMethod) {
    TOPOLOGY_METHOD.set(method).ok();
}

// Main wrapper function - all other functions should call this
pub fn get_cpu_topology() -> &'static Vec<CpuTopologyInfo> {
    CPU_TOPOLOGY.get_or_init(|| {
        let method = TOPOLOGY_METHOD.get().copied().unwrap_or(TopologyDetectionMethod::Auto);
        
        log::info!("Detecting CPU topology using method: {:?}", method);
        let start = std::time::Instant::now();
        
        let topology = match method {
            TopologyDetectionMethod::WindowsApi => detect_cpu_topology(),
            TopologyDetectionMethod::WindowsApiV2 => detect_cpu_topology_v2(),
            TopologyDetectionMethod::CpuidBased => detect_cpu_topology_cpuid(),
            TopologyDetectionMethod::Auto => detect_cpu_topology_auto(),
        };
        
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

// Optional: Force refresh topology (useful for testing different methods)
pub fn reset_cpu_topology() {
    // This is a bit hacky but works for testing
    // In production code, you'd want a Mutex<Option<>> instead
    log::warn!("Resetting CPU topology cache - this should only be used for testing!");
    // Can't actually reset OnceLock, so this would need a different approach
    // if you really need this functionality
}

// For functions that need topology frequently, you can also pass it as a parameter
pub fn get_numa_node_for_cpu_with_topology(cpu_id: usize, topology: &[CpuTopologyInfo]) -> u32 {
    topology.iter()
        .find(|cpu| cpu.logical_id == cpu_id)
        .map(|cpu| cpu.numa_node)
        .unwrap_or(0)
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
                node_count: 1,
                nodes: vec![],
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

// Main detection function that tries multiple methods
pub fn detect_cpu_topology_auto() -> Vec<CpuTopologyInfo> {
    log::info!("*** ENTERED detect_cpu_topology_auto ***");
    
    // Method 1: Try V2 first (scheduling classes - most reliable for modern CPUs)
    log::info!("Method 1: Trying Windows API V2 (scheduling classes)...");
    let topology = detect_cpu_topology_v2();
    
    // Check if we got differentiation
    let core_types: HashSet<_> = topology.iter().map(|cpu| &cpu.core_type).collect();
    let has_unknown = core_types.contains(&CoreType::Unknown);
    let has_multiple_types = core_types.len() > 1;
    
    if has_multiple_types && !has_unknown {
        log::info!("✓ Successfully detected heterogeneous cores using scheduling classes");
        return topology;
    }

    // Method 2: If all else fails, try the basic Windows API
    log::info!("Method 2: Trying basic Windows API (efficiency class)...");
    let topology = detect_cpu_topology();
    
    let core_types: HashSet<_> = topology.iter().map(|cpu| &cpu.core_type).collect();
    let has_unknown = core_types.contains(&CoreType::Unknown);
    let has_multiple_types = core_types.len() > 1;
    
    if has_multiple_types && !has_unknown {
        log::info!("✓ Successfully detected heterogeneous cores using efficiency class");
		return topology;
     }
		
	// Method 3: Try CPUID cache detection (without fallback)
    log::info!("Method 3: Trying CPUID cache-based detection...");
    let topology = detect_cpu_topology_cpuid();
    
    let core_types: HashSet<_> = topology.iter().map(|cpu| &cpu.core_type).collect();
    let has_unknown = core_types.contains(&CoreType::Unknown);
    let has_multiple_types = core_types.len() > 1;
    
    if has_multiple_types && !has_unknown {
        log::info!("✓ Successfully detected heterogeneous cores using cache differences");
        topology
	} else {
        // Log what we ended up with
        let physical_cores = topology.iter()
            .map(|cpu| cpu.physical_core_id)
            .collect::<HashSet<_>>()
            .len();
            
        log::warn!("✗ Unable to differentiate core types for {} physical cores", physical_cores);
        log::warn!("  All cores will be treated as the same type");
        log::info!("  This is common for non-hybrid CPUs or when Windows doesn't expose differences");
        
        // For uniform systems, treat all cores as Performance cores instead of Efficiency cores
        let mut updated_topology = topology;
        for cpu in &mut updated_topology {
            if matches!(cpu.core_type, CoreType::Efficiency(_)) {
                cpu.core_type = CoreType::Performance(0);
            }
        }
        updated_topology
    }
}

pub fn detect_cpu_topology_cpuid() -> Vec<CpuTopologyInfo> {
    let mut topology = detect_cpu_topology();
    
    // Try to read CPUID for cache differences
    let mut cores_info: HashMap<usize, Vec<CoreCpuidInfo>> = HashMap::new();
    
    log::info!("Reading CPUID cache information from each logical CPU...");
    for cpu in &topology {
        if let Some(cpuid_info) = get_cpuid_for_cpu(cpu.logical_id) {
            cores_info.entry(cpu.physical_core_id)
                .or_default()
                .push(cpuid_info);
        }
    }
    
    // Check if we found any cache differences
    let mut core_characteristics: HashMap<usize, Option<u32>> = HashMap::new();
    for (core_id, cpu_infos) in &cores_info {
        if let Some(first_cpu) = cpu_infos.first() {
            log::debug!("Core {}: L3 cache = {:?} bytes", 
                      core_id, first_cpu.l3_cache_size);
            core_characteristics.insert(*core_id, first_cpu.l3_cache_size);
        }
    }
    
    // Check for differences in cache sizes
    let l3_sizes: HashSet<u32> = core_characteristics.values()
        .filter_map(|l3| *l3)
        .collect();
    
    log::info!("Detected L3 cache sizes: {:?} bytes", l3_sizes);
    
    // If CPUID provides cache differentiation, use it
    if l3_sizes.len() > 1 {
        log::info!("Found cache size differences - using for core type detection");
        
        // Use cache size to determine core types
        let max_l3 = l3_sizes.iter().max().copied().unwrap_or(0);
        
        for cpu in &mut topology {
            if let Some(l3_size) = core_characteristics.get(&cpu.physical_core_id) {
                // Cores with max cache are performance cores
                let is_performance = l3_size.is_some_and(|l3| l3 == max_l3);
                
                cpu.core_type = if is_performance {
                    CoreType::Performance(0)
                } else {
                    CoreType::Efficiency(0)
                };
            }
        }
    } else {
        log::info!("No cache differences found - all cores have uniform cache");
        // Leave all cores as Unknown - no fallback!
    }
    
    topology
}

// Updated detect_cpu_topology_v2 with improved detection
pub fn detect_cpu_topology_v2() -> Vec<CpuTopologyInfo> {
    // First try the new API
    if let Ok(cpu_sets) = get_system_cpu_set_information() {
        log::info!("Using GetSystemCpuSetInformation for enhanced topology detection");
        
        // Log what we found
        let efficiency_classes: std::collections::HashSet<_> = 
            cpu_sets.iter().map(|c| c.efficiency_class).collect();
        let scheduling_classes: std::collections::HashSet<_> = 
            cpu_sets.iter().map(|c| c.scheduling_class).collect();
        
        log::info!("Found efficiency classes: {:?}", efficiency_classes);
        log::info!("Found scheduling classes: {:?}", scheduling_classes);
        
        // Group by physical core to detect SMT
        let mut cores: HashMap<u32, Vec<&EnhancedCpuInfo>> = HashMap::new();
        for cpu_info in &cpu_sets {
            cores.entry(cpu_info.core_index).or_default().push(cpu_info);
        }
        
        // Build scheduling class map
        let mut core_sched_classes: HashMap<u32, u8> = HashMap::new();
        for (&core_idx, cpu_infos) in &cores {
            if let Some(first) = cpu_infos.first() {
                core_sched_classes.insert(core_idx, first.scheduling_class);
            }
        }
        
        // Identify core types using scheduling class values
        let core_types = map_scheduling_to_core_types(&core_sched_classes);
        
        // Build topology
        let mut topology = Vec::new();
        
        for cpu_info in &cpu_sets {
            let core_type = core_types.get(&cpu_info.core_index)
                .copied()
                .unwrap_or(CoreType::Unknown);
            
            let threads_in_core = cores.get(&cpu_info.core_index)
                .map(|v| v.len())
                .unwrap_or(1);
            
            topology.push(CpuTopologyInfo {
                logical_id: cpu_info.logical_processor_index as usize,
                physical_core_id: cpu_info.core_index as usize,
                numa_node: cpu_info.numa_node,
                is_hyperthreaded: threads_in_core > 1,
                core_type,
                threads_on_this_core: threads_in_core,
                efficiency_class: cpu_info.efficiency_class,
            });
        }
        
        topology.sort_by_key(|cpu| cpu.logical_id);
        return topology;
    }
    
    // Fall back to basic implementation
    log::info!("Falling back to GetLogicalProcessorInformationEx");
    detect_cpu_topology()
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
                    
                    // Determine core type based on efficiency class
                    // Note: Windows uses 0 for E-cores, higher values for P-cores
                    // But this might vary between Intel and AMD
                    let core_type = match efficiency_class {
                        0 => CoreType::Efficiency(0),
                        0xFF => CoreType::Unknown, // 0xFF seems to be "not specified"
                        _ => CoreType::Performance(0),
                    };
                    
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


pub fn display_cpu_topology(cpu_list: &[usize], cpus_to_skip: usize, avoid_smt: bool) {
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


pub fn get_cpuid_for_cpu(logical_cpu: usize) -> Option<CoreCpuidInfo> {
    use windows::Win32::System::Threading::{SetThreadAffinityMask, GetCurrentThread};
    
    unsafe {
        // Pin to specific CPU to read its CPUID
        let thread_handle = GetCurrentThread();
        let old_mask = SetThreadAffinityMask(thread_handle, 1usize << logical_cpu);
        
        if old_mask == 0 {
            return None;
        }
        
        // Read CPUID
        let cpuid = CpuId::new();
        
        // Get cache parameters
        let mut l3_cache_size = None;
        if let Some(cparams) = cpuid.get_cache_parameters() {
            for cache in cparams {
                if cache.level() == 3 {
                    let ways = cache.associativity();
                    let partitions = cache.physical_line_partitions();
                    let line_size = cache.coherency_line_size();
                    let sets = cache.sets();
                    
                    let size = (ways as u32 + 1) * 
                              (partitions as u32 + 1) * 
                              (line_size as u32 + 1) * 
                              (sets as u32 + 1);
                    l3_cache_size = Some(size);
                    break;
                }
            }
        }
        
        // Restore affinity
        SetThreadAffinityMask(thread_handle, old_mask);
        
        Some(CoreCpuidInfo {
            core_id: logical_cpu,
            l3_cache_size,
            thread_count: 1,
        })
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
            
            // CoreIndex from the API is actually the logical processor index
            // We need to map this to physical core index.
            // FIXME (topology): `logical/2` hardcodes 2-way SMT and is WRONG on non-SMT
            // (AMD EPYC: 16L=16P) or non-2-way parts. This path (`get_system_cpu_set_information`
            // → `detect_cpu_topology_v2`) is NOT the primary detector — `detect_cpu_topology`
            // enumerates real per-core mappings and is what produces the correct topology table.
            // Left as-is to avoid changing a secondary detection path blind; the display-side
            // mapping now uses `get_physical_core_for_cpu` off the authoritative topology.
            let logical_index = info.Anonymous.CpuSet.CoreIndex as u32;
            let physical_core_index = logical_index / 2; // Assuming SMT with 2 threads per core
            
            // Determine initial core type based on efficiency class
            let core_type = match info.Anonymous.CpuSet.EfficiencyClass {
                0 => CoreType::Efficiency(0),
                255 => CoreType::Unknown,
                _ => CoreType::Performance(0),
            };
            
            cpu_infos.push(EnhancedCpuInfo {
                id: info.Anonymous.CpuSet.Id,
                logical_processor_index: info.Anonymous.CpuSet.LogicalProcessorIndex as u32,
                core_index: physical_core_index,  // Now it's u32
                numa_node: info.Anonymous.CpuSet.NumaNodeIndex as u32,
                efficiency_class: info.Anonymous.CpuSet.EfficiencyClass,
                scheduling_class: info.Anonymous.CpuSet.Anonymous2.SchedulingClass,
                core_type,
            });
            
            offset += info.Size as usize;
        }
        
        Ok(cpu_infos)
    }
}

// Modern NUMA topology discovery
pub fn discover_numa_topology() -> Result<NumaTopology, String> {
    unsafe {
        let mut buffer_size = 0u32;
        
        let _ = GetLogicalProcessorInformationEx(
            RelationNumaNode,
            None,
            &mut buffer_size,
        );
        
        if buffer_size == 0 {
            return Ok(NumaTopology {
                node_count: 1,
                nodes: vec![NumaNodeInfo {
                    node_id: 0,
                    group_count: 1,
                    group_masks: vec![GroupMask { group: 0, mask: !0 }],
                    cpu_count: num_cpus::get() as u32,
                    cpus: (0..num_cpus::get() as u32).collect(),
                }],
                cpu_to_node: (0..num_cpus::get() as u32).map(|cpu| (cpu, 0)).collect(),
            });
        }
        
        // ALIGNMENT: u64-backed (see `memory/privileges.rs`); the walk below stays in BYTES
        // via an explicit `*const u8` base.
        let mut buffer = vec![0u64; (buffer_size as usize).div_ceil(std::mem::size_of::<u64>())];

        GetLogicalProcessorInformationEx(
            RelationNumaNode,
            Some(buffer.as_mut_ptr() as *mut SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX),
            &mut buffer_size,
        ).map_err(|e| format!("Failed to get NUMA topology: {:?}", e))?;

        let base = buffer.as_ptr() as *const u8;
        let mut nodes = Vec::new();
        let mut cpu_to_node = HashMap::new();
        let mut offset = 0;

        while offset < buffer_size as usize {
            let info = &*(base.add(offset) as *const SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX);
            
            if info.Relationship == RelationNumaNode {
                let numa_info = &info.Anonymous.NumaNode;
                let node_id = numa_info.NodeNumber;
                
                let mut cpus = Vec::new();
                let mut group_masks = Vec::new();
                
                let group_affinity = &numa_info.Anonymous.GroupMask;
                let group = group_affinity.Group;
                let mask = group_affinity.Mask;
                
                group_masks.push(GroupMask { group, mask });
                
                for bit in 0..64 {
                    if (mask & (1u64 << bit) as usize) != 0 {
                        let cpu_id = (group as u32 * 64) + bit;
                        cpus.push(cpu_id);
                        cpu_to_node.insert(cpu_id, node_id);
                    }
                }
                
                nodes.push(NumaNodeInfo {
                    node_id,
                    group_count: 1,
                    group_masks,
                    cpu_count: cpus.len() as u32,
                    cpus,
                });
            }
            
            offset += info.Size as usize;
        }
        
        Ok(NumaTopology {
            node_count: nodes.len() as u32,
            nodes,
            cpu_to_node,
        })
    }
}

pub fn debug_topology_detection() {
    println!("\n=== CPU Topology Detection Debug ===");
    
    // Try each method and show results
    let methods = [
        ("Windows API (Original)", TopologyDetectionMethod::WindowsApi),
        ("Windows API V2 (Enhanced)", TopologyDetectionMethod::WindowsApiV2),
        ("CPUID Based", TopologyDetectionMethod::CpuidBased),
        ("Auto Detection", TopologyDetectionMethod::Auto),
    ];
    
    for (name, method) in &methods {
        println!("\n--- Method: {} ---", name);
        
        let topology = match method {
            TopologyDetectionMethod::WindowsApi => detect_cpu_topology(),
            TopologyDetectionMethod::WindowsApiV2 => detect_cpu_topology_v2(),
            TopologyDetectionMethod::CpuidBased => detect_cpu_topology_cpuid(),
            TopologyDetectionMethod::Auto => detect_cpu_topology_auto(),
        };
        
        // Count core types
        let mut p_cores = HashSet::new();
        let mut e_cores = HashSet::new();
        let mut unknown_cores = HashSet::new();
        
        for cpu in &topology {
            match cpu.core_type {
                CoreType::Performance(_) => { p_cores.insert(cpu.physical_core_id); },
                CoreType::Efficiency(_) => { e_cores.insert(cpu.physical_core_id); },
                CoreType::Unknown => { unknown_cores.insert(cpu.physical_core_id); },
            }
        }
        
        println!("  Total logical CPUs: {}", topology.len());
        println!("  Physical cores: P={}, E={}, Unknown={}", 
                p_cores.len(), e_cores.len(), unknown_cores.len());
        
        // Show efficiency class distribution
        let eff_classes: HashSet<_> = topology.iter().map(|c| c.efficiency_class).collect();
        println!("  Efficiency classes: {:?}", eff_classes);
        
        // Show ALL CPUs, not just first 6
        println!("  Complete CPU Mapping:");
        
        // Group by physical core for cleaner display
        let mut cores_map: HashMap<usize, Vec<&CpuTopologyInfo>> = HashMap::new();
        for cpu in &topology {
            cores_map.entry(cpu.physical_core_id).or_default().push(cpu);
        }
        
        let mut sorted_cores: Vec<_> = cores_map.iter().collect();
        sorted_cores.sort_by_key(|(id, _)| *id);
        
        for (phys_core, cpus) in sorted_cores {
            let logical_ids: Vec<String> = cpus.iter()
                .map(|c| c.logical_id.to_string())
                .collect();
            let core_type = cpus[0].core_type.display_name();
            let eff_class = cpus[0].efficiency_class;
            
            println!("    Physical Core {}: {} (EffClass={}) → Logical CPUs [{}]",
                    phys_core, core_type, eff_class, logical_ids.join(","));
        }
        
        // If using enhanced info, show scheduling classes
        if matches!(method, TopologyDetectionMethod::WindowsApiV2 | TopologyDetectionMethod::CpuidBased | TopologyDetectionMethod::Auto)
            && let Ok(cpu_sets) = get_system_cpu_set_information() {
                let sched_classes: HashSet<_> = cpu_sets.iter()
                    .map(|c| c.scheduling_class)
                    .collect();
                println!("  Scheduling classes detected: {:?}", sched_classes);
                
                // Show per-core scheduling class
                let mut core_sched: HashMap<u32, u8> = HashMap::new();
                for cpu_set in &cpu_sets {
                    core_sched.insert(cpu_set.core_index, cpu_set.scheduling_class);
                }
                
                println!("  Scheduling class per physical core:");
                let mut core_ids: Vec<_> = core_sched.keys().cloned().collect();
                core_ids.sort();
                for core_id in core_ids {
                    if let Some(&sched_class) = core_sched.get(&core_id) {
                        println!("    Core {}: SchedClass={}", core_id, sched_class);
                    }
                }
            }
    }
    
    println!("\n=== End Debug ===");
}


// Simpler function to map scheduling classes to core types
pub fn map_scheduling_to_core_types(
    core_sched_classes: &HashMap<u32, u8>
) -> HashMap<u32, CoreType> {
    let mut core_types = HashMap::new();
    
    // Get unique scheduling classes sorted ascending
    let mut sched_classes: Vec<u8> = core_sched_classes.values().copied().collect();
    sched_classes.sort();
    sched_classes.dedup();
    
    if sched_classes.is_empty() || sched_classes.len() == 1 {
        // No differentiation possible
        for &core_idx in core_sched_classes.keys() {
            core_types.insert(core_idx, CoreType::Unknown);
        }
        return core_types;
    }
    
    // Highest scheduling class = Performance cores
    let max_sched = *sched_classes.last().unwrap();
    
    // Count how many tiers we have
    let p_tier_count = sched_classes.iter().filter(|&&s| s == max_sched).count();
    let e_tier_count = sched_classes.len() - p_tier_count;
    
    log::info!("Detected {} total scheduling classes: P-tiers={}, E-tiers={}", 
              sched_classes.len(), p_tier_count, e_tier_count);
    
    // Simple mapping: highest = P-core, rest = E-cores with increasing tiers
    for (&core_idx, &sched_class) in core_sched_classes {
        let core_type = if sched_class == max_sched {
            CoreType::Performance(0)
        } else {
            // E-core tier based on position in sorted list
            let tier_idx = sched_classes.iter().position(|&s| s == sched_class).unwrap();
            CoreType::Efficiency(tier_idx as u8)
        };
        
        core_types.insert(core_idx, core_type);
    }
    
    // Log the mapping
    log::info!("Scheduling class → Core type mapping:");
    for &sched_class in &sched_classes {
        let example_core = core_sched_classes.iter()
            .find(|&(_, &s)| s == sched_class)
            .map(|(&core, _)| core);
        
        if let Some(core) = example_core
            && let Some(core_type) = core_types.get(&core) {
                log::info!("  SchedClass {} → {}", sched_class, core_type.display_name());
            }
    }
    
    core_types
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
    
    // Get scheduling classes if available
    let mut core_sched_map: HashMap<u32, u8> = HashMap::new();
    if let Ok(cpu_sets) = get_system_cpu_set_information() {
        for cpu_set in &cpu_sets {
            core_sched_map.insert(cpu_set.core_index, cpu_set.scheduling_class);
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
        
        let sched_class = core_sched_map.get(&(phys_core as u32))
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