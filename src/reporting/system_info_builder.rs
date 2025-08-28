/// System info report builder - converts TMR's internal system info to reporting format
/// Reuses cached memory info to avoid duplicate Windows API calls
use crate::reporting::models::{SystemInfoReport, CpuInfo, CacheInfo, MemoryInfo, TopologyInfo, NumaNodeInfo};
use crate::cache::SystemInfo;
use crate::cpu_topology::CpuTopologyInfo;
use crate::memory::allocation_strategy::SystemMemoryInfo;

/// Build a SystemInfoReport from TMR's internal system info and cached memory info
pub fn build_system_info_report(
    system_info: &SystemInfo,
    cached_memory_info: &SystemMemoryInfo,
    topology: &[CpuTopologyInfo],
    simd_caps: &str,
    assigned_cpus: &[usize]
) -> SystemInfoReport {
    let cache = system_info.get_cache_info();
    
    // Use cached memory info instead of calling Windows APIs again
    let large_pages_available = crate::memory::check_large_page_privilege().is_ok();
    let huge_pages_available = crate::driver::get_global_driver_handle().is_ok();
    
    // Build NUMA nodes info
    let numa_nodes = if topology.is_empty() {
        vec![NumaNodeInfo {
            node_id: 0,
            cpu_count: assigned_cpus.len(),
            memory_bytes: cached_memory_info.total_installed_bytes,
        }]
    } else {
        // Group by NUMA node
        let mut nodes = std::collections::HashMap::new();
        for cpu in topology {
            let entry = nodes.entry(cpu.numa_node as usize).or_insert((0, 0u64));
            entry.0 += 1; // CPU count
            entry.1 = cached_memory_info.total_installed_bytes; // Memory (simplified)
        }
        
        nodes.into_iter()
            .map(|(node_id, (cpu_count, memory_bytes))| NumaNodeInfo {
                node_id,
                cpu_count,
                memory_bytes,
            })
            .collect()
    };
    
    // Determine core types
    let (is_hybrid, p_cores, e_cores) = if topology.is_empty() {
        (false, system_info.physical_cores, 0)
    } else {
        let hybrid = crate::cpu_topology::is_hybrid_cpu(topology);
        let p_count = topology.iter().filter(|cpu| matches!(cpu.core_type, crate::cpu_topology::CoreType::Performance(_))).count();
        let e_count = topology.iter().filter(|cpu| matches!(cpu.core_type, crate::cpu_topology::CoreType::Efficiency(_))).count();
        (hybrid, p_count, e_count)
    };
    
    SystemInfoReport {
        cpu_info: CpuInfo {
            brand: system_info.cpu_brand.clone(),
            vendor: system_info.cpu_vendor.clone(),
            family: system_info.cpu_family,
            model: system_info.cpu_model,
            stepping: system_info.cpu_stepping,
            physical_cores: system_info.physical_cores,
            logical_cores: system_info.logical_cores,
            has_hyperthreading: system_info.has_hyperthreading,
            simd_capabilities: simd_caps.split(", ").map(|s| s.to_string()).collect(),
        },
        cache_info: CacheInfo {
            l1_data_total: cache.l1_data_cache as u64,
            l1_instruction_total: cache.l1_instruction_cache as u64,
            l2_total: cache.l2_cache as u64,
            l3_total: cache.l3_cache as u64,
            per_core_l1d: cache.per_core_l1d as u64,
            per_core_l1i: cache.per_core_l1i as u64,
            per_core_l2: cache.per_core_l2 as u64,
            cache_line_size: cache.cache_line_size as u64,
            detection_method: cache.detection_method.clone(),
        },
        memory_info: MemoryInfo {
            total_installed: cached_memory_info.total_installed_bytes,
            total_physical: cached_memory_info.total_physical_bytes,
            available_physical: cached_memory_info.available_physical_bytes,
            large_pages_available,
            huge_pages_available,
            numa_nodes: numa_nodes.len(),
        },
        topology_info: TopologyInfo {
            is_hybrid,
            p_cores,
            e_cores,
            threads_per_core: if system_info.has_hyperthreading { 2 } else { 1 },
            numa_nodes,
            assigned_cpus: assigned_cpus.to_vec(),
            detection_method: "Auto".to_string(),
        },
    }
}