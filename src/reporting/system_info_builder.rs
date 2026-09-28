/// System info report builder - converts TMR's internal system info to reporting format
use crate::reporting::models::{SystemInfoReport, CpuInfo, CacheInfo, TscCalibrationInfo};
use crate::cache::SystemInfo;

/// Build a SystemInfoReport from TMR's internal system info
pub fn build_system_info_report(system_info: &SystemInfo, simd_caps: &str) -> SystemInfoReport {
    let cache = system_info.get_cache_info();

    // Build TSC calibration info
    let tsc = &cache.tsc_info;
    let (samples, calibration_time_ms, std_dev_ghz, converged) =
        if let Some(ref stats) = tsc.calibration_stats {
            (stats.samples, stats.calibration_time_ms, stats.std_dev_ghz(), stats.converged)
        } else {
            (0, 0, 0.0, false)
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
        tsc_info: TscCalibrationInfo {
            frequency_ghz: tsc.frequency_ghz,
            detection_method: tsc.detection_method.to_string(),
            is_invariant: tsc.is_invariant,
            confidence_percent: tsc.confidence * 100.0,
            samples,
            calibration_time_ms,
            std_dev_ghz,
            converged,
        },
    }
}