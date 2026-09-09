/// Enhanced memory allocation strategy with clear reserve semantics
/// This replaces the ambiguous percentage-based system with explicit reserve types
///
/// # Memory Allocation Modes
///
/// ## Standard Operating Modes (Production Use)
/// - **ReserveFromAvailable**: Reserves from currently available memory (DEFAULT)
/// - **LegacyTM5**: TM5-compatible format, reserves from available memory
///
/// ## Failure Mode Testing (Development/Testing Use)
/// - **ReserveFromTotal**: Reserves from total installed memory (can be unrealistic)
/// - **AllocateTarget**: Direct allocation target (can exceed available memory)
///
/// # Usage Examples
///
/// ## Standard/Production Examples:
/// ```text
/// memory=20%-from-available    # Reserve 20% from currently available (DEFAULT behavior)
/// memory=4GiB-from-available   # Reserve 4 GiB from currently available
/// memory=2048MB                # TM5 format: Reserve 2048MB from available
/// memory=15%                   # Shorthand: Reserve 15% from available
/// ```
///
/// ## Failure Mode Testing Examples:
/// ```text
/// memory=50%-from-total        # Reserve 50% from total (may exceed available)
/// memory=8GiB-from-total       # Reserve 8 GiB from total (ignores current usage)
/// memory=64GiB-target          # Target 64 GiB (may fail if not available)
/// memory=120%-target           # Target 120% of total memory (will definitely fail)
/// ```
///
/// # Why Failure Mode Testing?
/// 
/// The `-from-total` and `-target` modes allow testing how the memory allocator
/// handles unrealistic or impossible allocation requests:
/// 
/// - **Memory pressure testing**: See how system behaves under extreme memory pressure
/// - **Allocation failure paths**: Test error handling when allocations fail
/// - **Edge case validation**: Validate system behavior at memory limits
/// - **Stress testing configurations**: Create configurations that push system limits
///
/// # Safety Warnings
///
/// Failure mode configurations may:
/// - Cause allocation failures (by design)
/// - Make system unresponsive due to memory pressure  
/// - Trigger out-of-memory conditions
/// - Cause test crashes or hangs
/// 
/// Use these modes only for development/testing purposes!
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX, GetPhysicallyInstalledSystemMemory};
use crate::constants::{BYTES_PER_GIB, BYTES_PER_MIB_USIZE, gib_to_bytes, bytes_to_gib_f64};

/// Memory information gathered from the system
#[derive(Debug, Clone)]
pub struct SystemMemoryInfo {
    /// Total physically installed RAM (from GetPhysicallyInstalledSystemMemory)
    pub total_installed_bytes: u64,
    
    /// Total physical memory available to OS (from GlobalMemoryStatusEx)
    pub total_physical_bytes: u64,
    
    /// Currently available physical memory (from GlobalMemoryStatusEx)
    pub available_physical_bytes: u64,
    
    /// Currently used physical memory (calculated)
    pub used_physical_bytes: u64,
    
    /// Total virtual memory (page file + physical)
    pub total_virtual_bytes: u64,
    
    /// Available virtual memory
    pub available_virtual_bytes: u64,
    
    /// Memory load percentage (0-100)
    pub memory_load_percent: u32,
    
    /// Minimum recommended start address for allocations (calculated)
    pub min_start_address: u64,
}

impl SystemMemoryInfo {
    /// Gather current system memory information
    pub fn gather() -> Result<Self, String> {
        unsafe {
            // Get total installed memory
            let mut total_installed_kb: u64 = 0;
            let total_installed_bytes = match GetPhysicallyInstalledSystemMemory(&mut total_installed_kb) {
                Ok(_) => total_installed_kb * 1024,
                Err(_) => return Err("Failed to get total installed memory".to_string()),
            };

            // Get current memory status
            let mut mem_status = MEMORYSTATUSEX {
                dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
                ..Default::default()
            };

            if GlobalMemoryStatusEx(&mut mem_status).is_err() {
                return Err("Failed to get current memory status".to_string());
            }

            let used_physical_bytes = mem_status.ullTotalPhys - mem_status.ullAvailPhys;
            
            // Calculate default minimum start address (current usage + 1GB buffer)
            let buffer_bytes = BYTES_PER_GIB; // 1GB buffer
            let min_start_address = used_physical_bytes + buffer_bytes;

            Ok(SystemMemoryInfo {
                total_installed_bytes,
                total_physical_bytes: mem_status.ullTotalPhys,
                available_physical_bytes: mem_status.ullAvailPhys,
                used_physical_bytes,
                total_virtual_bytes: mem_status.ullTotalPageFile,
                available_virtual_bytes: mem_status.ullAvailPageFile,
                memory_load_percent: mem_status.dwMemoryLoad,
                min_start_address,
            })
        }
    }

    /// Calculate actual start address based on start address mode
    /// Note: For SplitReserve mode, this is just a placeholder - the real calculation
    /// happens in calculate_allocation_with_split_reserve
    pub fn calculate_start_address(&self, mode: &StartAddressMode) -> u64 {
        match mode {
            StartAddressMode::Offset { offset_gib } => {
                // Simple: End of used memory + offset
                if *offset_gib >= 0.0 {
                    let offset_bytes = gib_to_bytes(*offset_gib);
                    self.used_physical_bytes + offset_bytes
                } else {
                    let offset_bytes = gib_to_bytes(offset_gib.abs());
                    self.used_physical_bytes.saturating_sub(offset_bytes)
                }
            }
            StartAddressMode::SplitReserve { .. } => {
                // Placeholder - actual calculation done in calculate_allocation_with_split_reserve
                // This ensures we have a reasonable fallback for any edge cases
                self.used_physical_bytes + BYTES_PER_GIB // 1GB buffer
            }
        }
    }

    // Note: Detailed memory analysis is now shown in consolidated memory report instead
}

/// Start address control for memory allocation
#[derive(Debug, Clone)]
pub enum StartAddressMode {
    /// Simple offset from end of used memory (0-based)
    /// Examples: +1GB, +0.5GB, +2GB
    Offset { offset_gib: f64 },
    
    /// Split reserve into pre-buffer and post-reserve
    /// Examples: split=20%:80%, split=auto
    SplitReserve { pre_percent: f64, post_percent: f64 },
}

impl Default for StartAddressMode {
    fn default() -> Self {
        // Post-boot default: Split reserve with aggressive testing optimizations
        // 5% pre-buffer (fragmentation protection) + 95% post-reserve (maximum testing)
        StartAddressMode::SplitReserve { pre_percent: 5.0, post_percent: 95.0 }
    }
}

/// Enhanced allocation modes with clear semantics
#[derive(Debug, Clone)]
pub enum AllocationMode {
    /// Reserve from currently available memory (standard/default behavior, matches TM5)
    /// Examples: "memory=4GiB-from-available", "memory=15%-from-available", "memory=1024MB-from-available"
    /// This is what TM5 configs use - reserves from currently available memory
    ReserveFromAvailable {
        reserve: ReserveAmount,
    },
    
    /// Reserve from total installed memory (for failure mode testing - unrealistic configs)
    /// Examples: "memory=8GiB-from-total", "memory=20%-from-total", "memory=2048MB-from-total"
    /// WARNING: Can create unrealistic allocations that may fail - intended for testing failure modes
    ReserveFromTotal {
        reserve: ReserveAmount,
    },
    
    /// Allocate specific amount regardless of available memory (for failure mode testing)
    /// Examples: "memory=16GiB-target", "memory=80%-target", "memory=8192MB-target"
    /// WARNING: Can request more than available - intended for testing failure modes
    AllocateTarget {
        target: ReserveAmount,
    },
    
    /// Legacy TM5 compatibility mode (reserves MB from available memory, like original TM5)
    /// Examples: "memory=2048MB", "memory=1024MB-legacy"
    /// This is the standard TM5 behavior - reserves from available memory
    LegacyTM5 {
        reserve_mb: u32,
    },
}

/// Amount specification with multiple unit support
#[derive(Debug, Clone)]
pub enum ReserveAmount {
    /// Absolute amount in bytes
    Bytes(u64),
    
    /// Percentage (0.0 to 100.0)
    Percentage(f64),
}

impl ReserveAmount {
    /// Parse from string with unit detection
    /// Supports: "8GiB", "2048MB", "20%", "16384MB", "4GB", etc.
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim().to_lowercase();
        
        if s.ends_with('%') {
            let percent_str = &s[..s.len()-1];
            match percent_str.parse::<f64>() {
                Ok(pct) if (0.0..=100.0).contains(&pct) => Ok(ReserveAmount::Percentage(pct)),
                Ok(_) => Err(format!("Percentage must be between 0-100, got {}", percent_str)),
                Err(_) => Err(format!("Invalid percentage format: {}", s)),
            }
        } else if s.ends_with("gib") || s.ends_with("gb") {
            let num_str = if s.ends_with("gib") { &s[..s.len()-3] } else { &s[..s.len()-2] };
            match num_str.parse::<f64>() {
                Ok(gib) => {
                    let bytes = if s.ends_with("gib") {
                        gib_to_bytes(gib)  // GiB (binary)
                    } else {
                        (gib * 1000.0 * 1000.0 * 1000.0) as u64  // GB (decimal)
                    };
                    Ok(ReserveAmount::Bytes(bytes))
                },
                Err(_) => Err(format!("Invalid GiB/GB format: {}", s)),
            }
        } else if s.ends_with("mib") || s.ends_with("mb") {
            let num_str = if s.ends_with("mib") { &s[..s.len()-3] } else { &s[..s.len()-2] };
            match num_str.parse::<f64>() {
                Ok(mib) => {
                    let bytes = if s.ends_with("mib") {
                        (mib * 1024.0 * 1024.0) as u64  // MiB (binary)
                    } else {
                        (mib * 1000.0 * 1000.0) as u64  // MB (decimal)
                    };
                    Ok(ReserveAmount::Bytes(bytes))
                },
                Err(_) => Err(format!("Invalid MiB/MB format: {}", s)),
            }
        } else {
            // Try parsing as raw number (assume bytes)
            match s.parse::<u64>() {
                Ok(bytes) => Ok(ReserveAmount::Bytes(bytes)),
                Err(_) => Err(format!("Unrecognized memory format: {}", s)),
            }
        }
    }
    
    /// Calculate actual bytes based on a reference value
    pub fn calculate_bytes(&self, reference_bytes: u64) -> u64 {
        match self {
            ReserveAmount::Bytes(bytes) => *bytes,
            ReserveAmount::Percentage(pct) => (reference_bytes as f64 * pct / 100.0) as u64,
        }
    }

    /// Canonical spec text for this amount, in the form `parse` accepts (`"20%"`, `"8GiB"`).
    /// Recorded into result files so a run's memory request is reconstructable (TODO #67).
    pub fn describe_spec(&self) -> String {
        match self {
            // GiB with 3 decimals: a byte count from `parse` is always a clean GiB/MiB value, and
            // 3 decimals round-trips MiB-granular specs (e.g. 2048MB -> "1.953GiB") without
            // printing 10 digits of noise.
            ReserveAmount::Bytes(bytes) => format!("{:.3}GiB", bytes_to_gib_f64(*bytes)),
            ReserveAmount::Percentage(pct) => format!("{}%", pct),
        }
    }
}

impl StartAddressMode {
    /// Parse start address parameter
    /// Examples:
    /// - "+2GiB" or "offset:2GiB" -> Offset { offset_gib: 2.0 }
    /// - "-1GiB" or "offset:-1GiB" -> Offset { offset_gib: -1.0 }
    /// - "split:20%:80%" -> SplitReserve { pre_percent: 20.0, post_percent: 80.0 }
    /// - "split:auto" -> SplitReserve with optimized post-boot defaults
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim().to_lowercase();
        
        if let Some(split_part) = s.strip_prefix("split:") {
            // Skip "split:"
            
            if split_part == "auto" {
                // Post-boot optimized defaults: 10% pre-buffer, 90% post-reserve
                Ok(StartAddressMode::SplitReserve { pre_percent: 10.0, post_percent: 90.0 })
            } else if split_part.contains(":") {
                let parts: Vec<&str> = split_part.split(":").collect();
                if parts.len() != 2 {
                    return Err("Split reserve format should be 'split:X%:Y%'".to_string());
                }
                
                let pre_percent = Self::parse_percentage(parts[0])?;
                let post_percent = Self::parse_percentage(parts[1])?;
                
                // Validate percentages sum to 100% (with small tolerance)
                if (pre_percent + post_percent - 100.0).abs() > 0.1 {
                    return Err(format!("Split percentages must sum to 100%, got {}% + {}% = {}%", 
                                     pre_percent, post_percent, pre_percent + post_percent));
                }
                
                Ok(StartAddressMode::SplitReserve { pre_percent, post_percent })
            } else {
                Err("Split reserve format should be 'split:X%:Y%' or 'split:auto'".to_string())
            }
        } else if s.starts_with("+") || s.starts_with("offset:") {
            let offset_str = if let Some(stripped) = s.strip_prefix("+") {
                stripped
            } else {
                &s[7..] // Skip "offset:"
            };
            let offset_gib = Self::parse_gib_amount(offset_str)?;
            Ok(StartAddressMode::Offset { offset_gib })
        } else if let Some(offset_str) = s.strip_prefix("-") {
            let offset_gib = -Self::parse_gib_amount(offset_str)?;
            Ok(StartAddressMode::Offset { offset_gib })
        } else {
            // Default to simple offset parsing if no prefix
            let offset_gib = Self::parse_gib_amount(&s)?;
            Ok(StartAddressMode::Offset { offset_gib })
        }
    }
    
    /// Parse percentage from string (e.g., "20%" -> 20.0)
    fn parse_percentage(s: &str) -> Result<f64, String> {
        if let Some(num_str) = s.strip_suffix("%") {
            num_str.parse::<f64>().map_err(|_| format!("Invalid percentage value: {}", s))
        } else {
            Err(format!("Percentage must end with '%': {}", s))
        }
    }

    /// Parse GiB amount from string
    fn parse_gib_amount(s: &str) -> Result<f64, String> {
        if let Some(stripped) = s.strip_suffix("gib") {
            stripped.parse::<f64>().map_err(|_| format!("Invalid GiB value: {}", s))
        } else if let Some(stripped) = s.strip_suffix("gb") {
            let gb = stripped.parse::<f64>().map_err(|_| format!("Invalid GB value: {}", s))?;
            Ok(gb * 1000.0 / 1024.0) // Convert GB to GiB
        } else if let Some(stripped) = s.strip_suffix("mb") {
            let mb = stripped.parse::<f64>().map_err(|_| format!("Invalid MB value: {}", s))?;
            Ok(mb / 1024.0) // Convert MB to GiB
        } else {
            s.parse::<f64>().map_err(|_| format!("Invalid numeric value: {}", s))
        }
    }

    /// Canonical spec text, in the form `parse` accepts (`"split:5%:95%"`, `"+2GiB"`).
    /// Note `split:auto` is *not* reproduced — it resolves to concrete percentages at parse
    /// time, and the concrete values are what actually shaped the run.
    pub fn describe_spec(&self) -> String {
        match self {
            StartAddressMode::Offset { offset_gib } => format!("{:+}GiB", offset_gib),
            StartAddressMode::SplitReserve { pre_percent, post_percent } => {
                format!("split:{}%:{}%", pre_percent, post_percent)
            }
        }
    }
}

impl AllocationMode {
    /// Canonical spec text for the `memory=` value, minus the `start=` suffix
    /// (`"20%-from-available"`, `"8.000GiB-target"`).
    pub fn describe_spec(&self) -> String {
        match self {
            AllocationMode::ReserveFromAvailable { reserve } => {
                format!("{}-from-available", reserve.describe_spec())
            }
            AllocationMode::ReserveFromTotal { reserve } => {
                format!("{}-from-total", reserve.describe_spec())
            }
            AllocationMode::AllocateTarget { target } => {
                format!("{}-target", target.describe_spec())
            }
            AllocationMode::LegacyTM5 { reserve_mb } => format!("{}MB", reserve_mb),
        }
    }

    /// Whether this mode produces the **same allocation size** on every run of the same machine.
    ///
    /// Only the `-from-total` and `-target` forms do: their reference is total installed RAM (a
    /// constant) or an explicit figure. The `-from-available` forms — including the `--quick-test`
    /// default and TM5 legacy configs — key off *currently free* memory, so two runs minutes apart
    /// can size differently and their throughput numbers are not directly comparable. Recorded in
    /// result files so `--compare-results` can say so instead of the user having to know (TODO #67).
    pub fn is_deterministic(&self) -> bool {
        match self {
            AllocationMode::ReserveFromTotal { .. } | AllocationMode::AllocateTarget { .. } => true,
            AllocationMode::ReserveFromAvailable { .. } | AllocationMode::LegacyTM5 { .. } => false,
        }
    }
}

impl AllocationMode {
    /// Calculate allocation with thread-aware sizing for identical per-thread layouts
    fn calculate_allocation_with_split_reserve_and_threads(&self, mem_info: &SystemMemoryInfo, pre_percent: f64, post_percent: f64, thread_count: Option<usize>) -> AllocationResult {
        // Validate percentages
        if (pre_percent + post_percent - 100.0).abs() > 0.01 {
            log::warn!("Split reserve percentages don't sum to 100%: {}% + {}% = {}%", 
                      pre_percent, post_percent, pre_percent + post_percent);
        }
        
        // Calculate total reserve bytes based on allocation mode
        let total_reserve_bytes = match self {
            AllocationMode::ReserveFromAvailable { reserve } => {
                reserve.calculate_bytes(mem_info.available_physical_bytes)
            },
            AllocationMode::ReserveFromTotal { reserve } => {
                reserve.calculate_bytes(mem_info.total_installed_bytes)
            },
            AllocationMode::AllocateTarget { target } => {
                // For target mode, calculate reverse reserve (what's left after target)
                let target_bytes = target.calculate_bytes(mem_info.total_installed_bytes);
                mem_info.total_installed_bytes.saturating_sub(target_bytes)
            },
            AllocationMode::LegacyTM5 { reserve_mb } => {
                (*reserve_mb as u64) * 1024 * 1024
            },
        };
        
        // Split reserve into pre-buffer and post-reserve
        let pre_buffer_bytes = (total_reserve_bytes as f64 * pre_percent / 100.0) as u64;
        let post_reserve_bytes = total_reserve_bytes.saturating_sub(pre_buffer_bytes);
        
        // Calculate raw start address: end of used memory + pre-buffer
        let raw_start_address = mem_info.used_physical_bytes + pre_buffer_bytes;
        
        // Align start address UP to 1GB boundary for huge page support
        const GB_BOUNDARY: u64 = BYTES_PER_GIB;
        let aligned_start_address = raw_start_address.div_ceil(GB_BOUNDARY) * GB_BOUNDARY;
        let start_adjustment = aligned_start_address - raw_start_address;
        
        log::info!("Start address alignment: raw={:.3} GiB, aligned={:.3} GiB, adjustment=+{:.3} GiB",
                  raw_start_address as f64 / GB_BOUNDARY as f64,
                  aligned_start_address as f64 / GB_BOUNDARY as f64,
                  start_adjustment as f64 / GB_BOUNDARY as f64);
        
        // Calculate reference memory for allocation
        let reference_bytes = match self {
            AllocationMode::ReserveFromAvailable { .. } | AllocationMode::LegacyTM5 { .. } => {
                mem_info.available_physical_bytes
            },
            AllocationMode::ReserveFromTotal { .. } | AllocationMode::AllocateTarget { .. } => {
                mem_info.total_installed_bytes
            },
        };
        
        // Calculate base allocation size
        let raw_allocation_bytes = reference_bytes.saturating_sub(total_reserve_bytes);
        let base_allocation_bytes = raw_allocation_bytes.saturating_sub(start_adjustment);
        
        // Thread-aware allocation strategy: Round UP per-thread allocation for identical layouts
        let (final_allocation_bytes, allocation_strategy) = if let Some(threads) = thread_count {
            let per_thread_target = base_allocation_bytes / threads as u64;
            
            // Standard chunk sizes for rounding (1GB, 512MB, 256MB, 128MB)
            let chunk_sizes = [GB_BOUNDARY, 512*BYTES_PER_MIB_USIZE as u64, 256*BYTES_PER_MIB_USIZE as u64, 128*BYTES_PER_MIB_USIZE as u64];
            
            // Round each thread's allocation UP to next clean chunk combination
            let rounded_per_thread = Self::round_up_to_chunk_combination(per_thread_target, &chunk_sizes);
            let total_rounded = rounded_per_thread * threads as u64;
            
            log::info!("Thread-aware allocation: {} threads × {:.3} GiB → rounded to {:.3} GiB each",
                      threads, 
                      per_thread_target as f64 / GB_BOUNDARY as f64,
                      rounded_per_thread as f64 / GB_BOUNDARY as f64);
            
            log::info!("Total allocation: {:.3} GiB → {:.3} GiB (overage: +{:.3} GiB absorbed by reserves)",
                      base_allocation_bytes as f64 / GB_BOUNDARY as f64,
                      total_rounded as f64 / GB_BOUNDARY as f64,
                      (total_rounded - base_allocation_bytes) as f64 / GB_BOUNDARY as f64);
            
            (total_rounded, format!("thread-aware ({}×{:.2} GiB)", threads, rounded_per_thread as f64 / GB_BOUNDARY as f64))
        } else {
            // Fallback: Round down to 1GB boundary (legacy behavior)
            let allocation_bytes = (base_allocation_bytes / GB_BOUNDARY) * GB_BOUNDARY;
            
            log::info!("Legacy allocation: {:.3} GiB → {:.3} GiB (rounded down to 1GB boundary)",
                      base_allocation_bytes as f64 / GB_BOUNDARY as f64,
                      allocation_bytes as f64 / GB_BOUNDARY as f64);
            
            (allocation_bytes, "1GB-aligned".to_string())
        };
        
        // Calculate final size adjustment and effective reserves
        let size_adjustment = base_allocation_bytes.saturating_sub(final_allocation_bytes);
        
        let overage = final_allocation_bytes.saturating_sub(base_allocation_bytes);
        
        let effective_reserve_bytes = total_reserve_bytes + start_adjustment + size_adjustment + overage;
        let effective_pre_buffer = pre_buffer_bytes + start_adjustment;
        let effective_post_reserve = post_reserve_bytes + size_adjustment + overage;
        
        log::info!("Final reserves: total={:.3} GiB (was {:.3}), pre={:.3} GiB, post={:.3} GiB",
                  effective_reserve_bytes as f64 / GB_BOUNDARY as f64,
                  total_reserve_bytes as f64 / GB_BOUNDARY as f64,
                  effective_pre_buffer as f64 / GB_BOUNDARY as f64,
                  effective_post_reserve as f64 / GB_BOUNDARY as f64);
        
        // Determine allocation type description
        let base_type = match self {
            AllocationMode::ReserveFromAvailable { .. } => "Reserve pre/post split",
            AllocationMode::ReserveFromTotal { .. } => "Reserve pre/post split from Total",
            AllocationMode::AllocateTarget { .. } => "Target allocation pre/post split",
            AllocationMode::LegacyTM5 { .. } => "Legacy TM5 pre/post split",
        };
        
        AllocationResult {
            allocation_bytes: final_allocation_bytes,
            reserve_bytes: effective_reserve_bytes,
            reference_bytes,
            min_start_address: aligned_start_address,
            allocation_type: format!("{} ({}%:{}%) [{}]", base_type, pre_percent as u32, post_percent as u32, allocation_strategy),
            split_details: Some(SplitReserveInfo {
                pre_percent,
                post_percent,
                pre_buffer_bytes: effective_pre_buffer,
                post_reserve_bytes: effective_post_reserve,
            }),
        }
    }
    
    /// Round up allocation size to optimal chunk combinations
    /// Uses greedy algorithm to find combination of 1GB, 512MB, 256MB, 128MB chunks
    fn round_up_to_chunk_combination(target_bytes: u64, chunk_sizes: &[u64]) -> u64 {
        if target_bytes == 0 {
            return 0;
        }
        
        let mut remaining = target_bytes;
        let mut total = 0;
        
        // Greedy approach: use largest chunks first
        for &chunk_size in chunk_sizes {
            let chunks_needed = remaining.div_ceil(chunk_size); // Round up division
            if chunks_needed > 0 {
                // Check if this single chunk type can satisfy the remaining needs
                if chunks_needed * chunk_size >= remaining {
                    total += chunks_needed * chunk_size;
                    return total;
                }
                
                // Otherwise, use as many full chunks as possible and continue with remainder
                let full_chunks = remaining / chunk_size;
                total += full_chunks * chunk_size;
                remaining -= full_chunks * chunk_size;
            }
        }
        
        // If we still have remainder, round up with the smallest chunk
        if remaining > 0 && !chunk_sizes.is_empty() {
            let smallest_chunk = *chunk_sizes.last().unwrap();
            let final_chunks = remaining.div_ceil(smallest_chunk);
            total += final_chunks * smallest_chunk;
        }
        
        total
    }

    /// Parse memory parameter string into AllocationMode
    /// Examples:
    /// - "20%-from-available" -> ReserveFromAvailable { reserve: Percentage(20.0) } (STANDARD)
    /// - "8GiB-from-total" -> ReserveFromTotal { reserve: Bytes(8*1024^3) } (FAILURE MODE TESTING)
    /// - "16GiB-target" -> AllocateTarget { target: Bytes(16*1024^3) } (FAILURE MODE TESTING)
    /// - "2048MB" -> LegacyTM5 { reserve_mb: 2048 } (TM5 compatibility - uses available memory)
    /// 
    /// With start address control:
    /// - "20%-from-available:start=auto:2GiB" -> Allocation + Auto start with 2GiB buffer
    /// - "16GiB-target:start=0x800000000" -> Allocation + Fixed start address
    /// - "2048MB:start=+4GiB" -> Allocation + Offset start address
    pub fn parse(s: &str) -> Result<(Self, StartAddressMode), String> {
        let s = s.trim().to_lowercase();
        
        // Check if start address is specified
        let (allocation_part, start_mode) = if s.contains(":start=") {
            let parts: Vec<&str> = s.split(":start=").collect();
            if parts.len() != 2 {
                return Err("Invalid start address format".to_string());
            }
            let start_mode = StartAddressMode::parse(parts[1])?;
            (parts[0], start_mode)
        } else {
            (s.as_str(), StartAddressMode::default())
        };
        
        // Parse allocation mode
        let allocation_mode = if allocation_part.contains("-from-available") {
            let amount_str = allocation_part.split("-from-available").next().unwrap();
            let reserve = ReserveAmount::parse(amount_str)?;
            AllocationMode::ReserveFromAvailable { reserve }
        } else if allocation_part.contains("-from-total") {
            let amount_str = allocation_part.split("-from-total").next().unwrap();
            let reserve = ReserveAmount::parse(amount_str)?;
            AllocationMode::ReserveFromTotal { reserve }
        } else if allocation_part.contains("-target") {
            let amount_str = allocation_part.split("-target").next().unwrap();
            let target = ReserveAmount::parse(amount_str)?;
            AllocationMode::AllocateTarget { target }
        } else if allocation_part.contains("-legacy") || (allocation_part.ends_with("mb") && !allocation_part.contains("-")) {
            // Legacy TM5 mode or simple "2048mb" format - both reserve from available (TM5 behavior)
            let amount_str = if allocation_part.contains("-legacy") {
                allocation_part.split("-legacy").next().unwrap()
            } else {
                &allocation_part[..allocation_part.len()-2] // Remove "mb"
            };
            match amount_str.parse::<u32>() {
                Ok(mb) => AllocationMode::LegacyTM5 { reserve_mb: mb },
                Err(_) => return Err(format!("Invalid legacy MB format: {}", allocation_part)),
            }
        } else {
            // Default interpretation - try to parse as reserve from available (standard behavior)
            let reserve = ReserveAmount::parse(allocation_part)?;
            AllocationMode::ReserveFromAvailable { reserve }
        };
        
        Ok((allocation_mode, start_mode))
    }
    
    /// Calculate the allocation amount based on system memory info
    pub fn calculate_allocation(&self, mem_info: &SystemMemoryInfo, start_address_mode: &StartAddressMode) -> AllocationResult {
        self.calculate_allocation_with_threads(mem_info, start_address_mode, None)
    }
    
    /// Calculate allocation with thread count for thread-aware sizing
    pub fn calculate_allocation_with_threads(&self, mem_info: &SystemMemoryInfo, start_address_mode: &StartAddressMode, thread_count: Option<usize>) -> AllocationResult {
        // Handle split reserve mode specially
        if let StartAddressMode::SplitReserve { pre_percent, post_percent } = start_address_mode {
            return self.calculate_allocation_with_split_reserve_and_threads(mem_info, *pre_percent, *post_percent, thread_count);
        }
        
        let actual_start_address = mem_info.calculate_start_address(start_address_mode);
        match self {
            AllocationMode::ReserveFromTotal { reserve } => {
                let reserve_bytes = reserve.calculate_bytes(mem_info.total_installed_bytes);
                let available_for_allocation = mem_info.total_installed_bytes.saturating_sub(reserve_bytes);
                AllocationResult {
                    allocation_bytes: available_for_allocation,
                    reserve_bytes,
                    reference_bytes: mem_info.total_installed_bytes,
                    min_start_address: actual_start_address,
                    allocation_type: "Reserve from Total".to_string(),
                    split_details: None,
                }
            },
            AllocationMode::ReserveFromAvailable { reserve } => {
                let reserve_bytes = reserve.calculate_bytes(mem_info.available_physical_bytes);
                let available_for_allocation = mem_info.available_physical_bytes.saturating_sub(reserve_bytes);
                AllocationResult {
                    allocation_bytes: available_for_allocation,
                    reserve_bytes,
                    reference_bytes: mem_info.available_physical_bytes,
                    min_start_address: actual_start_address,
                    allocation_type: "Reserve from Available".to_string(),
                    split_details: None,
                }
            },
            AllocationMode::AllocateTarget { target } => {
                let target_bytes = target.calculate_bytes(mem_info.total_installed_bytes);
                let max_safe = mem_info.available_physical_bytes.saturating_sub(BYTES_PER_GIB); // Leave 1GB safety
                let allocation_bytes = target_bytes.min(max_safe);
                AllocationResult {
                    allocation_bytes,
                    reserve_bytes: mem_info.total_installed_bytes.saturating_sub(allocation_bytes),
                    reference_bytes: mem_info.total_installed_bytes,
                    min_start_address: actual_start_address,
                    allocation_type: "Target Allocation".to_string(),
                    split_details: None,
                }
            },
            AllocationMode::LegacyTM5 { reserve_mb } => {
                let reserve_bytes = (*reserve_mb as u64) * 1024 * 1024;
                let available_for_allocation = mem_info.available_physical_bytes.saturating_sub(reserve_bytes);
                AllocationResult {
                    allocation_bytes: available_for_allocation,
                    reserve_bytes,
                    reference_bytes: mem_info.available_physical_bytes,
                    min_start_address: actual_start_address,
                    allocation_type: "Legacy TM5 (from available)".to_string(),
                    split_details: None,
                }
            },
        }
    }
}

/// Result of allocation calculation
#[derive(Debug, Clone)]
pub struct AllocationResult {
    /// Total bytes to allocate for testing
    pub allocation_bytes: u64,
    
    /// Bytes reserved (not used for testing)
    pub reserve_bytes: u64,
    
    /// Reference bytes used for calculation (total/available)
    pub reference_bytes: u64,
    
    /// Minimum recommended start address
    pub min_start_address: u64,
    
    /// Human-readable description of allocation type
    pub allocation_type: String,
    
    /// Split reserve details if using split mode
    pub split_details: Option<SplitReserveInfo>,
}

/// Split reserve breakdown information
#[derive(Debug, Clone)]
pub struct SplitReserveInfo {
    pub pre_percent: f64,
    pub post_percent: f64,
    pub pre_buffer_bytes: u64,
    pub post_reserve_bytes: u64,
}

impl AllocationResult {
    /// Print allocation summary
    pub fn print_summary(&self, mem_info: &SystemMemoryInfo) {
        // Note: Detailed allocation plan is now shown in consolidated memory report
        
        // Gather warnings
        let allocation_percent = (self.allocation_bytes as f64 / self.reference_bytes as f64) * 100.0;
        let mut warnings = Vec::new();
        
        if allocation_percent > 90.0 {
            warnings.push(format!("High memory allocation (>{:.1}%) may cause system instability", allocation_percent));
        }
        
        if self.allocation_bytes > mem_info.available_physical_bytes {
            let allocation_gib = bytes_to_gib_f64(self.allocation_bytes);
            warnings.push(format!(
                "Requested allocation exceeds currently available memory (Requested: {:.2} GiB, Available: {:.2} GiB)",
                allocation_gib,
                bytes_to_gib_f64(mem_info.available_physical_bytes)
            ));
        }
        
        // Start address validation warnings
        let max_physical_addr = mem_info.total_physical_bytes;
        if self.min_start_address > max_physical_addr {
            warnings.push(format!(
                "Start address (0x{:016X}) exceeds physical memory range (0x0 - 0x{:016X})",
                self.min_start_address, max_physical_addr
            ));
        }
        
        if self.min_start_address + self.allocation_bytes > max_physical_addr {
            warnings.push(format!(
                "Allocation range exceeds physical memory (Start: 0x{:016X}, End: 0x{:016X}, Limit: 0x{:016X})",
                self.min_start_address,
                self.min_start_address + self.allocation_bytes,
                max_physical_addr
            ));
        }
        
        // Failure mode testing warnings
        if self.allocation_type.contains("from Total") || self.allocation_type.contains("Target") {
            warnings.push("🧪 FAILURE MODE TESTING DETECTED - This configuration may intentionally cause allocation failures".to_string());
            warnings.push("Use only for development/testing purposes!".to_string());
            
            if self.allocation_bytes > mem_info.available_physical_bytes * 2 {
                warnings.push("🚨 EXTREME: Allocation >2x available memory - will likely fail".to_string());
            }
        }
        
        // Note: Detailed allocation plan is now shown in consolidated memory report
    }
}

/// Enhanced memory strategy with new allocation modes
#[derive(Debug, Clone)]
pub struct EnhancedMemoryStrategy {
    /// How to calculate memory allocation amount
    pub allocation_mode: AllocationMode,
    
    /// Control over allocation start address
    pub start_address_mode: StartAddressMode,
    
    // WindowMode and ChunkMode removed - these are test configuration concerns, not allocation strategy concerns
}

impl Default for EnhancedMemoryStrategy {
    fn default() -> Self {
        Self {
            // Post-boot optimized: Reserve only 10% from available (aggressive testing)
            allocation_mode: AllocationMode::ReserveFromAvailable { 
                reserve: ReserveAmount::Percentage(10.0) 
            },
            start_address_mode: StartAddressMode::default(),
        }
    }
}

impl EnhancedMemoryStrategy {
    /// Canonical `memory=` spec that reproduces this strategy, e.g.
    /// `"20%-from-available:start=split:5%:95%"`. Written into result files so a run's request is
    /// reconstructable from the result alone rather than only from `logs/` (TODO #67).
    pub fn describe_spec(&self) -> String {
        format!(
            "{}:start={}",
            self.allocation_mode.describe_spec(),
            self.start_address_mode.describe_spec()
        )
    }

    /// Create a comprehensive memory layout with enhanced allocation calculation
    pub fn create_layout(&self, thread_count: usize) -> Result<crate::layout::EnhancedMemoryLayout, String> {
        let mem_info = SystemMemoryInfo::gather()?;
        // Use thread-aware allocation calculation for optimal per-thread layouts
        let allocation_result = self.allocation_mode.calculate_allocation_with_threads(&mem_info, &self.start_address_mode, Some(thread_count));
        
        // Create per-thread blocks with identical sizes (thread-aware allocation ensures this)
        let allocation_per_thread = allocation_result.allocation_bytes / thread_count as u64;
        let blocks = (0..thread_count)
            .map(|thread_id| crate::layout::BlockInfo {
                size_bytes: allocation_per_thread as usize,
                thread_id,
            })
            .collect();
        
        Ok(crate::layout::EnhancedMemoryLayout {
            system_memory_info: mem_info,
            allocation_result,
            blocks,
            strategy: self.clone(),
        })
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reserve_amount_parsing() {
        assert!(matches!(ReserveAmount::parse("20%"), Ok(ReserveAmount::Percentage(20.0))));
        assert!(matches!(ReserveAmount::parse("8GiB"), Ok(ReserveAmount::Bytes(_))));
        assert!(matches!(ReserveAmount::parse("2048MB"), Ok(ReserveAmount::Bytes(_))));
        assert!(ReserveAmount::parse("150%").is_err()); // Invalid percentage
    }

    #[test]
    fn test_allocation_mode_parsing() {
        assert!(matches!(
            AllocationMode::parse("8GiB-from-total"), 
            Ok((AllocationMode::ReserveFromTotal { .. }, _))
        ));
        assert!(matches!(
            AllocationMode::parse("20%-from-available"), 
            Ok((AllocationMode::ReserveFromAvailable { .. }, _))
        ));
        assert!(matches!(
            AllocationMode::parse("16GiB-target"), 
            Ok((AllocationMode::AllocateTarget { .. }, _))
        ));
        assert!(matches!(
            AllocationMode::parse("2048MB"), 
            Ok((AllocationMode::LegacyTM5 { reserve_mb: 2048 }, _))
        ));
    }

    #[test]
    fn test_legacy_conversion() {
        // Test ReserveFromAvailable with percentage  
        let mode = AllocationMode::ReserveFromAvailable { 
            reserve: ReserveAmount::Percentage(70.0) 
        };
        assert!(matches!(mode, AllocationMode::ReserveFromAvailable { .. }));

        // Test LegacyTM5 conversion
        let mode = AllocationMode::LegacyTM5 { reserve_mb: 2048 };
        assert!(matches!(mode, AllocationMode::LegacyTM5 { reserve_mb: 2048 }));
    }
}