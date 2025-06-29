use std::arch::x86_64::*;

#[derive(Debug, Clone)]
pub struct CacheInfo {
    pub l1_data_cache: usize,
    pub l1_instruction_cache: usize,
    pub l2_cache: usize,
    pub l3_cache: usize,
    pub cache_line_size: usize,
    pub detection_method: String,
}

impl Default for CacheInfo {
    fn default() -> Self {
        // Fallback values if detection fails
        Self {
            l1_data_cache: 32 * 1024,      // 32KB
            l1_instruction_cache: 32 * 1024, // 32KB
            l2_cache: 256 * 1024,          // 256KB
            l3_cache: 8 * 1024 * 1024,     // 8MB (conservative)
            cache_line_size: 64,           // 64 bytes
            detection_method: "Default fallback values".to_string(),
        }
    }
}

impl CacheInfo {
    pub fn detect() -> Self {
        // Try CPUID detection first (most accurate)
        if let Some(cache_info) = detect_cache_via_cpuid() {
            return cache_info;
        }

        // Fallback to empirical detection
        if let Some(cache_info) = detect_cache_empirically() {
            return cache_info;
        }

        // Use defaults as last resort
        log::warn!("Could not detect cache sizes, using conservative defaults");
        Self::default()
    }

    pub fn print_info(&self) {
        log::info!("Cache Architecture Detected ({})", self.detection_method);
        log::info!("  L1 Data Cache: {:.1} KB", self.l1_data_cache as f64 / 1024.0);
        log::info!("  L1 Instruction Cache: {:.1} KB", self.l1_instruction_cache as f64 / 1024.0);
        log::info!("  L2 Cache: {:.1} KB", self.l2_cache as f64 / 1024.0);
        log::info!("  L3 Cache: {:.1} MB", self.l3_cache as f64 / (1024.0 * 1024.0));
        log::info!("  Cache Line Size: {} bytes", self.cache_line_size);
    }

    // Get the effective cache size for memory testing windows
    pub fn get_optimal_window_size(&self, test_type: &str) -> usize {
        match test_type {
            // Cache-focused tests should work within L3 but larger than L2
            "CacheBusting" => (self.l3_cache / 2).max(self.l2_cache * 4),
            
            // Random access tests benefit from larger windows
            "RandomTorture" => self.l3_cache * 2,
            
            // Memory bandwidth tests should exceed all cache levels
            "BandwidthSat" => self.l3_cache * 4,
            
            // SIMD tests work well with L3-sized windows
            "MirrorMove128NonTemporal" | "MirrorMove256NonTemporal" | "MirrorMove512NonTemporal" => {
                self.l3_cache
            }
            
            // General tests use L3 as baseline
            _ => self.l3_cache.max(64 * 1024 * 1024), // At least 64MB for safety
        }
    }
}

// CPUID-based cache detection (most accurate)
fn detect_cache_via_cpuid() -> Option<CacheInfo> {
    if !is_x86_feature_detected!("sse") {
        return None;
    }

    unsafe {
        // Check if we have CPUID with cache info support
        let cpuid_result = __cpuid(0);
        if cpuid_result.eax < 4 {
            return None;
        }

        let mut cache_info = CacheInfo::default();
        cache_info.detection_method = "CPUID instruction".to_string();

        // Intel/AMD cache detection via CPUID leaf 4 (Intel) or 0x8000001D (AMD)
        if let Some(info) = detect_intel_cache_cpuid() {
            cache_info = info;
        } else if let Some(info) = detect_amd_cache_cpuid() {
            cache_info = info;
        } else {
            // Try legacy CPUID leaf 2 (Intel)
            if let Some(info) = detect_legacy_intel_cache() {
                cache_info = info;
            } else {
                return None;
            }
        }

        // Detect cache line size via CPUID leaf 1
        let cpuid_1 = __cpuid(1);
        let cache_line_size = ((cpuid_1.ebx >> 8) & 0xFF) * 8;
        if cache_line_size > 0 && cache_line_size <= 128 {
            cache_info.cache_line_size = cache_line_size as usize;
        }

        Some(cache_info)
    }
}

// Intel cache detection using CPUID leaf 4
unsafe fn detect_intel_cache_cpuid() -> Option<CacheInfo> {
    let mut cache_info = CacheInfo::default();
    cache_info.detection_method = "Intel CPUID leaf 4".to_string();

    for index in 0..32 {
        let result = __cpuid_count(4, index);
        let cache_type = result.eax & 0x1F;
        
        // Break if no more cache levels
        if cache_type == 0 {
            break;
        }

        let cache_level = (result.eax >> 5) & 0x7;
        let ways = ((result.ebx >> 22) & 0x3FF) + 1;
        let partitions = ((result.ebx >> 12) & 0x3FF) + 1;
        let line_size = (result.ebx & 0xFFF) + 1;
        let sets = result.ecx + 1;
        
        let cache_size = (ways * partitions * line_size * sets) as usize;

        match (cache_level, cache_type) {
            (1, 1) => cache_info.l1_data_cache = cache_size, // L1 Data
            (1, 2) => cache_info.l1_instruction_cache = cache_size, // L1 Instruction
            (2, 3) => cache_info.l2_cache = cache_size, // L2 Unified
            (3, 3) => cache_info.l3_cache = cache_size, // L3 Unified
            _ => {}
        }

        if line_size > 0 && line_size <= 128 {
            cache_info.cache_line_size = line_size as usize;
        }
    }

    // Validate that we got reasonable values
    if cache_info.l1_data_cache > 4096 && cache_info.l2_cache > cache_info.l1_data_cache {
        Some(cache_info)
    } else {
        None
    }
}

// AMD cache detection using CPUID leaf 0x8000001D
unsafe fn detect_amd_cache_cpuid() -> Option<CacheInfo> {
    // Check if extended CPUID is available
    let cpuid_result = __cpuid(0x80000000);
    if cpuid_result.eax < 0x8000001D {
        return None;
    }

    let mut cache_info = CacheInfo::default();
    cache_info.detection_method = "AMD CPUID leaf 0x8000001D".to_string();

    for index in 0..8 {
        let result = __cpuid_count(0x8000001D, index);
        let cache_type = result.eax & 0x1F;
        
        if cache_type == 0 {
            break;
        }

        let cache_level = (result.eax >> 5) & 0x7;
        let ways = ((result.ebx >> 22) & 0x3FF) + 1;
        let partitions = ((result.ebx >> 12) & 0x3FF) + 1;
        let line_size = (result.ebx & 0xFFF) + 1;
        let sets = result.ecx + 1;
        
        let cache_size = (ways * partitions * line_size * sets) as usize;

        match (cache_level, cache_type) {
            (1, 1) => cache_info.l1_data_cache = cache_size,
            (1, 2) => cache_info.l1_instruction_cache = cache_size,
            (2, 3) => cache_info.l2_cache = cache_size,
            (3, 3) => cache_info.l3_cache = cache_size,
            _ => {}
        }

        if line_size > 0 && line_size <= 128 {
            cache_info.cache_line_size = line_size as usize;
        }
    }

    if cache_info.l1_data_cache > 4096 && cache_info.l2_cache > cache_info.l1_data_cache {
        Some(cache_info)
    } else {
        None
    }
}

// Legacy Intel cache detection using CPUID leaf 2
unsafe fn detect_legacy_intel_cache() -> Option<CacheInfo> {
    let result = __cpuid(2);
    let mut cache_info = CacheInfo::default();
    cache_info.detection_method = "Legacy Intel CPUID leaf 2".to_string();

    // This is a simplified parser for common cache descriptors
    // In practice, you'd need a full lookup table
    let descriptors = [
        (result.eax >> 8) as u8,
        (result.eax >> 16) as u8,
        (result.eax >> 24) as u8,
        result.ebx as u8,
        (result.ebx >> 8) as u8,
        (result.ebx >> 16) as u8,
        (result.ebx >> 24) as u8,
        result.ecx as u8,
        (result.ecx >> 8) as u8,
        (result.ecx >> 16) as u8,
        (result.ecx >> 24) as u8,
        result.edx as u8,
        (result.edx >> 8) as u8,
        (result.edx >> 16) as u8,
        (result.edx >> 24) as u8,
    ];

    for &descriptor in &descriptors {
        match descriptor {
            // Common L1 data cache sizes
            0x0A => cache_info.l1_data_cache = 8 * 1024,    // 8KB
            0x0C => cache_info.l1_data_cache = 16 * 1024,   // 16KB
            0x0D => cache_info.l1_data_cache = 16 * 1024,   // 16KB
            0x2C => cache_info.l1_data_cache = 32 * 1024,   // 32KB
            0x60 => cache_info.l1_data_cache = 16 * 1024,   // 16KB
            0x66 => cache_info.l1_data_cache = 8 * 1024,    // 8KB
            0x67 => cache_info.l1_data_cache = 16 * 1024,   // 16KB
            0x68 => cache_info.l1_data_cache = 32 * 1024,   // 32KB
            
            // Common L2 cache sizes
            0x39 => cache_info.l2_cache = 128 * 1024,       // 128KB
            0x3A => cache_info.l2_cache = 192 * 1024,       // 192KB
            0x3B => cache_info.l2_cache = 128 * 1024,       // 128KB
            0x3C => cache_info.l2_cache = 256 * 1024,       // 256KB
            0x3D => cache_info.l2_cache = 384 * 1024,       // 384KB
            0x3E => cache_info.l2_cache = 512 * 1024,       // 512KB
            0x41 => cache_info.l2_cache = 128 * 1024,       // 128KB
            0x42 => cache_info.l2_cache = 256 * 1024,       // 256KB
            0x43 => cache_info.l2_cache = 512 * 1024,       // 512KB
            0x44 => cache_info.l2_cache = 1024 * 1024,      // 1MB
            0x45 => cache_info.l2_cache = 2 * 1024 * 1024,  // 2MB
            
            // Common L3 cache sizes
            0x22 => cache_info.l3_cache = 512 * 1024,       // 512KB
            0x23 => cache_info.l3_cache = 1024 * 1024,      // 1MB
            0x25 => cache_info.l3_cache = 2 * 1024 * 1024,  // 2MB
            0x29 => cache_info.l3_cache = 4 * 1024 * 1024,  // 4MB
            0x2C => cache_info.l3_cache = 8 * 1024 * 1024,  // 8MB
            0x46 => cache_info.l3_cache = 4 * 1024 * 1024,  // 4MB
            0x47 => cache_info.l3_cache = 8 * 1024 * 1024,  // 8MB
            0x48 => cache_info.l3_cache = 3 * 1024 * 1024,  // 3MB
            0x49 => cache_info.l3_cache = 4 * 1024 * 1024,  // 4MB
            0x4A => cache_info.l3_cache = 6 * 1024 * 1024,  // 6MB
            0x4B => cache_info.l3_cache = 8 * 1024 * 1024,  // 8MB
            0x4C => cache_info.l3_cache = 12 * 1024 * 1024, // 12MB
            0x4D => cache_info.l3_cache = 16 * 1024 * 1024, // 16MB
            0x4E => cache_info.l3_cache = 24 * 1024 * 1024, // 24MB
            
            _ => {} // Unknown descriptor
        }
    }

    if cache_info.l1_data_cache > 4096 {
        Some(cache_info)
    } else {
        None
    }
}

// Empirical cache detection through memory access timing
fn detect_cache_empirically() -> Option<CacheInfo> {
    use std::time::Instant;
    use std::ptr;

    let mut cache_info = CacheInfo::default();
    cache_info.detection_method = "Empirical timing analysis".to_string();

    // Allocate a large buffer for testing (16MB should be enough)
    let test_size = 16 * 1024 * 1024;
    let test_buffer: Vec<u64> = vec![0; test_size / 8];
    let base_ptr = test_buffer.as_ptr() as *mut u64;

    // Test various sizes and measure access times
    let test_sizes = [
        4 * 1024,      // 4KB
        8 * 1024,      // 8KB
        16 * 1024,     // 16KB
        32 * 1024,     // 32KB
        64 * 1024,     // 64KB
        128 * 1024,    // 128KB
        256 * 1024,    // 256KB
        512 * 1024,    // 512KB
        1024 * 1024,   // 1MB
        2 * 1024 * 1024,   // 2MB
        4 * 1024 * 1024,   // 4MB
        8 * 1024 * 1024,   // 8MB
        12 * 1024 * 1024,  // 12MB
    ];

    let mut timings = Vec::new();

    for &size in &test_sizes {
        if size > test_size {
            break;
        }

        let iterations = 10000;
        let stride = 64; // Cache line size
        let elements = size / 8;
        
        // Warm up
        unsafe {
            for _ in 0..100 {
                for i in (0..elements).step_by(stride / 8) {
                    ptr::read_volatile(base_ptr.add(i));
                }
            }
        }

        // Measure access time
        let start = Instant::now();
        unsafe {
            for _ in 0..iterations {
                for i in (0..elements).step_by(stride / 8) {
                    ptr::read_volatile(base_ptr.add(i));
                }
            }
        }
        let elapsed = start.elapsed();
        
        let ns_per_access = elapsed.as_nanos() / (iterations * (elements / (stride / 8))) as u128;
        timings.push((size, ns_per_access));
        
        log::debug!("Cache timing test: {} KB -> {} ns/access", size / 1024, ns_per_access);
    }

    // Analyze timing jumps to detect cache boundaries
    if timings.len() >= 3 {
        let baseline = timings[0].1;
        
        for i in 1..timings.len() {
            let current_time = timings[i].1;
            let prev_time = timings[i-1].1;
            
            // Look for significant timing increases (> 50% jump)
            if current_time > prev_time * 3 / 2 && current_time > baseline * 3 / 2 {
                let cache_size = timings[i-1].0;
                
                // Categorize based on size
                if cache_size <= 64 * 1024 && cache_info.l1_data_cache == CacheInfo::default().l1_data_cache {
                    cache_info.l1_data_cache = cache_size;
                    log::debug!("Detected L1 cache boundary at {} KB", cache_size / 1024);
                } else if cache_size <= 1024 * 1024 && cache_info.l2_cache == CacheInfo::default().l2_cache {
                    cache_info.l2_cache = cache_size;
                    log::debug!("Detected L2 cache boundary at {} KB", cache_size / 1024);
                } else if cache_size > 1024 * 1024 && cache_info.l3_cache == CacheInfo::default().l3_cache {
                    cache_info.l3_cache = cache_size;
                    log::debug!("Detected L3 cache boundary at {} MB", cache_size / (1024 * 1024));
                }
            }
        }
    }

    // Validate results
    if cache_info.l1_data_cache < CacheInfo::default().l1_data_cache && 
       cache_info.l2_cache > cache_info.l1_data_cache {
        Some(cache_info)
    } else {
        None
    }
}