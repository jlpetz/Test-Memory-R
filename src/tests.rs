use crate::ErrorMode;
use crate::cache::{CacheInfo, SystemInfo};
use crate::driver::MemoryType;
use crate::constants::{MB, MB_16, MB_32, MB_64, MB_F64, KB, PAGE_SIZE_4KB};
use std::simd::*; // Docs here https://doc.rust-lang.org/std/simd/index.html
use std::simd::cmp::SimdPartialEq;
use std::sync::atomic::Ordering;
use std::sync::OnceLock;

const CACHE_BUSTING_STRIDE: usize = PAGE_SIZE_4KB;

// Test memory configuration enums (moved from layout.rs - these are test concerns, not allocation concerns)
#[derive(Debug, Clone)]
pub enum WindowMode {
    /// Use entire per-thread allocation (no window — sweep all memory).
    FullAllocation,
    /// Tier-aware sizing. Target string parsed via `CacheTarget::parse()`
    /// (e.g. "L3/2", "L3*4", "DRAM*8"). Divides per-thread share for L3,
    /// per-SMT-sibling share for L1/L2. Calibration-aware when tmr-cfg.json data exists.
    Cache { target: CacheTarget },
    /// Coarse `(L1+L2+L3) × fraction` working-set size. NOT tier-aware, NOT thread-aware.
    /// Originally designed as a "spill-the-whole-hierarchy" floor (multiplier > 1).
    /// Use `Cache` for precise tier targeting; this is for portable cross-machine ratios.
    CacheTotal { fraction: f64 },
    /// Hard-coded byte size. Replaces former FixedSize (MB) and FixedBytes.
    /// JSON: "absolute" mode with size string "880MB", "4GiB", "448B", etc.
    Absolute { size_bytes: usize },
}

impl WindowMode {
    /// Get target level name for logging (e.g., "L1", "L2", "L3", "DRAM", "DRAM-Full", or "Memory")
    pub fn target_level_name(&self) -> &'static str {
        match self {
            WindowMode::Cache { target } => target.level_name(),
            WindowMode::FullAllocation => "DRAM",
            WindowMode::Absolute { .. } => "Memory",
            WindowMode::CacheTotal { .. } => "Cache",
        }
    }
}

/// Parse a size string like "880MB", "4GiB", "448B", "64KiB" into bytes.
///
/// Supported suffixes (case-insensitive):
/// - `B` — bytes
/// - `KB` = 1000 / `KiB` = 1024
/// - `MB` = 1000² / `MiB` = 1024²
/// - `GB` = 1000³ / `GiB` = 1024³
///
/// **Default for unsuffixed values is MB** (TM5 backward-compat: `testing_window_size_mb=64`
/// becomes `"64"` which means 64 MiB). Pure numeric strings should pass through cleanly.
///
/// Returns Err with a human-readable reason on malformed input.
pub fn parse_size_string(s: &str) -> Result<usize, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty size string".to_string());
    }

    // Find where the numeric prefix ends
    let split_idx = s
        .find(|c: char| !c.is_ascii_digit() && c != '.' && c != '_')
        .unwrap_or(s.len());
    let (num_part, suffix) = s.split_at(split_idx);
    let num_part = num_part.replace('_', "");
    let suffix = suffix.trim().to_ascii_uppercase();

    let value: f64 = num_part
        .parse()
        .map_err(|_| format!("invalid number in size string: '{}'", s))?;
    if value < 0.0 {
        return Err(format!("size cannot be negative: '{}'", s));
    }

    let multiplier: u64 = match suffix.as_str() {
        "" => 1024 * 1024, // unsuffixed defaults to MiB (TM5 compat)
        "B" => 1,
        "KB" => 1000,
        "KIB" | "K" => 1024,
        "MB" => 1000 * 1000,
        "MIB" | "M" => 1024 * 1024,
        "GB" => 1000 * 1000 * 1000,
        "GIB" | "G" => 1024 * 1024 * 1024,
        other => return Err(format!("unknown size suffix '{}' in '{}'", other, s)),
    };

    Ok((value * multiplier as f64) as usize)
}

/// Cache level targeting for window/chunk sizing.
///
/// Each tier accepts a `scale: f64` interpreted relative to that tier's natural size:
/// - L1/L2: per-core size ÷ active SMT siblings × scale
/// - L3: per-thread share (total L3 ÷ thread_count) × scale
/// - DRAM: total L3 × scale (no thread divisor — each thread independently spills)
///
/// `DRAMFull` is a sentinel meaning "use the full thread allocation" (no scale).
///
/// Sanity range for scale: 0.01..=100.0. Both `*N` and `/N` syntax accepted in parser
/// (e.g. `L3/2` → scale 0.5; `L3*0.5` → scale 0.5; `DRAM*8` → scale 8.0; `DRAM/2` → scale 0.5).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CacheTarget {
    /// Target L1 data cache (per-core, ÷ SMT siblings).
    L1 { scale: f64 },
    /// Target L2 cache (per-core, ÷ SMT siblings).
    L2 { scale: f64 },
    /// Target L3 cache (shared, ÷ thread_count).
    L3 { scale: f64 },
    /// Target DRAM. Per-thread working set = total L3 × scale.
    /// No thread divisor — sharing L3 across threads would shrink the per-thread region.
    DRAM { scale: f64 },
    /// Sentinel: use the entire per-thread allocation.
    DRAMFull,
}

/// Minimum and maximum sanity bounds for cache target scale values.
const CACHE_SCALE_MIN: f64 = 0.01;
const CACHE_SCALE_MAX: f64 = 100.0;

impl CacheTarget {
    /// L1 with default scale 0.5 (50% of per-core L1).
    pub const L1_DEFAULT: Self = Self::L1 { scale: 0.5 };
    /// L2 with default scale 0.5 (50% of per-core L2).
    pub const L2_DEFAULT: Self = Self::L2 { scale: 0.5 };
    /// L3 with default scale 0.5 (50% of per-thread L3 share).
    pub const L3_DEFAULT: Self = Self::L3 { scale: 0.5 };
    /// DRAM with default scale 4.0 (per-thread = total L3 × 4).
    pub const DRAM_DEFAULT: Self = Self::DRAM { scale: 4.0 };
    /// DRAM-Full uses the entire thread allocation.
    pub const DRAM_FULL_DEFAULT: Self = Self::DRAMFull;

    /// Get the scale for this target (None for DRAMFull which has no scale).
    pub fn scale(&self) -> Option<f64> {
        match self {
            CacheTarget::L1 { scale }
            | CacheTarget::L2 { scale }
            | CacheTarget::L3 { scale }
            | CacheTarget::DRAM { scale } => Some(*scale),
            CacheTarget::DRAMFull => None,
        }
    }

    /// Map CacheTarget to the corresponding calibration CacheTier
    fn to_calibration_tier(self) -> Option<crate::calibration::CacheTier> {
        match self {
            CacheTarget::L1 { .. } => Some(crate::calibration::CacheTier::L1),
            CacheTarget::L2 { .. } => Some(crate::calibration::CacheTier::L2),
            CacheTarget::L3 { .. } => Some(crate::calibration::CacheTier::L3),
            CacheTarget::DRAM { .. } => Some(crate::calibration::CacheTier::Dram),
            CacheTarget::DRAMFull => None, // Always uses full allocation
        }
    }

    /// Calculate window size using calibration data (measured tier boundaries).
    /// Returns None if calibration data is unavailable or missing the needed tier.
    fn calculate_from_calibration(
        &self,
        cal: &crate::calibration::CalibrationResults,
        _cache_info: &CacheInfo,
        thread_count: usize,
    ) -> Option<usize> {
        let tier = self.to_calibration_tier()?;
        let tier_result = cal.tiers.get(&tier)?;

        // The calibrated optimal_size is the largest working set that stays in this tier
        // (single-threaded measurement). Only topology sharing matters — no CPUID heuristics.
        // Final 64-byte alignment is applied downstream in TestMemoryConfig::calculate_window_size.
        let calibrated = tier_result.optimal_size as f64;

        let size = match self {
            CacheTarget::L1 { scale } | CacheTarget::L2 { scale } => {
                // Per-core caches shared with SMT sibling
                let per_thread = calibrated / get_active_threads_per_core() as f64;
                (per_thread * scale) as usize
            }
            CacheTarget::L3 { scale } => {
                // Shared across all cores — divide by thread count
                let per_thread = calibrated / thread_count.max(1) as f64;
                (per_thread * scale) as usize
            }
            CacheTarget::DRAM { scale } => {
                // DRAM: each thread's working set is sized off the L3-spill threshold
                // (calibrated total L3). No thread divisor — sharing L3 across threads
                // would make the per-thread region too small for a clean DRAM measurement
                // under multi-thread cache competition.
                let cal_l3 = cal.tiers.get(&crate::calibration::CacheTier::L3)
                    .map(|t| t.optimal_size as f64)
                    .unwrap_or(calibrated);
                (cal_l3 * scale) as usize
            }
            CacheTarget::DRAMFull => return None, // Handled by sentinel
        };

        log::debug!("Calibrated window: {:?} → {} bytes (calibrated_optimal={}, threads={}, smt={})",
            tier, size, calibrated, thread_count, get_active_threads_per_core());

        Some(size)
    }

    /// Calculate actual window size in bytes based on cache info and thread count.
    /// Uses calibration data when available, falls back to CPUID heuristics.
    pub fn calculate_window_size(&self, cache_info: &CacheInfo, thread_count: usize) -> usize {
        // Try calibrated sizing first
        if let Some(cal) = get_calibration_data()
            && let Some(size) = self.calculate_from_calibration(cal, cache_info, thread_count) {
                return size;
            }

        // CPUID-based fallback
        self.calculate_window_size_cpuid(cache_info, thread_count)
    }

    /// CPUID-based window sizing (original heuristic path)
    fn calculate_window_size_cpuid(&self, cache_info: &CacheInfo, thread_count: usize) -> usize {
        match self {
            CacheTarget::L1 { scale } => {
                let per_thread = cache_info.per_core_l1d as f64 / get_active_threads_per_core() as f64;
                (per_thread * scale) as usize
            }
            CacheTarget::L2 { scale } => {
                let per_thread = cache_info.per_core_l2 as f64 / get_active_threads_per_core() as f64;
                (per_thread * scale) as usize
            }
            CacheTarget::L3 { scale } => {
                // For VMs: apply extra /2 factor because reported L3 is shared with other VMs
                let vm_factor = if cache_info.is_virtual_machine { 2.0 } else { 1.0 };
                let min_l3_size = cache_info.per_core_l2 + (64 * 1024);

                let per_thread = if thread_count == 1 {
                    cache_info.l3_cache as f64 / vm_factor
                } else {
                    (cache_info.l3_cache as f64 / vm_factor) / thread_count as f64
                };
                ((per_thread * scale) as usize).max(min_l3_size)
            }
            CacheTarget::DRAM { scale } => {
                // No thread divisor — each thread independently exceeds L3 (see calibration path)
                (cache_info.l3_cache as f64 * scale) as usize
            }
            CacheTarget::DRAMFull => {
                usize::MAX
            }
        }
    }

    /// Format the scale fragment, choosing `*N` or `/N` based on which reads cleaner.
    /// Returns "" for scale = 1.0.
    fn format_scale(scale: f64) -> String {
        if (scale - 1.0).abs() < 1e-9 {
            return String::new();
        }
        if scale > 1.0 {
            format!("*{}", trim_float(scale))
        } else {
            // Prefer integer divisor when scale is exactly 1/N for small N
            let inv = 1.0 / scale;
            let inv_rounded = inv.round();
            if (inv - inv_rounded).abs() < 1e-6 && (2.0..=1024.0).contains(&inv_rounded) {
                format!("/{}", inv_rounded as u32)
            } else {
                format!("*{}", trim_float(scale))
            }
        }
    }

    /// Get a human-readable name for this target
    pub fn name(&self) -> String {
        match self {
            CacheTarget::L1 { scale } => format!("L1{}", Self::format_scale(*scale)),
            CacheTarget::L2 { scale } => format!("L2{}", Self::format_scale(*scale)),
            CacheTarget::L3 { scale } => format!("L3{}", Self::format_scale(*scale)),
            CacheTarget::DRAM { scale } => format!("DRAM{}", Self::format_scale(*scale)),
            CacheTarget::DRAMFull => "DRAM-Full".to_string(),
        }
    }

    /// Get a human-readable name that reflects actual calculation with thread count
    pub fn name_with_threads(&self, thread_count: usize) -> String {
        self.name_with_context(thread_count, false)
    }

    /// Get a human-readable name that reflects actual calculation with thread count and VM status.
    /// Shows `[cal]` suffix when calibration data is active for this tier.
    pub fn name_with_context(&self, thread_count: usize, is_vm: bool) -> String {
        let has_cal = get_calibration_data()
            .and_then(|cal| {
                let tier = self.to_calibration_tier()?;
                cal.tiers.get(&tier)
            })
            .is_some();
        let suffix = if has_cal { " [cal]" } else { "" };

        match self {
            CacheTarget::L1 { scale } => format!("L1{}{}", Self::format_scale(*scale), suffix),
            CacheTarget::L2 { scale } => format!("L2{}{}", Self::format_scale(*scale), suffix),
            CacheTarget::L3 { scale } => {
                if has_cal {
                    // Calibrated: show thread count division (calibration optimal_size already
                    // captures the true L3 ceiling, so the only post-divisor is thread sharing)
                    let scale_frag = Self::format_scale(*scale);
                    if thread_count == 1 {
                        format!("L3{} [cal]", scale_frag)
                    } else {
                        format!("L3/{}{} [cal]", thread_count, scale_frag)
                    }
                } else {
                    let vm_factor = if is_vm { 2 } else { 1 };
                    let scale_frag = Self::format_scale(*scale);
                    let total_div = if thread_count == 1 { vm_factor } else { thread_count * vm_factor };
                    if total_div == 1 {
                        format!("L3{}", scale_frag)
                    } else {
                        format!("L3/{}{}", total_div, scale_frag)
                    }
                }
            }
            CacheTarget::DRAM { scale } => format!("DRAM{}{}", Self::format_scale(*scale), suffix),
            CacheTarget::DRAMFull => "DRAM-Full".to_string(),
        }
    }

    /// Get the base level name (without scale)
    pub fn level_name(&self) -> &'static str {
        match self {
            CacheTarget::L1 { .. } => "L1",
            CacheTarget::L2 { .. } => "L2",
            CacheTarget::L3 { .. } => "L3",
            CacheTarget::DRAM { .. } => "DRAM",
            CacheTarget::DRAMFull => "DRAM-Full",
        }
    }

    /// Parse from string like "L1", "L1/2", "L2*0.8", "L3/4", "DRAM", "DRAM*8", "DRAM/2", "DRAM-FULL".
    /// Both `/N` and `*N` operators are accepted on every tier; values may be decimal.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim().to_uppercase();

        // DRAM-FULL must come before DRAM (longer prefix first). It's scaleless.
        if s == "DRAM-FULL" || s == "DRAMFULL" || s == "RAM-FULL" || s == "RAMFULL" {
            return Some(CacheTarget::DRAMFull);
        }

        if let Some(rest) = s.strip_prefix("L1") {
            let scale = Self::parse_scale(rest).unwrap_or(0.5);
            Some(CacheTarget::L1 { scale })
        } else if let Some(rest) = s.strip_prefix("L2") {
            let scale = Self::parse_scale(rest).unwrap_or(0.5);
            Some(CacheTarget::L2 { scale })
        } else if let Some(rest) = s.strip_prefix("L3") {
            let scale = Self::parse_scale(rest).unwrap_or(0.5);
            Some(CacheTarget::L3 { scale })
        } else if let Some(rest) = s.strip_prefix("DRAM") {
            let scale = Self::parse_scale(rest).unwrap_or(4.0);
            Some(CacheTarget::DRAM { scale })
        } else if s == "RAM" {
            // Alias for DRAM
            Some(CacheTarget::DRAM_DEFAULT)
        } else {
            None
        }
    }

    /// Parse a scale fragment like `/2`, `*8`, `*0.8`. Empty string returns None
    /// (so caller can apply the tier-specific default). Returns None for out-of-range
    /// values (must be in CACHE_SCALE_MIN..=CACHE_SCALE_MAX) or unparseable input.
    fn parse_scale(s: &str) -> Option<f64> {
        let s = s.trim();
        if s.is_empty() {
            return None;
        }
        let (op, rest) = match s.chars().next()? {
            '/' => ('/', &s[1..]),
            '*' => ('*', &s[1..]),
            _ => return None,
        };
        let n: f64 = rest.trim().parse().ok()?;
        if !n.is_finite() || n <= 0.0 {
            return None;
        }
        let scale = if op == '/' { 1.0 / n } else { n };
        if (CACHE_SCALE_MIN..=CACHE_SCALE_MAX).contains(&scale) {
            Some(scale)
        } else {
            None
        }
    }
}

/// Format a float for display, trimming trailing zeros (e.g., 2.0 → "2", 0.5 → "0.5").
fn trim_float(v: f64) -> String {
    if (v - v.round()).abs() < 1e-9 {
        format!("{}", v.round() as i64)
    } else {
        // Use up to 4 decimal places, then trim trailing zeros
        let s = format!("{:.4}", v);
        let s = s.trim_end_matches('0').trim_end_matches('.');
        s.to_string()
    }
}

#[derive(Debug, Clone)]
pub enum ChunkMode {
    /// Per-test heuristic chunk sizing.
    Auto,
    /// Tier-aware sizing — same target syntax as `WindowMode::Cache`.
    /// L3/N keeps writes warm through verify; DRAM*N forces eviction (refresh stress).
    Cache { target: CacheTarget },
    /// `(L1+L2+L3) × fraction`. NOT tier-aware. Coarse parity with `WindowMode::CacheTotal`.
    CacheTotal { fraction: f64 },
    /// Hard-coded byte size. Replaces former FixedSize (MB).
    Absolute { size_bytes: usize },
    /// Fraction of the resolved window size. Independent of cache hierarchy.
    Fraction { fraction: f64 },
}

/// Controls error checking frequency within tests using power-of-2 intervals
/// for optimal hot loop performance with bitwise operations
#[derive(Debug, Clone, Copy)]
pub struct ErrorCheckInterval {
    /// Power-of-2 shift for check interval (0 = every op, 9 = every 512 ops, etc.)
    /// Special value: u32::MAX = check only at chunk boundaries
    pub power_of_two_shift: u32,
}

impl ErrorCheckInterval {
    /// Check every element (shift = 0, check every 2^0 = 1 operation)
    pub const EVERY_ELEMENT: Self = Self { power_of_two_shift: 0 };
    
    /// Check only at chunk boundaries (shift = MAX, effectively never within chunk)
    pub const PER_CHUNK: Self = Self { power_of_two_shift: u32::MAX };
    
    /// Create from TM5 Parameter value, rounding to nearest power-of-2
    pub fn from_parameter(param: u32) -> Self {
        match param {
            0 => Self::PER_CHUNK,  // TM5 Parameter=0 means full chunk
            1 => Self::EVERY_ELEMENT,  // TM5 Parameter=1 means every element
            n => {
                // Round up to nearest power of 2 and get shift
                let power_of_2 = n.next_power_of_two();
                let shift = power_of_2.trailing_zeros();
                
                if n != power_of_2 {
                    log::debug!("Parameter {} rounded to {} (2^{}) for performance", 
                              n, power_of_2, shift);
                }
                
                Self { power_of_two_shift: shift }
            }
        }
    }
    
    /// Get the mask for bitwise AND checking in hot loops
    /// Returns None for PER_CHUNK mode (no checking within chunk)
    #[inline]
    pub fn get_check_mask(&self) -> Option<u32> {
        if self.power_of_two_shift >= 31 {
            None  // No checking within chunk
        } else {
            Some((1u32 << self.power_of_two_shift) - 1)
        }
    }
}

// Global system info that gets detected once at runtime
static SYSTEM_INFO: OnceLock<SystemInfo> = OnceLock::new();

// Global calibration data — set from tmr-cfg.json on startup, used by CacheTarget sizing
static CALIBRATION_DATA: OnceLock<Option<crate::calibration::CalibrationResults>> = OnceLock::new();

// Active SMT threads per physical core (1 if cputype=cores or no HT, 2 if cputype=threads with HT)
// Set by main.rs after topology + cputype are resolved. Defaults to 1 if not set.
static ACTIVE_THREADS_PER_CORE: OnceLock<usize> = OnceLock::new();

/// Set the global calibration data (called once at startup from tmr-cfg.json)
pub fn set_calibration_data(data: Option<crate::calibration::CalibrationResults>) {
    let _ = CALIBRATION_DATA.set(data);
}

/// Get the global calibration data if available
pub fn get_calibration_data() -> Option<&'static crate::calibration::CalibrationResults> {
    CALIBRATION_DATA.get().and_then(|opt| opt.as_ref())
}

/// Set the active SMT threads per core (called once by main.rs after topology detection)
/// - 2 if hyperthreading enabled AND cputype=threads (both siblings active)
/// - 1 if cputype=cores (only one thread per physical core) or no SMT
pub fn set_active_threads_per_core(count: usize) {
    let _ = ACTIVE_THREADS_PER_CORE.set(count);
}

/// Get the active SMT threads per core (defaults to 1 if not set)
pub fn get_active_threads_per_core() -> usize {
    ACTIVE_THREADS_PER_CORE.get().copied().unwrap_or(1)
}

pub fn get_system_info() -> &'static SystemInfo {
    SYSTEM_INFO.get_or_init(|| {
        let system_info = SystemInfo::detect();
        system_info.print_system_info();
        system_info
    })
}

pub fn get_cache_info() -> &'static CacheInfo {
    get_system_info().get_cache_info()
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub enum TestAction {
    Read,
    Write,
    ReadWrite,
    WriteVerify,
    Copy,
    Verify,
    WriteWaitVerify,
    CacheBusting,
    RandomAccess,
    StuckBitTest,
    Latency,
}

impl TestAction {
    pub fn label(&self) -> &'static str {
        match self {
            TestAction::Read => "Read",
            TestAction::Write => "Write",
            TestAction::ReadWrite => "Read/Write",
            TestAction::WriteVerify => "Write + Verify",
            TestAction::Copy => "Copy",
            TestAction::Verify => "Verify",
            TestAction::WriteWaitVerify => "Write + Wait + Verify",
            TestAction::CacheBusting => "Cache Busting",
            TestAction::RandomAccess => "Random Access",
            TestAction::StuckBitTest => "Stuck Bit Test",
            TestAction::Latency => "Latency Measurement",
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone)]
pub struct TestStats {
    pub name: &'static str,
    pub action: TestAction,
    pub bytes_processed: usize,
    pub elapsed_ms: u128,
    pub thread_id: usize,
    pub error_count: u64,
    pub total_operations: u64,  // Total operation count (lightweight tracking)
    pub cycles_completed: u32,  // Actual cycles completed
    pub cycles_planned: Option<u32>, // Planned cycles (None = unlimited)
    pub stopped_by_time_limit: bool, // True if stopped due to time, false if stopped due to cycle limit
}

/// Optional progress reporting structure for tests
/// Allows tests to report progress back to the coordinator at checkpoints
#[derive(Debug)]
pub struct TestProgress {
    pub cycles_completed: std::sync::atomic::AtomicU32,
    pub bytes_processed: std::sync::atomic::AtomicU64,
    pub errors_found: std::sync::atomic::AtomicU64,
    pub last_update_ms: std::sync::atomic::AtomicU64,  // Timestamp of last update
}

impl Default for TestProgress {
    fn default() -> Self {
        Self::new()
    }
}

impl TestProgress {
    pub fn new() -> Self {
        use std::sync::atomic::AtomicU32;
        use std::sync::atomic::AtomicU64;

        Self {
            cycles_completed: AtomicU32::new(0),
            bytes_processed: AtomicU64::new(0),
            errors_found: AtomicU64::new(0),
            last_update_ms: AtomicU64::new(0),
        }
    }
}

/// Detailed operation breakdown calculated post-test from total_operations
#[derive(Debug, Clone)]
pub struct DetailedOperationCount {
    pub total_reads: u64,
    pub total_writes: u64,
    pub total_verifies: u64,
    pub total_simd_ops: u64,
    pub total_fence_ops: u64,
    pub total_cache_ops: u64,
    pub simd_type: SIMDType,
    pub access_pattern: AccessPattern,
}

#[derive(Debug, Clone)]
pub enum SIMDType {
    None,
    SSE2_128,
    AVX2_256,
    AVX512_512,
}

#[derive(Debug, Clone)]
pub enum AccessPattern {
    Sequential,
    Strided(usize),
    Random,
    Mirror,
    BlockCopy,
    CacheBusting,
}

/// Operation metadata for each test - defines what constitutes one "operation"
#[derive(Debug, Clone)]
pub struct OperationMetadata {
    // Operations per single "operation unit" (typically per loop iteration)
    pub reads_per_op: u64,
    pub writes_per_op: u64,
    pub verifies_per_op: u64,
    pub simd_ops_per_op: u64,
    pub fence_ops_per_op: u64,
    pub cache_ops_per_op: u64,
    
    // Test characteristics
    pub simd_type: SIMDType,
    pub access_pattern: AccessPattern,
    pub memory_coverage: f64,  // Fraction of allocated memory touched per operation
    pub locality_sensitive: bool,
}

// Test execution timing configuration
#[derive(Debug, Clone)]
pub struct TestTiming {
    pub cycles: Option<u32>,           // Number of times to repeat the test (None = unlimited)
    pub duration_secs: Option<u32>,    // Maximum time to run test (None = unlimited)
    pub min_duration_secs: Option<u32>, // Minimum time even if cycles complete
}

impl Default for TestTiming {
    fn default() -> Self {
        Self {
            cycles: Some(1),
            duration_secs: None,
            min_duration_secs: None,
        }
    }
}

impl TestTiming {
    pub fn cycles_only(cycles: u32) -> Self {
        Self {
            cycles: Some(cycles),
            duration_secs: None,
            min_duration_secs: None,
        }
    }
    
    pub fn duration_only(duration_secs: u32) -> Self {
        Self {
            cycles: None,
            duration_secs: Some(duration_secs),
            min_duration_secs: None,
        }
    }
    
    pub fn hybrid(cycles: u32, max_duration_secs: u32) -> Self {
        Self {
            cycles: Some(cycles),
            duration_secs: Some(max_duration_secs),
            min_duration_secs: None,
        }
    }
    
    pub fn should_continue(&self, current_cycle: u32, elapsed_secs: u32) -> bool {
        // Check minimum duration first
        if let Some(min_secs) = self.min_duration_secs
            && elapsed_secs < min_secs {
                return true;
            }
        
        // Check maximum duration
        if let Some(max_secs) = self.duration_secs
            && elapsed_secs >= max_secs {
                return false;
            }
        
        // Check cycle limit
        if let Some(max_cycles) = self.cycles
            && current_cycle >= max_cycles {
                return false;
            }
        
        // If no limits set, run forever (or until externally stopped)
        true
    }
}

// Three-stage memory configuration with corrected window logic
#[derive(Debug, Clone)]
pub struct TestMemoryConfig {
    pub window_mode: WindowMode,
    pub chunk_mode: ChunkMode,
    pub allow_misaligned: bool,
    pub requires_locality: bool,    // True if test needs temporal locality (small window)
    pub timing: TestTiming,
    pub pattern_mode: Option<u32>,  // TM5 pattern mode
    pub pattern_param0: Option<u64>, // TM5 pattern parameter 0
    pub pattern_param1: Option<u64>, // TM5 pattern parameter 1
    pub memory_type: Option<MemoryType>,
    pub error_check_interval: ErrorCheckInterval,  // Controls error checking frequency
    pub tsc_frequency_ghz: f64,     // TSC frequency detected at startup (for latency tests)
    pub thread_count: usize,        // Total thread count for cache-aware window calculations
    /// v2: Correctly interpreted TM5 parameter context (stride, subblocks, page stride).
    /// None for v1 tests or TMR-native configs that don't originate from TM5.
    pub parameter_context: Option<crate::config::TestParameterContext>,
    /// Cache line size in bytes (from CacheInfo, typically 64). Used by TM5-faithful pattern
    /// modes for cache-line-boundary complement toggle and page evolution.
    pub cache_line_bytes: usize,
    /// Number of verify passes per cycle (TM5 write-then-multi-read).
    /// TM5 typically uses dLoopCounter=5. Default 1 = single verify per cycle.
    pub verify_reps: u32,
    /// Number of test operation repetitions per cycle (e.g., MirrorMove mirror round-trips).
    /// Default 1 = single test op per cycle.
    pub test_reps: u32,
    /// Number of write+verify cycles per chunk. TM5 SimpleTest uses ST_WriteReadCycles=4:
    /// each chunk gets (1 write + 5 reads) × 4 = tight repeated access stress.
    /// Default 1 for non-TM5 tests. Set to 4 for TM5-faithful SimpleTest behavior.
    pub write_read_cycles: u32,
    /// Skip the init phase (dependent mode). When true, the harness assumes a prior test
    /// in the plan already wrote the expected patterns. The init_fn closure is still provided
    /// (for pattern knowledge / error repair) but not called at startup.
    /// Plan validation ensures the required pattern gen test appeared earlier.
    pub skip_init: bool,
    /// Flush each chunk out of the cache hierarchy (CLFLUSHOPT + MFENCE) between the write and
    /// verify phases, so the verify **round-trips through DRAM** instead of reading the
    /// still-hot copy the test just wrote (TODO #59).
    ///
    /// This is the user-mode replacement for UC/WC driver memory: without it, a flipped DRAM
    /// bit can be masked by a valid cached line, and whether a verify reaches DRAM at all is an
    /// accident of chunk-size-vs-cache rather than a guarantee. Costs real bandwidth (the reads
    /// become cold), which is the point — enable it on tests whose job is to prove what actually
    /// landed in DRAM.
    ///
    /// Default `false`: existing tests keep their current behaviour and timings.
    pub flush_before_verify: bool,
}

impl TestMemoryConfig {
    pub fn new(window_mode: WindowMode, chunk_mode: ChunkMode, allow_misaligned: bool, requires_locality: bool) -> Self {
        Self {
            window_mode,
            chunk_mode,
            allow_misaligned,
            requires_locality,
            timing: TestTiming::default(),
            pattern_mode: None,
            pattern_param0: None,
            pattern_param1: None,
            memory_type: None,
            error_check_interval: ErrorCheckInterval::PER_CHUNK,  // Default: check at chunk boundaries
            tsc_frequency_ghz: 0.0,  // MUST be set from detected CacheInfo before running latency tests
            thread_count: 1,  // Default to 1, should be set by runner for accurate cache calculations
            parameter_context: None,  // v2: set by config loader for TM5-derived configs
            cache_line_bytes: 64,  // Default 64, set from CacheInfo at startup
            verify_reps: 1,  // Default 1, set from TM5 config or CLI
            test_reps: 1,  // Default 1, set from TM5 config or CLI
            write_read_cycles: 1,  // Default 1, TM5 SimpleTest uses 4
            skip_init: false,  // Default: always run init (independent mode)
            flush_before_verify: false,  // Default: keep existing (cache-resident) verify behaviour
        }
    }

    /// Builder: force the verify phase to read DRAM by flushing each chunk out of cache first
    /// (CLFLUSHOPT + MFENCE between write and verify). See `flush_before_verify`.
    pub fn with_flush_before_verify(mut self, flush: bool) -> Self {
        self.flush_before_verify = flush;
        self
    }

    /// Get operation metadata for a specific test
    pub fn get_operation_metadata(&self, test_name: &str) -> OperationMetadata {
        match test_name {
            "Mem-StuckBit" | "Mem-StuckBit-Flush" => OperationMetadata {
                reads_per_op: 3,  // 3 verification reads per cycle per element
                writes_per_op: 3,  // 3 pattern writes per cycle per element
                verifies_per_op: 3,  // Same as reads for this test
                simd_ops_per_op: 0,
                fence_ops_per_op: 1,  // Per cycle, not per element
                cache_ops_per_op: 0,
                simd_type: SIMDType::None,
                access_pattern: AccessPattern::Sequential,
                memory_coverage: 1.0,
                locality_sensitive: false,
            },
            "Mem-StuckBit128" | "Mem-StuckBit-Flush128" => OperationMetadata {
                reads_per_op: 3,  // 3 verification reads per cycle per element
                writes_per_op: 3,  // 3 pattern writes per cycle per element
                verifies_per_op: 3,  // Same as reads for this test
                simd_ops_per_op: 6,  // 3 loads + 3 stores per cycle per u64x2
                fence_ops_per_op: 1,  // Per cycle, not per element
                cache_ops_per_op: 0,
                simd_type: SIMDType::SSE2_128,
                access_pattern: AccessPattern::Sequential,
                memory_coverage: 1.0,
                locality_sensitive: false,
            },
            "Mem-StuckBit256" | "Mem-StuckBit-Flush256" => OperationMetadata {
                reads_per_op: 3,  // 3 verification reads per cycle per element
                writes_per_op: 3,  // 3 pattern writes per cycle per element
                verifies_per_op: 3,  // Same as reads for this test
                simd_ops_per_op: 6,  // 3 loads + 3 stores per cycle per u64x4
                fence_ops_per_op: 1,  // Per cycle, not per element
                cache_ops_per_op: 0,
                simd_type: SIMDType::AVX2_256,
                access_pattern: AccessPattern::Sequential,
                memory_coverage: 1.0,
                locality_sensitive: false,
            },
            "Mem-StuckBit512" | "Mem-StuckBit-Flush512" => OperationMetadata {
                reads_per_op: 3,  // 3 verification reads per cycle per element
                writes_per_op: 3,  // 3 pattern writes per cycle per element
                verifies_per_op: 3,  // Same as reads for this test
                simd_ops_per_op: 6,  // 3 loads + 3 stores per cycle per u64x8
                fence_ops_per_op: 1,  // Per cycle, not per element
                cache_ops_per_op: 0,
                simd_type: SIMDType::AVX512_512,
                access_pattern: AccessPattern::Sequential,
                memory_coverage: 1.0,
                locality_sensitive: false,
            },
            "Mem-SimpleNT-128" | "Mem-SimpleNT-256" | "Mem-SimpleNT-512" | "Mem-SimpleNT-Auto" => OperationMetadata {
                reads_per_op: 1,  // Verify read (regular loads)
                writes_per_op: 1,  // NT store (bypasses cache)
                verifies_per_op: 1,
                simd_ops_per_op: 1,
                fence_ops_per_op: 1,  // sfence per chunk
                cache_ops_per_op: 0,  // NT stores bypass cache
                simd_type: match test_name {
                    "Mem-SimpleNT-512" => SIMDType::AVX512_512,
                    "Mem-SimpleNT-256" => SIMDType::AVX2_256,
                    _ => SIMDType::SSE2_128,
                },
                access_pattern: AccessPattern::Sequential,
                memory_coverage: 1.0,
                locality_sensitive: false,
            },
            "Mem-Refresh" => OperationMetadata {
                reads_per_op: 1,  // Verify read per element
                writes_per_op: 1,  // Pattern write per element
                verifies_per_op: 1,  // Same as reads
                simd_ops_per_op: 0,
                fence_ops_per_op: 1,  // Per cycle
                cache_ops_per_op: 0,
                simd_type: SIMDType::None,
                access_pattern: AccessPattern::Sequential,
                memory_coverage: 1.0,
                locality_sensitive: true,
            },
            "Mem-Refresh128" => OperationMetadata {
                reads_per_op: 1,  // Verify read per element
                writes_per_op: 1,  // Pattern write per element
                verifies_per_op: 1,  // Same as reads
                simd_ops_per_op: 2,  // 1 load + 1 store per u64x2
                fence_ops_per_op: 1,  // Per cycle
                cache_ops_per_op: 0,
                simd_type: SIMDType::SSE2_128,
                access_pattern: AccessPattern::Sequential,
                memory_coverage: 1.0,
                locality_sensitive: true,
            },
            "Mem-Refresh256" => OperationMetadata {
                reads_per_op: 1,  // Verify read per element
                writes_per_op: 1,  // Pattern write per element
                verifies_per_op: 1,  // Same as reads
                simd_ops_per_op: 2,  // 1 load + 1 store per u64x4
                fence_ops_per_op: 1,  // Per cycle
                cache_ops_per_op: 0,
                simd_type: SIMDType::AVX2_256,
                access_pattern: AccessPattern::Sequential,
                memory_coverage: 1.0,
                locality_sensitive: true,
            },
            "Mem-Refresh512" => OperationMetadata {
                reads_per_op: 1,  // Verify read per element
                writes_per_op: 1,  // Pattern write per element
                verifies_per_op: 1,  // Same as reads
                simd_ops_per_op: 2,  // 1 load + 1 store per u64x8
                fence_ops_per_op: 1,  // Per cycle
                cache_ops_per_op: 0,
                simd_type: SIMDType::AVX512_512,
                access_pattern: AccessPattern::Sequential,
                memory_coverage: 1.0,
                locality_sensitive: true,
            },
            "Mem-CacheBust" => OperationMetadata {
                reads_per_op: 1,
                writes_per_op: 1,
                verifies_per_op: 1,
                simd_ops_per_op: 0,
                fence_ops_per_op: 1,
                cache_ops_per_op: 0,
                simd_type: SIMDType::None,
                access_pattern: AccessPattern::CacheBusting,
                memory_coverage: 1.0,  // All memory touched with stride pattern for cache busting
                locality_sensitive: false,
            },
            "Mem-Random" => OperationMetadata {
                reads_per_op: 1,  // Random verification read
                writes_per_op: 0,  // No writes in hot loop
                verifies_per_op: 1,  // Same as reads
                simd_ops_per_op: 0,
                fence_ops_per_op: 0,  // No fences in hot loop
                cache_ops_per_op: 0,
                simd_type: SIMDType::None,
                access_pattern: AccessPattern::Random,
                memory_coverage: 0.1,  // Random coverage varies, use conservative estimate
                locality_sensitive: false,
            },
            "Mem-Stride" => OperationMetadata {
                reads_per_op: 1,
                writes_per_op: 1,
                verifies_per_op: 1,
                simd_ops_per_op: 0,
                fence_ops_per_op: 1,  // Per stride pattern
                cache_ops_per_op: 0,
                simd_type: SIMDType::None,
                access_pattern: AccessPattern::Strided(1024),  // Average stride
                memory_coverage: 0.85,  // Aggregate across all strides
                locality_sensitive: false,
            },
            "Mem-BlockMove" => OperationMetadata {
                reads_per_op: 2,  // Source read + destination verify read
                writes_per_op: 1,  // Destination write
                verifies_per_op: 1,  // Destination verify
                simd_ops_per_op: 0,
                fence_ops_per_op: 1,  // Per cycle
                cache_ops_per_op: 0,
                simd_type: SIMDType::None,
                access_pattern: AccessPattern::BlockCopy,
                memory_coverage: 0.5,  // Uses half of allocation (source to dest)
                locality_sensitive: false,
            },
            // Sequential bandwidth tests — Write
            s if s.starts_with("Spd-") && s.contains("-Write") => OperationMetadata {
                reads_per_op: 0,
                writes_per_op: 1,
                verifies_per_op: 0,
                simd_ops_per_op: 1,
                fence_ops_per_op: if s.contains("DRAM") { 1 } else { 0 },
                cache_ops_per_op: 0,
                simd_type: SIMDType::AVX512_512,
                access_pattern: AccessPattern::Sequential,
                memory_coverage: 1.0,
                locality_sensitive: false,
            },
            // Sequential bandwidth tests — Read
            s if s.starts_with("Spd-") && s.contains("-Read") => OperationMetadata {
                reads_per_op: 1,
                writes_per_op: 0,
                verifies_per_op: 0,
                simd_ops_per_op: 1,
                fence_ops_per_op: 0,
                cache_ops_per_op: 0,
                simd_type: SIMDType::AVX512_512,
                access_pattern: AccessPattern::Sequential,
                memory_coverage: 1.0,
                locality_sensitive: false,
            },
            // Sequential bandwidth tests — Copy
            s if s.starts_with("Spd-") && s.contains("-Copy") => OperationMetadata {
                reads_per_op: 1,
                writes_per_op: 1,
                verifies_per_op: 0,
                simd_ops_per_op: 1,
                fence_ops_per_op: if s.contains("DRAM") { 1 } else { 0 },
                cache_ops_per_op: 0,
                simd_type: SIMDType::AVX512_512,
                access_pattern: AccessPattern::Sequential,
                memory_coverage: 0.5,  // Split-half: read first half, write second half
                locality_sensitive: false,
            },
            _ => panic!("Unknown test '{}' - add explicit metadata to get_operation_metadata()", test_name),
        }
    }

    pub fn with_memory_type(mut self, memory_type: Option<MemoryType>) -> Self {
        self.memory_type = memory_type;
        self
    }
    
    pub fn with_timing(mut self, timing: TestTiming) -> Self {
        self.timing = timing;
        self
    }

    pub fn with_tsc(mut self, tsc_frequency_ghz: f64) -> Self {
        self.tsc_frequency_ghz = tsc_frequency_ghz;
        self
    }

    pub fn with_error_check_interval(mut self, interval: ErrorCheckInterval) -> Self {
        self.error_check_interval = interval;
        self
    }
    
    pub fn with_pattern_config(mut self, mode: Option<u32>, param0: Option<u64>, param1: Option<u64>) -> Self {
        self.pattern_mode = mode;
        self.pattern_param0 = param0;
        self.pattern_param1 = param1;
        self
    }

    pub fn with_thread_count(mut self, thread_count: usize) -> Self {
        self.thread_count = thread_count.max(1);
        self
    }

    pub fn with_parameter_context(mut self, ctx: crate::config::TestParameterContext) -> Self {
        self.parameter_context = Some(ctx);
        self
    }

    pub fn with_skip_init(mut self, skip: bool) -> Self {
        self.skip_init = skip;
        self
    }

    // Calculate window size with corrected logic
    pub fn calculate_window_size(&self, test_name: &str, allocated_size: usize) -> usize {
        let size = match &self.window_mode {
            WindowMode::FullAllocation => {
                // Use full allocation unless test specifically requires locality
                if self.requires_locality {
                    self.calculate_locality_window_size(test_name, allocated_size)
                } else {
                    allocated_size
                }
            }
            WindowMode::Absolute { size_bytes } => {
                (*size_bytes).min(allocated_size)
            }
            WindowMode::CacheTotal { fraction } => {
                let cache_info = get_cache_info();
                let cache_based_size = (cache_info.total_cache as f64 * fraction) as usize;
                cache_based_size.min(allocated_size)
            }
            WindowMode::Cache { target } => {
                // Calculate window size based on cache target using actual thread count
                let cache_info = get_cache_info();
                let calculated = target.calculate_window_size(cache_info, self.thread_count);
                // DRAMFull returns usize::MAX as sentinel to indicate "use full allocation"
                if calculated == usize::MAX {
                    allocated_size
                } else {
                    calculated.min(allocated_size)
                }
            }
        };

        // Round down to 64-byte boundary so SIMD operations (especially NT stores) don't fault.
        // This is the single source of alignment for all window modes — calibrated values are
        // raw measurements and topology divisions can produce non-aligned results, both expected.
        let aligned = (size / 64) * 64;
        if aligned != size {
            log::debug!(
                "{}: window size {} bytes not 64-byte aligned ({:?}) — rounded down to {} bytes",
                test_name, size, self.window_mode, aligned
            );
        }

        aligned
    }
    
    // Calculate locality-specific window for tests that need it
    fn calculate_locality_window_size(&self, test_name: &str, allocated_size: usize) -> usize {
        let cache_info = get_cache_info();

        let optimal_size = match test_name {
            "Mem-CacheBust" => (cache_info.l3_cache / 2).max(cache_info.l2_cache * 4),
            // Refresh now flushes each chunk to DRAM before the per-chunk 64ms
            // sleep, so window size controls how many DRAM cells get a retention
            // check. L3*2 covers far more cells than the old l2*2 while keeping
            // the (per-chunk) sleep count bounded — full-allocation would multiply
            // runtime by the chunk count.
            "Mem-Refresh" => cache_info.l3_cache * 2,
            _ => cache_info.total_cache * 2,
        };

        optimal_size.min(allocated_size)
    }

    // Calculate optimal chunk size with alignment  
    pub fn calculate_chunk_size(&self, test_name: &str, window_size: usize) -> usize {
        let cache_info = get_cache_info();
        
        let raw_chunk_size = match &self.chunk_mode {
            ChunkMode::Absolute { size_bytes } => {
                if self.allow_misaligned {
                    *size_bytes
                } else {
                    align_to_boundary(*size_bytes, cache_info.cache_line_size)
                }
            }
            ChunkMode::Fraction { fraction } => {
                let fraction_size = (window_size as f64 * fraction) as usize;
                if self.allow_misaligned {
                    fraction_size
                } else {
                    align_to_boundary(fraction_size, cache_info.cache_line_size)
                }
            }
            ChunkMode::CacheTotal { fraction } => {
                let cache_based = (cache_info.total_cache as f64 * fraction) as usize;
                if self.allow_misaligned {
                    cache_based
                } else {
                    align_to_boundary(cache_based, cache_info.cache_line_size)
                }
            }
            ChunkMode::Auto => {
                self.calculate_optimal_block_for_test(test_name, window_size, cache_info)
            }
            ChunkMode::Cache { target } => {
                // Reuse the calibration-aware sizing path. DRAMFull returns usize::MAX
                // as a sentinel meaning "use the whole window".
                let calculated = target.calculate_window_size(cache_info, self.thread_count);
                let raw = if calculated == usize::MAX { window_size } else { calculated };
                if self.allow_misaligned {
                    raw
                } else {
                    align_to_boundary(raw, cache_info.cache_line_size)
                }
            }
        };

        // Calculate minimum chunk size considering SIMD operations and variant requirements
        // Extract the relevant variant count from parameter_context (stride_patterns, rng_sequences, subdivisions, copy_directions)
        let variant_count = self.parameter_context.as_ref().map(|ctx| {
            ctx.stride_patterns
                .or(ctx.rng_sequences)
                .or(ctx.subdivisions)
                .or(ctx.copy_directions)
                .unwrap_or(1)
        }).unwrap_or(1);
        let minimum_chunk_size = self.calculate_minimum_chunk_size(test_name, variant_count);
        
        // Cap at window size first
        let window_capped_size = raw_chunk_size.min(window_size);
        
        // Apply minimum size requirements
        let final_chunk_size = window_capped_size.max(minimum_chunk_size);
        
        // Log corrections for user awareness
        if final_chunk_size != raw_chunk_size {
            let raw_mb = raw_chunk_size as f64 / MB as f64;
            let final_mb = final_chunk_size as f64 / MB as f64;
            
            if final_chunk_size > raw_chunk_size {
                log::debug!("🔧 Chunk size corrected for {}: {:.2}MB → {:.2}MB (minimum required for {} variants + SIMD alignment)",
                           test_name, raw_mb, final_mb, variant_count);
            } else {
                log::debug!("🔧 Chunk size capped for {}: {:.2}MB → {:.2}MB (limited by window size)", 
                           test_name, raw_mb, final_mb);
            }
        }
        
        final_chunk_size
    }
    
    fn calculate_minimum_chunk_size(&self, test_name: &str, variant_count: u32) -> usize {
        // SIMD operation size requirements - explicit for each test to catch missing implementations
        let simd_requirement = match test_name {
            "Mem-StuckBit" | "Mem-StuckBit-Flush" => 8,           // Basic u64 operations
            "Mem-StuckBit128" | "Mem-StuckBit-Flush128" => 16,    // 128-bit SIMD operations
            "Mem-StuckBit256" | "Mem-StuckBit-Flush256" => 32,    // 256-bit SIMD operations
            "Mem-StuckBit512" | "Mem-StuckBit-Flush512" => 64,    // 512-bit SIMD operations
            "Mem-SimpleNT-128" => 16,          // 128-bit NT SIMD
            "Mem-SimpleNT-256" => 32,          // 256-bit NT SIMD
            "Mem-SimpleNT-512" => 64,          // 512-bit NT SIMD
            "Mem-SimpleNT-Auto" => 64,         // Auto-dispatched NT SIMD
            "Mem-Refresh" => 8,              // Basic u64 operations
            "Mem-Refresh128" => 16,          // 128-bit SIMD operations
            "Mem-Refresh256" => 32,          // 256-bit SIMD operations
            "Mem-Refresh512" => 64,          // 512-bit SIMD operations
            "Mem-CacheBust" => 64,        // Cache line operations
            "Mem-Random" => 8,       // Basic u64 operations
            "Mem-Stride" => 8,               // Basic u64 operations
            "Mem-BlockMove" => 64,                  // Block operations
            // v2 tests
            "Mem-SimpleV2" => 8,                     // Basic u64 operations (scalar, auto-vectorized)
            "Mem-SimpleV2-128" => 16,                // u64x2 (128-bit)
            "Mem-SimpleV2-256" => 32,                // u64x4 (256-bit)
            "Mem-SimpleV2-512" => 64,                // u64x8 (512-bit)
            "Mem-SimpleV2-Auto" => 64,               // Auto-dispatched, assume AVX-512 possible
            "Mem-MirrorV2" => 8,                     // u64 scalar mirror
            "Mem-MirrorV2-128" => 16,                // u64x2 (128-bit)
            "Mem-MirrorV2-256" => 32,                // u64x4 (256-bit)
            "Mem-MirrorV2-512" => 64,                // u64x8 (512-bit)
            "Mem-MirrorV2-Auto" => 64,               // Auto-dispatched, assume AVX-512 possible
            // Bench-Init and Bench-Verify tests (all scalar u64)
            "Bench-Init-TM5-0" | "Bench-Init-TM5-1" | "Bench-Init-TM5-2"
            | "Bench-Init-TMR-0" | "Bench-Init-TMR-1" | "Bench-Init-TMR-2" | "Bench-Init-TMR-3"
            | "Bench-Verify-TM5-0" | "Bench-Verify-TM5-1" | "Bench-Verify-TM5-2"
            | "Bench-Verify-TMR-0" | "Bench-Verify-TMR-1" | "Bench-Verify-TMR-2" | "Bench-Verify-TMR-3" => 8,
            // Sequential bandwidth tests (all SIMD variants: 128/256/512/Auto)
            s if s.starts_with("Spd-") => 64,
            _ => panic!("Unknown test '{}' - add explicit SIMD requirement to calculate_minimum_chunk_size()", test_name),
        };
        
        // Variant division requirement: each variant needs at least simd_requirement bytes
        let variant_requirement = simd_requirement * variant_count.max(1) as usize;

        // Performance minimum: 64KB for reasonable cache behavior
        let performance_minimum = 64 * 1024; // 64KB

        // Ensure result is aligned to u64 boundaries and power-of-2 element count for fast variant operations
        let minimum_bytes = variant_requirement.max(performance_minimum);
        let elements = minimum_bytes / std::mem::size_of::<u64>();
        let power_of_2_elements = elements.next_power_of_two();
        
        power_of_2_elements * std::mem::size_of::<u64>()
    }
    
    fn calculate_optimal_block_for_test(&self, test_name: &str, window_size: usize, cache_info: &CacheInfo) -> usize {
        match test_name {
            // SIMD tests need specific alignments
            "MirrorMove128" => {
                align_to_boundary(MB_16, 128 / 8) // 128-bit alignment
            }
            "MirrorMove256" => {
                align_to_boundary(MB_32, 256 / 8) // 256-bit alignment  
            }
            "MirrorMove512" => {
                align_to_boundary(MB_64, 512 / 8) // 512-bit alignment
            }
            
            // Full memory tests should use large blocks for efficiency
            "StuckBitTest" | "FullMemoryPattern" => {
                align_to_boundary(window_size / 16, cache_info.cache_line_size).max(MB)
            }
            
            // Cache tests use cache-line aligned blocks
            "CacheBusting" => {
                align_to_boundary(MB, cache_info.cache_line_size)
            }
            
            // Small blocks for refresh testing
            "RefreshStable" => {
                align_to_boundary(512 * KB, cache_info.cache_line_size)
            }
            
            _ => {
                if self.allow_misaligned {
                    8 * MB // 8MB default unaligned
                } else {
                    align_to_boundary(8 * MB, cache_info.cache_line_size)
                }
            }
        }
    }

    // Ensure window size is multiple of block size
    pub fn align_window_to_blocks(&self, window_size: usize, block_size: usize) -> usize {
        if block_size == 0 || window_size == 0 {
            return window_size;
        }
        
        let aligned_window = (window_size / block_size) * block_size;
        let _was_adjusted = aligned_window != window_size;
        
        // Ensure we have at least one full block
        
        
        if aligned_window < block_size {
            block_size
        } else {
            aligned_window
        }
    }
}

fn align_to_boundary(size: usize, alignment: usize) -> usize {
    size.div_ceil(alignment) * alignment
}

/// Flush a byte range out of the cache hierarchy so subsequent reads round-trip
/// through DRAM. Issues CLFLUSHOPT over each cache line, then a trailing MFENCE
/// to drain all outstanding flushes before any later load can issue.
///
/// This is the user-mode equivalent of UC memory: it defeats cache masking
/// (where a verify-load hits the still-hot L1/L2/L3 copy and silently misses a
/// DRAM bit error). Use it between a write phase and a verify phase whenever the
/// verify must observe what actually landed in DRAM — refresh/bit-fade tests, or
/// any cache-resident working set. See `doc/cache_management.md`.
///
/// CLFLUSHOPT is emitted unconditionally: the startup CPUID gate (`main.rs`)
/// guarantees the feature, so there is no `_mm_clflush` fallback. The function
/// carries `#[target_feature(enable = "clflushopt")]` (gated by the unstable
/// `clflushopt_target_feature`, landed in nightly via rustc PR #157098).
///
/// This flush-only loop uses the `_mm_clflushopt` intrinsic (stdarch PR #2141,
/// now synced into nightly behind `simd_x86_clflushopt`, tracking #157096),
/// NOT inline `asm!`. The intrinsic lowers to a real LLVM `clflushopt` op, so
/// LLVM can unroll and schedule the loop freely; the asm form is an opaque
/// `#APP` block LLVM cannot see through (verified by `--emit asm`: the intrinsic
/// loop unrolls, the asm loop does not). The asm idiom is reserved for a *mixed*
/// flush + NT-store hot loop, where NT stores are themselves asm in stdarch
/// (`doc/nt_stores.md`) and one consistent `#APP` boundary avoids
/// intrinsic→asm→intrinsic `#APP`/`#NO_APP` churn — we have no such loop today.
/// We do NOT use the SSE2 `_mm_clflush` — it emits the slow, globally-serialized
/// CLFLUSH (~15× slower; see `../clflush-test`).
///
/// Not manually unrolled: clflushopt is throughput-bound on its own issue rate,
/// so hand-unroll buys nothing (and is slightly worse at small ranges —
/// benchmarked in `../clflush-test`); LLVM unrolls the intrinsic loop as it sees
/// fit. `cache_line_bytes` comes from the detected `CacheInfo`
/// (e.g. `config.cache_line_bytes`) — do not hardcode 64.
///
/// # Safety
/// `base..base+len_bytes` must be a valid, mapped range for the duration of the call.
#[inline]
#[target_feature(enable = "clflushopt")]
pub unsafe fn flush_range_to_dram(base: *const u8, len_bytes: usize, cache_line_bytes: usize) {
    let line = cache_line_bytes.max(1);
    let line_count = len_bytes.div_ceil(line);
    for i in 0..line_count {
        let addr = unsafe { base.add(i * line) };
        unsafe { std::arch::x86_64::_mm_clflushopt(addr); }
    }
    // Mandatory: CLFLUSHOPT is weakly ordered. Without this fence a verify-load
    // could issue while flushes are still draining and read stale cached data.
    unsafe { std::arch::x86_64::_mm_mfence(); }
}

/// Round down to nearest power of 2 (for optimal bit masking in hot loops)
pub fn round_down_to_power_of_2(size: usize) -> usize {
    if size == 0 { return 0; }
    if size.is_power_of_two() { return size; }
    
    // Find the highest set bit position
    let mut v = size;
    v |= v >> 1;
    v |= v >> 2;
    v |= v >> 4;
    v |= v >> 8;
    v |= v >> 16;
    if std::mem::size_of::<usize>() > 4 {
        v |= v >> 32;
    }
    // v is now the next power of 2 minus 1
    // The power of 2 we want is (v + 1) >> 1
    (v + 1) >> 1
}

/// Calculate ideal chunk size once at test start (power-of-2 elements for fast stream operations)
/// Block sizes are already guaranteed to be powers-of-2, so we just need to ensure chunk elements are power-of-2
pub fn calculate_ideal_chunk_size(config: &TestMemoryConfig, test_name: &str, total_memory_size: usize) -> usize {
    // Get base chunk size from user configuration
    let base_chunk_size = config.calculate_chunk_size(test_name, total_memory_size);
    
    // Convert to elements and ensure power-of-2 for fast stream operations (no remainder calculations needed)
    let base_elements = base_chunk_size / std::mem::size_of::<u64>();
    let power_of_2_elements = base_elements.next_power_of_two();
    
    power_of_2_elements * std::mem::size_of::<u64>()
}

/// Simple clamp chunk size to current block size (blocks are already power-of-2)
#[inline]
pub fn get_safe_chunk_size(ideal_chunk_size: usize, block_size_bytes: usize) -> usize {
    ideal_chunk_size.min(block_size_bytes)
}

/// Represents a block with window-limited test size
/// Used by MultiBlock tests to respect window limits while maintaining power-of-2 alignment
#[derive(Debug)]
pub struct TestBlock<'a> {
    pub block: &'a crate::runner::AllocationBlock,
    pub test_size: usize,  // How much to test (power-of-2, <= block size)
}

/// Prepare blocks for testing with window size limits
/// Returns a list of blocks with their test sizes, ensuring:
/// - Only complete blocks are tested (never split blocks)
/// - Blocks are added until total meets or exceeds window_size
/// - At least one block is tested (even if window < block size)
pub fn prepare_blocks_for_window<'a>(
    blocks: &'a [crate::runner::AllocationBlock],
    window_size: usize,
    test_name: &str,
) -> Vec<TestBlock<'a>> {
    if blocks.is_empty() {
        return Vec::new();
    }

    let mut result = Vec::new();
    let mut accumulated = 0usize;

    // Add complete blocks until we meet or exceed the window size
    // Blocks are pre-sorted by allocator (largest first)
    for block in blocks.iter() {
        let block_size = block.buffer.size();

        // Always test complete blocks (never split)
        result.push(TestBlock {
            block,
            test_size: block_size,
        });
        accumulated += block_size;

        log::debug!(
            "{}: Block {} - size {:.2} MiB, testing complete block",
            test_name,
            result.len() - 1,
            block_size as f64 / MB_F64
        );

        // Stop when we've met or exceeded the window
        if accumulated >= window_size {
            break;
        }
    }

    if accumulated > window_size {
        log::debug!(
            "{}: Block total {:.2} MiB > window {:.2} MiB — processing large single block up to window limit",
            test_name,
            accumulated as f64 / MB_F64,
            window_size as f64 / MB_F64
        );
    }

    result
}

/// Expand total_operations into detailed operation breakdown using test metadata
pub fn expand_operations(
    test_stats: &TestStats,
    metadata: &OperationMetadata,
    memory_size: usize,
) -> DetailedOperationCount {
    // Calculate how many elements were processed based on memory coverage
    let total_elements = memory_size / std::mem::size_of::<u64>();
    let covered_elements = (total_elements as f64 * metadata.memory_coverage) as u64;
    
    // For most tests, total_operations represents total loop iterations
    // Each iteration processes `covered_elements` worth of operations
    let operations_per_element = if covered_elements > 0 {
        test_stats.total_operations / covered_elements.max(1)
    } else {
        test_stats.total_operations
    };
    
    DetailedOperationCount {
        total_reads: operations_per_element * metadata.reads_per_op * covered_elements,
        total_writes: operations_per_element * metadata.writes_per_op * covered_elements,
        total_verifies: operations_per_element * metadata.verifies_per_op * covered_elements,
        total_simd_ops: operations_per_element * metadata.simd_ops_per_op * covered_elements,
        total_fence_ops: operations_per_element * metadata.fence_ops_per_op,  // Per iteration, not per element
        total_cache_ops: operations_per_element * metadata.cache_ops_per_op * covered_elements,
        simd_type: metadata.simd_type.clone(),
        access_pattern: metadata.access_pattern.clone(),
    }
}

/// Aggregate operation counts from multiple threads
pub fn aggregate_operation_counts(counts: &[DetailedOperationCount]) -> DetailedOperationCount {
    if counts.is_empty() {
        return DetailedOperationCount {
            total_reads: 0,
            total_writes: 0,
            total_verifies: 0,
            total_simd_ops: 0,
            total_fence_ops: 0,
            total_cache_ops: 0,
            simd_type: SIMDType::None,
            access_pattern: AccessPattern::Sequential,
        };
    }
    
    let mut aggregated = DetailedOperationCount {
        total_reads: counts.iter().map(|c| c.total_reads).sum(),
        total_writes: counts.iter().map(|c| c.total_writes).sum(),
        total_verifies: counts.iter().map(|c| c.total_verifies).sum(),
        total_simd_ops: counts.iter().map(|c| c.total_simd_ops).sum(),
        total_fence_ops: counts.iter().map(|c| c.total_fence_ops).sum(),
        total_cache_ops: counts.iter().map(|c| c.total_cache_ops).sum(),
        simd_type: counts[0].simd_type.clone(),  // Use first thread's SIMD type
        access_pattern: counts[0].access_pattern.clone(),  // Use first thread's access pattern
    };
    
    // If threads have different SIMD types, use the most advanced one
    for count in counts {
        aggregated.simd_type = match (&aggregated.simd_type, &count.simd_type) {
            (SIMDType::None, other) => other.clone(),
            (_current, SIMDType::AVX512_512) => SIMDType::AVX512_512,
            (SIMDType::SSE2_128, SIMDType::AVX2_256) => SIMDType::AVX2_256,
            (current, _) => current.clone(),
        };
    }
    
    aggregated
}


// === NEW: Full Memory Stuck Bit Test ===
/// # Safety
/// Caller must ensure all blocks are valid for reads/writes.
///
/// StuckBit alternating-bit patterns.
///
/// NOTE (TODO #61): these must NOT be byte-uniform (all 8 bytes equal) or LLVM's
/// LoopIdiomRecognize rewrites the fill loop into `memset`, collapsing every SIMD-width
/// variant to one identical libcall and erasing the width. `0xAA55...`/`0x55AA...` alternate
/// the bytes while staying exact bitwise complements of each other — every bit is still
/// tested as both 1 and 0 across the two phases (the stuck-at coverage is unchanged). The
/// two constants XOR to all-ones (`P1 ^ P2 == !0`), i.e. P2 == !P1: a true inversion test.
pub const STUCKBIT_P1: u64 = 0xAA55AA55AA55AA55u64; // 10101010 01010101 ... (per byte)
pub const STUCKBIT_P2: u64 = 0x55AA55AA55AA55AAu64; // exact complement of P1

/// Generates a StuckBit `_impl` at a given SIMD width. The `_multi` runtime-gate wrapper
/// stays hand-written (it returns zero-stats when the feature is absent); this macro is the
/// hot body it calls.
///
/// 3-phase alternating-bit test per chunk: write P1 → verify → write P2 → verify → write P1 →
/// verify. Verify uses **4 independent accumulator chains** (MLP — see
/// `memory/simd-loop-optimization.md`: single-chain starves memory-level parallelism, worst
/// on Intel 256-bit; N=4 fixes it, free on 128/512). No intermediate `ErrorCheckInterval`
/// mode — on error the chunk trips and TODO #27's two-tier verifier does exact localization.
/// The scaffolding (`TestRunner`) owns window/timer/progress/stats/shutdown; the hot loop is
/// byte-identical across widths modulo the `$simd_type`.
macro_rules! stuck_bit_impl {
    ($fn_name:ident, $test_name:literal, $simd_type:ty, $tf:literal) => {
        #[doc = include_str!("test_fn_safety.md")]
        #[inline]
        #[target_feature(enable = $tf)]
        unsafe fn $fn_name(
            blocks: &[crate::runner::AllocationBlock],
            thread_id: usize,
            error_mode: ErrorMode,
            timing: &TestTiming,
            config: &TestMemoryConfig,
            progress: Option<&TestProgress>,
        ) -> TestStats {
            let test_name = $test_name;
            let lanes = std::mem::size_of::<$simd_type>();
            let (mut runner, test_blocks) = crate::test_scaffolding::TestRunner::new(
                blocks, thread_id, error_mode, timing, config, progress,
                test_name, TestAction::StuckBitTest,
            );

            let p1 = <$simd_type>::splat(STUCKBIT_P1);
            let p2 = <$simd_type>::splat(STUCKBIT_P2);

            // Hoist config flags into locals — never read a struct field inside the hot loop.
            let flush_before_verify = config.flush_before_verify;
            let line_bytes = config.cache_line_bytes;

            loop {
                runner.begin_cycle();
                let mut cycle_errors = 0u64;

                for test_block in test_blocks.iter() {
                    let base = test_block.block.buffer.as_mut_ptr() as *mut $simd_type;
                    let len = test_block.test_size / lanes;

                    let chunk_size_bytes = runner.chunk_size_bytes(test_block.test_size);
                    let chunk_size_operations = (chunk_size_bytes / lanes).max(1024);

                    let mut processed = 0;
                    while processed < len {
                        let chunk_end = (processed + chunk_size_operations).min(len);

                        // Phase 1: write P1, verify. Phase 2: write P2, verify.
                        // Phase 3: write P1 again, verify (catches transition-induced flips).
                        stuck_bit_write_verify!($simd_type, base, processed, chunk_end, p1, &mut cycle_errors, test_name, thread_id, 1, flush_before_verify, line_bytes);
                        stuck_bit_write_verify!($simd_type, base, processed, chunk_end, p2, &mut cycle_errors, test_name, thread_id, 2, flush_before_verify, line_bytes);
                        stuck_bit_write_verify!($simd_type, base, processed, chunk_end, p1, &mut cycle_errors, test_name, thread_id, 3, flush_before_verify, line_bytes);

                        if runner.should_halt(cycle_errors) {
                            let total_operations = (runner.bytes_processed() / lanes) as u64;
                            return runner.finish_aborted(cycle_errors, total_operations);
                        }

                        processed = chunk_end;

                        if runner.shutdown_requested() {
                            let total_operations = (runner.bytes_processed() / lanes) as u64;
                            return runner.finish_aborted(cycle_errors, total_operations);
                        }
                    }

                    // 3 writes + 3 reads per chunk over the whole block.
                    runner.add_bytes(test_block.test_size * 6);
                }

                runner.commit_cycle_errors(cycle_errors);
                runner.update_progress();
                if !runner.should_continue() {
                    let total_operations = (runner.bytes_processed() / lanes) as u64;
                    return runner.finish_completed(total_operations);
                }
            }
        }
    };
}

/// One write+verify phase of StuckBit at a SIMD width. Writes `$pat` across the chunk, fences,
/// then verifies with 4 independent XOR/OR accumulator chains (MLP), merged once at the end.
/// Increments `$errs` per tripped chunk (coarse — #27 does exact localization). Expanded
/// inside the `#[target_feature]` fn so the SIMD width is honored (no fn-call boundary).
macro_rules! stuck_bit_write_verify {
    ($simd_type:ty, $base:expr, $start:expr, $end:expr, $pat:expr, $errs:expr, $test_name:expr, $thread_id:expr, $phase:literal, $flush:expr, $line_bytes:expr) => {{
        // Write phase.
        for i in $start..$end {
            *$base.add(i) = $pat;
        }
        std::sync::atomic::fence(Ordering::SeqCst);

        // Optional flush phase (TODO #59): evict this chunk so the verify below round-trips
        // through DRAM instead of reading the cache-resident copy we just wrote. Without it,
        // whether the verify reaches DRAM is an accident of chunk-size-vs-cache. Placed after
        // the fence (writes ordered) and before the reads; flush_range_to_dram ends in MFENCE
        // so flushes drain before any verify load issues.
        if $flush {
            let chunk_ptr = $base.add($start) as *const u8;
            let chunk_bytes = ($end - $start) * std::mem::size_of::<$simd_type>();
            flush_range_to_dram(chunk_ptr, chunk_bytes, $line_bytes);
        }

        // Verify phase — 4 independent accumulator chains (MLP).
        let mut a0 = <$simd_type>::splat(0);
        let mut a1 = <$simd_type>::splat(0);
        let mut a2 = <$simd_type>::splat(0);
        let mut a3 = <$simd_type>::splat(0);
        let mut i = $start;
        while i + 4 <= $end {
            a0 |= *$base.add(i) ^ $pat;
            a1 |= *$base.add(i + 1) ^ $pat;
            a2 |= *$base.add(i + 2) ^ $pat;
            a3 |= *$base.add(i + 3) ^ $pat;
            i += 4;
        }
        while i < $end {
            a0 |= *$base.add(i) ^ $pat;
            i += 1;
        }
        let acc = (a0 | a1) | (a2 | a3);
        if acc.simd_ne(<$simd_type>::splat(0)).any() {
            *$errs += 1;
            log::error!("{}: memory error detected in phase {} chunk (thread {})",
                        $test_name, $phase, $thread_id);
        }
    }};
}

/// StuckBitTest MultiBlock implementation - tests for stuck bits using alternating patterns.
///
/// 3-Phase Pattern per cycle:
/// - Phase 1: Write STUCKBIT_P1 (0xAA55...) → Verify
/// - Phase 2: Write STUCKBIT_P2 (0x55AA..., = !P1) → Verify
/// - Phase 3: Write STUCKBIT_P1 again → Verify
///
/// Tests blocks in interleaved fashion with shared timer to fix N×duration bug.
#[doc = include_str!("test_fn_safety.md")]
pub unsafe fn stuck_bit_test_multi(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> TestStats {
    let test_name = "Mem-StuckBit";
    let (mut runner, test_blocks) = crate::test_scaffolding::TestRunner::new(
        blocks, thread_id, error_mode, timing, config, progress,
        test_name, TestAction::StuckBitTest,
    );

    // Hoist config flags into locals — never read a struct field inside the hot loop.
    let flush_before_verify = config.flush_before_verify;
    let line_bytes = config.cache_line_bytes;

    loop {
        runner.begin_cycle();
        let mut cycle_errors = 0u64;

        // Interleave testing across all blocks with shared timer
        for test_block in test_blocks.iter() {
            let base = test_block.block.buffer.as_mut_ptr() as *mut u64;
            let len = test_block.test_size / std::mem::size_of::<u64>();

            // Recalculate chunk size for THIS block's size
            let chunk_size_bytes = runner.chunk_size_bytes(test_block.test_size);
            let chunk_size_operations = (chunk_size_bytes / std::mem::size_of::<u64>()).max(1024);

            // Process this block in chunks
            let mut processed = 0;
            while processed < len {
                let chunk_end = (processed + chunk_size_operations).min(len);

                // Patterns must NOT be byte-uniform (would memset) — see STUCKBIT_P1 note.
                let pattern1 = STUCKBIT_P1;
                let pattern2 = STUCKBIT_P2;

                // Optional flush so the verify reads DRAM, not the line we just wrote (#59).
                // Expanded inline (not a fn) to keep it out of the hot path when disabled.
                macro_rules! flush_chunk_if_enabled {
                    () => {
                        if flush_before_verify {
                            let chunk_ptr = base.add(processed) as *const u8;
                            let chunk_bytes = (chunk_end - processed) * std::mem::size_of::<u64>();
                            flush_range_to_dram(chunk_ptr, chunk_bytes, line_bytes);
                        }
                    };
                }

                // Phase 1: Write P1 (0xAA55...), verify
                for i in processed..chunk_end {
                    *base.add(i) = pattern1;
                }

                std::sync::atomic::fence(Ordering::SeqCst);
                flush_chunk_if_enabled!();

                for i in processed..chunk_end {
                    let v = *base.add(i);
                    if v != pattern1 {
                        cycle_errors += 1;
                        log::error!("{}: Phase 1 memory error at index {} - expected {:#x}, got {:#x}",
                                   test_name, i, pattern1, v);
                    }
                }

                // Phase 2: Write P2 (0x55AA..., = !P1), verify
                for i in processed..chunk_end {
                    *base.add(i) = pattern2;
                }

                std::sync::atomic::fence(Ordering::SeqCst);
                flush_chunk_if_enabled!();

                for i in processed..chunk_end {
                    let v = *base.add(i);
                    if v != pattern2 {
                        cycle_errors += 1;
                        log::error!("{}: Phase 2 memory error at index {} - expected {:#x}, got {:#x}",
                                   test_name, i, pattern2, v);
                    }
                }

                // Phase 3: Write back to P1, verify
                for i in processed..chunk_end {
                    *base.add(i) = pattern1;
                }

                std::sync::atomic::fence(Ordering::SeqCst);
                flush_chunk_if_enabled!();

                for i in processed..chunk_end {
                    let v = *base.add(i);
                    if v != pattern1 {
                        cycle_errors += 1;
                        log::error!("{}: Phase 3 memory error at index {} - expected {:#x}, got {:#x}",
                                   test_name, i, pattern1, v);
                    }
                }

                // Handle errors if found
                if runner.should_halt(cycle_errors) {
                    let total_operations = (runner.bytes_processed() / std::mem::size_of::<u64>()) as u64;
                    return runner.finish_aborted(cycle_errors, total_operations);
                }

                processed = chunk_end;

                // Check for shutdown request
                if runner.shutdown_requested() {
                    let total_operations = (runner.bytes_processed() / std::mem::size_of::<u64>()) as u64;
                    return runner.finish_aborted(cycle_errors, total_operations);
                }
            }

            // Update bytes processed for this block (3 writes + 3 reads)
            runner.add_bytes(test_block.test_size * 6);
        }

        runner.commit_cycle_errors(cycle_errors);
        runner.update_progress();

        // Check if should continue based on timing
        if !runner.should_continue() {
            let total_operations = (runner.bytes_processed() / std::mem::size_of::<u64>()) as u64;
            return runner.finish_completed(total_operations);
        }
    }
}

/// # Safety
/// Caller must ensure all blocks are valid for reads/writes.
///
/// StuckBitTest128 MultiBlock implementation (SSE2) - tests for stuck bits using alternating patterns.
/// Preserves EXACT SSE2 algorithm from old version.
/// StuckBitTest128 - SSE2 optimized - MultiBlock pattern
/// Runtime-checked wrapper for SSE2 availability
pub unsafe fn stuck_bit_test_128_multi(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> TestStats {
    let test_name = "Mem-StuckBit128";

    // Runtime CPU feature check
    if !is_x86_feature_detected!("sse2") {
        log::warn!("{}: SSE2 not available, returning zero stats", test_name);
        return TestStats {
            name: test_name,
            action: TestAction::StuckBitTest,
            bytes_processed: 0,
            elapsed_ms: 0,
            thread_id,
            error_count: 0,
            total_operations: 0,
            cycles_completed: 0,
            cycles_planned: timing.cycles,
            stopped_by_time_limit: false,
        };
    }

    // Call the SSE2 optimized implementation
    stuck_bit_test_128_impl(blocks, thread_id, error_mode, timing, config, progress)
}

stuck_bit_impl!(stuck_bit_test_128_impl, "Mem-StuckBit128", u64x2, "sse4.2,sse4.1,ssse3,sse3,sse2,popcnt");

/// StuckBitTest256 - AVX2 optimized - MultiBlock pattern
/// Runtime-checked wrapper for AVX2 availability
#[allow(clippy::missing_safety_doc)]
pub unsafe fn stuck_bit_test_256_multi(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> TestStats {
    let test_name = "Mem-StuckBit256";

    // Runtime CPU feature check
    if !is_x86_feature_detected!("avx2") {
        log::warn!("{}: AVX2 not available, returning zero stats", test_name);
        return TestStats {
            name: test_name, action: TestAction::StuckBitTest,
            bytes_processed: 0, elapsed_ms: 0, thread_id,
            error_count: 0, total_operations: 0,
            cycles_completed: 0, cycles_planned: timing.cycles,
            stopped_by_time_limit: false,
        };
    }

    // Call the AVX2 optimized implementation
    stuck_bit_test_256_impl(blocks, thread_id, error_mode, timing, config, progress)
}

stuck_bit_impl!(stuck_bit_test_256_impl, "Mem-StuckBit256", u64x4, "avx2,avx,fma,bmi1,bmi2");

/// StuckBitTest512 - AVX-512 optimized - MultiBlock pattern
/// Runtime-checked wrapper for AVX-512 availability
#[allow(clippy::missing_safety_doc)]
pub unsafe fn stuck_bit_test_512_multi(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> TestStats {
    let test_name = "Mem-StuckBit512";

    // Runtime CPU feature check
    if !is_x86_feature_detected!("avx512f") {
        log::warn!("{}: AVX-512 not available, returning zero stats", test_name);
        return TestStats {
            name: test_name, action: TestAction::StuckBitTest,
            bytes_processed: 0, elapsed_ms: 0, thread_id,
            error_count: 0, total_operations: 0,
            cycles_completed: 0, cycles_planned: timing.cycles,
            stopped_by_time_limit: false,
        };
    }

    // Call the AVX-512 optimized implementation
    stuck_bit_test_512_impl(blocks, thread_id, error_mode, timing, config, progress)
}

stuck_bit_impl!(stuck_bit_test_512_impl, "Mem-StuckBit512", u64x8, "avx512f,avx512bw,avx512cd,avx512dq,avx512vl,avx2,avx,fma,bmi1,bmi2");

// === STUCK BIT TEST SIMD VARIANTS ===

/// StuckBitTestAuto - MultiBlock dispatcher
/// Auto-selects best SIMD implementation: AVX-512 > AVX2 > SSE2 > scalar
///
/// # Safety
/// Caller must ensure blocks contain valid, aligned memory
#[allow(clippy::missing_safety_doc)]
pub unsafe fn stuck_bit_test_auto_multi(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> TestStats {
    // Check CPU capabilities and dispatch to the best available MultiBlock implementation
    if is_x86_feature_detected!("avx512f") {
        stuck_bit_test_512_multi(blocks, thread_id, error_mode, timing, config, progress)
    } else if is_x86_feature_detected!("avx2") {
        stuck_bit_test_256_multi(blocks, thread_id, error_mode, timing, config, progress)
    } else if is_x86_feature_detected!("sse2") {
        stuck_bit_test_128_multi(blocks, thread_id, error_mode, timing, config, progress)
    } else {
        // Fallback to scalar MultiBlock implementation
        stuck_bit_test_multi(blocks, thread_id, error_mode, timing, config, progress)
    }
}

// ================================================================================================
// RefreshStable MultiBlock Implementations
// ================================================================================================

/// Refresh fill pattern. Non-byte-uniform (0xA5/0x5A alternating) so the fill stays a real
/// SIMD store loop instead of being rewritten to `memset` by LoopIdiomRecognize (#61).
pub const REFRESH_PATTERN: u64 = 0xA55AA55AA55AA55Au64;

/// Generates a Refresh `_impl` at a given SIMD width (the `_multi` runtime gate stays
/// hand-written and calls this). Per chunk: write pattern → CLFLUSHOPT the chunk to DRAM →
/// MFENCE → sleep 64ms → verify. The flush is what makes the post-sleep verify actually read
/// DRAM (defeats cache masking of bit-fade). Verify uses **4 independent accumulator chains**
/// (MLP; see `memory/simd-loop-optimization.md`) — this is the cold-DRAM regime where MLP
/// mattered most in benchmarking. No intermediate `ErrorCheckInterval` mode (TODO #27 does
/// exact localization). `TestRunner` owns window/timer/progress/stats/shutdown.
///
/// The chunk-vector floor stays `.max(256)` (matching the pre-macro per-width code; note the
/// scalar `refresh_stable_multi` uses its own operation-based floor).
macro_rules! refresh_impl {
    ($fn_name:ident, $test_name:literal, $simd_type:ty, $tf:literal) => {
        #[doc = include_str!("test_fn_safety.md")]
        #[inline]
        #[target_feature(enable = $tf)]
        unsafe fn $fn_name(
            blocks: &[crate::runner::AllocationBlock],
            thread_id: usize,
            error_mode: ErrorMode,
            timing: &TestTiming,
            config: &TestMemoryConfig,
            progress: Option<&TestProgress>,
        ) -> TestStats {
            let test_name = $test_name;
            let lanes = std::mem::size_of::<$simd_type>();
            let (mut runner, test_blocks) = crate::test_scaffolding::TestRunner::new(
                blocks, thread_id, error_mode, timing, config, progress,
                test_name, TestAction::WriteWaitVerify,
            );

            let pattern = <$simd_type>::splat(REFRESH_PATTERN);

            loop {
                runner.begin_cycle();
                let mut cycle_errors = 0u64;

                for test_block in test_blocks.iter() {
                    let base = test_block.block.buffer.as_mut_ptr() as *mut $simd_type;
                    let len = test_block.test_size / lanes;

                    let chunk_size_bytes = runner.chunk_size_bytes(test_block.test_size);
                    let chunk_size_vectors = (chunk_size_bytes / lanes).max(256);

                    let mut processed = 0;
                    while processed < len {
                        let chunk_end = (processed + chunk_size_vectors).min(len);

                        // Write phase.
                        for i in processed..chunk_end {
                            *base.add(i) = pattern;
                        }

                        // Flush the written chunk to DRAM so the post-sleep verify reads DRAM,
                        // not a cache-resident copy that would mask bit-fade.
                        let chunk_ptr = base.add(processed) as *const u8;
                        let chunk_bytes = (chunk_end - processed) * lanes;
                        flush_range_to_dram(chunk_ptr, chunk_bytes, config.cache_line_bytes);

                        std::sync::atomic::fence(Ordering::SeqCst);
                        std::thread::sleep(std::time::Duration::from_millis(64));

                        // Verify — 4 independent accumulator chains (MLP).
                        let mut a0 = <$simd_type>::splat(0);
                        let mut a1 = <$simd_type>::splat(0);
                        let mut a2 = <$simd_type>::splat(0);
                        let mut a3 = <$simd_type>::splat(0);
                        let mut i = processed;
                        while i + 4 <= chunk_end {
                            a0 |= *base.add(i) ^ pattern;
                            a1 |= *base.add(i + 1) ^ pattern;
                            a2 |= *base.add(i + 2) ^ pattern;
                            a3 |= *base.add(i + 3) ^ pattern;
                            i += 4;
                        }
                        while i < chunk_end {
                            a0 |= *base.add(i) ^ pattern;
                            i += 1;
                        }
                        let acc = (a0 | a1) | (a2 | a3);
                        if acc.simd_ne(<$simd_type>::splat(0)).any() {
                            cycle_errors += 1;
                            log::error!("{}: memory error detected in chunk (thread {})", test_name, thread_id);
                        }

                        if runner.should_halt(cycle_errors) {
                            let total_operations = (runner.bytes_processed() / lanes) as u64;
                            return runner.finish_aborted(cycle_errors, total_operations);
                        }

                        processed = chunk_end;

                        if runner.shutdown_requested() {
                            let total_operations = (runner.bytes_processed() / lanes) as u64;
                            return runner.finish_aborted(cycle_errors, total_operations);
                        }
                    }

                    // 1 write + 1 read per chunk over the whole block.
                    runner.add_bytes(test_block.test_size * 2);
                }

                runner.commit_cycle_errors(cycle_errors);
                runner.update_progress();
                if !runner.should_continue() {
                    let total_operations = (runner.bytes_processed() / lanes) as u64;
                    return runner.finish_completed(total_operations);
                }
            }
        }
    };
}

/// RefreshStable MultiBlock implementation - tests DRAM refresh stability.
///
/// Pattern per cycle:
/// - Write 0xA55AA55AA55AA55A pattern (non-byte-uniform — see the pattern-def
///   comments below and #61 for why it must not be a single repeated byte)
/// - Sleep 64ms (DRAM refresh cycle timing)
/// - Verify pattern unchanged
///
/// Tests blocks in interleaved fashion with shared timer to fix N×duration bug.
#[doc = include_str!("test_fn_safety.md")]
pub unsafe fn refresh_stable_multi(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> TestStats {
    let test_name = "Mem-Refresh";
    let (mut runner, test_blocks) = crate::test_scaffolding::TestRunner::new(
        blocks, thread_id, error_mode, timing, config, progress,
        test_name, TestAction::WriteWaitVerify,
    );

    loop {
        runner.begin_cycle();
        let mut cycle_errors = 0u64;

        // Interleave testing across all blocks with shared timer
        for test_block in test_blocks.iter() {
            let base = test_block.block.buffer.as_mut_ptr() as *mut u64;
            let len = test_block.test_size / std::mem::size_of::<u64>();

            // Recalculate chunk size for THIS block's size
            let chunk_size_bytes = runner.chunk_size_bytes(test_block.test_size);
            let chunk_size_operations = (chunk_size_bytes / std::mem::size_of::<u64>()).max(1024);

            // Process this block in chunks
            let mut processed = 0;
            while processed < len {
                let chunk_end = (processed + chunk_size_operations).min(len);

                // Non-byte-uniform (see REFRESH_PATTERN / #61): stays a real store loop,
                // not memset.
                let pattern = REFRESH_PATTERN;
                for i in processed..chunk_end {
                    *base.add(i) = pattern;
                }

                // Flush the written chunk to DRAM so the verify after the sleep
                // round-trips through DRAM instead of reading a cache-resident
                // copy. Without this the refresh/bit-fade test is silently inert:
                // the cached line masks any decay that happened in DRAM.
                let chunk_ptr = base.add(processed) as *const u8;
                let chunk_bytes = (chunk_end - processed) * std::mem::size_of::<u64>();
                flush_range_to_dram(chunk_ptr, chunk_bytes, config.cache_line_bytes);

                std::sync::atomic::fence(Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(64)); // DRAM refresh cycle timing

                // Verify phase
                for i in processed..chunk_end {
                    let v = *base.add(i);
                    if v != pattern {
                        cycle_errors += 1;
                        log::error!("{}: memory error at index {} - expected {:#x}, got {:#x}",
                                   test_name, i, pattern, v);
                    }
                }

                // Handle errors if found
                if runner.should_halt(cycle_errors) {
                    let total_operations = (runner.bytes_processed() / std::mem::size_of::<u64>()) as u64;
                    return runner.finish_aborted(cycle_errors, total_operations);
                }

                processed = chunk_end;

                // Check for shutdown request
                if runner.shutdown_requested() {
                    let total_operations = (runner.bytes_processed() / std::mem::size_of::<u64>()) as u64;
                    return runner.finish_aborted(cycle_errors, total_operations);
                }
            }

            // Update bytes processed for this block (1 write + 1 read)
            runner.add_bytes(test_block.test_size * 2);
        }

        runner.commit_cycle_errors(cycle_errors);
        runner.update_progress();

        // Check if should continue based on timing
        if !runner.should_continue() {
            let total_operations = (runner.bytes_processed() / std::mem::size_of::<u64>()) as u64;
            return runner.finish_completed(total_operations);
        }
    }
}

/// RefreshStable128 MultiBlock implementation (SSE2) - tests DRAM refresh stability.
///
/// # Safety
/// Caller must ensure blocks contain valid, aligned memory
#[allow(clippy::missing_safety_doc)]
/// RefreshStable128 - SSE2 optimized - MultiBlock pattern
/// Runtime-checked wrapper for SSE2 availability
pub unsafe fn refresh_stable_128_multi(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> TestStats {
    let test_name = "Mem-Refresh128";

    // Runtime CPU feature check
    if !is_x86_feature_detected!("sse2") {
        log::warn!("{}: SSE2 not available, returning zero stats", test_name);
        return TestStats {
            name: test_name,
            action: TestAction::WriteWaitVerify,
            bytes_processed: 0,
            elapsed_ms: 0,
            thread_id,
            error_count: 0,
            total_operations: 0,
            cycles_completed: 0,
            cycles_planned: timing.cycles,
            stopped_by_time_limit: false,
        };
    }

    // Call the SSE2 optimized implementation
    refresh_stable_128_impl(blocks, thread_id, error_mode, timing, config, progress)
}

refresh_impl!(refresh_stable_128_impl, "Mem-Refresh128", u64x2, "sse4.2,sse4.1,ssse3,sse3,sse2,popcnt");

/// RefreshStable256 - AVX2 optimized - MultiBlock pattern
/// Runtime-checked wrapper for AVX2 availability
///
/// # Safety
/// Caller must ensure blocks contain valid, aligned memory
#[allow(clippy::missing_safety_doc)]
pub unsafe fn refresh_stable_256_multi(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> TestStats {
    let test_name = "Mem-Refresh256";

    // Runtime CPU feature check
    if !is_x86_feature_detected!("avx2") {
        log::warn!("{}: AVX2 not available, returning zero stats", test_name);
        return TestStats {
            name: test_name,
            action: TestAction::WriteWaitVerify,
            bytes_processed: 0,
            elapsed_ms: 0,
            thread_id,
            error_count: 0,
            total_operations: 0,
            cycles_completed: 0,
            cycles_planned: timing.cycles,
            stopped_by_time_limit: false,
        };
    }

    // Call the AVX2 optimized implementation
    refresh_stable_256_impl(blocks, thread_id, error_mode, timing, config, progress)
}

refresh_impl!(refresh_stable_256_impl, "Mem-Refresh256", u64x4, "avx2,avx,fma,bmi1,bmi2");

/// RefreshStable512 - AVX-512 optimized - MultiBlock pattern
/// Runtime-checked wrapper for AVX-512 availability
///
/// # Safety
/// Caller must ensure blocks contain valid, aligned memory
#[allow(clippy::missing_safety_doc)]
pub unsafe fn refresh_stable_512_multi(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> TestStats {
    let test_name = "Mem-Refresh512";

    // Runtime CPU feature check
    if !is_x86_feature_detected!("avx512f") {
        log::warn!("{}: AVX-512 not available, returning zero stats", test_name);
        return TestStats {
            name: test_name,
            action: TestAction::WriteWaitVerify,
            bytes_processed: 0,
            elapsed_ms: 0,
            thread_id,
            error_count: 0,
            total_operations: 0,
            cycles_completed: 0,
            cycles_planned: timing.cycles,
            stopped_by_time_limit: false,
        };
    }

    // Call the AVX-512 optimized implementation
    refresh_stable_512_impl(blocks, thread_id, error_mode, timing, config, progress)
}

refresh_impl!(refresh_stable_512_impl, "Mem-Refresh512", u64x8, "avx512f,avx512bw,avx512cd,avx512dq,avx512vl,avx2,avx,fma,bmi1,bmi2");

/// RefreshStableAuto - MultiBlock dispatcher
/// Auto-selects best SIMD implementation: AVX-512 > AVX2 > SSE2 > scalar
///
/// # Safety
/// Caller must ensure blocks contain valid, aligned memory
#[allow(clippy::missing_safety_doc)]
pub unsafe fn refresh_stable_auto_multi(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> TestStats {
    // Check CPU capabilities and dispatch to the best available MultiBlock implementation
    if is_x86_feature_detected!("avx512f") {
        refresh_stable_512_multi(blocks, thread_id, error_mode, timing, config, progress)
    } else if is_x86_feature_detected!("avx2") {
        refresh_stable_256_multi(blocks, thread_id, error_mode, timing, config, progress)
    } else if is_x86_feature_detected!("sse2") {
        refresh_stable_128_multi(blocks, thread_id, error_mode, timing, config, progress)
    } else {
        // Fallback to scalar MultiBlock implementation
        refresh_stable_multi(blocks, thread_id, error_mode, timing, config, progress)
    }
}

// ================================================================================================
// CacheBusting MultiBlock Implementation
// ================================================================================================

/// CacheBusting MultiBlock implementation - tests memory with stride-based cache-busting patterns.
///
/// Pattern per cycle:
/// - Writes with large strides (CACHE_BUSTING_STRIDE) to evict from cache
/// - Verifies the pattern
/// - Supports single stream and multi-stream modes
///
/// Tests blocks in interleaved fashion with shared timer to fix N×duration bug.
#[doc = include_str!("test_fn_safety.md")]
pub unsafe fn cache_busting_multi(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> TestStats {
    let test_name = "Mem-CacheBust";

    let stride_patterns = config.parameter_context.as_ref()
        .and_then(|c| c.stride_patterns)
        .expect("CacheBust requires stride_patterns in parameter_context") as usize;

    // Pre-calculate stride constants outside all loops
    let base_stride = CACHE_BUSTING_STRIDE / std::mem::size_of::<u64>();
    let pattern_base = 0x0123456789ABCDEFu64.wrapping_add(thread_id as u64);

    let (mut runner, test_blocks) = crate::test_scaffolding::TestRunner::new(
        blocks, thread_id, error_mode, timing, config, progress,
        test_name, TestAction::CacheBusting,
    );

    loop {
        runner.begin_cycle();
        let mut cycle_errors = 0u64;

        // Interleave testing across all blocks with shared timer
        for test_block in test_blocks.iter() {
            let base = test_block.block.buffer.as_mut_ptr() as *mut u64;
            let len = test_block.test_size / std::mem::size_of::<u64>();

            // Recalculate chunk size for THIS block's size
            let chunk_size_bytes = runner.chunk_size_bytes(test_block.test_size);
            let chunk_size_operations = (chunk_size_bytes / std::mem::size_of::<u64>()).max(1024);

            // Process this block in chunks
            let mut processed = 0;
            while processed < len {
                let chunk_end = (processed + chunk_size_operations).min(len);

                // Apply stride-pattern-based access patterns within chunk
                match stride_patterns {
                    1 => {
                        // Single pattern with large strides to bust cache
                        for offset in 0..base_stride.min(chunk_end - processed) {
                            let mut i = processed + offset;
                            while i < chunk_end {
                                *base.add(i) = pattern_base.wrapping_add(i as u64);
                                i += base_stride;
                                if i >= chunk_end { break; }
                            }
                        }

                        std::sync::atomic::fence(Ordering::SeqCst);

                        // Verify with same stride pattern within chunk
                        for offset in 0..base_stride.min(chunk_end - processed) {
                            let mut i = processed + offset;
                            while i < chunk_end {
                                let v = *base.add(i);
                                let expected = pattern_base.wrapping_add(i as u64);
                                if v != expected {
                                    cycle_errors += 1;
                                    log::error!("{}: memory error at index {} - expected {:#x}, got {:#x}",
                                               test_name, i, expected, v);
                                }
                                i += base_stride;
                                if i >= chunk_end { break; }
                            }
                        }
                    }
                    _ => {
                        // Multiple patterns - divide offsets among stride variants
                        // Each variant handles a subset of offsets, but ALL offsets are covered
                        for offset in 0..base_stride.min(chunk_end - processed) {
                            let variant = (offset % stride_patterns) as u64;
                            let pattern = pattern_base.wrapping_add(variant * 0x1111111111111111u64);

                            let mut i = processed + offset;
                            while i < chunk_end {
                                *base.add(i) = pattern.wrapping_add(i as u64);
                                i += base_stride;
                                if i >= chunk_end { break; }
                            }
                        }

                        std::sync::atomic::fence(Ordering::SeqCst);

                        // Verify all offsets with their respective variant patterns
                        for offset in 0..base_stride.min(chunk_end - processed) {
                            let variant = (offset % stride_patterns) as u64;
                            let pattern = pattern_base.wrapping_add(variant * 0x1111111111111111u64);

                            let mut i = processed + offset;
                            while i < chunk_end {
                                let v = *base.add(i);
                                let expected = pattern.wrapping_add(i as u64);
                                if v != expected {
                                    cycle_errors += 1;
                                    log::error!("{}: memory error at index {} - expected {:#x}, got {:#x}",
                                               test_name, i, expected, v);
                                }
                                i += base_stride;
                                if i >= chunk_end { break; }
                            }
                        }
                    }
                }

                // Handle errors if found
                if runner.should_halt(cycle_errors) {
                    let total_operations = (runner.bytes_processed() / std::mem::size_of::<u64>()) as u64;
                    return runner.finish_aborted(cycle_errors, total_operations);
                }

                processed = chunk_end;

                // Check for shutdown request
                if runner.shutdown_requested() {
                    let total_operations = (runner.bytes_processed() / std::mem::size_of::<u64>()) as u64;
                    return runner.finish_aborted(cycle_errors, total_operations);
                }
            }

            // Update bytes processed for this block (write + verify = 2×)
            runner.add_bytes(test_block.test_size * 2);
        }

        runner.commit_cycle_errors(cycle_errors);
        runner.update_progress();

        // Check if should continue based on timing
        if !runner.should_continue() {
            let total_operations = (runner.bytes_processed() / std::mem::size_of::<u64>()) as u64;
            return runner.finish_completed(total_operations);
        }
    }
}

// ================================================================================================
// RandomTorture MultiBlock Implementation
// ================================================================================================

/// RandomTorture MultiBlock implementation - random access pattern testing.
///
/// Pattern per cycle:
/// - Initializes memory with sequential pattern (index as value)
/// - Performs random reads using XORshift RNG
/// - Verifies each random read matches expected value
/// - Supports multiple RNG sequences with different seeds
///
/// Tests blocks in interleaved fashion with shared timer to fix N×duration bug.
#[doc = include_str!("test_fn_safety.md")]
pub unsafe fn random_torture_multi(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> TestStats {
    let test_name = "Mem-Random";

    let (mut runner, test_blocks) = crate::test_scaffolding::TestRunner::new(
        blocks, thread_id, error_mode, timing, config, progress,
        test_name, TestAction::RandomAccess,
    );
    let total_test_size: usize = test_blocks.iter().map(|b| b.test_size).sum();

    // Initialize all blocks with known pattern once
    for test_block in test_blocks.iter() {
        let base = test_block.block.buffer.as_mut_ptr() as *mut u64;
        let len = test_block.test_size / std::mem::size_of::<u64>();

        for i in 0..len {
            *base.add(i) = i as u64;
        }
        std::sync::atomic::fence(Ordering::SeqCst);
        runner.add_bytes(test_block.test_size);
    }

    loop {
        let cycle = runner.begin_cycle();
        let mut cycle_errors = 0u64;

        // Interleave testing across all blocks with shared timer
        for test_block in test_blocks.iter() {
            let base = test_block.block.buffer.as_mut_ptr() as *mut u64;
            let len = test_block.test_size / std::mem::size_of::<u64>();

            // Assert power-of-2 size for optimal performance
            if !len.is_power_of_two() {
                panic!("{}: Window size {} is not power-of-2! This is a bug in the alignment code.",
                       test_name, len);
            }
            let mask = len - 1;

            // Calculate chunk size for responsive shutdown
            let chunk_size_bytes = runner.chunk_size_bytes(test_block.test_size);
            let chunk_size_operations = chunk_size_bytes / std::mem::size_of::<u64>();

            // Random access torture with configurable RNG sequences
            let rng_sequences = config.parameter_context.as_ref()
                .and_then(|c| c.rng_sequences)
                .expect("RandomTorture requires rng_sequences in parameter_context");
            let base_iterations = (len / 1000).clamp(5000, 50000);
            let seq_count = rng_sequences.max(1) as usize;
            let seq_shift = seq_count.trailing_zeros();
            let iterations_per_seq = (base_iterations >> seq_shift).max(1);

            // Process RNG sequences in chunks for responsive shutdown
            for seq in 0..rng_sequences {
                let mut rng_state = 0x123456789ABCDEFu64
                    .wrapping_add(thread_id as u64)
                    .wrapping_add(cycle as u64)
                    .wrapping_add((seq as u64).wrapping_mul(0x8765432187654321u64));

                // Random read verification for this sequence - chunked for responsive shutdown
                for chunk_start in (0..iterations_per_seq).step_by(chunk_size_operations) {
                    let chunk_end = (chunk_start + chunk_size_operations).min(iterations_per_seq);

                    for _iteration in chunk_start..chunk_end {
                        rng_state ^= rng_state << 13;
                        rng_state ^= rng_state >> 17;
                        rng_state ^= rng_state << 5;

                        let idx = (rng_state as usize) & mask;
                        let expected = idx as u64;
                        let actual = *base.add(idx);

                        if actual != expected {
                            cycle_errors += 1;
                            log::error!(
                                "{}: memory error at index {}, iteration {}, rng_seq {}, expected {}, actual {}",
                                test_name,
                                idx,
                                _iteration,
                                seq,
                                expected,
                                actual
                            );
                        }
                    }

                    // Handle errors if found
                    if runner.should_halt(cycle_errors) {
                        let total_operations = cycle as u64 * (chunk_end - chunk_start) as u64;
                        return runner.finish_aborted(cycle_errors, total_operations);
                    }

                    // Check for shutdown request
                    if runner.shutdown_requested() {
                        let total_operations = cycle as u64 * (chunk_end - chunk_start) as u64;
                        return runner.finish_aborted(cycle_errors, total_operations);
                    }
                }
            }

            // Update bytes processed for this block
            let bytes_this_cycle = iterations_per_seq
                .saturating_mul(rng_sequences as usize)
                .saturating_mul(std::mem::size_of::<u64>());
            runner.add_bytes(bytes_this_cycle);
        }

        runner.commit_cycle_errors(cycle_errors);
        runner.update_progress();

        // Check if should continue based on timing
        if !runner.should_continue() {
            // Calculate total operations
            let base_iterations = (total_test_size / std::mem::size_of::<u64>() / 1000).clamp(5000, 50000);
            let rng_seq_count = config.parameter_context.as_ref()
                .and_then(|c| c.rng_sequences)
                .expect("RandomTorture requires rng_sequences in parameter_context") as usize;
            let iterations_per_seq_final = (base_iterations >> rng_seq_count.trailing_zeros()).max(1);
            let total_operations: u64 = cycle as u64 * (iterations_per_seq_final * rng_seq_count) as u64;

            return runner.finish_completed(total_operations);
        }
    }
}

// ================================================================================================
// StrideAccess, BandwidthSat, BlockMove MultiBlock Implementations
// ================================================================================================

/// StrideAccess MultiBlock implementation - tests various stride patterns.
#[doc = include_str!("test_fn_safety.md")]
pub unsafe fn stride_access_multi(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> TestStats {
    let test_name = "Mem-Stride";

    let subdivisions = config.parameter_context.as_ref()
        .and_then(|c| c.subdivisions)
        .expect("StrideAccess requires subdivisions in parameter_context") as usize;
    let subdiv_shift = subdivisions.trailing_zeros();

    let (mut runner, test_blocks) = crate::test_scaffolding::TestRunner::new(
        blocks, thread_id, error_mode, timing, config, progress,
        test_name, TestAction::ReadWrite,
    );
    let total_test_size: usize = test_blocks.iter().map(|b| b.test_size).sum();

    loop {
        let cycle = runner.begin_cycle();
        let mut cycle_errors = 0u64;
        let strides = [1, 16, 64, 256, 1024, 4096];
        let pattern_base = 0xFEDCBA9876543210u64.wrapping_add(thread_id as u64).wrapping_add(cycle as u64);

        for test_block in test_blocks.iter() {
            let base = test_block.block.buffer.as_mut_ptr() as *mut u64;
            let len = test_block.test_size / std::mem::size_of::<u64>();
            let chunk_size_bytes = runner.chunk_size_bytes(test_block.test_size);
            let chunk_size_elements = chunk_size_bytes / std::mem::size_of::<u64>();

            'stride_loop: for &stride in &strides {
                if stride >= len { continue; }

                for chunk_start in (0..len).step_by(chunk_size_elements) {
                    let chunk_end = (chunk_start + chunk_size_elements).min(len);
                    let chunk_len = chunk_end - chunk_start;
                    let elements_per_subdiv = chunk_len >> subdiv_shift;

                    for subdiv in 0..subdivisions {
                        let pattern = pattern_base.wrapping_add((stride as u64) << 32).wrapping_add((subdiv as u64) << 48);
                        let subdiv_start = chunk_start + subdiv * elements_per_subdiv;
                        let subdiv_end = subdiv_start + elements_per_subdiv;
                        let mut pos = subdiv_start;
                        while pos < subdiv_end {
                            *base.add(pos) = pattern.wrapping_add(pos as u64);
                            pos += stride;
                            if pos >= subdiv_end { break; }
                        }
                    }

                    std::sync::atomic::fence(Ordering::SeqCst);

                    for subdiv in 0..subdivisions {
                        let pattern = pattern_base.wrapping_add((stride as u64) << 32).wrapping_add((subdiv as u64) << 48);
                        let subdiv_start = chunk_start + subdiv * elements_per_subdiv;
                        let subdiv_end = subdiv_start + elements_per_subdiv;
                        let mut pos = subdiv_start;
                        while pos < subdiv_end {
                            let expected = pattern.wrapping_add(pos as u64);
                            let actual = *base.add(pos);
                            if actual != expected {
                                cycle_errors += 1;
                                log::error!("{}: memory error at index {} - expected {:#x}, got {:#x}", test_name, pos, expected, actual);
                            }
                            pos += stride;
                            if pos >= subdiv_end { break; }
                        }
                    }

                    if runner.should_halt(cycle_errors) {
                        break 'stride_loop;
                    }

                    if runner.shutdown_requested() {
                        runner.add_bytes((chunk_end - chunk_start) * std::mem::size_of::<u64>() * 2);
                        let total_operations = cycle as u64 * (chunk_end - chunk_start) as u64;
                        return runner.finish_aborted(cycle_errors, total_operations);
                    }
                }
            }

            let mut bytes_this_cycle = 0;
            for &stride in &strides {
                if stride < len {
                    bytes_this_cycle += (len / stride) * std::mem::size_of::<u64>() * 2;
                }
            }
            runner.add_bytes(bytes_this_cycle);
        }

        runner.commit_cycle_errors(cycle_errors);
        runner.update_progress();

        if !runner.should_continue() {
            let strides = [1, 16, 64, 256, 1024, 4096];
            let mut total_elements_per_cycle = 0u64;
            for &stride in &strides {
                if stride < (total_test_size / std::mem::size_of::<u64>()) {
                    total_elements_per_cycle += ((total_test_size / std::mem::size_of::<u64>()) / stride) as u64;
                }
            }
            let total_operations: u64 = cycle as u64 * total_elements_per_cycle;
            return runner.finish_completed(total_operations);
        }
    }
}

/// BlockMove MultiBlock - sequential block memory moves.
#[doc = include_str!("test_fn_safety.md")]
pub unsafe fn block_move_multi(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> TestStats {
    let test_name = "Mem-BlockMove";

    let (mut runner, test_blocks) = crate::test_scaffolding::TestRunner::new(
        blocks, thread_id, error_mode, timing, config, progress,
        test_name, TestAction::ReadWrite,
    );
    let total_test_size: usize = test_blocks.iter().map(|b| b.test_size).sum();

    // Initialize source memory in each block
    let pattern_base = 0xDEADBEEFCAFEBABEu64;
    for test_block in test_blocks.iter() {
        // Divide block in half: first half = source, second half = destination
        let half_size = test_block.test_size / 2;
        let src_base = test_block.block.buffer.as_mut_ptr() as *mut u64;
        let len = half_size / std::mem::size_of::<u64>();

        // Initialize source with pattern
        for i in 0..len {
            *src_base.add(i) = pattern_base.wrapping_add(i as u64);
        }
    }
    std::sync::atomic::fence(Ordering::SeqCst);

    loop {
        let cycle = runner.begin_cycle();
        let mut cycle_errors = 0u64;

        for test_block in test_blocks.iter() {
            // Divide block: source (first half) → destination (second half)
            let half_size = test_block.test_size / 2;
            let src_base = test_block.block.buffer.as_mut_ptr() as *mut u64;
            let dst_base = src_base.add(half_size / std::mem::size_of::<u64>());
            let len = half_size / std::mem::size_of::<u64>();

            // Calculate chunk size for responsive shutdown
            let chunk_size_bytes = runner.chunk_size_bytes(test_block.test_size);
            let chunk_size_operations = chunk_size_bytes / (2 * std::mem::size_of::<u64>());

            // Process in chunks
            let mut processed = 0;
            while processed < len {
                let chunk_end = (processed + chunk_size_operations).min(len);

                // Copy from source to destination with direction patterns
                let copy_dirs = config.parameter_context.as_ref()
                    .and_then(|c| c.copy_directions)
                    .expect("BlockMove requires copy_directions in parameter_context");
                match copy_dirs {
                    1 => {
                        // Single direction: Simple forward copy
                        for i in processed..chunk_end {
                            let val = *src_base.add(i);
                            *dst_base.add(i) = val;
                        }
                    }
                    2 => {
                        // Two directions: Copy forward and backward simultaneously
                        let chunk_size = chunk_end - processed;
                        let mid = processed + chunk_size / 2;

                        // Stream 1: First half forward
                        for i in processed..mid {
                            *dst_base.add(i) = *src_base.add(i);
                        }

                        // Stream 2: Second half backward
                        for i in 0..(chunk_end - mid) {
                            let idx = chunk_end - 1 - i;
                            *dst_base.add(idx) = *src_base.add(idx);
                        }
                    }
                    4 => {
                        // Four directions: Interleaved block copy with different patterns
                        let chunk_size = chunk_end - processed;
                        let block_size = chunk_size / 4;

                        for stream in 0..4 {
                            let start_idx = processed + stream * block_size;
                            let end_idx = (processed + (stream + 1) * block_size).min(chunk_end);

                            if start_idx < end_idx {
                                match stream & 3 {
                                    0 | 3 => {
                                        // Forward copy
                                        for i in start_idx..end_idx {
                                            *dst_base.add(i) = *src_base.add(i);
                                        }
                                    }
                                    1 => {
                                        // Backward copy within block
                                        for i in 0..(end_idx - start_idx) {
                                            let idx = end_idx - 1 - i;
                                            *dst_base.add(idx) = *src_base.add(idx);
                                        }
                                    }
                                    _ => {
                                        // Skip pattern (every other element)
                                        for i in (start_idx..end_idx).step_by(2) {
                                            *dst_base.add(i) = *src_base.add(i);
                                        }
                                        for i in ((start_idx + 1)..end_idx).step_by(2) {
                                            *dst_base.add(i) = *src_base.add(i);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    _ => {
                        // Many directions: Strided copy pattern
                        let dirs = copy_dirs as usize;
                        for dir in 0..dirs {
                            for i in (processed + dir..chunk_end).step_by(dirs) {
                                *dst_base.add(i) = *src_base.add(i);
                            }
                        }
                    }
                }

                std::sync::atomic::fence(Ordering::SeqCst);

                // Verify copied data
                for i in processed..chunk_end {
                    let expected = pattern_base.wrapping_add(i as u64);
                    let actual = *dst_base.add(i);
                    if actual != expected {
                        cycle_errors += 1;
                        log::error!("{}: error at {} - expected {:#x}, got {:#x}",
                                   test_name, i, expected, actual);
                    }
                }

                // Handle errors
                if runner.should_halt(cycle_errors) {
                    let total_operations = cycle as u64 * len as u64;
                    return runner.finish_aborted(cycle_errors, total_operations);
                }

                // Check for shutdown
                if runner.shutdown_requested() {
                    runner.add_bytes((chunk_end - processed) * std::mem::size_of::<u64>() * 2);
                    let total_operations = cycle as u64 * (chunk_end - processed) as u64;
                    return runner.finish_aborted(cycle_errors, total_operations);
                }

                processed = chunk_end;
            }

            runner.add_bytes(test_block.test_size * 3 / 2); // Copy (R source + W dest = 1×) + Verify (R dest = 0.5×) = 1.5×
        }

        runner.commit_cycle_errors(cycle_errors);
        runner.update_progress();

        // Check timing
        if !runner.should_continue() {
            let total_operations: u64 = cycle as u64 * (total_test_size / (std::mem::size_of::<u64>() * 2)) as u64;
            return runner.finish_completed(total_operations);
        }
    }
}

// ============================================================================
// Phased Test Implementations (run_phased_test harness)
// ============================================================================
//
// These use the run_phased_test harness for zero-cost orchestration of
// init → test × N → verify × M phases. Replaces duplicated boilerplate.
//
// Key improvements over v1 manual-loop tests:
// - Mode 2 PRNG is a real LCG chain (not a static seed)
// - Parameter field correctly interpreted (stride, subblocks, page stride)
// - All tests use u64 lanes (v1 MirrorMove i32 removed)
// - Strided access support for SimpleTest
// - Configurable test_reps, verify_reps, write_read_cycles

use crate::pattern_gen;
use crate::test_harness::{run_phased_test, ChunkCtx};

// ─── SimpleTest v2 ───────────────────────────────────────────────────────────

/// Pattern-generation parameters for the SimpleTest v2 implementations.
/// Bundles the mode selector, its two free parameters, and the access stride
/// so the test functions keep to the standard 6-arg test signature + this config.
#[derive(Clone, Copy)]
struct SimplePatternConfig {
    mode: u32,
    param0: u64,
    param1: u64,
    /// Element stride for strided access; `0` means sequential.
    stride: usize,
}

/// SimpleTest v2 entry point — correct Mode 2 LCG + strided access.
///
/// # Safety
/// Caller must ensure all blocks contain valid, aligned, writable memory.
pub unsafe fn simple_test_v2_multi(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> TestStats {
    // Determine stride from parameter_context (v2 correct interpretation)
    let stride = config.parameter_context.as_ref()
        .and_then(|ctx| ctx.stride_elements)
        .unwrap_or(0);

    // Determine pattern mode and params
    let pattern = SimplePatternConfig {
        mode: config.pattern_mode.unwrap_or(0),
        param0: config.pattern_param0.unwrap_or(0xDEADBEEFDEADBEEF),
        param1: config.pattern_param1.unwrap_or(0xCAFEBABECAFEBABE),
        stride,
    };

    if stride > 0 {
        simple_test_v2_strided(blocks, thread_id, error_mode, timing, config, progress, pattern)
    } else {
        simple_test_v2_sequential(blocks, thread_id, error_mode, timing, config, progress, pattern)
    }
}

/// Sequential SimpleTest v2 — Mode 0/1/2 with correct LCG.
#[doc = include_str!("test_fn_safety.md")]
unsafe fn simple_test_v2_sequential(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
    pattern: SimplePatternConfig,
) -> TestStats {
    let SimplePatternConfig { mode: pattern_mode, param0, param1, .. } = pattern;
    let test_name = "Mem-SimpleV2";
    let cl_shift = pattern_gen::cache_line_shift(config.cache_line_bytes);
    let cl_elements = config.cache_line_bytes / std::mem::size_of::<u64>();

    match pattern_mode {
        0 => {
            // Mode 0 (TM5-faithful): bit dispersion + branchless 4KB page complement
            run_phased_test(
                blocks, thread_id, error_mode, timing, config, progress,
                test_name, TestAction::WriteVerify, 1, false, config.test_reps, config.verify_reps,
                |ctx: &ChunkCtx| {
                    let seed = pattern_gen::block_seed(ctx.ptr as usize, ctx.thread_id, ctx.cycle);
                    for idx in ctx.chunk_start..ctx.chunk_end {
                        *ctx.ptr.add(idx) = pattern_gen::pattern_mode0(idx as u64, seed);
                    }
                },
                |ctx: &ChunkCtx| {
                    let seed = pattern_gen::block_seed(ctx.ptr as usize, ctx.thread_id, ctx.cycle);
                    for idx in ctx.chunk_start..ctx.chunk_end {
                        *ctx.ptr.add(idx) = pattern_gen::pattern_mode0(idx as u64, seed);
                    }
                },
                |ctx: &ChunkCtx| -> u64 {
                    let seed = pattern_gen::block_seed(ctx.ptr as usize, ctx.thread_id, ctx.cycle);
                    let mut total_errors = 0u64;
                    for idx in ctx.chunk_start..ctx.chunk_end {
                        let expected = pattern_gen::pattern_mode0(idx as u64, seed);
                        let actual = *ctx.ptr.add(idx);
                        if actual != expected {
                            total_errors += 1;
                            if total_errors <= 10 {
                                log::error!("{}: error at idx {} - expected {:#x}, got {:#x}",
                                           test_name, idx, expected, actual);
                            }
                        }
                    }
                    total_errors
                },
            )
        }
        1 => {
            // Mode 1 (TM5-faithful): linear step + cache-line complement
            run_phased_test(
                blocks, thread_id, error_mode, timing, config, progress,
                test_name, TestAction::WriteVerify, 1, false, config.test_reps, config.verify_reps,
                |ctx: &ChunkCtx| {
                    let seed = pattern_gen::block_seed(ctx.ptr as usize, ctx.thread_id, ctx.cycle);
                    for idx in ctx.chunk_start..ctx.chunk_end {
                        *ctx.ptr.add(idx) = pattern_gen::pattern_mode1(idx as u64, seed, cl_shift);
                    }
                },
                |ctx: &ChunkCtx| {
                    let seed = pattern_gen::block_seed(ctx.ptr as usize, ctx.thread_id, ctx.cycle);
                    for idx in ctx.chunk_start..ctx.chunk_end {
                        *ctx.ptr.add(idx) = pattern_gen::pattern_mode1(idx as u64, seed, cl_shift);
                    }
                },
                |ctx: &ChunkCtx| -> u64 {
                    let seed = pattern_gen::block_seed(ctx.ptr as usize, ctx.thread_id, ctx.cycle);
                    let mut total_errors = 0u64;
                    for idx in ctx.chunk_start..ctx.chunk_end {
                        let expected = pattern_gen::pattern_mode1(idx as u64, seed, cl_shift);
                        let actual = *ctx.ptr.add(idx);
                        if actual != expected {
                            total_errors += 1;
                            if total_errors <= 10 {
                                log::error!("{}: error at idx {} - expected {:#x}, got {:#x}",
                                           test_name, idx, expected, actual);
                            }
                        }
                    }
                    total_errors
                },
            )
        }
        2 => {
            // Mode 2 (TM5-faithful): two-level evolving pattern (PMULLW-style)
            run_phased_test(
                blocks, thread_id, error_mode, timing, config, progress,
                test_name, TestAction::WriteVerify, 1, false, config.test_reps, config.verify_reps,
                |ctx: &ChunkCtx| {
                    let base_seed = pattern_gen::block_seed(ctx.ptr as usize, ctx.thread_id, ctx.cycle);
                    let mut page_seed = base_seed;
                    let mut page_step = base_seed.wrapping_mul(0x5DEECE66D);
                    let mut idx = ctx.chunk_start;
                    while idx < ctx.chunk_end {
                        let page_end = (idx + cl_elements).min(ctx.chunk_end);
                        for i in idx..page_end {
                            *ctx.ptr.add(i) = pattern_gen::mode2_element(page_seed, (i - idx) as u64, page_step);
                        }
                        let (ns, nst) = pattern_gen::mode2_evolve(page_seed, page_step, param0, param1);
                        page_seed = ns;
                        page_step = nst;
                        idx = page_end;
                    }
                },
                |ctx: &ChunkCtx| {
                    let base_seed = pattern_gen::block_seed(ctx.ptr as usize, ctx.thread_id, ctx.cycle);
                    let mut page_seed = base_seed;
                    let mut page_step = base_seed.wrapping_mul(0x5DEECE66D);
                    let mut idx = ctx.chunk_start;
                    while idx < ctx.chunk_end {
                        let page_end = (idx + cl_elements).min(ctx.chunk_end);
                        for i in idx..page_end {
                            *ctx.ptr.add(i) = pattern_gen::mode2_element(page_seed, (i - idx) as u64, page_step);
                        }
                        let (ns, nst) = pattern_gen::mode2_evolve(page_seed, page_step, param0, param1);
                        page_seed = ns;
                        page_step = nst;
                        idx = page_end;
                    }
                },
                |ctx: &ChunkCtx| -> u64 {
                    let base_seed = pattern_gen::block_seed(ctx.ptr as usize, ctx.thread_id, ctx.cycle);
                    let mut page_seed = base_seed;
                    let mut page_step = base_seed.wrapping_mul(0x5DEECE66D);
                    let mut total_errors = 0u64;
                    let mut idx = ctx.chunk_start;
                    while idx < ctx.chunk_end {
                        let page_end = (idx + cl_elements).min(ctx.chunk_end);
                        for i in idx..page_end {
                            let expected = pattern_gen::mode2_element(page_seed, (i - idx) as u64, page_step);
                            let actual = *ctx.ptr.add(i);
                            if actual != expected {
                                total_errors += 1;
                                if total_errors <= 10 {
                                    log::error!("{}: error at idx {} - expected {:#x}, got {:#x}",
                                               test_name, i, expected, actual);
                                }
                            }
                        }
                        let (ns, nst) = pattern_gen::mode2_evolve(page_seed, page_step, param0, param1);
                        page_seed = ns;
                        page_step = nst;
                        idx = page_end;
                    }
                    total_errors
                },
            )
        }
        12 => {
            // Mode 12 (TMR-native): LCG chain — real PRNG.
            let multiplier = param0;
            let addend = param1;
            // Initial seed from thread_id for per-thread uniqueness
            let initial_seed = (thread_id as u64).wrapping_mul(0x9E3779B97F4A7C15).wrapping_add(1);

            run_phased_test(
                blocks, thread_id, error_mode, timing, config, progress,
                test_name, TestAction::WriteVerify, 1, false, config.test_reps, config.verify_reps,
                // Init: first write (same as test_fn — ensures memory has valid patterns before cycle loop)
                |ctx: &ChunkCtx| {
                    let mut state = pattern_gen::lcg_next(
                        initial_seed.wrapping_add(ctx.chunk_start as u64),
                        multiplier, addend,
                    );
                    for idx in ctx.chunk_start..ctx.chunk_end {
                        *ctx.ptr.add(idx) = state;
                        state = pattern_gen::lcg_next(state, multiplier, addend);
                    }
                },
                // Test: write LCG sequence (every cycle, matching v1 write+verify structure)
                |ctx: &ChunkCtx| {
                    let mut state = pattern_gen::lcg_next(
                        initial_seed.wrapping_add(ctx.chunk_start as u64),
                        multiplier, addend,
                    );
                    for idx in ctx.chunk_start..ctx.chunk_end {
                        *ctx.ptr.add(idx) = state;
                        state = pattern_gen::lcg_next(state, multiplier, addend);
                    }
                },
                // Verify: regenerate LCG and compare (with error_check_interval for v1 parity)
                |ctx: &ChunkCtx| -> u64 {
                    let mut total_errors = 0u64;
                    let mut state = pattern_gen::lcg_next(
                        initial_seed.wrapping_add(ctx.chunk_start as u64),
                        multiplier, addend,
                    );
                    match ctx.check_mask {
                        Some(check_mask) => {
                            let mut interval_errors = 0u64;
                            let mut element_count = 0u32;
                            for idx in ctx.chunk_start..ctx.chunk_end {
                                let actual = *ctx.ptr.add(idx);
                                if actual != state {
                                    interval_errors += 1;
                                    if interval_errors <= 10 {
                                        log::error!("{}: error at idx {} - expected {:#x}, got {:#x}",
                                                   test_name, idx, state, actual);
                                    }
                                }
                                state = pattern_gen::lcg_next(state, multiplier, addend);
                                element_count += 1;
                                if (element_count & check_mask) == 0
                                    && interval_errors > 0 {
                                        total_errors += interval_errors;
                                        interval_errors = 0;
                                    }
                            }
                            total_errors += interval_errors;
                        }
                        None => {
                            for idx in ctx.chunk_start..ctx.chunk_end {
                                let actual = *ctx.ptr.add(idx);
                                if actual != state {
                                    total_errors += 1;
                                    if total_errors <= 10 {
                                        log::error!("{}: error at idx {} - expected {:#x}, got {:#x}",
                                                   test_name, idx, state, actual);
                                    }
                                }
                                state = pattern_gen::lcg_next(state, multiplier, addend);
                            }
                        }
                    }
                    total_errors
                },
            )
        }
        11 => {
            // Mode 11 (TMR-native): inverted constant
            let combined = param0 ^ param1;
            run_phased_test(
                blocks, thread_id, error_mode, timing, config, progress,
                test_name, TestAction::WriteVerify, 1, false, config.test_reps, config.verify_reps,
                // Init: first write
                |ctx: &ChunkCtx| {
                    for idx in ctx.chunk_start..ctx.chunk_end {
                        *ctx.ptr.add(idx) = pattern_gen::pattern_mode11(idx as u64, combined);
                    }
                },
                // Test: write patterns (every cycle)
                |ctx: &ChunkCtx| {
                    for idx in ctx.chunk_start..ctx.chunk_end {
                        *ctx.ptr.add(idx) = pattern_gen::pattern_mode11(idx as u64, combined);
                    }
                },
                // Verify: read and compare (with error_check_interval for v1 parity)
                |ctx: &ChunkCtx| -> u64 {
                    let mut total_errors = 0u64;
                    match ctx.check_mask {
                        Some(check_mask) => {
                            let mut interval_errors = 0u64;
                            let mut element_count = 0u32;
                            for idx in ctx.chunk_start..ctx.chunk_end {
                                let expected = pattern_gen::pattern_mode11(idx as u64, combined);
                                let actual = *ctx.ptr.add(idx);
                                if actual != expected {
                                    interval_errors += 1;
                                    if interval_errors <= 10 {
                                        log::error!("{}: error at idx {} - expected {:#x}, got {:#x}",
                                                   test_name, idx, expected, actual);
                                    }
                                }
                                element_count += 1;
                                if (element_count & check_mask) == 0
                                    && interval_errors > 0 {
                                        total_errors += interval_errors;
                                        interval_errors = 0;
                                    }
                            }
                            total_errors += interval_errors;
                        }
                        None => {
                            for idx in ctx.chunk_start..ctx.chunk_end {
                                let expected = pattern_gen::pattern_mode11(idx as u64, combined);
                                let actual = *ctx.ptr.add(idx);
                                if actual != expected {
                                    total_errors += 1;
                                    if total_errors <= 10 {
                                        log::error!("{}: error at idx {} - expected {:#x}, got {:#x}",
                                                   test_name, idx, expected, actual);
                                    }
                                }
                            }
                        }
                    }
                    total_errors
                },
            )
        }
        _ => {
            // Mode 10 (TMR-native): address-derived unique (default fallback)
            let base = param0;
            run_phased_test(
                blocks, thread_id, error_mode, timing, config, progress,
                test_name, TestAction::WriteVerify, 1, false, config.test_reps, config.verify_reps,
                // Init: first write
                |ctx: &ChunkCtx| {
                    for idx in ctx.chunk_start..ctx.chunk_end {
                        *ctx.ptr.add(idx) = pattern_gen::pattern_mode10(idx as u64, base);
                    }
                },
                // Test: write patterns (every cycle)
                |ctx: &ChunkCtx| {
                    for idx in ctx.chunk_start..ctx.chunk_end {
                        *ctx.ptr.add(idx) = pattern_gen::pattern_mode10(idx as u64, base);
                    }
                },
                // Verify: read and compare (with error_check_interval for v1 parity)
                |ctx: &ChunkCtx| -> u64 {
                    let mut total_errors = 0u64;
                    match ctx.check_mask {
                        Some(check_mask) => {
                            let mut interval_errors = 0u64;
                            let mut element_count = 0u32;
                            for idx in ctx.chunk_start..ctx.chunk_end {
                                let expected = pattern_gen::pattern_mode10(idx as u64, base);
                                let actual = *ctx.ptr.add(idx);
                                if actual != expected {
                                    interval_errors += 1;
                                    if interval_errors <= 10 {
                                        log::error!("{}: error at idx {} - expected {:#x}, got {:#x}",
                                                   test_name, idx, expected, actual);
                                    }
                                }
                                element_count += 1;
                                if (element_count & check_mask) == 0
                                    && interval_errors > 0 {
                                        total_errors += interval_errors;
                                        interval_errors = 0;
                                    }
                            }
                            total_errors += interval_errors;
                        }
                        None => {
                            for idx in ctx.chunk_start..ctx.chunk_end {
                                let expected = pattern_gen::pattern_mode10(idx as u64, base);
                                let actual = *ctx.ptr.add(idx);
                                if actual != expected {
                                    total_errors += 1;
                                    if total_errors <= 10 {
                                        log::error!("{}: error at idx {} - expected {:#x}, got {:#x}",
                                                   test_name, idx, expected, actual);
                                    }
                                }
                            }
                        }
                    }
                    total_errors
                },
            )
        }
    }
}

/// Strided SimpleTest v2 — tests DRAM row-stress patterns via strided access.
///
/// TM5's Parameter field specifies stride in cache lines. When Parameter=254:
/// stride = 254 * 8 = 2032 u64 elements = 16,256 bytes per step.
/// This forces access across different DRAM rows, stressing row buffer conflicts.
unsafe fn simple_test_v2_strided(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
    pattern: SimplePatternConfig,
) -> TestStats {
    let SimplePatternConfig { mode: pattern_mode, param0, param1, stride } = pattern;
    let test_name = "Mem-SimpleV2";
    // For strided access, stateful modes (2, 12) fall back to positional mode 10.
    // LCG chains don't compose with non-sequential access. Positional modes (0/1/10/11) work fine.
    let effective_mode = match pattern_mode {
        0 | 1 | 10 | 11 => pattern_mode,
        _ => 10, // Stateful modes fall back to TMR-native positional
    };
    let base = match effective_mode {
        11 => param0 ^ param1,
        _ => param0,
    };
    let cl_shift = pattern_gen::cache_line_shift(config.cache_line_bytes);

    // Dispatch pattern function once outside hot loop (avoids per-element branch)
    let gen_pattern: fn(u64, u64, u32) -> u64 = match effective_mode {
        0 => |idx, seed, _cl| pattern_gen::pattern_mode0(idx, seed),
        1 => |idx, seed, cl| pattern_gen::pattern_mode1(idx, seed, cl),
        11 => |idx, combined, _cl| pattern_gen::pattern_mode11(idx, combined),
        _ => |idx, base, _cl| pattern_gen::pattern_mode10(idx, base),
    };

    run_phased_test(
        blocks, thread_id, error_mode, timing, config, progress,
        test_name, TestAction::WriteVerify, 1, false, config.test_reps, config.verify_reps,
        // Init: first write with stride pattern covering all elements
        |ctx: &ChunkCtx| {
            let seed = if effective_mode <= 1 {
                pattern_gen::block_seed(ctx.ptr as usize, ctx.thread_id, ctx.cycle)
            } else { base };
            let len = ctx.chunk_end - ctx.chunk_start;
            if len == 0 { return; }
            for sub_offset in 0..stride.min(len) {
                let mut idx = ctx.chunk_start + sub_offset;
                while idx < ctx.chunk_end {
                    *ctx.ptr.add(idx) = gen_pattern(idx as u64, seed, cl_shift);
                    idx += stride;
                }
            }
        },
        // Test: write with stride (every cycle, matching v1 write+verify structure)
        |ctx: &ChunkCtx| {
            let seed = if effective_mode <= 1 {
                pattern_gen::block_seed(ctx.ptr as usize, ctx.thread_id, ctx.cycle)
            } else { base };
            let len = ctx.chunk_end - ctx.chunk_start;
            if len == 0 { return; }
            for sub_offset in 0..stride.min(len) {
                let mut idx = ctx.chunk_start + sub_offset;
                while idx < ctx.chunk_end {
                    *ctx.ptr.add(idx) = gen_pattern(idx as u64, seed, cl_shift);
                    idx += stride;
                }
            }
        },
        // Verify: same strided order (with error_check_interval for v1 parity)
        |ctx: &ChunkCtx| -> u64 {
            let seed = if effective_mode <= 1 {
                pattern_gen::block_seed(ctx.ptr as usize, ctx.thread_id, ctx.cycle)
            } else { base };
            let mut total_errors = 0u64;
            let len = ctx.chunk_end - ctx.chunk_start;
            if len == 0 { return 0; }
            match ctx.check_mask {
                Some(check_mask) => {
                    let mut interval_errors = 0u64;
                    let mut element_count = 0u32;
                    for sub_offset in 0..stride.min(len) {
                        let mut idx = ctx.chunk_start + sub_offset;
                        while idx < ctx.chunk_end {
                            let expected = gen_pattern(idx as u64, seed, cl_shift);
                            let actual = *ctx.ptr.add(idx);
                            if actual != expected {
                                interval_errors += 1;
                                if interval_errors <= 10 {
                                    log::error!("{}: strided error at idx {} (stride={}) - expected {:#x}, got {:#x}",
                                               test_name, idx, stride, expected, actual);
                                }
                            }
                            element_count += 1;
                            if (element_count & check_mask) == 0
                                && interval_errors > 0 {
                                    total_errors += interval_errors;
                                    interval_errors = 0;
                                }
                            idx += stride;
                        }
                    }
                    total_errors += interval_errors;
                }
                None => {
                    for sub_offset in 0..stride.min(len) {
                        let mut idx = ctx.chunk_start + sub_offset;
                        while idx < ctx.chunk_end {
                            let expected = gen_pattern(idx as u64, seed, cl_shift);
                            let actual = *ctx.ptr.add(idx);
                            if actual != expected {
                                total_errors += 1;
                                if total_errors <= 10 {
                                    log::error!("{}: strided error at idx {} (stride={}) - expected {:#x}, got {:#x}",
                                               test_name, idx, stride, expected, actual);
                                }
                            }
                            idx += stride;
                        }
                    }
                }
            }
            total_errors
        },
    )
}

// ─── MirrorMove v2 — u64 migration ──────────────────────────────────────────

// ─── SwapMode: determines mirror access pattern from TM5 Parameter ──────────

/// Mirror swap pattern, derived from config's TestParameterContext.
///
/// - `Full`: Mirror entire region as one piece (Parameter=0 or 1)
/// - `Subblocks(n)`: Split into n subblocks, mirror each in lockstep (Parameter=2-4)
/// - `PageStride(param)`: Strided access with interleave passes (MirrorMove128 Parameter=N)
///   The `param` value is scaled by SIMD width at use site.
#[derive(Debug, Clone, Copy)]
enum SwapMode {
    Full,
    Subblocks(usize),
    PageStride(usize),
}

impl SwapMode {
    /// Derive swap mode from test config's parameter_context.
    fn from_config(config: &TestMemoryConfig) -> Self {
        if let Some(ctx) = &config.parameter_context {
            if let Some(stride_bytes) = ctx.page_stride_bytes
                && stride_bytes > 0 {
                    // Store raw parameter for SIMD-width scaling at use site.
                    // TM5 formula: page_stride_bytes = (param+1)*128, so param = stride_bytes/128 - 1
                    // But we have raw_parameter directly.
                    return SwapMode::PageStride(ctx.raw_parameter as usize);
                }
            if let Some(sub_count) = ctx.subblock_count
                && sub_count >= 2 {
                    return SwapMode::Subblocks(sub_count as usize);
                }
        }
        SwapMode::Full
    }
}

// ─── SIMD MirrorMove v2 Macros ──────────────────────────────────────────────
//
// These macros stamp out the Init, Swap, and Verify phases for any SIMD width.
// Each macro is parameterized by SIMD type and element count. The master macro
// `mirror_move_v2_impl!` combines them into a complete test function.

/// SIMD Init: write incrementing pattern using SIMD stores.
/// Pattern: (element_idx + thread_base) * MIRROR_CONST — linear, so stride is constant.
macro_rules! mirror_init_simd {
    ($simd_type:ty, $simd_w:expr, $ctx:expr,
     $base_vec:expr, $const_vec:expr, $lane_offsets:expr, $step:expr) => {{
        let _len = $ctx.chunk_end - $ctx.chunk_start;
        debug_assert!(_len % $simd_w == 0, "chunk not aligned to SIMD width");
        let mut expected = (<$simd_type>::splat($ctx.chunk_start as u64)
            + $lane_offsets + $base_vec) * $const_vec;
        for i in ($ctx.chunk_start..$ctx.chunk_end).step_by($simd_w) {
            *($ctx.ptr.add(i) as *mut $simd_type) = expected;
            expected += $step;
        }
    }}
}

/// SIMD Swap — subblocks mode: split chunk into N subblocks, mirror each in lockstep.
/// N=1 is equivalent to full mirror. N=2-4 creates cross-region cache pressure.
///
/// Uses computed indices per iteration — LLVM strength-reduces the multiplies to
/// additive increments internally while keeping all values in registers (no array spills).
macro_rules! mirror_swap_subblocks {
    ($simd_type:ty, $simd_w:expr, $ctx:expr, $n_sub:expr) => {{
        let chunk_len = $ctx.chunk_end - $ctx.chunk_start;
        let sub_size = chunk_len / $n_sub;
        let pairs = sub_size / ($simd_w * 2);

        // Forward mirror: all subblocks advance in lockstep
        for iter in 0..pairs {
            for sub in 0..$n_sub {
                let lo = $ctx.chunk_start + sub * sub_size + iter * $simd_w;
                let hi = $ctx.chunk_start + (sub + 1) * sub_size - (iter + 1) * $simd_w;
                let a = *($ctx.ptr.add(lo) as *const $simd_type);
                let b = *($ctx.ptr.add(hi) as *const $simd_type);
                *($ctx.ptr.add(lo) as *mut $simd_type) = b;
                *($ctx.ptr.add(hi) as *mut $simd_type) = a;
            }
        }
        // Reverse mirror (unmirror): restore original positions
        for iter in 0..pairs {
            for sub in 0..$n_sub {
                let lo = $ctx.chunk_start + sub * sub_size + iter * $simd_w;
                let hi = $ctx.chunk_start + (sub + 1) * sub_size - (iter + 1) * $simd_w;
                let a = *($ctx.ptr.add(lo) as *const $simd_type);
                let b = *($ctx.ptr.add(hi) as *const $simd_type);
                *($ctx.ptr.add(lo) as *mut $simd_type) = b;
                *($ctx.ptr.add(hi) as *mut $simd_type) = a;
            }
        }
    }}
}

/// SIMD Swap — page stride mode: strided mirror with interleave passes.
/// `param` is the raw TM5 Parameter value. Stride scales with SIMD width:
/// block = simd_w elements, stride = param * simd_w, total = (param+1) * simd_w.
macro_rules! mirror_swap_strided {
    ($simd_type:ty, $simd_w:expr, $ctx:expr, $param:expr) => {{
        let block_elements: usize = $simd_w;
        let stride_elements: usize = $param * block_elements;
        let total_stride: usize = stride_elements + block_elements;
        let interleave_passes: usize = if block_elements > 0 { total_stride / block_elements } else { 1 };

        // Forward strided mirror
        for pass in 0..interleave_passes {
            let offset = pass * block_elements;
            let mut lo = $ctx.chunk_start + offset;
            let hi_base = $ctx.chunk_end;
            // hi starts from the end, offset inward by the same pass offset
            let mut hi = if hi_base >= block_elements + offset {
                hi_base - block_elements - offset
            } else {
                continue;
            };
            while lo < hi {
                let a = *($ctx.ptr.add(lo) as *const $simd_type);
                let b = *($ctx.ptr.add(hi) as *const $simd_type);
                *($ctx.ptr.add(lo) as *mut $simd_type) = b;
                *($ctx.ptr.add(hi) as *mut $simd_type) = a;
                lo += total_stride;
                if hi < total_stride { break; }
                hi -= total_stride;
            }
        }
        // Reverse strided mirror (unmirror)
        for pass in 0..interleave_passes {
            let offset = pass * block_elements;
            let mut lo = $ctx.chunk_start + offset;
            let hi_base = $ctx.chunk_end;
            let mut hi = if hi_base >= block_elements + offset {
                hi_base - block_elements - offset
            } else {
                continue;
            };
            while lo < hi {
                let a = *($ctx.ptr.add(lo) as *const $simd_type);
                let b = *($ctx.ptr.add(hi) as *const $simd_type);
                *($ctx.ptr.add(lo) as *mut $simd_type) = b;
                *($ctx.ptr.add(hi) as *mut $simd_type) = a;
                lo += total_stride;
                if hi < total_stride { break; }
                hi -= total_stride;
            }
        }
    }}
}

/// SIMD Swap — full mirror: simple two-pointer walk, no arrays, no subblock overhead.
/// Keeps lo/hi as scalar registers for optimal codegen on the most common path.
macro_rules! mirror_swap_full {
    ($simd_type:ty, $simd_w:expr, $ctx:expr) => {{
        let mut lo = $ctx.chunk_start;
        let mut hi = $ctx.chunk_end - $simd_w;

        // Forward mirror
        while lo < hi {
            let a = *($ctx.ptr.add(lo) as *const $simd_type);
            let b = *($ctx.ptr.add(hi) as *const $simd_type);
            *($ctx.ptr.add(lo) as *mut $simd_type) = b;
            *($ctx.ptr.add(hi) as *mut $simd_type) = a;
            lo += $simd_w;
            hi -= $simd_w;
        }

        // Reverse mirror (unmirror)
        lo = $ctx.chunk_start;
        hi = $ctx.chunk_end - $simd_w;
        while lo < hi {
            let a = *($ctx.ptr.add(lo) as *const $simd_type);
            let b = *($ctx.ptr.add(hi) as *const $simd_type);
            *($ctx.ptr.add(lo) as *mut $simd_type) = b;
            *($ctx.ptr.add(hi) as *mut $simd_type) = a;
            lo += $simd_w;
            hi -= $simd_w;
        }
    }}
}

/// SIMD Swap dispatcher: routes to full, subblocks, or strided based on SwapMode.
macro_rules! mirror_swap_simd {
    ($simd_type:ty, $simd_w:expr, $ctx:expr, $swap_mode:expr) => {{
        match $swap_mode {
            SwapMode::Full => {
                mirror_swap_full!($simd_type, $simd_w, $ctx);
            }
            SwapMode::Subblocks(n) => {
                mirror_swap_subblocks!($simd_type, $simd_w, $ctx, n);
            }
            SwapMode::PageStride(param) => {
                mirror_swap_strided!($simd_type, $simd_w, $ctx, param);
            }
        }
    }}
}

/// SIMD Verify: incrementing pattern with XOR+OR accumulator.
/// Unified error check: `.simd_ne(zero).any()` across all widths.
///
/// When check_mask is set, processes in fixed-size batches to eliminate
/// per-iteration counter and branch from the hot loop. The inner batch
/// loop has a known trip count, enabling better code generation.
macro_rules! mirror_verify_simd {
    ($simd_type:ty, $simd_w:expr, $ctx:expr,
     $base_vec:expr, $const_vec:expr, $lane_offsets:expr, $step:expr, $zero:expr) => {{
        let _len = $ctx.chunk_end - $ctx.chunk_start;
        debug_assert!(_len % $simd_w == 0, "chunk not aligned to SIMD width");
        let mut total_errors = 0u64;

        let mut expected = (<$simd_type>::splat($ctx.chunk_start as u64)
            + $lane_offsets + $base_vec) * $const_vec;

        match $ctx.check_mask {
            Some(check_mask) => {
                // Batch processing: accumulate for exactly (check_mask+1) vectors,
                // then check once. No per-iteration counter or branch.
                let vectors_per_check = (check_mask as usize) + 1;
                let batch_elements = vectors_per_check * $simd_w;
                let mut pos = $ctx.chunk_start;
                let aligned_end = $ctx.chunk_end - ($ctx.chunk_end - $ctx.chunk_start) % batch_elements;

                // Main batched loop: known trip count per batch
                while pos < aligned_end {
                    let mut error_acc = $zero;
                    let batch_end = pos + batch_elements;
                    for i in (pos..batch_end).step_by($simd_w) {
                        let actual = *($ctx.ptr.add(i) as *const $simd_type);
                        error_acc |= actual ^ expected;
                        expected += $step;
                    }
                    if error_acc.simd_ne($zero).any() {
                        total_errors += 1;
                    }
                    pos = batch_end;
                }

                // Remainder (fewer than one full batch)
                if pos < $ctx.chunk_end {
                    let mut error_acc = $zero;
                    for i in (pos..$ctx.chunk_end).step_by($simd_w) {
                        let actual = *($ctx.ptr.add(i) as *const $simd_type);
                        error_acc |= actual ^ expected;
                        expected += $step;
                    }
                    if error_acc.simd_ne($zero).any() {
                        total_errors += 1;
                    }
                }
            }
            None => {
                let mut error_acc = $zero;
                for i in ($ctx.chunk_start..$ctx.chunk_end).step_by($simd_w) {
                    let actual = *($ctx.ptr.add(i) as *const $simd_type);
                    error_acc |= actual ^ expected;
                    expected += $step;
                }
                if error_acc.simd_ne($zero).any() {
                    total_errors += 1;
                }
            }
        }

        // Repair on error: re-write correct pattern to this chunk
        if total_errors > 0 {
            mirror_init_simd!($simd_type, $simd_w, $ctx,
                $base_vec, $const_vec, $lane_offsets, $step);
        }
        total_errors
    }}
}

/// Master macro: stamps out a complete MirrorMove v2 impl function for a given SIMD width.
/// Generates an `unsafe fn` with `#[target_feature]` that handles all swap modes.
macro_rules! mirror_move_v2_impl {
    (
        $fn_name:ident,
        $test_name:literal,
        $simd_type:ty,
        $simd_w:expr,
        $lane_offsets:expr,
        $target_feature:literal
    ) => {
        #[target_feature(enable = $target_feature)]
        unsafe fn $fn_name(
            blocks: &[crate::runner::AllocationBlock],
            thread_id: usize,
            error_mode: ErrorMode,
            timing: &TestTiming,
            config: &TestMemoryConfig,
            progress: Option<&TestProgress>,
        ) -> TestStats {
            let test_name = $test_name;
            let thread_base = pattern_gen::mirror_thread_base(thread_id);
            let simd_elements: usize = $simd_w;

            // Pre-compute SIMD constants for pattern generation.
            const MIRROR_CONST: u64 = 0x0123456789ABCDEFu64;
            let base_vec = <$simd_type>::splat(thread_base);
            let const_vec = <$simd_type>::splat(MIRROR_CONST);
            let lane_offsets = <$simd_type>::from_array($lane_offsets);
            let step = <$simd_type>::splat((simd_elements as u64).wrapping_mul(MIRROR_CONST));
            let zero = <$simd_type>::splat(0);

            // Determine swap mode from config parameter
            let swap_mode = SwapMode::from_config(config);

            run_phased_test(
                blocks, thread_id, error_mode, timing, config, progress,
                test_name, TestAction::WriteWaitVerify,
                4,  // bytes_per_test_op: 2R + 2W per mirror round-trip
                false,  // skip_init: always init (independent mode)
                config.test_reps,  // test_reps: mirror round-trips before verify
                config.verify_reps,  // verify_reps: verification passes

                // Init: SIMD pattern writes
                |ctx: &ChunkCtx| {
                    mirror_init_simd!($simd_type, simd_elements, ctx,
                        base_vec, const_vec, lane_offsets, step);
                },

                // Test: mirror swap (supports Full, Subblocks, PageStride)
                |ctx: &ChunkCtx| {
                    mirror_swap_simd!($simd_type, simd_elements, ctx, swap_mode);
                },

                // Verify: SIMD incrementing pattern + accumulator
                |ctx: &ChunkCtx| -> u64 {
                    mirror_verify_simd!($simd_type, simd_elements, ctx,
                        base_vec, const_vec, lane_offsets, step, zero)
                },
            )
        }
    }
}

/// MirrorMove v2 scalar — u64 patterns with full 64-bit coverage.
///
/// # Safety
/// Caller must ensure all blocks contain valid, aligned, writable memory.
pub unsafe fn mirror_move_v2_multi(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> TestStats {
    let test_name = "Mem-MirrorV2";
    // Use upper 32 bits for thread separation (vs old << 16)
    let thread_base = pattern_gen::mirror_thread_base(thread_id);

    run_phased_test(
        blocks, thread_id, error_mode, timing, config, progress,
        test_name, TestAction::WriteWaitVerify,
        4, // bytes_per_test_op: 2R + 2W per mirror round-trip
        false,  // skip_init: always init (independent mode)
        config.test_reps,  // test_reps: mirror round-trips before verify
        config.verify_reps,  // verify_reps: verification passes
        // Init: write forward patterns once (TM5: RS_Set fills before test sequence)
        |ctx: &ChunkCtx| {
            for i in ctx.chunk_start..ctx.chunk_end {
                *ctx.ptr.add(i) = pattern_gen::mirror_pattern_u64(i as u64, thread_base);
            }
        },
        // Test: round-trip mirror swap (TM5: single loop that crosses midpoint)
        // Mirror then unmirror — data returns to original position each cycle
        |ctx: &ChunkCtx| {
            // First pass: mirror (reverse the chunk)
            let mut lo = ctx.chunk_start;
            let mut hi = ctx.chunk_end - 1;
            while lo < hi {
                let a = *ctx.ptr.add(lo);
                let b = *ctx.ptr.add(hi);
                *ctx.ptr.add(lo) = b;
                *ctx.ptr.add(hi) = a;
                lo += 1;
                hi -= 1;
            }
            // Second pass: unmirror (restore original order)
            lo = ctx.chunk_start;
            hi = ctx.chunk_end - 1;
            while lo < hi {
                let a = *ctx.ptr.add(lo);
                let b = *ctx.ptr.add(hi);
                *ctx.ptr.add(lo) = b;
                *ctx.ptr.add(hi) = a;
                lo += 1;
                hi -= 1;
            }
        },
        // Verify: check original (forward) patterns + repair chunk on error
        // (with error_check_interval for v1 parity)
        |ctx: &ChunkCtx| -> u64 {
            let mut total_errors = 0u64;
            match ctx.check_mask {
                Some(check_mask) => {
                    let mut interval_errors = 0u64;
                    let mut element_count = 0u32;
                    for i in ctx.chunk_start..ctx.chunk_end {
                        let expected = pattern_gen::mirror_pattern_u64(i as u64, thread_base);
                        let actual = *ctx.ptr.add(i);
                        if actual != expected {
                            interval_errors += 1;
                            if interval_errors <= 10 {
                                log::error!("{}: error at idx {} - expected {:#x}, got {:#x}",
                                           test_name, i, expected, actual);
                            }
                        }
                        element_count += 1;
                        if (element_count & check_mask) == 0
                            && interval_errors > 0 {
                                total_errors += interval_errors;
                                interval_errors = 0;
                            }
                    }
                    total_errors += interval_errors;
                }
                None => {
                    for i in ctx.chunk_start..ctx.chunk_end {
                        let expected = pattern_gen::mirror_pattern_u64(i as u64, thread_base);
                        let actual = *ctx.ptr.add(i);
                        if actual != expected {
                            total_errors += 1;
                            if total_errors <= 10 {
                                log::error!("{}: error at idx {} - expected {:#x}, got {:#x}",
                                           test_name, i, expected, actual);
                            }
                        }
                    }
                }
            }
            // Repair this chunk if errors detected (TM5: RS_Set on failure)
            if total_errors > 0 {
                for i in ctx.chunk_start..ctx.chunk_end {
                    *ctx.ptr.add(i) = pattern_gen::mirror_pattern_u64(i as u64, thread_base);
                }
            }
            total_errors
        },
    )
}

/// MirrorMove v2 SSE2 (u64x2) — 128-bit SIMD with u64 lanes.
///
/// # Safety
/// Caller must ensure all blocks contain valid, aligned, writable memory.
pub unsafe fn mirror_move_v2_128_multi(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> TestStats {
    if is_x86_feature_detected!("sse2") {
        mirror_move_v2_128_impl(blocks, thread_id, error_mode, timing, config, progress)
    } else {
        mirror_move_v2_multi(blocks, thread_id, error_mode, timing, config, progress)
    }
}

mirror_move_v2_impl!(mirror_move_v2_128_impl, "Mem-MirrorV2-128", u64x2, 2, [0, 1], "sse4.2,sse4.1,ssse3,sse3,sse2,popcnt");

/// MirrorMove v2 AVX2 (u64x4) — 256-bit SIMD with u64 lanes.
///
/// # Safety
/// Caller must ensure all blocks contain valid, aligned, writable memory.
pub unsafe fn mirror_move_v2_256_multi(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> TestStats {
    if is_x86_feature_detected!("avx2") {
        mirror_move_v2_256_impl(blocks, thread_id, error_mode, timing, config, progress)
    } else {
        mirror_move_v2_128_multi(blocks, thread_id, error_mode, timing, config, progress)
    }
}

mirror_move_v2_impl!(mirror_move_v2_256_impl, "Mem-MirrorV2-256", u64x4, 4, [0, 1, 2, 3], "avx2,avx,fma,bmi1,bmi2");

/// MirrorMove v2 AVX-512 (u64x8) — 512-bit SIMD with u64 lanes.
///
/// # Safety
/// Caller must ensure all blocks contain valid, aligned, writable memory.
pub unsafe fn mirror_move_v2_512_multi(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> TestStats {
    if is_x86_feature_detected!("avx512f") {
        mirror_move_v2_512_impl(blocks, thread_id, error_mode, timing, config, progress)
    } else {
        mirror_move_v2_256_multi(blocks, thread_id, error_mode, timing, config, progress)
    }
}

mirror_move_v2_impl!(mirror_move_v2_512_impl, "Mem-MirrorV2-512", u64x8, 8, [0, 1, 2, 3, 4, 5, 6, 7], "avx512f,avx512bw,avx512cd,avx512dq,avx512vl,avx2,avx,fma,bmi1,bmi2");

// MirrorMove v2 auto-dispatch — selects best SIMD variant at runtime.
crate::auto_dispatch!(
    pub mirror_move_v2_auto_multi,
    mirror_move_v2_multi,
    mirror_move_v2_128_multi,
    mirror_move_v2_256_multi,
    mirror_move_v2_512_multi
);

// ─── SIMD SimpleTest v2 Macros ──────────────────────────────────────────────
//
// These macros stamp out SIMD Write and Verify phases for SimpleTest at any width.
// Two code paths: positional (Mode 0/1: idx ^ base) and LCG (Mode 2: PRNG chain).
// The master macro `simple_test_v2_impl!` combines them into a complete test function.

/// SIMD Write for positional patterns (Mode 0/1): stores (idx ^ base) per element.
/// Pattern is purely position-based, so no state to carry between chunks.
macro_rules! simple_write_positional_simd {
    ($simd_type:ty, $simd_w:expr, $ctx:expr, $base_vec:expr, $lane_offsets:expr) => {{
        let _len = $ctx.chunk_end - $ctx.chunk_start;
        debug_assert!(_len % $simd_w == 0, "chunk not aligned to SIMD width");
        let step = <$simd_type>::splat($simd_w as u64);
        let mut idx_vec = <$simd_type>::splat($ctx.chunk_start as u64) + $lane_offsets;
        for i in ($ctx.chunk_start..$ctx.chunk_end).step_by($simd_w) {
            *($ctx.ptr.add(i) as *mut $simd_type) = idx_vec ^ $base_vec;
            idx_vec += step;
        }
    }}
}

/// SIMD Verify for positional patterns (Mode 0/1): XOR+OR accumulator.
/// Batched error checking matches MirrorMove verify for consistent codegen.
macro_rules! simple_verify_positional_simd {
    ($simd_type:ty, $simd_w:expr, $ctx:expr,
     $base_vec:expr, $lane_offsets:expr, $zero:expr) => {{
        let _len = $ctx.chunk_end - $ctx.chunk_start;
        debug_assert!(_len % $simd_w == 0, "chunk not aligned to SIMD width");
        let step = <$simd_type>::splat($simd_w as u64);
        let mut idx_vec = <$simd_type>::splat($ctx.chunk_start as u64) + $lane_offsets;
        let mut total_errors = 0u64;

        match $ctx.check_mask {
            Some(check_mask) => {
                let vectors_per_check = (check_mask as usize) + 1;
                let batch_elements = vectors_per_check * $simd_w;
                let mut pos = $ctx.chunk_start;
                let aligned_end = $ctx.chunk_end - ($ctx.chunk_end - $ctx.chunk_start) % batch_elements;

                while pos < aligned_end {
                    let mut error_acc = $zero;
                    let batch_end = pos + batch_elements;
                    for i in (pos..batch_end).step_by($simd_w) {
                        let actual = *($ctx.ptr.add(i) as *const $simd_type);
                        error_acc |= actual ^ (idx_vec ^ $base_vec);
                        idx_vec += step;
                    }
                    if error_acc.simd_ne($zero).any() {
                        total_errors += 1;
                    }
                    pos = batch_end;
                }

                if pos < $ctx.chunk_end {
                    let mut error_acc = $zero;
                    for i in (pos..$ctx.chunk_end).step_by($simd_w) {
                        let actual = *($ctx.ptr.add(i) as *const $simd_type);
                        error_acc |= actual ^ (idx_vec ^ $base_vec);
                        idx_vec += step;
                    }
                    if error_acc.simd_ne($zero).any() {
                        total_errors += 1;
                    }
                }
            }
            None => {
                let mut error_acc = $zero;
                for i in ($ctx.chunk_start..$ctx.chunk_end).step_by($simd_w) {
                    let actual = *($ctx.ptr.add(i) as *const $simd_type);
                    error_acc |= actual ^ (idx_vec ^ $base_vec);
                    idx_vec += step;
                }
                if error_acc.simd_ne($zero).any() {
                    total_errors += 1;
                }
            }
        }
        total_errors
    }}
}

/// SIMD Write for LCG patterns (Mode 2): uses LcgSimdN for N-lane parallel PRNG.
/// LCG state is seeded per chunk from chunk_start for deterministic replay.
macro_rules! simple_write_lcg_simd {
    ($lcg_type:ty, $simd_type:ty, $simd_w:expr, $ctx:expr,
     $initial_seed:expr, $multiplier:expr, $addend:expr) => {{
        let _len = $ctx.chunk_end - $ctx.chunk_start;
        debug_assert!(_len % $simd_w == 0, "chunk not aligned to SIMD width");
        let chunk_seed = pattern_gen::lcg_next(
            $initial_seed.wrapping_add($ctx.chunk_start as u64),
            $multiplier, $addend,
        );
        let mut lcg = <$lcg_type>::new(chunk_seed, $multiplier, $addend);
        for i in ($ctx.chunk_start..$ctx.chunk_end).step_by($simd_w) {
            *($ctx.ptr.add(i) as *mut $simd_type) = lcg.next();
        }
    }}
}

/// SIMD Verify for LCG patterns (Mode 2): replay LCG and XOR+OR accumulate.
macro_rules! simple_verify_lcg_simd {
    ($lcg_type:ty, $simd_type:ty, $simd_w:expr, $ctx:expr,
     $initial_seed:expr, $multiplier:expr, $addend:expr, $zero:expr) => {{
        let _len = $ctx.chunk_end - $ctx.chunk_start;
        debug_assert!(_len % $simd_w == 0, "chunk not aligned to SIMD width");
        let chunk_seed = pattern_gen::lcg_next(
            $initial_seed.wrapping_add($ctx.chunk_start as u64),
            $multiplier, $addend,
        );
        let mut lcg = <$lcg_type>::new(chunk_seed, $multiplier, $addend);
        let mut total_errors = 0u64;

        match $ctx.check_mask {
            Some(check_mask) => {
                let vectors_per_check = (check_mask as usize) + 1;
                let batch_elements = vectors_per_check * $simd_w;
                let mut pos = $ctx.chunk_start;
                let aligned_end = $ctx.chunk_end - ($ctx.chunk_end - $ctx.chunk_start) % batch_elements;

                while pos < aligned_end {
                    let mut error_acc = $zero;
                    let batch_end = pos + batch_elements;
                    for i in (pos..batch_end).step_by($simd_w) {
                        let actual = *($ctx.ptr.add(i) as *const $simd_type);
                        error_acc |= actual ^ lcg.next();
                    }
                    if error_acc.simd_ne($zero).any() {
                        total_errors += 1;
                    }
                    pos = batch_end;
                }

                if pos < $ctx.chunk_end {
                    let mut error_acc = $zero;
                    for i in (pos..$ctx.chunk_end).step_by($simd_w) {
                        let actual = *($ctx.ptr.add(i) as *const $simd_type);
                        error_acc |= actual ^ lcg.next();
                    }
                    if error_acc.simd_ne($zero).any() {
                        total_errors += 1;
                    }
                }
            }
            None => {
                let mut error_acc = $zero;
                for i in ($ctx.chunk_start..$ctx.chunk_end).step_by($simd_w) {
                    let actual = *($ctx.ptr.add(i) as *const $simd_type);
                    error_acc |= actual ^ lcg.next();
                }
                if error_acc.simd_ne($zero).any() {
                    total_errors += 1;
                }
            }
        }
        total_errors
    }}
}

/// SIMD Write for positional patterns with stride — block-strided access.
/// Writes contiguous SIMD blocks spaced apart by stride, with interleave passes
/// for full coverage. Same approach as MirrorMove's `mirror_swap_strided!`.
///
/// ```text
/// stride_param=3, simd_w=8:  total_stride = (3+1)*8 = 32 elements
///
/// Pass 0: [████████]________________________[████████]________________________
///         pos 0                              pos 32
/// Pass 1: ________[████████]________________________[████████]________________
///         pos 8                              pos 40
/// Pass 2: ________________[████████]________________________[████████]________
///         pos 16                             pos 48
/// Pass 3: ________________________[████████]________________________[████████]
///         pos 24                             pos 56
/// ```
macro_rules! simple_write_strided_positional_simd {
    ($simd_type:ty, $simd_w:expr, $ctx:expr,
     $base_vec:expr, $lane_offsets:expr, $stride_param:expr) => {{
        let block_elements: usize = $simd_w;
        let stride_elements: usize = $stride_param * block_elements;
        let total_stride: usize = stride_elements + block_elements;
        let interleave_passes: usize = total_stride / block_elements;

        for pass in 0..interleave_passes {
            let offset = pass * block_elements;
            let mut pos = $ctx.chunk_start + offset;
            while pos + block_elements <= $ctx.chunk_end {
                let idx_vec = <$simd_type>::splat(pos as u64) + $lane_offsets;
                *($ctx.ptr.add(pos) as *mut $simd_type) = idx_vec ^ $base_vec;
                pos += total_stride;
            }
        }
    }}
}

/// SIMD Verify for positional patterns with stride — block-strided access.
/// Walks the same strided pattern as write, XOR+OR accumulates errors.
macro_rules! simple_verify_strided_positional_simd {
    ($simd_type:ty, $simd_w:expr, $ctx:expr,
     $base_vec:expr, $lane_offsets:expr, $zero:expr, $stride_param:expr) => {{
        let block_elements: usize = $simd_w;
        let stride_elements: usize = $stride_param * block_elements;
        let total_stride: usize = stride_elements + block_elements;
        let interleave_passes: usize = total_stride / block_elements;
        let mut total_errors = 0u64;
        let mut error_acc = $zero;

        for pass in 0..interleave_passes {
            let offset = pass * block_elements;
            let mut pos = $ctx.chunk_start + offset;
            while pos + block_elements <= $ctx.chunk_end {
                let idx_vec = <$simd_type>::splat(pos as u64) + $lane_offsets;
                let expected = idx_vec ^ $base_vec;
                let actual = *($ctx.ptr.add(pos) as *const $simd_type);
                error_acc |= actual ^ expected;
                pos += total_stride;
            }
        }

        if error_acc.simd_ne($zero).any() {
            total_errors += 1;
        }
        total_errors
    }}
}

/// SIMD Write for LCG patterns with stride — block-strided, re-seeded per block.
/// Each SIMD block gets an independent LCG seeded from its position, producing W
/// pseudo-random values per block. The stride provides DRAM row stress while SIMD
/// gives write throughput within each block.
macro_rules! simple_write_strided_lcg_simd {
    ($lcg_type:ty, $simd_type:ty, $simd_w:expr, $ctx:expr,
     $initial_seed:expr, $multiplier:expr, $addend:expr, $stride_param:expr) => {{
        let block_elements: usize = $simd_w;
        let stride_elements: usize = $stride_param * block_elements;
        let total_stride: usize = stride_elements + block_elements;
        let interleave_passes: usize = total_stride / block_elements;

        for pass in 0..interleave_passes {
            let offset = pass * block_elements;
            let mut pos = $ctx.chunk_start + offset;
            while pos + block_elements <= $ctx.chunk_end {
                let block_seed = pattern_gen::lcg_next(
                    $initial_seed.wrapping_add(pos as u64),
                    $multiplier, $addend,
                );
                let mut lcg = <$lcg_type>::new(block_seed, $multiplier, $addend);
                *($ctx.ptr.add(pos) as *mut $simd_type) = lcg.next();
                pos += total_stride;
            }
        }
    }}
}

/// SIMD Verify for LCG patterns with stride — replay per-block LCG, XOR+OR accumulate.
macro_rules! simple_verify_strided_lcg_simd {
    ($lcg_type:ty, $simd_type:ty, $simd_w:expr, $ctx:expr,
     $initial_seed:expr, $multiplier:expr, $addend:expr, $zero:expr, $stride_param:expr) => {{
        let block_elements: usize = $simd_w;
        let stride_elements: usize = $stride_param * block_elements;
        let total_stride: usize = stride_elements + block_elements;
        let interleave_passes: usize = total_stride / block_elements;
        let mut total_errors = 0u64;
        let mut error_acc = $zero;

        for pass in 0..interleave_passes {
            let offset = pass * block_elements;
            let mut pos = $ctx.chunk_start + offset;
            while pos + block_elements <= $ctx.chunk_end {
                let block_seed = pattern_gen::lcg_next(
                    $initial_seed.wrapping_add(pos as u64),
                    $multiplier, $addend,
                );
                let mut lcg = <$lcg_type>::new(block_seed, $multiplier, $addend);
                let actual = *($ctx.ptr.add(pos) as *const $simd_type);
                error_acc |= actual ^ lcg.next();
                pos += total_stride;
            }
        }

        if error_acc.simd_ne($zero).any() {
            total_errors += 1;
        }
        total_errors
    }}
}

/// Master macro: stamps out a complete SIMD SimpleTest v2 function for a given width.
/// Handles all 3 pattern modes: Mode 0 (idx^base), Mode 1 (idx^combined), Mode 2 (LCG).
/// Generates 4 specialized `#[target_feature]` functions (one per pattern/stride combo)
/// plus a lightweight dispatch function. This prevents LLVM from inlining all 4 code paths
/// into one mega-function, which caused 9K+ lines of assembly, 46 `vzeroupper` calls,
/// and instruction cache pressure that made wider SIMD slower than narrower.
///
/// Each specialized function contains exactly ONE run_phased_test call with its closures,
/// producing tight, focused assembly. The dispatch function has NO `#[target_feature]`
/// so it compiles to a simple branch without pulling in SIMD register state.
macro_rules! simple_test_v2_impl {
    (
        $dispatch_fn:ident,
        $pos_seq_fn:ident,
        $pos_str_fn:ident,
        $lcg_seq_fn:ident,
        $lcg_str_fn:ident,
        $test_name:literal,
        $simd_type:ty,
        $simd_w:expr,
        $lane_offsets:expr,
        $lcg_type:ty,
        $target_feature:literal
    ) => {
        /// Positional sequential — contiguous access with idx^base pattern.
        #[target_feature(enable = $target_feature)]
        unsafe fn $pos_seq_fn(
            blocks: &[crate::runner::AllocationBlock],
            thread_id: usize,
            error_mode: ErrorMode,
            timing: &TestTiming,
            config: &TestMemoryConfig,
            progress: Option<&TestProgress>,
        ) -> TestStats {
            let test_name = $test_name;
            let simd_elements: usize = $simd_w;
            let lane_offsets = <$simd_type>::from_array($lane_offsets);
            let zero = <$simd_type>::splat(0);
            let pattern_mode = config.pattern_mode.unwrap_or(0);
            let param0 = config.pattern_param0.unwrap_or(0xDEADBEEFDEADBEEF);
            let param1 = config.pattern_param1.unwrap_or(0xCAFEBABECAFEBABE);
            let base = match pattern_mode {
                1 | 11 => param0 ^ param1,
                _ => param0,
            };
            let base_vec = <$simd_type>::splat(base);

            run_phased_test(
                blocks, thread_id, error_mode, timing, config, progress,
                test_name, TestAction::WriteVerify, 1, false, config.test_reps, config.verify_reps,
                |ctx: &ChunkCtx| {
                    simple_write_positional_simd!($simd_type, simd_elements, ctx,
                        base_vec, lane_offsets);
                },
                |ctx: &ChunkCtx| {
                    simple_write_positional_simd!($simd_type, simd_elements, ctx,
                        base_vec, lane_offsets);
                },
                |ctx: &ChunkCtx| -> u64 {
                    simple_verify_positional_simd!($simd_type, simd_elements, ctx,
                        base_vec, lane_offsets, zero)
                },
            )
        }

        /// Positional strided — block-strided access with idx^base pattern.
        #[target_feature(enable = $target_feature)]
        unsafe fn $pos_str_fn(
            blocks: &[crate::runner::AllocationBlock],
            thread_id: usize,
            error_mode: ErrorMode,
            timing: &TestTiming,
            config: &TestMemoryConfig,
            progress: Option<&TestProgress>,
        ) -> TestStats {
            let test_name = $test_name;
            let simd_elements: usize = $simd_w;
            let lane_offsets = <$simd_type>::from_array($lane_offsets);
            let zero = <$simd_type>::splat(0);
            let pattern_mode = config.pattern_mode.unwrap_or(0);
            let param0 = config.pattern_param0.unwrap_or(0xDEADBEEFDEADBEEF);
            let param1 = config.pattern_param1.unwrap_or(0xCAFEBABECAFEBABE);
            let stride_param = config.parameter_context.as_ref()
                .and_then(|ctx| ctx.stride_elements)
                .unwrap_or(0);
            let base = match pattern_mode {
                1 | 11 => param0 ^ param1,
                _ => param0,
            };
            let base_vec = <$simd_type>::splat(base);

            run_phased_test(
                blocks, thread_id, error_mode, timing, config, progress,
                test_name, TestAction::WriteVerify, 1, false, config.test_reps, config.verify_reps,
                |ctx: &ChunkCtx| {
                    simple_write_strided_positional_simd!($simd_type, simd_elements, ctx,
                        base_vec, lane_offsets, stride_param);
                },
                |ctx: &ChunkCtx| {
                    simple_write_strided_positional_simd!($simd_type, simd_elements, ctx,
                        base_vec, lane_offsets, stride_param);
                },
                |ctx: &ChunkCtx| -> u64 {
                    simple_verify_strided_positional_simd!($simd_type, simd_elements, ctx,
                        base_vec, lane_offsets, zero, stride_param)
                },
            )
        }

        /// LCG sequential — contiguous access with chained PRNG pattern.
        #[target_feature(enable = $target_feature)]
        unsafe fn $lcg_seq_fn(
            blocks: &[crate::runner::AllocationBlock],
            thread_id: usize,
            error_mode: ErrorMode,
            timing: &TestTiming,
            config: &TestMemoryConfig,
            progress: Option<&TestProgress>,
        ) -> TestStats {
            let test_name = $test_name;
            let simd_elements: usize = $simd_w;
            let zero = <$simd_type>::splat(0);
            let multiplier = config.pattern_param0.unwrap_or(0xDEADBEEFDEADBEEF);
            let addend = config.pattern_param1.unwrap_or(0xCAFEBABECAFEBABE);
            let initial_seed = (thread_id as u64)
                .wrapping_mul(0x9E3779B97F4A7C15)
                .wrapping_add(1);

            run_phased_test(
                blocks, thread_id, error_mode, timing, config, progress,
                test_name, TestAction::WriteVerify, 1, false, config.test_reps, config.verify_reps,
                |ctx: &ChunkCtx| {
                    simple_write_lcg_simd!($lcg_type, $simd_type, simd_elements, ctx,
                        initial_seed, multiplier, addend);
                },
                |ctx: &ChunkCtx| {
                    simple_write_lcg_simd!($lcg_type, $simd_type, simd_elements, ctx,
                        initial_seed, multiplier, addend);
                },
                |ctx: &ChunkCtx| -> u64 {
                    simple_verify_lcg_simd!($lcg_type, $simd_type, simd_elements, ctx,
                        initial_seed, multiplier, addend, zero)
                },
            )
        }

        /// LCG strided — block-strided access with re-seeded PRNG pattern.
        #[target_feature(enable = $target_feature)]
        unsafe fn $lcg_str_fn(
            blocks: &[crate::runner::AllocationBlock],
            thread_id: usize,
            error_mode: ErrorMode,
            timing: &TestTiming,
            config: &TestMemoryConfig,
            progress: Option<&TestProgress>,
        ) -> TestStats {
            let test_name = $test_name;
            let simd_elements: usize = $simd_w;
            let zero = <$simd_type>::splat(0);
            let multiplier = config.pattern_param0.unwrap_or(0xDEADBEEFDEADBEEF);
            let addend = config.pattern_param1.unwrap_or(0xCAFEBABECAFEBABE);
            let stride_param = config.parameter_context.as_ref()
                .and_then(|ctx| ctx.stride_elements)
                .unwrap_or(0);
            let initial_seed = (thread_id as u64)
                .wrapping_mul(0x9E3779B97F4A7C15)
                .wrapping_add(1);

            run_phased_test(
                blocks, thread_id, error_mode, timing, config, progress,
                test_name, TestAction::WriteVerify, 1, false, config.test_reps, config.verify_reps,
                |ctx: &ChunkCtx| {
                    simple_write_strided_lcg_simd!($lcg_type, $simd_type, simd_elements, ctx,
                        initial_seed, multiplier, addend, stride_param);
                },
                |ctx: &ChunkCtx| {
                    simple_write_strided_lcg_simd!($lcg_type, $simd_type, simd_elements, ctx,
                        initial_seed, multiplier, addend, stride_param);
                },
                |ctx: &ChunkCtx| -> u64 {
                    simple_verify_strided_lcg_simd!($lcg_type, $simd_type, simd_elements, ctx,
                        initial_seed, multiplier, addend, zero, stride_param)
                },
            )
        }

        /// Dispatch — selects the right specialized function based on pattern mode and stride.
        /// No `#[target_feature]` so this compiles to a simple branch without SIMD register state.
        #[inline(always)]
        unsafe fn $dispatch_fn(
            blocks: &[crate::runner::AllocationBlock],
            thread_id: usize,
            error_mode: ErrorMode,
            timing: &TestTiming,
            config: &TestMemoryConfig,
            progress: Option<&TestProgress>,
        ) -> TestStats {
            let is_lcg = config.pattern_mode.unwrap_or(0) == 12;
            let has_stride = config.parameter_context.as_ref()
                .and_then(|ctx| ctx.stride_elements)
                .map_or(false, |s| s > 0);

            match (is_lcg, has_stride) {
                (true, true)   => $lcg_str_fn(blocks, thread_id, error_mode, timing, config, progress),
                (true, false)  => $lcg_seq_fn(blocks, thread_id, error_mode, timing, config, progress),
                (false, true)  => $pos_str_fn(blocks, thread_id, error_mode, timing, config, progress),
                (false, false) => $pos_seq_fn(blocks, thread_id, error_mode, timing, config, progress),
            }
        }
    }
}

// Stamp out SIMD SimpleTest v2 implementations for each width.
// Each invocation generates 4 specialized #[target_feature] functions + 1 dispatch.
simple_test_v2_impl!(simple_test_v2_128_impl,
    simple_test_v2_128_pos_seq, simple_test_v2_128_pos_str,
    simple_test_v2_128_lcg_seq, simple_test_v2_128_lcg_str,
    "Mem-SimpleV2-128", u64x2, 2, [0, 1],
    pattern_gen::LcgSimd2, "sse4.2,sse4.1,ssse3,sse3,sse2,popcnt");
simple_test_v2_impl!(simple_test_v2_256_impl,
    simple_test_v2_256_pos_seq, simple_test_v2_256_pos_str,
    simple_test_v2_256_lcg_seq, simple_test_v2_256_lcg_str,
    "Mem-SimpleV2-256", u64x4, 4, [0, 1, 2, 3],
    pattern_gen::LcgSimd4, "avx2,avx,fma,bmi1,bmi2");
simple_test_v2_impl!(simple_test_v2_512_impl,
    simple_test_v2_512_pos_seq, simple_test_v2_512_pos_str,
    simple_test_v2_512_lcg_seq, simple_test_v2_512_lcg_str,
    "Mem-SimpleV2-512", u64x8, 8, [0, 1, 2, 3, 4, 5, 6, 7],
    pattern_gen::LcgSimd8, "avx512f,avx512bw,avx512cd,avx512dq,avx512vl,avx2,avx,fma,bmi1,bmi2");

/// SimpleTest v2 SSE2 (u64x2) — 128-bit SIMD.
///
/// # Safety
/// Caller must ensure all blocks contain valid, aligned, writable memory.
pub unsafe fn simple_test_v2_128_multi(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> TestStats {
    if is_x86_feature_detected!("sse2") {
        simple_test_v2_128_impl(blocks, thread_id, error_mode, timing, config, progress)
    } else {
        simple_test_v2_multi(blocks, thread_id, error_mode, timing, config, progress)
    }
}

/// SimpleTest v2 AVX2 (u64x4) — 256-bit SIMD.
///
/// # Safety
/// Caller must ensure all blocks contain valid, aligned, writable memory.
pub unsafe fn simple_test_v2_256_multi(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> TestStats {
    if is_x86_feature_detected!("avx2") {
        simple_test_v2_256_impl(blocks, thread_id, error_mode, timing, config, progress)
    } else {
        simple_test_v2_128_multi(blocks, thread_id, error_mode, timing, config, progress)
    }
}

/// SimpleTest v2 AVX-512 (u64x8) — 512-bit SIMD.
///
/// # Safety
/// Caller must ensure all blocks contain valid, aligned, writable memory.
pub unsafe fn simple_test_v2_512_multi(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> TestStats {
    if is_x86_feature_detected!("avx512f") {
        simple_test_v2_512_impl(blocks, thread_id, error_mode, timing, config, progress)
    } else {
        simple_test_v2_256_multi(blocks, thread_id, error_mode, timing, config, progress)
    }
}

// SimpleTest v2 auto-dispatch — selects best SIMD variant at runtime.
// SIMD variants handle both sequential and strided access (block-strided with interleave).
crate::auto_dispatch!(
    pub simple_test_v2_auto_multi,
    simple_test_v2_multi,
    simple_test_v2_128_multi,
    simple_test_v2_256_multi,
    simple_test_v2_512_multi
);
// =============================================================================
// SimpleTestNT — Non-Temporal SIMD Bandwidth Tests
// =============================================================================
//
// Uses streaming stores (_mm_stream_si128 / _mm256_stream_si256 / _mm512_stream_si512)
// which bypass the CPU cache, writing directly to DRAM. This eliminates the
// read-for-ownership (RFO) traffic that normal stores incur, roughly doubling
// write bandwidth. Verify uses regular loads (reads through cache as normal).
//
// Compare Mem-SimpleNT-* against Mem-SimpleV2-* to see the impact of NT stores
// vs regular (temporal) stores on the same positional pattern.
//
// WHY MANUAL UNROLL (not LLVM-driven):
//
// There is no way in current Rust/LLVM (as of LLVM 22.1, rustc 1.95-nightly) to
// get both correct non-temporal stores AND LLVM-driven loop optimization. We tested
// every available approach in an isolated benchmark (see ../nt-test/):
//
// 1. core::intrinsics::nontemporal_store — LLVM beautifully unrolls the loop but
//    STRIPS the !nontemporal metadata during optimization passes. All stores become
//    regular vmovdqa64/vmovaps. Tested on scalar u64, u64x8 constant, u64x8 with
//    compute pattern — NT metadata is lost in every case. LLVM bug #56703 fixed
//    ArgumentPromotionPass but other passes still strip it.
//
// 2. std::arch intrinsics (_mm512_stream_si512 etc.) — emit correct vmovntdq but
//    as inline asm (#APP/#NO_APP blocks). LLVM treats these as opaque and will NOT
//    unroll the loop automatically. Each iteration does only 1 store.
//
// 3. Manual asm!("vmovntdq ...") — same as #2, correct NT but no auto-unroll.
//
// 4. LLVM flag -C llvm-args=-unroll-count=4 — forces LLVM to unroll even with
//    inline asm, but produces suboptimal code (extra leaq address calculations
//    between each #APP/#NO_APP block). Also a global flag affecting ALL loops.
//
// SOLUTION: Manual 4x unroll using std::arch intrinsics. This gives us:
//   - Correct NT stores (vmovntdq confirmed in assembly output)
//   - 4 stores per loop iteration (amortizes loop overhead)
//   - Clean address offsets (ptr+0, ptr+w, ptr+2w, ptr+3w)
//
// UNROLL FACTOR: 4x was chosen after benchmarking 1x/2x/4x/8x/16x across all
// three widths (128/256/512). On a single thread, all factors hit the same ~24 GiB/s
// memory bandwidth ceiling. In multi-threaded TMR with interleaved compute+verify,
// 4x provides enough pipeline depth without excessive code bloat. Going higher
// (8x/16x) showed no benefit and risks instruction cache pressure.
//
// SIMD WIDTH PERFORMANCE: 128-bit ≈ 256-bit >> 512-bit for NT stores.
// 512-bit stores fill write-combine buffers faster (64B = 1 full cache line per
// store) creating back-pressure when the memory controller can't drain fast enough.
// This is a hardware characteristic, not fixable in software.
//
// WHY MACROS (not generic traits):
// #[target_feature] does not propagate through generic function call boundaries.
// A generic fn<T: SimdOps>() called from a #[target_feature(enable = "avx512f")]
// function does NOT inherit AVX-512. The inner function compiles at baseline ISA.
// Macros physically expand the code at the call site, guaranteeing correct codegen.
//
// =============================================================================

/// NT (non-temporal) SIMD Write for positional patterns.
/// Uses streaming stores to bypass cache, writing idx^base directly to DRAM.
/// Requires _mm_sfence() after completion to ensure stores are globally visible.
macro_rules! simple_write_nt_positional_simd {
    ($simd_type:ty, $simd_w:expr, $ctx:expr, $base_vec:expr, $lane_offsets:expr,
     $arch_type:ty, $stream_fn:path) => {{
        let w = $simd_w;
        let step1 = <$simd_type>::splat(w as u64);
        let step4 = <$simd_type>::splat((w * 4) as u64);
        let mut idx_vec = <$simd_type>::splat($ctx.chunk_start as u64) + $lane_offsets;
        let len = $ctx.chunk_end - $ctx.chunk_start;
        let unrolled_end = $ctx.chunk_start + (len / (w * 4)) * (w * 4);

        // Unrolled main loop: 4 NT stores per iteration
        let mut i = $ctx.chunk_start;
        while i < unrolled_end {
            let v0: $simd_type = idx_vec ^ $base_vec;
            let idx1 = idx_vec + step1;
            let v1: $simd_type = idx1 ^ $base_vec;
            let idx2 = idx1 + step1;
            let v2: $simd_type = idx2 ^ $base_vec;
            let idx3 = idx2 + step1;
            let v3: $simd_type = idx3 ^ $base_vec;
            $stream_fn($ctx.ptr.add(i) as *mut $arch_type, std::mem::transmute::<$simd_type, $arch_type>(v0));
            $stream_fn($ctx.ptr.add(i + w) as *mut $arch_type, std::mem::transmute::<$simd_type, $arch_type>(v1));
            $stream_fn($ctx.ptr.add(i + w * 2) as *mut $arch_type, std::mem::transmute::<$simd_type, $arch_type>(v2));
            $stream_fn($ctx.ptr.add(i + w * 3) as *mut $arch_type, std::mem::transmute::<$simd_type, $arch_type>(v3));
            idx_vec += step4;
            i += w * 4;
        }

        // Remainder: 1 NT store per iteration
        while i < $ctx.chunk_end {
            let val: $simd_type = idx_vec ^ $base_vec;
            $stream_fn($ctx.ptr.add(i) as *mut $arch_type, std::mem::transmute::<$simd_type, $arch_type>(val));
            idx_vec += step1;
            i += w;
        }
        std::arch::x86_64::_mm_sfence();
    }}
}

/// Stamps out a complete SimpleTestNT implementation for a given SIMD width.
/// Generates one #[target_feature] impl function + one public wrapper.
macro_rules! simple_test_nt_impl {
    (
        $impl_fn:ident,
        $pub_fn:ident,
        $test_name:literal,
        $simd_type:ty,
        $simd_w:expr,
        $lane_offsets:expr,
        $arch_type:ty,
        $stream_fn:path,
        $target_feature:literal
    ) => {
        #[target_feature(enable = $target_feature)]
        unsafe fn $impl_fn(
            blocks: &[crate::runner::AllocationBlock],
            thread_id: usize,
            error_mode: ErrorMode,
            timing: &TestTiming,
            config: &TestMemoryConfig,
            progress: Option<&TestProgress>,
        ) -> TestStats {
            let test_name = $test_name;
            let base = 0xDEADBEEFDEADBEEF_u64 ^ thread_id as u64;
            let base_vec = <$simd_type>::splat(base);
            let lane_offsets = <$simd_type>::from_array($lane_offsets);
            let zero = <$simd_type>::splat(0);

            run_phased_test(
                blocks, thread_id, error_mode, timing, config, progress,
                test_name, TestAction::WriteVerify, 1, false, 1, 1,
                // Init: NT write (fill memory with pattern, bypassing cache)
                |ctx: &ChunkCtx| {
                    simple_write_nt_positional_simd!($simd_type, $simd_w, ctx,
                        base_vec, lane_offsets, $arch_type, $stream_fn);
                },
                // Test: NT write (re-write same pattern via streaming stores)
                |ctx: &ChunkCtx| {
                    simple_write_nt_positional_simd!($simd_type, $simd_w, ctx,
                        base_vec, lane_offsets, $arch_type, $stream_fn);
                },
                // Verify: regular loads with XOR+OR accumulator
                |ctx: &ChunkCtx| -> u64 {
                    simple_verify_positional_simd!($simd_type, $simd_w, ctx,
                        base_vec, lane_offsets, zero)
                },
            )
        }

        #[doc = include_str!("test_fn_safety.md")]
        pub unsafe fn $pub_fn(
            blocks: &[crate::runner::AllocationBlock],
            thread_id: usize,
            error_mode: ErrorMode,
            timing: &TestTiming,
            config: &TestMemoryConfig,
            progress: Option<&TestProgress>,
        ) -> TestStats {
            $impl_fn(blocks, thread_id, error_mode, timing, config, progress)
        }
    }
}

// Stamp out NT implementations for each SIMD width.
simple_test_nt_impl!(
    simple_test_nt_128_impl, simple_test_nt_128_multi,
    "Mem-SimpleNT-128", u64x2, 2, [0, 1],
    std::arch::x86_64::__m128i, std::arch::x86_64::_mm_stream_si128,
    "sse4.2,sse4.1,ssse3,sse3,sse2,popcnt"
);

simple_test_nt_impl!(
    simple_test_nt_256_impl, simple_test_nt_256_multi,
    "Mem-SimpleNT-256", u64x4, 4, [0, 1, 2, 3],
    std::arch::x86_64::__m256i, std::arch::x86_64::_mm256_stream_si256,
    "avx2,avx,fma,bmi1,bmi2"
);

simple_test_nt_impl!(
    simple_test_nt_512_impl, simple_test_nt_512_multi,
    "Mem-SimpleNT-512", u64x8, 8, [0, 1, 2, 3, 4, 5, 6, 7],
    std::arch::x86_64::__m512i, std::arch::x86_64::_mm512_stream_si512,
    "avx512f,avx512bw,avx512cd,avx512dq,avx512vl,avx2,avx,fma,bmi1,bmi2"
);

// SimpleTestNT auto-dispatch — selects best SIMD variant at runtime.
crate::auto_dispatch!(
    pub simple_test_nt_auto_multi,
    simple_test_nt_128_multi,  // SSE2 fallback (baseline, always available)
    simple_test_nt_128_multi,
    simple_test_nt_256_multi,
    simple_test_nt_512_multi
);

// v1 stream2/stream4/streamN variants removed — replaced by v2 stride + SimpleTestNT.
// See git history for the original implementations.


// ============================================================================
// Bench-Init: Standalone pattern generation benchmarks (TODO #17)
// Measures raw INIT/write throughput per pattern mode with no-op test/verify.
// ============================================================================

/// Bench-Init dispatcher: routes to the correct pattern mode based on config.pattern_mode.
/// Each Bench-Init-* test sets pattern_mode in its config, then calls this.
#[doc = include_str!("test_fn_safety.md")]
pub unsafe fn bench_init_multi(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> TestStats {
    let mode = config.pattern_mode.unwrap_or(10);
    let param0 = config.pattern_param0.unwrap_or(0xDEADBEEFDEADBEEF);
    let param1 = config.pattern_param1.unwrap_or(0xCAFEBABECAFEBABE);
    let cl_shift = pattern_gen::cache_line_shift(config.cache_line_bytes);

    let skip_init = config.skip_init;

    let test_name = match mode {
        0 => "Bench-Init-TM5-0",
        1 => "Bench-Init-TM5-1",
        2 => "Bench-Init-TM5-2",
        10 => "Bench-Init-TMR-0",
        11 => "Bench-Init-TMR-1",
        12 => "Bench-Init-TMR-2",
        13 => "Bench-Init-TMR-3",
        _ => "Bench-Init-Unknown",
    };

    match mode {
        // --- TM5-faithful modes ---
        // NOTE: block_seed uses cycle=0 (not ctx.cycle) so that the pattern written
        // here is stable across cycles. Bench-Verify depends on matching the exact
        // same seed — if we varied by cycle, the last-written cycle would be unknown
        // to Bench-Verify, causing massive false-positive errors.
        0 => {
            // Mode 0: bit dispersion + branchless 4KB page complement
            run_phased_test(
                blocks, thread_id, error_mode, timing, config, progress,
                test_name, TestAction::Write, 1, skip_init, 1, 0,
                |ctx: &ChunkCtx| {
                    let seed = pattern_gen::block_seed(ctx.ptr as usize, ctx.thread_id, 0);
                    for idx in ctx.chunk_start..ctx.chunk_end {
                        *ctx.ptr.add(idx) = pattern_gen::pattern_mode0(idx as u64, seed);
                    }
                },
                |ctx: &ChunkCtx| {
                    let seed = pattern_gen::block_seed(ctx.ptr as usize, ctx.thread_id, 0);
                    for idx in ctx.chunk_start..ctx.chunk_end {
                        *ctx.ptr.add(idx) = pattern_gen::pattern_mode0(idx as u64, seed);
                    }
                },
                |_ctx: &ChunkCtx| -> u64 { 0 },
            )
        }
        1 => {
            // Mode 1: linear step + cache-line complement
            run_phased_test(
                blocks, thread_id, error_mode, timing, config, progress,
                test_name, TestAction::Write, 1, skip_init, 1, 0,
                |ctx: &ChunkCtx| {
                    let seed = pattern_gen::block_seed(ctx.ptr as usize, ctx.thread_id, 0);
                    for idx in ctx.chunk_start..ctx.chunk_end {
                        *ctx.ptr.add(idx) = pattern_gen::pattern_mode1(idx as u64, seed, cl_shift);
                    }
                },
                |ctx: &ChunkCtx| {
                    let seed = pattern_gen::block_seed(ctx.ptr as usize, ctx.thread_id, 0);
                    for idx in ctx.chunk_start..ctx.chunk_end {
                        *ctx.ptr.add(idx) = pattern_gen::pattern_mode1(idx as u64, seed, cl_shift);
                    }
                },
                |_ctx: &ChunkCtx| -> u64 { 0 },
            )
        }
        2 => {
            // Mode 2: per-page seed+step evolution (PMULLW-style)
            // Stateful: carry evolve state across chunks so init and test_fn produce
            // the same continuous chain. Re-seed at chunk_start==0 (new block).
            let cl_elements = config.cache_line_bytes / std::mem::size_of::<u64>();
            let mut test_page_seed: u64 = 0;
            let mut test_page_step: u64 = 0;
            run_phased_test(
                blocks, thread_id, error_mode, timing, config, progress,
                test_name, TestAction::Write, 1, skip_init, 1, 0,
                |ctx: &ChunkCtx| {
                    let base_seed = pattern_gen::block_seed(ctx.ptr as usize, ctx.thread_id, 0);
                    let mut page_seed = base_seed;
                    let mut page_step = base_seed.wrapping_mul(0x5DEECE66D);
                    let mut idx = ctx.chunk_start;
                    while idx < ctx.chunk_end {
                        let page_end = (idx + cl_elements).min(ctx.chunk_end);
                        for i in idx..page_end {
                            *ctx.ptr.add(i) = pattern_gen::mode2_element(page_seed, (i - idx) as u64, page_step);
                        }
                        let (ns, nst) = pattern_gen::mode2_evolve(page_seed, page_step, param0, param1);
                        page_seed = ns;
                        page_step = nst;
                        idx = page_end;
                    }
                },
                |ctx: &ChunkCtx| {
                    let (mut page_seed, mut page_step) = if ctx.chunk_start == 0 {
                        let base_seed = pattern_gen::block_seed(ctx.ptr as usize, ctx.thread_id, 0);
                        (base_seed, base_seed.wrapping_mul(0x5DEECE66D))
                    } else {
                        (test_page_seed, test_page_step)
                    };
                    let mut idx = ctx.chunk_start;
                    while idx < ctx.chunk_end {
                        let page_end = (idx + cl_elements).min(ctx.chunk_end);
                        for i in idx..page_end {
                            *ctx.ptr.add(i) = pattern_gen::mode2_element(page_seed, (i - idx) as u64, page_step);
                        }
                        let (ns, nst) = pattern_gen::mode2_evolve(page_seed, page_step, param0, param1);
                        page_seed = ns;
                        page_step = nst;
                        idx = page_end;
                    }
                    test_page_seed = page_seed;
                    test_page_step = page_step;
                },
                |_ctx: &ChunkCtx| -> u64 { 0 },
            )
        }
        // --- TMR-native modes ---
        10 => {
            // Mode 10: idx ^ base (address-derived unique)
            let base = 0xDEADBEEFDEADBEEF_u64;
            run_phased_test(
                blocks, thread_id, error_mode, timing, config, progress,
                test_name, TestAction::Write, 1, skip_init, 1, 0,
                |ctx: &ChunkCtx| {
                    for idx in ctx.chunk_start..ctx.chunk_end {
                        *ctx.ptr.add(idx) = pattern_gen::pattern_mode10(idx as u64, base);
                    }
                },
                |ctx: &ChunkCtx| {
                    for idx in ctx.chunk_start..ctx.chunk_end {
                        *ctx.ptr.add(idx) = pattern_gen::pattern_mode10(idx as u64, base);
                    }
                },
                |_ctx: &ChunkCtx| -> u64 { 0 },
            )
        }
        11 => {
            // Mode 11: idx ^ combined (param0 ^ param1)
            let combined = param0 ^ param1;
            run_phased_test(
                blocks, thread_id, error_mode, timing, config, progress,
                test_name, TestAction::Write, 1, skip_init, 1, 0,
                |ctx: &ChunkCtx| {
                    for idx in ctx.chunk_start..ctx.chunk_end {
                        *ctx.ptr.add(idx) = pattern_gen::pattern_mode11(idx as u64, combined);
                    }
                },
                |ctx: &ChunkCtx| {
                    for idx in ctx.chunk_start..ctx.chunk_end {
                        *ctx.ptr.add(idx) = pattern_gen::pattern_mode11(idx as u64, combined);
                    }
                },
                |_ctx: &ChunkCtx| -> u64 { 0 },
            )
        }
        13 => {
            // Mode 13: Positional pseudo-random hash (splitmix64).
            // High entropy like LCG but each element computed independently from (idx, seed).
            // No sequential dependency, no carry-state, chunk-agnostic.
            let seed = param0;
            run_phased_test(
                blocks, thread_id, error_mode, timing, config, progress,
                test_name, TestAction::Write, 1, skip_init, 1, 0,
                |ctx: &ChunkCtx| {
                    for idx in ctx.chunk_start..ctx.chunk_end {
                        *ctx.ptr.add(idx) = pattern_gen::pattern_mode13(idx as u64, seed);
                    }
                },
                |ctx: &ChunkCtx| {
                    for idx in ctx.chunk_start..ctx.chunk_end {
                        *ctx.ptr.add(idx) = pattern_gen::pattern_mode13(idx as u64, seed);
                    }
                },
                |_ctx: &ChunkCtx| -> u64 { 0 },
            )
        }
        _ => {
            // Mode 12: LCG chain (real PRNG)
            // LCG is sequential — carry state across chunks so init, test_fn, and
            // bench_verify's verify_fn all produce the same continuous chain.
            let multiplier = param0;
            let addend = param1;
            let initial_seed = (thread_id as u64).wrapping_mul(0x9E3779B97F4A7C15).wrapping_add(1);
            let mut test_lcg_state: u64 = 0;
            run_phased_test(
                blocks, thread_id, error_mode, timing, config, progress,
                test_name, TestAction::Write, 1, skip_init, 1, 0,
                |ctx: &ChunkCtx| {
                    let mut state = pattern_gen::lcg_next(
                        initial_seed, multiplier, addend);
                    for idx in ctx.chunk_start..ctx.chunk_end {
                        *ctx.ptr.add(idx) = state;
                        state = pattern_gen::lcg_next(state, multiplier, addend);
                    }
                },
                |ctx: &ChunkCtx| {
                    let mut state = if ctx.chunk_start == 0 {
                        pattern_gen::lcg_next(initial_seed, multiplier, addend)
                    } else {
                        test_lcg_state
                    };
                    for idx in ctx.chunk_start..ctx.chunk_end {
                        *ctx.ptr.add(idx) = state;
                        state = pattern_gen::lcg_next(state, multiplier, addend);
                    }
                    test_lcg_state = state;
                },
                |_ctx: &ChunkCtx| -> u64 { 0 },
            )
        }
    }
}

// ============================================================================
// Bench-Verify: Standalone pattern verification benchmarks
// Measures raw VERIFY/read throughput per pattern mode with no-op test.
// Can run independently (writes patterns first) or dependently (skip_init=true,
// relies on a prior Bench-Init-* having written the matching patterns).
// ============================================================================

/// Bench-Verify dispatcher: routes to the correct pattern mode based on config.pattern_mode.
/// In dependent mode (config.skip_init=true), skips the init write — assumes a matching
/// Bench-Init-* test already wrote the patterns. The init closure is still provided so
/// the harness has pattern knowledge for error repair if needed.
#[doc = include_str!("test_fn_safety.md")]
pub unsafe fn bench_verify_multi(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> TestStats {
    let mode = config.pattern_mode.unwrap_or(10);
    let param0 = config.pattern_param0.unwrap_or(0xDEADBEEFDEADBEEF);
    let param1 = config.pattern_param1.unwrap_or(0xCAFEBABECAFEBABE);
    let cl_shift = pattern_gen::cache_line_shift(config.cache_line_bytes);
    let skip_init = config.skip_init;

    let test_name = match mode {
        0 => "Bench-Verify-TM5-0",
        1 => "Bench-Verify-TM5-1",
        2 => "Bench-Verify-TM5-2",
        10 => "Bench-Verify-TMR-0",
        11 => "Bench-Verify-TMR-1",
        12 => "Bench-Verify-TMR-2",
        13 => "Bench-Verify-TMR-3",
        _ => "Bench-Verify-Unknown",
    };

    match mode {
        // --- TM5-faithful modes ---
        // NOTE: block_seed uses cycle=0 (not ctx.cycle) to match Bench-Init's stable seed.
        // Both init and verify must produce identical seeds regardless of cycle number.
        0 => {
            // Mode 0: bit dispersion + branchless 4KB page complement
            run_phased_test(
                blocks, thread_id, error_mode, timing, config, progress,
                test_name, TestAction::Verify, 1, skip_init, 0, 1,
                |ctx: &ChunkCtx| {
                    let seed = pattern_gen::block_seed(ctx.ptr as usize, ctx.thread_id, 0);
                    for idx in ctx.chunk_start..ctx.chunk_end {
                        *ctx.ptr.add(idx) = pattern_gen::pattern_mode0(idx as u64, seed);
                    }
                },
                |_ctx: &ChunkCtx| {},
                |ctx: &ChunkCtx| -> u64 {
                    let seed = pattern_gen::block_seed(ctx.ptr as usize, ctx.thread_id, 0);
                    let mut total_errors = 0u64;
                    for idx in ctx.chunk_start..ctx.chunk_end {
                        let expected = pattern_gen::pattern_mode0(idx as u64, seed);
                        let actual = *ctx.ptr.add(idx);
                        if actual != expected {
                            total_errors += 1;
                            if total_errors <= 10 {
                                log::error!("{}: error at idx {} - expected {:#x}, got {:#x}",
                                           test_name, idx, expected, actual);
                            }
                        }
                    }
                    total_errors
                },
            )
        }
        1 => {
            // Mode 1: linear step + cache-line complement
            run_phased_test(
                blocks, thread_id, error_mode, timing, config, progress,
                test_name, TestAction::Verify, 1, skip_init, 0, 1,
                |ctx: &ChunkCtx| {
                    let seed = pattern_gen::block_seed(ctx.ptr as usize, ctx.thread_id, 0);
                    for idx in ctx.chunk_start..ctx.chunk_end {
                        *ctx.ptr.add(idx) = pattern_gen::pattern_mode1(idx as u64, seed, cl_shift);
                    }
                },
                |_ctx: &ChunkCtx| {},
                |ctx: &ChunkCtx| -> u64 {
                    let seed = pattern_gen::block_seed(ctx.ptr as usize, ctx.thread_id, 0);
                    let mut total_errors = 0u64;
                    for idx in ctx.chunk_start..ctx.chunk_end {
                        let expected = pattern_gen::pattern_mode1(idx as u64, seed, cl_shift);
                        let actual = *ctx.ptr.add(idx);
                        if actual != expected {
                            total_errors += 1;
                            if total_errors <= 10 {
                                log::error!("{}: error at idx {} - expected {:#x}, got {:#x}",
                                           test_name, idx, expected, actual);
                            }
                        }
                    }
                    total_errors
                },
            )
        }
        2 => {
            // Mode 2: per-page seed+step evolution (PMULLW-style)
            // Stateful: page_seed/page_step evolve per cache line. Init writes the full
            // block as one continuous chain. Verify runs per-chunk, so we carry the evolve
            // state across chunk calls. Re-seed when chunk_start==0 (new block).
            let cl_elements = config.cache_line_bytes / std::mem::size_of::<u64>();
            let mut verify_page_seed: u64 = 0;
            let mut verify_page_step: u64 = 0;
            run_phased_test(
                blocks, thread_id, error_mode, timing, config, progress,
                test_name, TestAction::Verify, 1, skip_init, 0, 1,
                |ctx: &ChunkCtx| {
                    let base_seed = pattern_gen::block_seed(ctx.ptr as usize, ctx.thread_id, 0);
                    let mut page_seed = base_seed;
                    let mut page_step = base_seed.wrapping_mul(0x5DEECE66D);
                    let mut idx = ctx.chunk_start;
                    while idx < ctx.chunk_end {
                        let page_end = (idx + cl_elements).min(ctx.chunk_end);
                        for i in idx..page_end {
                            *ctx.ptr.add(i) = pattern_gen::mode2_element(page_seed, (i - idx) as u64, page_step);
                        }
                        let (ns, nst) = pattern_gen::mode2_evolve(page_seed, page_step, param0, param1);
                        page_seed = ns;
                        page_step = nst;
                        idx = page_end;
                    }
                },
                |_ctx: &ChunkCtx| {},
                |ctx: &ChunkCtx| -> u64 {
                    let (mut page_seed, mut page_step) = if ctx.chunk_start == 0 {
                        let base_seed = pattern_gen::block_seed(ctx.ptr as usize, ctx.thread_id, 0);
                        (base_seed, base_seed.wrapping_mul(0x5DEECE66D))
                    } else {
                        (verify_page_seed, verify_page_step)
                    };
                    let mut total_errors = 0u64;
                    let mut idx = ctx.chunk_start;
                    while idx < ctx.chunk_end {
                        let page_end = (idx + cl_elements).min(ctx.chunk_end);
                        for i in idx..page_end {
                            let expected = pattern_gen::mode2_element(page_seed, (i - idx) as u64, page_step);
                            let actual = *ctx.ptr.add(i);
                            if actual != expected {
                                total_errors += 1;
                                if total_errors <= 10 {
                                    log::error!("{}: error at idx {} - expected {:#x}, got {:#x}",
                                               test_name, i, expected, actual);
                                }
                            }
                        }
                        let (ns, nst) = pattern_gen::mode2_evolve(page_seed, page_step, param0, param1);
                        page_seed = ns;
                        page_step = nst;
                        idx = page_end;
                    }
                    verify_page_seed = page_seed;
                    verify_page_step = page_step;
                    total_errors
                },
            )
        }
        // --- TMR-native modes ---
        10 => {
            // Mode 10: idx ^ base (address-derived unique)
            let base = 0xDEADBEEFDEADBEEF_u64;
            run_phased_test(
                blocks, thread_id, error_mode, timing, config, progress,
                test_name, TestAction::Verify, 1, skip_init, 0, 1,
                |ctx: &ChunkCtx| {
                    for idx in ctx.chunk_start..ctx.chunk_end {
                        *ctx.ptr.add(idx) = pattern_gen::pattern_mode10(idx as u64, base);
                    }
                },
                |_ctx: &ChunkCtx| {},
                |ctx: &ChunkCtx| -> u64 {
                    let mut total_errors = 0u64;
                    for idx in ctx.chunk_start..ctx.chunk_end {
                        let expected = pattern_gen::pattern_mode10(idx as u64, base);
                        let actual = *ctx.ptr.add(idx);
                        if actual != expected {
                            total_errors += 1;
                            if total_errors <= 10 {
                                log::error!("{}: error at idx {} - expected {:#x}, got {:#x}",
                                           test_name, idx, expected, actual);
                            }
                        }
                    }
                    total_errors
                },
            )
        }
        11 => {
            // Mode 11: idx ^ combined (param0 ^ param1)
            let combined = param0 ^ param1;
            run_phased_test(
                blocks, thread_id, error_mode, timing, config, progress,
                test_name, TestAction::Verify, 1, skip_init, 0, 1,
                |ctx: &ChunkCtx| {
                    for idx in ctx.chunk_start..ctx.chunk_end {
                        *ctx.ptr.add(idx) = pattern_gen::pattern_mode11(idx as u64, combined);
                    }
                },
                |_ctx: &ChunkCtx| {},
                |ctx: &ChunkCtx| -> u64 {
                    let mut total_errors = 0u64;
                    for idx in ctx.chunk_start..ctx.chunk_end {
                        let expected = pattern_gen::pattern_mode11(idx as u64, combined);
                        let actual = *ctx.ptr.add(idx);
                        if actual != expected {
                            total_errors += 1;
                            if total_errors <= 10 {
                                log::error!("{}: error at idx {} - expected {:#x}, got {:#x}",
                                           test_name, idx, expected, actual);
                            }
                        }
                    }
                    total_errors
                },
            )
        }
        13 => {
            // Mode 13: Positional pseudo-random hash (splitmix64).
            // High entropy like LCG but each element computed independently from (idx, seed).
            // No sequential dependency, no carry-state, chunk-agnostic.
            let seed = param0;
            run_phased_test(
                blocks, thread_id, error_mode, timing, config, progress,
                test_name, TestAction::Verify, 1, skip_init, 0, 1,
                |ctx: &ChunkCtx| {
                    for idx in ctx.chunk_start..ctx.chunk_end {
                        *ctx.ptr.add(idx) = pattern_gen::pattern_mode13(idx as u64, seed);
                    }
                },
                |_ctx: &ChunkCtx| {},
                |ctx: &ChunkCtx| -> u64 {
                    let mut total_errors = 0u64;
                    for idx in ctx.chunk_start..ctx.chunk_end {
                        let expected = pattern_gen::pattern_mode13(idx as u64, seed);
                        let actual = *ctx.ptr.add(idx);
                        if actual != expected {
                            total_errors += 1;
                            if total_errors <= 10 {
                                log::error!("{}: error at idx {} - expected {:#x}, got {:#x}",
                                           test_name, idx, expected, actual);
                            }
                        }
                    }
                    total_errors
                },
            )
        }
        _ => {
            // Mode 12: LCG chain (real PRNG)
            // LCG is sequential — state[N] depends on the full chain from state[0].
            // Init writes the full block as one continuous chain. Verify runs per-chunk,
            // so we carry the LCG state across chunk calls via a captured mutable variable.
            // Re-seed when chunk_start==0 (new block), continue otherwise.
            let multiplier = param0;
            let addend = param1;
            let initial_seed = (thread_id as u64).wrapping_mul(0x9E3779B97F4A7C15).wrapping_add(1);
            let mut verify_lcg_state: u64 = 0;
            run_phased_test(
                blocks, thread_id, error_mode, timing, config, progress,
                test_name, TestAction::Verify, 1, skip_init, 0, 1,
                |ctx: &ChunkCtx| {
                    let mut state = pattern_gen::lcg_next(
                        initial_seed, multiplier, addend);
                    for idx in ctx.chunk_start..ctx.chunk_end {
                        *ctx.ptr.add(idx) = state;
                        state = pattern_gen::lcg_next(state, multiplier, addend);
                    }
                },
                |_ctx: &ChunkCtx| {},
                |ctx: &ChunkCtx| -> u64 {
                    let mut state = if ctx.chunk_start == 0 {
                        pattern_gen::lcg_next(initial_seed, multiplier, addend)
                    } else {
                        verify_lcg_state
                    };
                    let mut total_errors = 0u64;
                    for idx in ctx.chunk_start..ctx.chunk_end {
                        let actual = *ctx.ptr.add(idx);
                        if actual != state {
                            total_errors += 1;
                            if total_errors <= 10 {
                                log::error!("{}: error at idx {} - expected {:#x}, got {:#x}",
                                           test_name, idx, state, actual);
                            }
                        }
                        state = pattern_gen::lcg_next(state, multiplier, addend);
                    }
                    verify_lcg_state = state;
                    total_errors
                },
            )
        }
    }
}

// ============================================================================
// Test Registry - Maps config test names to actual function implementations
// ============================================================================

/// Map test name (from config or CLI) to its function implementation.
/// This is the single source of truth for test name → function mapping.
pub fn get_test_function_by_name(name: &str) -> Option<crate::runner::TestFunction> {
    use crate::runner::TestFunction;

    match name {
        // Stuck Bit Tests
        "Mem-StuckBit" => Some(TestFunction::MultiBlock(stuck_bit_test_multi)),
        "Mem-StuckBit128" => Some(TestFunction::MultiBlock(stuck_bit_test_128_multi)),
        "Mem-StuckBit256" => Some(TestFunction::MultiBlock(stuck_bit_test_256_multi)),
        "Mem-StuckBit512" => Some(TestFunction::MultiBlock(stuck_bit_test_512_multi)),
        "Mem-StuckBitAuto" => Some(TestFunction::MultiBlock(stuck_bit_test_auto_multi)),

        // Mirror Move Tests (v1)

        // Mirror Move Tests (v2 — u64 lanes)
        "Mem-MirrorV2" => Some(TestFunction::MultiBlock(mirror_move_v2_multi)),
        "Mem-MirrorV2-128" => Some(TestFunction::MultiBlock(mirror_move_v2_128_multi)),
        "Mem-MirrorV2-256" => Some(TestFunction::MultiBlock(mirror_move_v2_256_multi)),
        "Mem-MirrorV2-512" => Some(TestFunction::MultiBlock(mirror_move_v2_512_multi)),
        "Mem-MirrorV2-Auto" => Some(TestFunction::MultiBlock(mirror_move_v2_auto_multi)),

        // SimpleTest NT (non-temporal stores — manual 4x unroll, std::arch intrinsics)
        "Mem-SimpleNT-128" => Some(TestFunction::MultiBlock(simple_test_nt_128_multi)),
        "Mem-SimpleNT-256" => Some(TestFunction::MultiBlock(simple_test_nt_256_multi)),
        "Mem-SimpleNT-512" => Some(TestFunction::MultiBlock(simple_test_nt_512_multi)),
        "Mem-SimpleNT-Auto" => Some(TestFunction::MultiBlock(simple_test_nt_auto_multi)),

        // Simple Test (v2 — correct LCG + strided access)
        "Mem-SimpleV2" => Some(TestFunction::MultiBlock(simple_test_v2_multi)),
        "Mem-SimpleV2-128" => Some(TestFunction::MultiBlock(simple_test_v2_128_multi)),
        "Mem-SimpleV2-256" => Some(TestFunction::MultiBlock(simple_test_v2_256_multi)),
        "Mem-SimpleV2-512" => Some(TestFunction::MultiBlock(simple_test_v2_512_multi)),
        "Mem-SimpleV2-Auto" => Some(TestFunction::MultiBlock(simple_test_v2_auto_multi)),

        // Refresh Tests
        "Mem-Refresh" => Some(TestFunction::MultiBlock(refresh_stable_multi)),
        "Mem-Refresh128" => Some(TestFunction::MultiBlock(refresh_stable_128_multi)),
        "Mem-Refresh256" => Some(TestFunction::MultiBlock(refresh_stable_256_multi)),
        "Mem-Refresh512" => Some(TestFunction::MultiBlock(refresh_stable_512_multi)),
        "Mem-RefreshAuto" => Some(TestFunction::MultiBlock(refresh_stable_auto_multi)),

        // Performance & Stress Tests
        "Mem-CacheBust" => Some(TestFunction::MultiBlock(cache_busting_multi)),
        "Mem-Random" => Some(TestFunction::MultiBlock(random_torture_multi)),
        "Mem-Stride" => Some(TestFunction::MultiBlock(stride_access_multi)),
        "Mem-BlockMove" => Some(TestFunction::MultiBlock(block_move_multi)),

        // Sequential Bandwidth Tests (Spd-*-Auto → auto-dispatched)
        // Cached (L1/L2/L3) variants
        "Spd-L1-Read-Auto" | "Spd-L2-Read-Auto" | "Spd-L3-Read-Auto"
            => Some(TestFunction::MultiBlock(crate::bandwidth_tests::spd_read_auto_multi)),
        "Spd-L1-Write-Auto" | "Spd-L2-Write-Auto" | "Spd-L3-Write-Auto"
            => Some(TestFunction::MultiBlock(crate::bandwidth_tests::spd_write_auto_multi)),
        "Spd-L1-Copy-Auto" | "Spd-L2-Copy-Auto" | "Spd-L3-Copy-Auto"
            => Some(TestFunction::MultiBlock(crate::bandwidth_tests::spd_copy_auto_multi)),
        // DRAM variants (NT writes)
        "Spd-DRAMSmall-Read-Auto" | "Spd-DRAMFull-Read-Auto"
            => Some(TestFunction::MultiBlock(crate::bandwidth_tests::spd_read_auto_multi)),
        "Spd-DRAMSmall-Write-Auto" | "Spd-DRAMFull-Write-Auto"
            => Some(TestFunction::MultiBlock(crate::bandwidth_tests::spd_write_nt_auto_multi)),
        "Spd-DRAMSmall-Copy-Auto" | "Spd-DRAMFull-Copy-Auto"
            => Some(TestFunction::MultiBlock(crate::bandwidth_tests::spd_copy_nt_auto_multi)),

        // Bench-Init: Pattern generation throughput benchmarks
        "Bench-Init-TM5-0" | "Bench-Init-TM5-1" | "Bench-Init-TM5-2"
        | "Bench-Init-TMR-0" | "Bench-Init-TMR-1" | "Bench-Init-TMR-2"
        | "Bench-Init-TMR-3"
            => Some(TestFunction::MultiBlock(bench_init_multi)),

        // Bench-Verify: Pattern verification throughput benchmarks
        // Can run independently (writes patterns first) or dependently (skip_init in config)
        "Bench-Verify-TM5-0" | "Bench-Verify-TM5-1" | "Bench-Verify-TM5-2"
        | "Bench-Verify-TMR-0" | "Bench-Verify-TMR-1" | "Bench-Verify-TMR-2"
        | "Bench-Verify-TMR-3"
            => Some(TestFunction::MultiBlock(bench_verify_multi)),

        _ => None,
    }
}
