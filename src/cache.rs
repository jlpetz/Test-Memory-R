use cache_size::*;
use raw_cpuid::CpuId;

#[derive(Debug, Clone)]
pub struct CacheInfo {
    pub l1_data_cache: usize,
    pub l1_instruction_cache: usize,
    pub l2_cache: usize,
    pub l3_cache: usize,
    pub cache_line_size: usize,
    pub total_cache: usize,
    pub detection_method: String,
    pub per_core_l1d: usize,    // L1D cache per core
    pub per_core_l1i: usize,    // L1I cache per core
    pub per_core_l2: usize,     // L2 cache per core
    pub core_count: usize,      // Number of cores for cache calculations
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
            l1_data_cache: 32 * 1024,      // 32KB
            l1_instruction_cache: 32 * 1024, // 32KB
            l2_cache: 256 * 1024,          // 256KB
            l3_cache: 8 * 1024 * 1024,     // 8MB
            cache_line_size: 64,           // 64 bytes
            total_cache: (32 + 32) * 1024 + 256 * 1024 + 8 * 1024 * 1024,
            detection_method: "Default fallback values".to_string(),
            per_core_l1d: 32 * 1024,
            per_core_l1i: 32 * 1024,
            per_core_l2: 256 * 1024,
            core_count: 1,
        }
    }
}

impl CacheInfo {
    pub fn detect() -> Self {
        let physical_cores = num_cpus::get_physical();
        
        // Try cache-size crate first (most reliable for total cache sizes)
        if let Some(cache_info) = detect_via_cache_size_crate(physical_cores) {
            return cache_info;
        }

        // Fallback to raw_cpuid for detailed per-core detection
        if let Some(cache_info) = detect_via_raw_cpuid(physical_cores) {
            return cache_info;
        }

        // Use defaults as last resort
        log::warn!("Could not detect cache sizes using either cache-size or raw_cpuid, using defaults");
        let mut default = Self::default();
        default.core_count = physical_cores;
        default.recalculate_totals();
        default
    }

    fn recalculate_totals(&mut self) {
        // Calculate total cache based on per-core values and core count
        self.l1_data_cache = self.per_core_l1d * self.core_count;
        self.l1_instruction_cache = self.per_core_l1i * self.core_count;
        self.l2_cache = self.per_core_l2 * self.core_count;
        // L3 is usually shared, so don't multiply by core count
        
        self.total_cache = self.l1_data_cache + self.l1_instruction_cache + self.l2_cache + self.l3_cache;
    }

    pub fn print_info(&self) {
        log::info!("Cache Architecture Detected ({})", self.detection_method);
        log::info!("  Cores: {} physical", self.core_count);
        log::info!("  L1 Data Cache: {:.1} KB total ({:.1} KB × {} cores)", 
                  self.l1_data_cache as f64 / 1024.0,
                  self.per_core_l1d as f64 / 1024.0,
                  self.core_count);
        log::info!("  L1 Instruction Cache: {:.1} KB total ({:.1} KB × {} cores)", 
                  self.l1_instruction_cache as f64 / 1024.0,
                  self.per_core_l1i as f64 / 1024.0,
                  self.core_count);
        log::info!("  L2 Cache: {:.1} KB total ({:.1} KB × {} cores)", 
                  self.l2_cache as f64 / 1024.0,
                  self.per_core_l2 as f64 / 1024.0,
                  self.core_count);
        log::info!("  L3 Cache: {:.1} MB (shared)", self.l3_cache as f64 / (1024.0 * 1024.0));
        log::info!("  Total Cache: {:.1} MB", self.total_cache as f64 / (1024.0 * 1024.0));
        log::info!("  Cache Line Size: {} bytes", self.cache_line_size);
        log::info!("  For DDR memory testing, windows should exceed {:.1} MB to avoid cache", 
                  self.total_cache as f64 / (1024.0 * 1024.0));
    }

    // Get the effective cache size for memory testing windows
    pub fn get_optimal_window_size(&self, test_type: &str) -> usize {
        match test_type {
            // Cache-focused tests should work within L3 but larger than L2
            "CacheBusting" => (self.l3_cache / 2).max(self.l2_cache * 4),
            
            // Random access tests benefit from larger windows
            "RandomTorture" => self.l3_cache * 2,
            
            // Memory bandwidth tests should exceed all cache levels significantly
            "BandwidthSat" => (self.total_cache * 2).max(64 * 1024 * 1024),
            
            // SIMD tests work well with L3-sized windows
            "MirrorMove128NonTemporal" | "MirrorMove256NonTemporal" | "MirrorMove512NonTemporal" => {
                self.l3_cache.max(32 * 1024 * 1024)
            }
            
            // General tests use total cache as baseline but ensure reasonable minimum
            _ => (self.total_cache * 2).max(64 * 1024 * 1024),
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
            (
                feature_info.family_id().into(),
                feature_info.model_id().into(),
                feature_info.stepping_id().into(),
            )
        } else {
            (0, 0, 0)
        };

        // Get core count information
        let physical_cores = num_cpus::get_physical();
        let logical_cores = num_cpus::get();
        let has_hyperthreading = logical_cores > physical_cores;

        let cache_info = CacheInfo::detect();

        Self {
            cpu_vendor,
            cpu_brand,
            cpu_family,
            cpu_model,
            cpu_stepping,
            physical_cores,
            logical_cores,
            has_hyperthreading,
            cache_info,
        }
    }

    pub fn print_system_info(&self) {
        log::info!("System Information:");
        log::info!("  CPU: {} ({})", self.cpu_brand, self.cpu_vendor);
        log::info!("  Family: {}, Model: {}, Stepping: {}", 
                  self.cpu_family, self.cpu_model, self.cpu_stepping);
        log::info!("  Cores: {} physical, {} logical{}", 
                  self.physical_cores, 
                  self.logical_cores,
                  if self.has_hyperthreading { " (Hyperthreading enabled)" } else { "" });
        
        // Print cache info
        self.cache_info.print_info();
    }

    pub fn get_cache_info(&self) -> &CacheInfo {
        &self.cache_info
    }
}

// Detection using cache-size crate (primary method) - enhanced for per-core detection
fn detect_via_cache_size_crate(physical_cores: usize) -> Option<CacheInfo> {
    // Try to get all cache levels
    let l1_data = l1_cache_size();
    let l2_size = l2_cache_size(); 
    let l3_size = l3_cache_size();
    let line_size = l1_cache_line_size();

    // Check if we got at least some cache information
    if l1_data.is_none() && l2_size.is_none() && l3_size.is_none() {
        log::debug!("cache-size crate failed to detect any cache levels");
        return None;
    }

    // For AMD systems, cache-size often returns total cache across all cores
    // We need to detect if values need per-core adjustment
    let raw_l1d = l1_data.unwrap_or(32 * 1024);
    let raw_l2 = l2_size.unwrap_or(256 * 1024);
    let raw_l3 = l3_size.unwrap_or(0); // L3 often fails with cache-size crate

    // Heuristic: if L1D or L2 values seem too large for a single core, they're likely totals
    let per_core_l1d = if raw_l1d > 128 * 1024 && physical_cores > 1 {
        // Likely total across cores, divide by core count
        raw_l1d / physical_cores
    } else {
        raw_l1d
    };

    let per_core_l2 = if raw_l2 > 2 * 1024 * 1024 && physical_cores > 1 {
        // Likely total across cores, divide by core count
        raw_l2 / physical_cores
    } else {
        raw_l2
    };

    // If L3 cache detection failed, try to estimate based on CPU model
    let l3_cache = if raw_l3 == 0 {
        estimate_l3_cache_from_cpu_info(physical_cores)
    } else {
        raw_l3
    };

    let mut cache_info = CacheInfo {
        per_core_l1d,
        per_core_l1i: per_core_l1d, // cache-size doesn't separate I/D cache
        per_core_l2,
        l3_cache,
        cache_line_size: line_size.unwrap_or(64),
        core_count: physical_cores,
        detection_method: if raw_l3 == 0 {
            "cache-size crate with per-core adjustment + L3 estimation".to_string()
        } else {
            "cache-size crate with per-core adjustment".to_string()
        },
        // These will be calculated by recalculate_totals()
        l1_data_cache: 0,
        l1_instruction_cache: 0,
        l2_cache: 0,
        total_cache: 0,
    };

    cache_info.recalculate_totals();

    log::debug!("cache-size detection (adjusted): L1D={}KB×{}, L2={}KB×{}, L3={}MB{}, Line={}B", 
               cache_info.per_core_l1d / 1024,
               cache_info.core_count,
               cache_info.per_core_l2 / 1024,
               cache_info.core_count,
               cache_info.l3_cache / (1024 * 1024),
               if raw_l3 == 0 { " (estimated)" } else { "" },
               cache_info.cache_line_size);

    Some(cache_info)
}

// Estimate L3 cache size based on CPU information when detection fails
fn estimate_l3_cache_from_cpu_info(physical_cores: usize) -> usize {
    let cpuid = CpuId::new();
    
    // Try to get CPU brand string for pattern matching
    if let Some(brand_info) = cpuid.get_processor_brand_string() {
        let brand = brand_info.as_str().to_lowercase();
        
        // AMD Ryzen patterns
        if brand.contains("8500g") {
            return 16 * 1024 * 1024; // 16MB for 8500G
        }
        
        if brand.contains("ryzen") {
            if brand.contains("7000") || brand.contains("8000") {
                // Modern Ryzen APUs typically have 16MB L3
                return 16 * 1024 * 1024;
            } else if brand.contains("5000") || brand.contains("6000") {
                // Zen 3/3+ typically 16-32MB depending on core count
                return if physical_cores >= 8 { 32 * 1024 * 1024 } else { 16 * 1024 * 1024 };
            } else if brand.contains("3000") || brand.contains("4000") {
                // Zen 2 typically 16-32MB
                return if physical_cores >= 8 { 32 * 1024 * 1024 } else { 16 * 1024 * 1024 };
            }
        }
        
        // Intel patterns
        if brand.contains("intel") {
            if physical_cores >= 8 {
                return 24 * 1024 * 1024; // 24MB for high-end Intel
            } else if physical_cores >= 4 {
                return 12 * 1024 * 1024; // 12MB for mid-range
            } else {
                return 8 * 1024 * 1024;  // 8MB for low-end
            }
        }
    }
    
    // Generic fallback based on core count
    match physical_cores {
        1..=2 => 4 * 1024 * 1024,   // 4MB
        3..=4 => 8 * 1024 * 1024,   // 8MB  
        5..=6 => 16 * 1024 * 1024,  // 16MB
        7..=8 => 24 * 1024 * 1024,  // 24MB
        _ => 32 * 1024 * 1024,      // 32MB for 9+ cores
    }
}

// Detection using raw_cpuid (fallback method with more detail) - enhanced for AMD detection
fn detect_via_raw_cpuid(physical_cores: usize) -> Option<CacheInfo> {
    let cpuid = CpuId::new();
    
    let mut cache_info = CacheInfo {
        per_core_l1d: 32 * 1024,
        per_core_l1i: 32 * 1024,
        per_core_l2: 256 * 1024,
        l3_cache: 8 * 1024 * 1024,
        cache_line_size: 64,
        core_count: physical_cores,
        detection_method: "raw_cpuid detailed detection".to_string(),
        l1_data_cache: 0,
        l1_instruction_cache: 0,
        l2_cache: 0,
        total_cache: 0,
    };
    
    let mut found_cache_info = false;
    
    // Try deterministic cache parameters (CPUID leaf 4) - works for Intel and modern AMD
    if let Some(cache_params) = cpuid.get_cache_parameters() {
        for cache in cache_params {
            let size = cache.associativity() * 
                      cache.physical_line_partitions() * 
                      cache.coherency_line_size() * 
                      cache.sets();
            
            let cache_type = cache.cache_type();
            let level = cache.level();
            
            log::debug!("raw_cpuid: Found L{} cache, type={:?}, size={}KB", 
                       level, &cache_type, size / 1024);
            
            match (level, &cache_type) {
                (1, raw_cpuid::CacheType::Data) => {
                    cache_info.per_core_l1d = size;
                    found_cache_info = true;
                }
                (1, raw_cpuid::CacheType::Instruction) => {
                    cache_info.per_core_l1i = size;
                    found_cache_info = true;
                }
                (2, raw_cpuid::CacheType::Unified) => {
                    cache_info.per_core_l2 = size;
                    found_cache_info = true;
                }
                (3, raw_cpuid::CacheType::Unified) => {
                    cache_info.l3_cache = size;
                    found_cache_info = true;
                }
                _ => {
                    log::debug!("raw_cpuid: Ignoring cache level {} type {:?}", level, &cache_type);
                }
            }
            
            // Get cache line size from any cache level
            let line_size = cache.coherency_line_size();
            if line_size > 0 && line_size <= 128 {
                cache_info.cache_line_size = line_size;
            }
        }
    }

    // Try AMD-specific extended cache info for older AMD CPUs
    if !found_cache_info {
        if let Some(extended_cache) = cpuid.get_l2_l3_cache_and_tlb_info() {
            log::debug!("raw_cpuid: Trying AMD extended cache detection");
            
            if extended_cache.l2cache_size() > 0 {
                cache_info.per_core_l2 = (extended_cache.l2cache_size() as usize) * 1024;
                found_cache_info = true;
            }
            if extended_cache.l3cache_size() > 0 {
                cache_info.l3_cache = (extended_cache.l3cache_size() as usize) * 1024 * 512; // AMD reports in 512KB units
                found_cache_info = true;
            }
            
            if found_cache_info {
                cache_info.detection_method = "raw_cpuid AMD extended".to_string();
            }
        }
    }

    if found_cache_info {
        cache_info.recalculate_totals();
        
        log::debug!("raw_cpuid detection successful: L1D={}KB×{}, L2={}KB×{}, L3={}MB", 
                   cache_info.per_core_l1d / 1024, cache_info.core_count,
                   cache_info.per_core_l2 / 1024, cache_info.core_count,
                   cache_info.l3_cache / (1024 * 1024));
        Some(cache_info)
    } else {
        log::debug!("raw_cpuid detection failed");
        None
    }
}