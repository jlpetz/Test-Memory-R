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
        }
    }
}

impl CacheInfo {
    pub fn detect() -> Self {
        // Try cache-size crate first (most reliable)
        if let Some(cache_info) = detect_via_cache_size_crate() {
            return cache_info;
        }

        // Fallback to raw_cpuid for detailed detection
        if let Some(cache_info) = detect_via_raw_cpuid() {
            return cache_info;
        }

        // Use defaults as last resort
        log::warn!("Could not detect cache sizes using either cache-size or raw_cpuid, using defaults");
        Self::default()
    }

    pub fn print_info(&self) {
        log::info!("Cache Architecture Detected ({})", self.detection_method);
        log::info!("  L1 Data Cache: {:.1} KB", self.l1_data_cache as f64 / 1024.0);
        log::info!("  L1 Instruction Cache: {:.1} KB", self.l1_instruction_cache as f64 / 1024.0);
        log::info!("  L2 Cache: {:.1} KB", self.l2_cache as f64 / 1024.0);
        log::info!("  L3 Cache: {:.1} MB", self.l3_cache as f64 / (1024.0 * 1024.0));
        log::info!("  Total Cache: {:.1} MB", self.total_cache as f64 / (1024.0 * 1024.0));
        log::info!("  Cache Line Size: {} bytes", self.cache_line_size);
        log::info!("  For DDR memory testing, windows should exceed {:.1} MB to avoid cache", 
                  self.total_cache as f64 / (1024.0 * 1024.0));
    }

    // Calculate total cache size
    fn calculate_total_cache(&mut self) {
        self.total_cache = self.l1_data_cache + self.l1_instruction_cache + self.l2_cache + self.l3_cache;
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

// Detection using cache-size crate (primary method)
fn detect_via_cache_size_crate() -> Option<CacheInfo> {
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

    let mut cache_info = CacheInfo {
        l1_data_cache: l1_data.unwrap_or(32 * 1024),
        l1_instruction_cache: l1_data.unwrap_or(32 * 1024), // cache-size doesn't separate I/D cache
        l2_cache: l2_size.unwrap_or(256 * 1024),
        l3_cache: l3_size.unwrap_or(8 * 1024 * 1024),
        cache_line_size: line_size.unwrap_or(64),
        total_cache: 0, // Will be calculated
        detection_method: "cache-size crate".to_string(),
    };

    cache_info.calculate_total_cache();

    log::debug!("cache-size detection: L1D={}KB, L2={}KB, L3={}MB, Line={}B", 
               cache_info.l1_data_cache / 1024,
               cache_info.l2_cache / 1024,
               cache_info.l3_cache / (1024 * 1024),
               cache_info.cache_line_size);

    Some(cache_info)
}

// Detection using raw_cpuid (fallback method with more detail)
fn detect_via_raw_cpuid() -> Option<CacheInfo> {
    let cpuid = CpuId::new();
    
    let mut cache_info = CacheInfo::default();
    cache_info.detection_method = "raw_cpuid detailed detection".to_string();
    
    // Try deterministic cache parameters (CPUID leaf 4) - works for both Intel and AMD
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
                    cache_info.l1_data_cache = size;
                }
                (1, raw_cpuid::CacheType::Instruction) => {
                    cache_info.l1_instruction_cache = size;
                }
                (2, raw_cpuid::CacheType::Unified) => {
                    cache_info.l2_cache = size;
                }
                (3, raw_cpuid::CacheType::Unified) => {
                    cache_info.l3_cache = size;
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
        
        cache_info.calculate_total_cache();
        
        // Validate we got reasonable values
        if cache_info.l1_data_cache > 4096 && cache_info.l2_cache > cache_info.l1_data_cache {
            log::debug!("raw_cpuid detection successful: Total cache = {}MB", 
                       cache_info.total_cache / (1024 * 1024));
            return Some(cache_info);
        }
    }

    // Try AMD-specific extended cache info as fallback
    if let Some(extended_cache) = cpuid.get_l2_l3_cache_and_tlb_info() {
        log::debug!("raw_cpuid: Trying AMD extended cache detection");
        
        if extended_cache.l2cache_size() > 0 {
            cache_info.l2_cache = (extended_cache.l2cache_size() as usize) * 1024;
        }
        if extended_cache.l3cache_size() > 0 {
            cache_info.l3_cache = (extended_cache.l3cache_size() as usize) * 1024 * 512; // AMD reports in 512KB units
        }
        
        cache_info.calculate_total_cache();
        
        if cache_info.l2_cache > 0 || cache_info.l3_cache > 0 {
            cache_info.detection_method = "raw_cpuid AMD extended".to_string();
            return Some(cache_info);
        }
    }

    log::debug!("raw_cpuid detection failed");
    None
}