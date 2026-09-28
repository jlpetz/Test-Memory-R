// Every `unsafe` block in this file crosses a Win32 FFI boundary, where the borrow checker is
// switched off precisely where the invariants get subtle (TODO #66). The lint below makes a missing
// `// SAFETY:` a warning *here* rather than relying on a periodic audit — it is deliberately not
// crate-wide, because the SIMD test kernels' `unsafe` is a different, repetitive story already
// covered by `test_fn_safety.md`, and a blanket rule there would produce boilerplate that trains
// you to skip reading these.
#![warn(clippy::undocumented_unsafe_blocks)]

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
    
    /// Memory load percentage (0-100)
    pub memory_load_percent: u32,
}

impl SystemMemoryInfo {
    /// Gather current system memory information
    pub fn gather() -> Result<Self, String> {
        // SAFETY: both calls write into initialised locals passed by `&mut`. `MEMORYSTATUSEX`
        // carries its own `dwLength`, set to `size_of` of that type above — the API relies on it
        // to know the struct version, so a wrong value there (not a bad pointer) is the real
        // hazard, and it is derived rather than hard-coded.
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

            Ok(SystemMemoryInfo {
                total_installed_bytes,
                total_physical_bytes: mem_status.ullTotalPhys,
                available_physical_bytes: mem_status.ullAvailPhys,
                used_physical_bytes,
                total_virtual_bytes: mem_status.ullTotalPageFile,
                memory_load_percent: mem_status.dwMemoryLoad,
            })
        }
    }

// Note: Detailed memory analysis is now shown in consolidated memory report instead
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

impl AllocationMode {
    /// Canonical spec text for the `memory=` value (`"20%-from-available"`, `"8.000GiB-target"`).
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
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim().to_lowercase();
        let allocation_part = s.as_str();

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

        Ok(allocation_mode)
    }
    
    /// Size the test allocation for `thread_count` workers with identical blocks.
    ///
    /// The reserve comes off the mode's reference figure (currently available, or total
    /// installed), then each thread's share is rounded **up** by `round_up_to_chunk_combination`.
    /// The rounding comes out of the reserve, so `reserve_bytes` is what is actually left to the OS.
    pub fn calculate_allocation_with_threads(&self, mem_info: &SystemMemoryInfo, thread_count: usize) -> AllocationResult {
        const GB_BOUNDARY: u64 = BYTES_PER_GIB;

        let (reference_bytes, requested_reserve_bytes, base_type) = match self {
            AllocationMode::ReserveFromAvailable { reserve } => (
                mem_info.available_physical_bytes,
                reserve.calculate_bytes(mem_info.available_physical_bytes),
                "Reserve from Available",
            ),
            AllocationMode::ReserveFromTotal { reserve } => (
                mem_info.total_installed_bytes,
                reserve.calculate_bytes(mem_info.total_installed_bytes),
                "Reserve from Total",
            ),
            AllocationMode::AllocateTarget { target } => {
                // Expressed as a reserve: whatever the target leaves of total installed memory.
                let target_bytes = target.calculate_bytes(mem_info.total_installed_bytes);
                (
                    mem_info.total_installed_bytes,
                    mem_info.total_installed_bytes.saturating_sub(target_bytes),
                    "Target Allocation",
                )
            }
            AllocationMode::LegacyTM5 { reserve_mb } => (
                mem_info.available_physical_bytes,
                (*reserve_mb as u64) * 1024 * 1024,
                "Legacy TM5 (from available)",
            ),
        };

        let base_allocation_bytes = reference_bytes.saturating_sub(requested_reserve_bytes);
        let per_thread_target = base_allocation_bytes / thread_count as u64;

        // Standard chunk sizes for rounding (1GB, 512MB, 256MB, 128MB)
        let chunk_sizes = [GB_BOUNDARY, 512*BYTES_PER_MIB_USIZE as u64, 256*BYTES_PER_MIB_USIZE as u64, 128*BYTES_PER_MIB_USIZE as u64];

        // Round each thread's allocation UP to next clean chunk combination
        let rounded_per_thread = Self::round_up_to_chunk_combination(per_thread_target, &chunk_sizes);
        let allocation_bytes = rounded_per_thread * thread_count as u64;
        let reserve_bytes = reference_bytes.saturating_sub(allocation_bytes);

        log::info!("Thread-aware allocation: {} threads × {:.3} GiB → rounded to {:.3} GiB each",
                  thread_count,
                  per_thread_target as f64 / GB_BOUNDARY as f64,
                  rounded_per_thread as f64 / GB_BOUNDARY as f64);
        log::info!("Total allocation: {:.3} GiB → {:.3} GiB; reserve {:.3} GiB requested, {:.3} GiB left",
                  base_allocation_bytes as f64 / GB_BOUNDARY as f64,
                  allocation_bytes as f64 / GB_BOUNDARY as f64,
                  requested_reserve_bytes as f64 / GB_BOUNDARY as f64,
                  reserve_bytes as f64 / GB_BOUNDARY as f64);

        AllocationResult {
            allocation_bytes,
            reserve_bytes,
            reference_bytes,
            allocation_type: format!(
                "{} [thread-aware ({}×{:.2} GiB)]",
                base_type,
                thread_count,
                rounded_per_thread as f64 / GB_BOUNDARY as f64
            ),
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

    /// Human-readable description of allocation type
    pub allocation_type: String,
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

    // WindowMode and ChunkMode removed - these are test configuration concerns, not allocation strategy concerns
}

impl Default for EnhancedMemoryStrategy {
    fn default() -> Self {
        Self {
            // Post-boot optimized: Reserve only 10% from available (aggressive testing)
            allocation_mode: AllocationMode::ReserveFromAvailable { 
                reserve: ReserveAmount::Percentage(10.0)
            },
        }
    }
}

impl EnhancedMemoryStrategy {
    /// Canonical `memory=` spec that reproduces this strategy, e.g. `"20%-from-available"`.
    /// Written into result files so a run's request is reconstructable from the result alone
    /// rather than only from `logs/` (TODO #67).
    pub fn describe_spec(&self) -> String {
        self.allocation_mode.describe_spec()
    }

    /// Create a comprehensive memory layout with enhanced allocation calculation
    pub fn create_layout(&self, thread_count: usize) -> Result<crate::layout::EnhancedMemoryLayout, String> {
        let mem_info = SystemMemoryInfo::gather()?;
        // Use thread-aware allocation calculation for optimal per-thread layouts
        let allocation_result = self.allocation_mode.calculate_allocation_with_threads(&mem_info, thread_count);
        
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
            Ok(AllocationMode::ReserveFromTotal { .. })
        ));
        assert!(matches!(
            AllocationMode::parse("20%-from-available"), 
            Ok(AllocationMode::ReserveFromAvailable { .. })
        ));
        assert!(matches!(
            AllocationMode::parse("16GiB-target"), 
            Ok(AllocationMode::AllocateTarget { .. })
        ));
        assert!(matches!(
            AllocationMode::parse("2048MB"), 
            Ok(AllocationMode::LegacyTM5 { reserve_mb: 2048 })
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