use crate::ErrorMode;
use crate::cache::{CacheInfo, SystemInfo};
use crate::memory::buffer::MemoryType;
use crate::constants::{MB, MB_16, MB_32, MB_64, KB, PAGE_SIZE_4KB};
use std::simd::*; // Docs here https://doc.rust-lang.org/std/simd/index.html
use std::simd::cmp::SimdPartialEq;
use std::sync::atomic::Ordering;
use std::sync::OnceLock;

const CACHE_BUSTING_STRIDE: usize = PAGE_SIZE_4KB;

// Test memory configuration enums (moved from layout.rs - these are test concerns, not allocation concerns)
#[derive(Debug, Clone)]
pub enum ExtentMode {
    /// Use the entire per-thread allocation (sweep all memory).
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

impl ExtentMode {
    /// Get target level name for logging (e.g., "L1", "L2", "L3", "DRAM", "DRAM-Full", or "Memory")
    pub fn target_level_name(&self) -> &'static str {
        match self {
            ExtentMode::Cache { target } => target.level_name(),
            ExtentMode::FullAllocation => "DRAM",
            ExtentMode::Absolute { .. } => "Memory",
            ExtentMode::CacheTotal { .. } => "Cache",
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

/// Cache level targeting for extent/chunk sizing.
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
    Dram { scale: f64 },
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
    pub const DRAM_DEFAULT: Self = Self::Dram { scale: 4.0 };
    /// DRAM-Full uses the entire thread allocation.
    pub const DRAM_FULL_DEFAULT: Self = Self::DRAMFull;

    /// Map CacheTarget to the corresponding calibration CacheTier
    fn to_calibration_tier(self) -> Option<crate::calibration::CacheTier> {
        match self {
            CacheTarget::L1 { .. } => Some(crate::calibration::CacheTier::L1),
            CacheTarget::L2 { .. } => Some(crate::calibration::CacheTier::L2),
            CacheTarget::L3 { .. } => Some(crate::calibration::CacheTier::L3),
            CacheTarget::Dram { .. } => Some(crate::calibration::CacheTier::Dram),
            CacheTarget::DRAMFull => None, // Always uses full allocation
        }
    }

    /// Calculate extent size using calibration data (measured tier boundaries).
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
        // Final 64-byte alignment is applied downstream in TestMemoryConfig::calculate_extent_size.
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
            CacheTarget::Dram { scale } => {
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

        log::debug!("Calibrated extent: {:?} → {} bytes (calibrated_optimal={}, threads={}, smt={})",
            tier, size, calibrated, thread_count, get_active_threads_per_core());

        Some(size)
    }

    /// Calculate actual extent size in bytes based on cache info and thread count.
    /// Uses calibration data when available, falls back to CPUID heuristics.
    pub fn size_bytes(&self, cache_info: &CacheInfo, thread_count: usize) -> usize {
        // Try calibrated sizing first
        if let Some(cal) = get_calibration_data()
            && let Some(size) = self.calculate_from_calibration(cal, cache_info, thread_count) {
                return size;
            }

        // CPUID-based fallback
        self.size_bytes_cpuid(cache_info, thread_count)
    }

    /// CPUID-based extent sizing (original heuristic path)
    fn size_bytes_cpuid(&self, cache_info: &CacheInfo, thread_count: usize) -> usize {
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
            CacheTarget::Dram { scale } => {
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
            CacheTarget::Dram { scale } => format!("DRAM{}", Self::format_scale(*scale)),
            CacheTarget::DRAMFull => "DRAM-Full".to_string(),
        }
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
            CacheTarget::Dram { scale } => format!("DRAM{}{}", Self::format_scale(*scale), suffix),
            CacheTarget::DRAMFull => "DRAM-Full".to_string(),
        }
    }

    /// Get the base level name (without scale)
    pub fn level_name(&self) -> &'static str {
        match self {
            CacheTarget::L1 { .. } => "L1",
            CacheTarget::L2 { .. } => "L2",
            CacheTarget::L3 { .. } => "L3",
            CacheTarget::Dram { .. } => "DRAM",
            CacheTarget::DRAMFull => "DRAM-Full",
        }
    }

    /// Parse from string like "L1", "L1/2", "L2*0.8", "L3/4", "DRAM", "DRAM*8", "DRAM/2", "DRAM-FULL".
    /// Both `/N` and `*N` operators are accepted on every tier; values may be decimal. No scale
    /// means the tier default; a scale that is there but invalid is `None`, never the default.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim().to_uppercase();

        // DRAM-FULL must come before DRAM (longer prefix first). It's scaleless.
        if s == "DRAM-FULL" || s == "DRAMFULL" || s == "RAM-FULL" || s == "RAMFULL" {
            return Some(CacheTarget::DRAMFull);
        }

        if let Some(rest) = s.strip_prefix("L1") {
            Some(CacheTarget::L1 { scale: Self::scale_or(rest, 0.5)? })
        } else if let Some(rest) = s.strip_prefix("L2") {
            Some(CacheTarget::L2 { scale: Self::scale_or(rest, 0.5)? })
        } else if let Some(rest) = s.strip_prefix("L3") {
            Some(CacheTarget::L3 { scale: Self::scale_or(rest, 0.5)? })
        } else if let Some(rest) = s.strip_prefix("DRAM") {
            Some(CacheTarget::Dram { scale: Self::scale_or(rest, 4.0)? })
        } else if s == "RAM" {
            // Alias for DRAM
            Some(CacheTarget::DRAM_DEFAULT)
        } else {
            None
        }
    }

    /// The scale in `rest`, or `default` when there is none. `None` when a scale is there but
    /// invalid: a bad operator, not a number, or out of range.
    fn scale_or(rest: &str, default: f64) -> Option<f64> {
        if rest.trim().is_empty() { Some(default) } else { Self::parse_scale(rest) }
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
    /// One chunk, the whole extent (TODO 76): the latency and bandwidth tests, which walk their
    /// extent as one working set. Resolved before the minimum, which those names don't have.
    Whole,
    /// Tier-aware sizing — same target syntax as `ExtentMode::Cache`.
    /// L3/N keeps writes warm through verify; DRAM*N forces eviction (refresh stress).
    Cache { target: CacheTarget },
    /// `(L1+L2+L3) × fraction`. NOT tier-aware. Coarse parity with `ExtentMode::CacheTotal`.
    CacheTotal { fraction: f64 },
    /// Hard-coded byte size. Replaces former FixedSize (MB).
    Absolute { size_bytes: usize },
    /// A TM5 `Test Block Size` code 0-3 (TODO 76): the smaller of the `.cfg`'s Testing Window
    /// Size and the extent, divided by `divisor` (code + 1), floored to `granularity` (the
    /// `.cfg`'s Lock Memory Granularity). Resolved against the extent, as TM5 resolves it against
    /// the memory it maps (`function.asm:497-506`, `MainThread.asm:624-661`).
    Tm5Block { window: usize, divisor: u32, granularity: usize },
}

/// TM5's block for a `Test Block Size` code over `base` bytes: `base / divisor` floored to
/// `granularity`, at least one granule (`MainThread.asm:627-661`). Code 0 (divisor 1) is `base`.
pub fn tm5_block_size(base: usize, divisor: u32, granularity: usize) -> usize {
    if divisor <= 1 {
        return base;
    }
    let granularity = granularity.max(1);
    (base / divisor as usize / granularity).max(1) * granularity
}

/// What a test's chunk spec asks for and the chunk it runs (TODO 76).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkResolution {
    /// The spec's size before the extent bounds it; a TM5 block code at the `.cfg`'s own window.
    pub requested: usize,
    /// The chunk the test runs: at least the test's minimum, at most the extent, a multiple of
    /// 4 KiB.
    pub resolved: usize,
}

impl ChunkResolution {
    /// The thread's memory made the chunk smaller than the spec asks for.
    pub fn shrunk_by_memory(&self) -> bool {
        self.resolved < self.requested.next_multiple_of(crate::test_memory::GRANULE)
    }
}

/// Controls error checking frequency within tests using power-of-2 intervals.
/// Only `PER_CHUNK` is constructed today, so the kernels' `check_mask` paths never run.
#[derive(Debug, Clone, Copy)]
pub struct ErrorCheckInterval {
    /// Power-of-2 shift for check interval (0 = every op, 9 = every 512 ops, etc.)
    /// Special value: u32::MAX = check only at chunk boundaries
    pub power_of_two_shift: u32,
}

impl ErrorCheckInterval {
    
    /// Check only at chunk boundaries (shift = MAX, effectively never within chunk)
    pub const PER_CHUNK: Self = Self { power_of_two_shift: u32::MAX };
    
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
    /// Bad words the seal check before a chunk found (TODO 74): data the previous step left
    /// sealed went bad, so they aren't this test's errors. TM5 numbers them 0.
    pub seal_errors: u64,
}

/// Optional progress reporting structure for tests
/// Allows tests to report progress back to the coordinator at checkpoints
///
/// One per worker, owned by the thread pool and read by the progress ticker. Each worker's slot is
/// on its own cache line so workers publishing never contend with one another.
#[derive(Debug)]
#[repr(align(64))]
pub struct TestProgress {
    pub cycles_completed: std::sync::atomic::AtomicU32,
    pub bytes_processed: std::sync::atomic::AtomicU64,
    pub errors_found: std::sync::atomic::AtomicU64,
    pub last_update_ms: std::sync::atomic::AtomicU64,  // ms since the test started; 0 = nothing published yet
    /// What the worker is doing (`Stage as u8`), for the ticker (TODO 74). Set between chunks.
    pub stage: std::sync::atomic::AtomicU8,
}

/// What a worker is doing, as the progress ticker names it (TODO 74).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Stage {
    /// The test itself; the ticker shows no stage.
    Testing = 0,
    SealingMemory = 1,
    CheckingSeal = 2,
    Resealing = 3,
    FinalSealCheck = 4,
    FillingMemory = 5,
    BuildingPointerChain = 6,
}

impl Stage {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Stage::SealingMemory,
            2 => Stage::CheckingSeal,
            3 => Stage::Resealing,
            4 => Stage::FinalSealCheck,
            5 => Stage::FillingMemory,
            6 => Stage::BuildingPointerChain,
            _ => Stage::Testing,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Stage::Testing => "",
            Stage::SealingMemory => "Sealing memory",
            Stage::CheckingSeal => "Checking seal",
            Stage::Resealing => "Resealing",
            Stage::FinalSealCheck => "Final seal check",
            Stage::FillingMemory => "Filling memory",
            Stage::BuildingPointerChain => "Building pointer chain",
        }
    }
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
            stage: std::sync::atomic::AtomicU8::new(Stage::Testing as u8),
        }
    }

    /// Name what the worker is doing now. A relaxed store, between chunks.
    pub fn set_stage(&self, stage: Stage) {
        self.stage.store(stage as u8, std::sync::atomic::Ordering::Relaxed);
    }

    /// Zero the slot for the next test. Only safe while its worker is idle between tests.
    pub fn reset(&self) {
        use std::sync::atomic::Ordering::Relaxed;
        self.cycles_completed.store(0, Relaxed);
        self.bytes_processed.store(0, Relaxed);
        self.errors_found.store(0, Relaxed);
        self.last_update_ms.store(0, Relaxed);
        self.stage.store(Stage::Testing as u8, Relaxed);
    }
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

// Three-stage memory configuration with corrected extent logic
#[derive(Debug, Clone)]
pub struct TestMemoryConfig {
    pub extent_mode: ExtentMode,
    pub chunk_mode: ChunkMode,
    pub timing: TestTiming,
    pub pattern_mode: Option<u32>,  // TM5 pattern mode
    pub pattern_param0: Option<u64>, // TM5 pattern parameter 0
    pub pattern_param1: Option<u64>, // TM5 pattern parameter 1
    pub memory_type: Option<MemoryType>,
    pub error_check_interval: ErrorCheckInterval,  // Controls error checking frequency
    pub tsc_frequency_ghz: f64,     // TSC frequency detected at startup (for latency tests)
    pub thread_count: usize,        // Total thread count for cache-aware extent calculations
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

    /// Registration/display name for **logging only** — never for lookups.
    ///
    /// Test fns bake in their own hardcoded `$test_name` (e.g. `"Mem-StuckBit128"`), which is the
    /// key used for extent/chunk sizing and metadata lookups and must not change. But several
    /// registrations share one fn: `Mem-StuckBit-Flush128` and the auto-dispatch `_A` variants
    /// both run `stuck_bit_test_128_impl`. Logging the baked-in name therefore mislabels those
    /// runs (`Mem-StuckBit-Flush128` was logging as `Mem-StuckBit128`).
    ///
    /// When set, `TestRunner::new` logs this instead. `None` falls back to the baked-in name, so
    /// tests constructed outside the registry are unaffected.
    pub display_name: Option<String>,
    /// The seal as this step runs it (TODO 74), set per step by the runner.
    pub seal: crate::seal::SealStep,
}

impl TestMemoryConfig {
    pub fn new(extent_mode: ExtentMode, chunk_mode: ChunkMode) -> Self {
        Self {
            extent_mode,
            chunk_mode,
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
            display_name: None,  // Logging only; falls back to the test fn's baked-in name
            seal: crate::seal::SealStep::default(),  // Unsealed; the runner sets it per step
        }
    }

    /// Builder: force the verify phase to read DRAM by flushing each chunk out of cache first
    /// (CLFLUSHOPT + MFENCE between write and verify). See `flush_before_verify`.
    pub fn with_flush_before_verify(mut self, flush: bool) -> Self {
        self.flush_before_verify = flush;
        self
    }

    /// Builder: set the name used in per-thread log lines. Logging only — sizing and metadata
    /// lookups still key off the test fn's baked-in name. See `display_name`.
    pub fn with_display_name(mut self, name: impl Into<String>) -> Self {
        self.display_name = Some(name.into());
        self
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

    pub fn with_pattern_config(mut self, mode: Option<u32>, param0: Option<u64>, param1: Option<u64>) -> Self {
        self.pattern_mode = mode;
        self.pattern_param0 = param0;
        self.pattern_param1 = param1;
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

    // Calculate extent size with corrected logic
    pub fn calculate_extent_size(&self, test_name: &str, allocated_size: usize) -> usize {
        let size = match &self.extent_mode {
            ExtentMode::FullAllocation => allocated_size,
            ExtentMode::Absolute { size_bytes } => {
                (*size_bytes).min(allocated_size)
            }
            ExtentMode::CacheTotal { fraction } => {
                let cache_info = get_cache_info();
                let cache_based_size = (cache_info.total_cache as f64 * fraction) as usize;
                cache_based_size.min(allocated_size)
            }
            ExtentMode::Cache { target } => {
                // Calculate extent size based on cache target using actual thread count
                let cache_info = get_cache_info();
                let calculated = target.size_bytes(cache_info, self.thread_count);
                // DRAMFull returns usize::MAX as sentinel to indicate "use full allocation"
                if calculated == usize::MAX {
                    allocated_size
                } else {
                    calculated.min(allocated_size)
                }
            }
        };

        // Round down to 64-byte boundary so SIMD operations (especially NT stores) don't fault.
        // This is the single source of alignment for all extent modes — calibrated values are
        // raw measurements and topology divisions can produce non-aligned results, both expected.
        let aligned = (size / 64) * 64;
        if aligned != size {
            log::debug!(
                "{}: extent size {} bytes not 64-byte aligned ({:?}) — rounded down to {} bytes",
                test_name, size, self.extent_mode, aligned
            );
        }

        aligned
    }
    
    /// The chunk for a test whose extent is `extent_size` bytes, resolved once per test (TODO 76):
    /// the configured size, at least the test's minimum, at most the extent, rounded up to
    /// `test_memory::GRANULE`. Any such multiple works: no kernel needs a power of two (TODO 76's
    /// audit). A chunk bigger than the extent is the extent, as TM5 clamps its block to its memory.
    pub fn calculate_chunk_size(&self, test_name: &str, extent_size: usize) -> usize {
        self.resolve_chunk(test_name, extent_size).resolved
    }

    /// `calculate_chunk_size`, with the size the spec asks for, so the plan can say when the
    /// thread's memory shrank it.
    pub fn resolve_chunk(&self, test_name: &str, extent_size: usize) -> ChunkResolution {
        if let ChunkMode::Whole = self.chunk_mode {
            let whole = extent_size.next_multiple_of(crate::test_memory::GRANULE).max(crate::test_memory::GRANULE);
            return ChunkResolution { requested: whole, resolved: whole };
        }
        let cache_info = get_cache_info();

        let requested = match &self.chunk_mode {
            ChunkMode::Tm5Block { window, divisor, granularity } => tm5_block_size(*window, *divisor, *granularity),
            _ => 0,
        };
        let raw_chunk_size = match &self.chunk_mode {
            ChunkMode::Whole => extent_size,
            ChunkMode::Absolute { size_bytes } => *size_bytes,
            ChunkMode::Tm5Block { window, divisor, granularity } => {
                tm5_block_size((*window).min(extent_size), *divisor, *granularity)
            }
            ChunkMode::CacheTotal { fraction } => (cache_info.total_cache as f64 * fraction) as usize,
            ChunkMode::Auto => {
                self.calculate_optimal_block_for_test(test_name, extent_size, cache_info)
            }
            ChunkMode::Cache { target } => {
                // Reuse the calibration-aware sizing path. DRAMFull returns usize::MAX
                // as a sentinel meaning "use the whole extent".
                let calculated = target.size_bytes(cache_info, self.thread_count);
                if calculated == usize::MAX { extent_size } else { calculated }
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
        let granule = crate::test_memory::GRANULE;

        // The minimum first, then the extent, which wins: a chunk never exceeds the extent.
        let sized = raw_chunk_size.max(minimum_chunk_size).min(extent_size);
        let final_chunk_size = sized.next_multiple_of(granule).max(granule);

        // Log corrections for user awareness
        if final_chunk_size != raw_chunk_size {
            let raw_mb = raw_chunk_size as f64 / MB as f64;
            let final_mb = final_chunk_size as f64 / MB as f64;

            if final_chunk_size > raw_chunk_size {
                log::debug!("🔧 Chunk size corrected for {}: {:.2}MB → {:.2}MB (minimum for {} variants, then a multiple of 4 KiB)",
                           test_name, raw_mb, final_mb, variant_count);
            } else {
                log::debug!("🔧 Chunk size capped for {}: {:.2}MB → {:.2}MB (limited by extent size)",
                           test_name, raw_mb, final_mb);
            }
        }

        ChunkResolution { requested: requested.max(raw_chunk_size), resolved: final_chunk_size }
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
            "Mem-Refresh" | "Mem-Refresh-Flush" => 8,   // Basic u64 operations
            "Mem-Refresh128" | "Mem-Refresh-Flush128" => 16,   // 128-bit SIMD operations
            "Mem-Refresh256" | "Mem-Refresh-Flush256" => 32,   // 256-bit SIMD operations
            "Mem-Refresh512" | "Mem-Refresh-Flush512" => 64,   // 512-bit SIMD operations
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
            // The seal kernels take 64 B-aligned ranges at every width (TODO 74)
            s if s.starts_with("Bench-Init-Seal-") || s.starts_with("Bench-Verify-Seal-") => 64,
            "Seal-Check" => 64,
            // Sequential bandwidth tests (all SIMD variants: 128/256/512/Auto)
            s if s.starts_with("Spd-") => 64,
            // Latency tests: one chunk, the extent, under the `whole` chunk mode they register
            // with; the minimum only matters for a config that gives them another one
            "ReadLatency" | "WriteLatency" | "CopyLatency" => 16,
            s if s.starts_with("Lat-") => 448,
            _ => panic!("Unknown test '{}' - add explicit SIMD requirement to calculate_minimum_chunk_size()", test_name),
        };
        
        // Variant division requirement: each variant needs at least simd_requirement bytes
        let variant_requirement = simd_requirement * variant_count.max(1) as usize;

        // Performance minimum: 64KB for reasonable cache behavior
        let performance_minimum = 64 * 1024; // 64KB

        // A multiple of the granule, like the chunk itself
        variant_requirement.max(performance_minimum).next_multiple_of(crate::test_memory::GRANULE)
    }
    
    fn calculate_optimal_block_for_test(&self, test_name: &str, extent_size: usize, cache_info: &CacheInfo) -> usize {
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
                align_to_boundary(extent_size / 16, cache_info.cache_line_size).max(MB)
            }
            
            // Cache tests use cache-line aligned blocks
            "CacheBusting" => {
                align_to_boundary(MB, cache_info.cache_line_size)
            }
            
            // Small blocks for refresh testing
            "RefreshStable" => {
                align_to_boundary(512 * KB, cache_info.cache_line_size)
            }
            
            _ => align_to_boundary(8 * MB, cache_info.cache_line_size),
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
/// CLFLUSHOPT is emitted unconditionally: the startup CPUID gate (`cli.rs` `require_cpu_features`)
/// guarantees the feature, so there is no `_mm_clflush` fallback. The function
/// carries `#[target_feature(enable = "clflushopt")]` (gated by the unstable
/// `clflushopt_target_feature`, landed in nightly via rustc PR #157098).
///
/// This flush-only loop uses the `_mm_clflushopt` intrinsic (stdarch PR #2141,
/// now synced into nightly behind `simd_x86_clflushopt`, tracking #157096),
/// NOT inline `asm!`. The intrinsic lowers to a real LLVM `clflushopt` op, so
/// LLVM can unroll and schedule the loop freely; the asm form is an opaque
/// `#APP` block LLVM cannot see through. The shipped binary unrolls it 8x plus a
/// remainder loop (checked 2026-10-03 in `tmr.exe`; the `--lib` asm is pre-LTO
/// and shows a plain loop). The asm idiom is reserved for a *mixed*
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
/// `base` must be aligned to `cache_line_bytes`: lines are counted from `base`, so an unaligned
/// `base` leaves the last line its range touches cached (TODO 79 B3). Chunk starts satisfy this.
/// Checked by `debug_assert!` only, to keep the release loop free of it.
#[inline]
#[target_feature(enable = "clflushopt")]
pub unsafe fn flush_range_to_dram(base: *const u8, len_bytes: usize, cache_line_bytes: usize) {
    let line = cache_line_bytes.max(1);
    debug_assert!((base as usize).is_multiple_of(line), "flush_range_to_dram: base {base:p} not aligned to {line} B");
    let line_count = len_bytes.div_ceil(line);
    for i in 0..line_count {
        let addr = unsafe { base.add(i * line) };
        unsafe { std::arch::x86_64::_mm_clflushopt(addr); }
    }
    // Mandatory: CLFLUSHOPT is weakly ordered. Without this fence a verify-load
    // could issue while flushes are still draining and read stale cached data.
    unsafe { std::arch::x86_64::_mm_mfence(); }
}

/// One contiguous piece of the extent a test runs on: `test_size` bytes from `ptr`.
/// The borrow ties it to the `AllocationBlock`s it lies in, which stay mapped for `'a`.
#[derive(Debug)]
pub struct TestBlock<'a> {
    pub ptr: *mut u8,
    /// How much to test, in bytes. Always derive loop bounds from this field.
    pub test_size: usize,
    _blocks: std::marker::PhantomData<&'a crate::runner::AllocationBlock>,
}

impl<'a> TestBlock<'a> {
    /// No memory: a thread without blocks.
    pub fn empty() -> Self {
        TestBlock { ptr: std::ptr::null_mut(), test_size: 0, _blocks: std::marker::PhantomData }
    }

    /// The extent at `ptr` inside `blocks` (it may span several of the span's blocks).
    pub fn in_blocks(blocks: &'a [crate::runner::AllocationBlock], ptr: *mut u8, test_size: usize) -> Self {
        debug_assert!({
            let (start, end) = (ptr as usize, ptr as usize + test_size);
            let lo = blocks.iter().map(|b| b.buffer.as_mut_ptr() as usize).min().unwrap_or(0);
            let hi = blocks.iter().map(|b| b.buffer.as_mut_ptr() as usize + b.buffer.size()).max().unwrap_or(0);
            lo <= start && end <= hi
        }, "piece outside its blocks");
        TestBlock { ptr, test_size, _blocks: std::marker::PhantomData }
    }
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
/// The scaffolding (`TestRunner`) owns extent/timer/progress/stats/shutdown; the hot loop is
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
            let (mut runner, extent) = crate::test_scaffolding::TestRunner::new(
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

                let base = extent.ptr as *mut $simd_type;

                // Chunks of exactly the test's size, spread evenly over the extent (TODO 76)
                let spread = runner.chunks(extent.test_size);
                let chunk_size_operations = spread.chunk() / lanes;

                for k in 0..spread.count() {
                    let processed = spread.start(k) / lanes;
                    let chunk_end = processed + chunk_size_operations;

                    // Phase 1: write P1, verify. Phase 2: write P2, verify.
                    // Phase 3: write P1 again, verify (catches transition-induced flips).
                    runner.check_seal(spread.start(k), spread.chunk());
                    stuck_bit_write_verify!($simd_type, base, processed, chunk_end, p1, &mut cycle_errors, test_name, thread_id, 1, flush_before_verify, line_bytes);
                    stuck_bit_write_verify!($simd_type, base, processed, chunk_end, p2, &mut cycle_errors, test_name, thread_id, 2, flush_before_verify, line_bytes);
                    stuck_bit_write_verify!($simd_type, base, processed, chunk_end, p1, &mut cycle_errors, test_name, thread_id, 3, flush_before_verify, line_bytes);
                    runner.reseal(spread.start(k), spread.chunk());

                    // Count the chunk when it is done; overlaps count each time
                    runner.add_bytes(spread.chunk() * 6);

                    if runner.should_halt(cycle_errors) {
                        let total_operations = (runner.bytes_processed() / lanes) as u64;
                        return runner.finish_aborted(cycle_errors, total_operations);
                    }

                    if runner.shutdown_requested() {
                        let total_operations = (runner.bytes_processed() / lanes) as u64;
                        return runner.finish_aborted(cycle_errors, total_operations);
                    }
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
/// Adds each bad word to `$errs`, counted in a cold rescan when an accumulator trips. Expanded
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
            // Count and log each bad word, as every width does (TODO 89)
            let words = std::mem::size_of::<$simd_type>() / std::mem::size_of::<u64>();
            let pat = $pat.as_array()[0];
            log::error!("{}: phase {} found a bad word (thread {})", crate::error_context::found_by($test_name), $phase, $thread_id);
            *$errs += rescan_words($base as *const u64, $start * words, $end * words, $test_name, |_| pat);
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
    let (mut runner, extent) = crate::test_scaffolding::TestRunner::new(
        blocks, thread_id, error_mode, timing, config, progress,
        test_name, TestAction::StuckBitTest,
    );

    // Hoist config flags into locals — never read a struct field inside the hot loop.
    let flush_before_verify = config.flush_before_verify;
    let line_bytes = config.cache_line_bytes;

    loop {
        runner.begin_cycle();
        let mut cycle_errors = 0u64;

        let base = extent.ptr as *mut u64;

        // Chunks of exactly the test's size, spread evenly over the extent (TODO 76)
        let spread = runner.chunks(extent.test_size);
        let chunk_size_operations = spread.chunk() / std::mem::size_of::<u64>();

        for k in 0..spread.count() {
            let processed = spread.start(k) / std::mem::size_of::<u64>();
            let chunk_end = processed + chunk_size_operations;

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

            runner.check_seal(spread.start(k), spread.chunk());

            // Phase 1: Write P1 (0xAA55...), verify
            for i in processed..chunk_end {
                *base.add(i) = pattern1;
            }

            std::sync::atomic::fence(Ordering::SeqCst);
            flush_chunk_if_enabled!();

            cycle_errors += verify_words(base, processed, chunk_end, test_name, |_| pattern1);

            // Phase 2: Write P2 (0x55AA..., = !P1), verify
            for i in processed..chunk_end {
                *base.add(i) = pattern2;
            }

            std::sync::atomic::fence(Ordering::SeqCst);
            flush_chunk_if_enabled!();

            cycle_errors += verify_words(base, processed, chunk_end, test_name, |_| pattern2);

            // Phase 3: Write back to P1, verify
            for i in processed..chunk_end {
                *base.add(i) = pattern1;
            }

            std::sync::atomic::fence(Ordering::SeqCst);
            flush_chunk_if_enabled!();

            cycle_errors += verify_words(base, processed, chunk_end, test_name, |_| pattern1);
            runner.reseal(spread.start(k), spread.chunk());

            // Count the chunk when it is done; overlaps count each time
            runner.add_bytes(spread.chunk() * 6);

            // Handle errors if found
            if runner.should_halt(cycle_errors) {
                let total_operations = (runner.bytes_processed() / std::mem::size_of::<u64>()) as u64;
                return runner.finish_aborted(cycle_errors, total_operations);
            }

            // Check for shutdown request
            if runner.shutdown_requested() {
                let total_operations = (runner.bytes_processed() / std::mem::size_of::<u64>()) as u64;
                return runner.finish_aborted(cycle_errors, total_operations);
            }
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
            seal_errors: 0,
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
            seal_errors: 0,
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
            seal_errors: 0,
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
/// mattered most in benchmarking. A tripped accumulator rescans the chunk to count and log each
/// bad word. `TestRunner` owns extent/timer/progress/stats/shutdown.
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
            let (mut runner, extent) = crate::test_scaffolding::TestRunner::new(
                blocks, thread_id, error_mode, timing, config, progress,
                test_name, TestAction::WriteWaitVerify,
            );

            let pattern = <$simd_type>::splat(REFRESH_PATTERN);
            // Hoisted out of the loops: never read a config field in the hot path.
            let flush_before_verify = config.flush_before_verify;

            loop {
                runner.begin_cycle();
                let mut cycle_errors = 0u64;

                let base = extent.ptr as *mut $simd_type;

                // Chunks of exactly the test's size, spread evenly over the extent (TODO 76)
                let spread = runner.chunks(extent.test_size);
                let chunk_size_vectors = spread.chunk() / lanes;

                for k in 0..spread.count() {
                    let processed = spread.start(k) / lanes;
                    let chunk_end = processed + chunk_size_vectors;

                    runner.check_seal(spread.start(k), spread.chunk());

                    // Write phase.
                    for i in processed..chunk_end {
                        *base.add(i) = pattern;
                    }

                    // Optional flush (TODO #59, opt-in): evict this chunk so the post-sleep
                    // verify reads DRAM rather than a cache-resident copy that would mask
                    // bit-fade. Default OFF — natural eviction already handles this here:
                    //   - the extent is `CacheTotal 2.0x`, i.e. 2× the whole hierarchy by
                    //     design, so writing it evicts its own earlier half;
                    //   - N threads run concurrently against a *shared* L3, cutting the
                    //     per-thread residency further (on a 482 MiB-cache box: ≤50% of the
                    //     extent could survive at 1 thread, but ≤6.4% at 8);
                    //   - the verify below sweeps *forward*, the same direction as the write,
                    //     so any surviving tail line is read last — after the verify's own
                    //     reads have pulled ~a whole extent through the cache. The residual is
                    //     both the smallest and the least-likely-resident part of the range.
                    // So this buys insurance against a fluke at ~20-30% throughput. For a
                    // tool where throughput *is* coverage-per-unit-time, faster cycles find
                    // more errors than a marginally stricter single pass. Turn it on to make
                    // the DRAM round-trip architecturally guaranteed instead of policy-
                    // dependent (shared virtualized L3, non-inclusive caches, prefetchers).
                    if flush_before_verify {
                        let chunk_ptr = base.add(processed) as *const u8;
                        let chunk_bytes = (chunk_end - processed) * lanes;
                        flush_range_to_dram(chunk_ptr, chunk_bytes, config.cache_line_bytes);
                    }

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
                        // Count and log each bad word, as every width does (TODO 89)
                        let words = lanes / std::mem::size_of::<u64>();
                        cycle_errors += rescan_words(base as *const u64, processed * words, chunk_end * words,
                                                     test_name, |_| REFRESH_PATTERN);
                    }
                    runner.reseal(spread.start(k), spread.chunk());

                    // Count the chunk when it is done; overlaps count each time
                    runner.add_bytes(spread.chunk() * 2);

                    if runner.should_halt(cycle_errors) {
                        let total_operations = (runner.bytes_processed() / lanes) as u64;
                        return runner.finish_aborted(cycle_errors, total_operations);
                    }

                    if runner.shutdown_requested() {
                        let total_operations = (runner.bytes_processed() / lanes) as u64;
                        return runner.finish_aborted(cycle_errors, total_operations);
                    }
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
    let (mut runner, extent) = crate::test_scaffolding::TestRunner::new(
        blocks, thread_id, error_mode, timing, config, progress,
        test_name, TestAction::WriteWaitVerify,
    );

    loop {
        runner.begin_cycle();
        let mut cycle_errors = 0u64;

        let base = extent.ptr as *mut u64;

        // Chunks of exactly the test's size, spread evenly over the extent (TODO 76)
        let spread = runner.chunks(extent.test_size);
        let chunk_size_operations = spread.chunk() / std::mem::size_of::<u64>();

        for k in 0..spread.count() {
            let processed = spread.start(k) / std::mem::size_of::<u64>();
            let chunk_end = processed + chunk_size_operations;

            runner.check_seal(spread.start(k), spread.chunk());

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
            cycle_errors += verify_words(base, processed, chunk_end, test_name, |_| pattern);
            runner.reseal(spread.start(k), spread.chunk());

            // Count the chunk when it is done; overlaps count each time
            runner.add_bytes(spread.chunk() * 2);

            // Handle errors if found
            if runner.should_halt(cycle_errors) {
                let total_operations = (runner.bytes_processed() / std::mem::size_of::<u64>()) as u64;
                return runner.finish_aborted(cycle_errors, total_operations);
            }

            // Check for shutdown request
            if runner.shutdown_requested() {
                let total_operations = (runner.bytes_processed() / std::mem::size_of::<u64>()) as u64;
                return runner.finish_aborted(cycle_errors, total_operations);
            }
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
            seal_errors: 0,
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
            seal_errors: 0,
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
            seal_errors: 0,
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

    let (mut runner, extent) = crate::test_scaffolding::TestRunner::new(
        blocks, thread_id, error_mode, timing, config, progress,
        test_name, TestAction::CacheBusting,
    );

    loop {
        runner.begin_cycle();
        let mut cycle_errors = 0u64;

        let base = extent.ptr as *mut u64;

        // Chunks of exactly the test's size, spread evenly over the extent (TODO 76)
        let spread = runner.chunks(extent.test_size);
        let chunk_size_operations = spread.chunk() / std::mem::size_of::<u64>();

        for k in 0..spread.count() {
            let processed = spread.start(k) / std::mem::size_of::<u64>();
            let chunk_end = processed + chunk_size_operations;

            runner.check_seal(spread.start(k), spread.chunk());

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

                    // Verify with same stride pattern within chunk; the walk covers every word
                    let expected = |i: usize| pattern_base.wrapping_add(i as u64);
                    let mut acc = [0u64; 4];
                    for offset in 0..base_stride.min(chunk_end - processed) {
                        accumulate_stride_4(&mut acc, base, processed + offset, chunk_end, base_stride, expected);
                    }
                    if (acc[0] | acc[1]) | (acc[2] | acc[3]) != 0 {
                        cycle_errors += rescan_words(base, processed, chunk_end, test_name, expected);
                    }
                }
                _ => {
                    // Multiple patterns - divide offsets among stride variants
                    // Each variant handles a subset of offsets, but ALL offsets are covered
                    for offset in 0..base_stride.min(chunk_end - processed) {
                        let variant = (((processed + offset) % base_stride) % stride_patterns) as u64;
                        let pattern = pattern_base.wrapping_add(variant * 0x1111111111111111u64);

                        let mut i = processed + offset;
                        while i < chunk_end {
                            *base.add(i) = pattern.wrapping_add(i as u64);
                            i += base_stride;
                            if i >= chunk_end { break; }
                        }
                    }

                    std::sync::atomic::fence(Ordering::SeqCst);

                    // Verify all offsets with their respective variant patterns; the walk covers
                    // every word, and a word's variant follows from its own index
                    let mut acc = [0u64; 4];
                    for offset in 0..base_stride.min(chunk_end - processed) {
                        let variant = (((processed + offset) % base_stride) % stride_patterns) as u64;
                        let pattern = pattern_base.wrapping_add(variant * 0x1111111111111111u64);
                        accumulate_stride_4(&mut acc, base, processed + offset, chunk_end, base_stride,
                                          |i| pattern.wrapping_add(i as u64));
                    }
                    if (acc[0] | acc[1]) | (acc[2] | acc[3]) != 0 {
                        let expected = |i: usize| {
                            let variant = ((i % base_stride) % stride_patterns) as u64;
                            pattern_base.wrapping_add(variant * 0x1111111111111111u64).wrapping_add(i as u64)
                        };
                        cycle_errors += rescan_words(base, processed, chunk_end, test_name, expected);
                    }
                }
            }
            runner.reseal(spread.start(k), spread.chunk());

            // Count the chunk when it is done; overlaps count each time
            runner.add_bytes(spread.chunk() * 2);

            // Handle errors if found
            if runner.should_halt(cycle_errors) {
                let total_operations = (runner.bytes_processed() / std::mem::size_of::<u64>()) as u64;
                return runner.finish_aborted(cycle_errors, total_operations);
            }

            // Check for shutdown request
            if runner.shutdown_requested() {
                let total_operations = (runner.bytes_processed() / std::mem::size_of::<u64>()) as u64;
                return runner.finish_aborted(cycle_errors, total_operations);
            }
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

/// Cold path of Mem-Random: rerun one chunk's reads from its saved RNG state, counting and logging
/// each mismatch. A fault that was gone by the reread still counts once.
#[cold]
#[inline(never)]
unsafe fn random_replay(base: *mut u64, len: usize, mut rng_state: u64, iterations: std::ops::Range<usize>,
                        seq: u32, test_name: &str) -> u64 {
    let mut errors = 0u64;
    for iteration in iterations {
        rng_state ^= rng_state << 13;
        rng_state ^= rng_state >> 17;
        rng_state ^= rng_state << 5;
        let idx = ((rng_state as u128 * len as u128) >> 64) as usize;
        let actual = unsafe { *base.add(idx) };
        if actual != idx as u64 {
            errors += 1;
            log::error!("{}, idx {} (iteration {}, rng_seq {}): expected {:#x}, got {:#x}",
                        crate::error_context::found_by(test_name), idx, iteration, seq, idx, actual);
        }
    }
    if errors == 0 {
        log::error!("{} in rng_seq {} that a reread no longer shows (transient)", crate::error_context::found_by(test_name), seq);
        errors = 1;
    }
    errors
}

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

    let (mut runner, extent) = crate::test_scaffolding::TestRunner::new(
        blocks, thread_id, error_mode, timing, config, progress,
        test_name, TestAction::RandomAccess,
    );
    // A thread with no memory has an empty extent, and the reads below need at least one word
    if extent.test_size == 0 {
        return runner.finish_completed(0);
    }
    let total_test_size = extent.test_size;

    // Initialize the extent with a known pattern once
    let base = extent.ptr as *mut u64;
    let len = extent.test_size / std::mem::size_of::<u64>();

    for i in 0..len {
        *base.add(i) = i as u64;
    }
    std::sync::atomic::fence(Ordering::SeqCst);
    runner.add_bytes(extent.test_size);

    loop {
        let cycle = runner.begin_cycle();
        let mut cycle_errors = 0u64;

        // Reads per batch, for responsive shutdown: the chunk's size in words
        let chunk_size_operations = runner.chunk_bytes() / std::mem::size_of::<u64>();

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

            // Random read verification for this sequence - chunked for responsive shutdown.
            // The index is `rng * len >> 64`: uniform over any length, one multiply off the
            // RNG's dependency chain (TODO 76; the old `& (len - 1)` needed a power of two).
            // Mismatches OR into one accumulator; a nonzero one replays the chunk to count
            // and log each bad word, so the loop has no branch and no logging.
            for chunk_start in (0..iterations_per_seq).step_by(chunk_size_operations) {
                let chunk_end = (chunk_start + chunk_size_operations).min(iterations_per_seq);
                let chunk_rng = rng_state;
                let mut acc = 0u64;

                for _ in chunk_start..chunk_end {
                    rng_state ^= rng_state << 13;
                    rng_state ^= rng_state >> 17;
                    rng_state ^= rng_state << 5;

                    let idx = ((rng_state as u128 * len as u128) >> 64) as usize;
                    acc |= *base.add(idx) ^ idx as u64;
                }

                if acc != 0 {
                    cycle_errors += random_replay(base, len, chunk_rng, chunk_start..chunk_end, seq, test_name);
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

    let (mut runner, extent) = crate::test_scaffolding::TestRunner::new(
        blocks, thread_id, error_mode, timing, config, progress,
        test_name, TestAction::ReadWrite,
    );
    let total_test_size = extent.test_size;

    loop {
        let cycle = runner.begin_cycle();
        let mut cycle_errors = 0u64;
        let strides = [1, 16, 64, 256, 1024, 4096];
        let pattern_base = 0xFEDCBA9876543210u64.wrapping_add(thread_id as u64).wrapping_add(cycle as u64);

        let base = extent.ptr as *mut u64;
        let len = extent.test_size / std::mem::size_of::<u64>();
        // Chunks of exactly the test's size, spread evenly over the extent (TODO 76)
        let spread = runner.chunks(extent.test_size);
        let chunk_len = spread.chunk() / std::mem::size_of::<u64>();
        let elements_per_subdiv = chunk_len >> subdiv_shift;

        // Each stride pass sweeps the extent, so its strided traffic reaches DRAM. In a sealed step
        // (TODO 74) the first pass checks each chunk's seal, the part no earlier chunk of the pass
        // has written (chunks may overlap), and the last pass reseals each chunk.
        let passes: Vec<usize> = strides.iter().copied().filter(|&stride| stride < len).collect();
        for (pass, &stride) in passes.iter().enumerate() {
            // Words written and read per chunk: each subdivision from its start, every stride
            let touched = subdivisions * elements_per_subdiv.div_ceil(stride);
            // A word's value depends on its index and the pass's stride, not on the chunk
            let pattern = pattern_base.wrapping_add((stride as u64) << 32);
            let mut checked_to = 0;

            for k in 0..spread.count() {
                let chunk_start = spread.start(k) / std::mem::size_of::<u64>();
                if pass == 0 {
                    let from = spread.start(k).max(checked_to);
                    runner.check_seal(from, spread.start(k) + spread.chunk() - from);
                    checked_to = spread.start(k) + spread.chunk();
                }

                for subdiv in 0..subdivisions {
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

                let expected = |pos: usize| pattern.wrapping_add(pos as u64);
                let mut acc = [0u64; 4];
                for subdiv in 0..subdivisions {
                    let subdiv_start = chunk_start + subdiv * elements_per_subdiv;
                    accumulate_stride_4(&mut acc, base, subdiv_start, subdiv_start + elements_per_subdiv, stride, expected);
                }
                if (acc[0] | acc[1]) | (acc[2] | acc[3]) != 0 {
                    let mut errors = 0;
                    for subdiv in 0..subdivisions {
                        let subdiv_start = chunk_start + subdiv * elements_per_subdiv;
                        errors += rescan_stride(base, subdiv_start, subdiv_start + elements_per_subdiv, stride, test_name, expected);
                    }
                    let chunk_end = chunk_start + subdivisions * elements_per_subdiv;
                    cycle_errors += transient_if_none(errors, test_name, chunk_start, chunk_end);
                }
                if pass + 1 == passes.len() {
                    runner.reseal(spread.start(k), spread.chunk());
                }

                // Count the chunk when it is done (one write and one read per word touched)
                runner.add_bytes(touched * std::mem::size_of::<u64>() * 2);

                // A halt stops the test, not just this stride pass (the finish reseals the extent)
                if runner.should_halt(cycle_errors) || runner.shutdown_requested() {
                    let total_operations = (runner.bytes_processed() / (2 * std::mem::size_of::<u64>())) as u64;
                    return runner.finish_aborted(cycle_errors, total_operations);
                }
            }
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

    let copy_dirs = config.parameter_context.as_ref()
        .and_then(|c| c.copy_directions)
        .expect("BlockMove requires copy_directions in parameter_context");

    let (mut runner, extent) = crate::test_scaffolding::TestRunner::new(
        blocks, thread_id, error_mode, timing, config, progress,
        test_name, TestAction::ReadWrite,
    );
    // A thread with no memory has an empty extent, which has no halves
    if extent.test_size == 0 {
        return runner.finish_completed(0);
    }
    let total_test_size = extent.test_size;

    // Initialize the source half
    let pattern_base = 0xDEADBEEFCAFEBABEu64;
    // Divide the extent in half: first half = source, second half = destination
    let half_size = extent.test_size / 2;
    let src_base = extent.ptr as *mut u64;
    let len = half_size / std::mem::size_of::<u64>();

    // Initialize source with pattern; a sealed step fills each source piece after its seal check
    // instead (TODO 74)
    let sealed = runner.seal_wrap();
    if !sealed {
        for i in 0..len {
            *src_base.add(i) = pattern_base.wrapping_add(i as u64);
        }
        std::sync::atomic::fence(Ordering::SeqCst);
    }

    loop {
        let cycle = runner.begin_cycle();
        let mut cycle_errors = 0u64;

        // Divide the extent: source (first half) → destination (second half)
        let half_size = extent.test_size / 2;
        let src_base = extent.ptr as *mut u64;
        let dst_base = src_base.add(half_size / std::mem::size_of::<u64>());

        // Half-chunks of exactly half the test's chunk, spread evenly over the source half
        // (TODO 76); each is copied to the same offset in the destination half
        let spread = runner.half_chunks(extent.test_size);
        let chunk_size_operations = spread.chunk() / std::mem::size_of::<u64>();

        for k in 0..spread.count() {
            let processed = spread.start(k) / std::mem::size_of::<u64>();
            let chunk_end = processed + chunk_size_operations;

            // The source piece and its destination, each checked, then the source filled
            runner.check_seal(spread.start(k), spread.chunk());
            runner.check_seal(half_size + spread.start(k), spread.chunk());
            if sealed {
                for i in processed..chunk_end {
                    *src_base.add(i) = pattern_base.wrapping_add(i as u64);
                }
            }

            // Copy from source to destination with direction patterns
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

            // Verify copied data (indices from the destination half's start)
            cycle_errors += verify_words(dst_base, processed, chunk_end, test_name, |i| pattern_base.wrapping_add(i as u64));
            runner.reseal(spread.start(k), spread.chunk());
            runner.reseal(half_size + spread.start(k), spread.chunk());

            // Count the half-chunk when it is done: copy (read + write) and verify (read)
            runner.add_bytes(spread.chunk() * 3);

            if runner.should_halt(cycle_errors) || runner.shutdown_requested() {
                let total_operations = (runner.bytes_processed() / (3 * std::mem::size_of::<u64>())) as u64;
                return runner.finish_aborted(cycle_errors, total_operations);
            }
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
// - Mode 2 is a real evolving chain (not a static seed), restarted per 4 KiB page
// - Parameter field correctly interpreted (stride, subblocks, page stride)
// - All tests use u64 lanes (v1 MirrorMove i32 removed)
// - Strided access support for SimpleTest
// - Configurable test_reps, verify_reps, write_read_cycles

use crate::pattern_gen;
use crate::pattern_gen::{LINE_WORDS, PAGE_WORDS};
use crate::test_harness::{run_phased_test, ChunkCtx};
use crate::config::MirrorMode;

// ─── Line patterns: modes 2 and 12 (TODO 76) ─────────────────────────────────
//
// Both write each 64 B line as `seed + j * step`. Mode 2 takes seed and step from TM5's chain,
// restarted at every 4 KiB page; mode 12 hashes them from the line's address. Either way a
// word depends only on its address, so chunks may overlap and start on any page. One writer
// and one verifier per mode serve SimpleV2 (seeded with the cycle) and Bench-Init/Verify
// (cycle 0, so a later Bench-Verify can check what Bench-Init wrote). The verifiers OR the
// differences into 4 accumulators and rescan, counting and logging, only when one is set.

/// Mode 2 for one thread and cycle.
#[derive(Clone, Copy)]
struct Mode2 {
    thread_id: usize,
    cycle: u32,
    param0: u64,
    param1: u64,
}

impl Mode2 {
    /// Calls `line(word_offset, expected)` for each line of words `[start, end)` of `ptr`,
    /// which are whole 4 KiB pages.
    #[inline(always)]
    unsafe fn walk(self, ptr: *const u64, start: usize, end: usize, mut line: impl FnMut(usize, [u64; LINE_WORDS])) {
        debug_assert!(start.is_multiple_of(PAGE_WORDS) && end.is_multiple_of(PAGE_WORDS), "mode 2 needs whole pages: {start}..{end}");
        let mut page = start;
        while page < end {
            let (mut seed, mut step) = pattern_gen::mode2_page_start(ptr.add(page) as usize, self.thread_id, self.cycle);
            for offset in (page..page + PAGE_WORDS).step_by(LINE_WORDS) {
                line(offset, pattern_gen::line_words(seed, step));
                (seed, step) = pattern_gen::mode2_evolve(seed, step, self.param0, self.param1);
            }
            page += PAGE_WORDS;
        }
    }

    #[inline(always)]
    unsafe fn fill(self, ptr: *mut u64, start: usize, end: usize) {
        self.walk(ptr, start, end, |offset, words| *(ptr.add(offset) as *mut [u64; LINE_WORDS]) = words);
    }

    /// Errors in words `[start, end)`: 0 if all hold, else counted and logged by a rescan.
    #[inline(always)]
    unsafe fn verify(self, ptr: *const u64, start: usize, end: usize, test_name: &str) -> u64 {
        let mut acc = [0u64; 4];
        self.walk(ptr, start, end, |offset, words| accumulate_line(&mut acc, ptr.add(offset), &words));
        if (acc[0] | acc[1]) | (acc[2] | acc[3]) == 0 {
            return 0;
        }
        self.rescan(ptr, start, end, test_name)
    }

    #[cold]
    #[inline(never)]
    unsafe fn rescan(self, ptr: *const u64, start: usize, end: usize, test_name: &str) -> u64 {
        let mut errors = 0u64;
        self.walk(ptr, start, end, |offset, words| count_line(&mut errors, ptr, offset, &words, test_name));
        transient_if_none(errors, test_name, start, end)
    }
}

/// Mode 12 for one thread and cycle.
#[derive(Clone, Copy)]
struct Mode12 {
    key: u64,
}

impl Mode12 {
    fn new(thread_id: usize, cycle: u32, param0: u64, param1: u64) -> Self {
        Self { key: pattern_gen::mode12_key(thread_id, cycle, param0, param1) }
    }

    /// Calls `line(word_offset, expected)` for each line of words `[start, end)` of `ptr`.
    #[inline(always)]
    unsafe fn walk(self, ptr: *const u64, start: usize, end: usize, mut line: impl FnMut(usize, [u64; LINE_WORDS])) {
        debug_assert!(start.is_multiple_of(LINE_WORDS) && end.is_multiple_of(LINE_WORDS), "mode 12 needs whole lines: {start}..{end}");
        for offset in (start..end).step_by(LINE_WORDS) {
            let (seed, step) = pattern_gen::mode12_line(ptr.add(offset) as usize, self.key);
            line(offset, pattern_gen::line_words(seed, step));
        }
    }

    #[inline(always)]
    unsafe fn fill(self, ptr: *mut u64, start: usize, end: usize) {
        self.walk(ptr, start, end, |offset, words| *(ptr.add(offset) as *mut [u64; LINE_WORDS]) = words);
    }

    /// Errors in words `[start, end)`: 0 if all hold, else counted and logged by a rescan.
    #[inline(always)]
    unsafe fn verify(self, ptr: *const u64, start: usize, end: usize, test_name: &str) -> u64 {
        let mut acc = [0u64; 4];
        self.walk(ptr, start, end, |offset, words| accumulate_line(&mut acc, ptr.add(offset), &words));
        if (acc[0] | acc[1]) | (acc[2] | acc[3]) == 0 {
            return 0;
        }
        self.rescan(ptr, start, end, test_name)
    }

    #[cold]
    #[inline(never)]
    unsafe fn rescan(self, ptr: *const u64, start: usize, end: usize, test_name: &str) -> u64 {
        let mut errors = 0u64;
        self.walk(ptr, start, end, |offset, words| count_line(&mut errors, ptr, offset, &words, test_name));
        transient_if_none(errors, test_name, start, end)
    }
}

/// ORs one line's differences into 4 accumulators: words k and k + 4 into accumulator k, so a
/// vectorised build reads each half-line as one 32 B lane group, with no shuffles.
#[inline(always)]
unsafe fn accumulate_line(acc: &mut [u64; 4], line: *const u64, words: &[u64; LINE_WORDS]) {
    let actual = *(line as *const [u64; LINE_WORDS]);
    for k in 0..4 {
        acc[k] |= (actual[k] ^ words[k]) | (actual[k + 4] ^ words[k + 4]);
    }
}

/// Counts one line's bad words, logging the first 10 of the scan.
#[inline(always)]
unsafe fn count_line(errors: &mut u64, ptr: *const u64, offset: usize, words: &[u64; LINE_WORDS], test_name: &str) {
    for (j, &expected) in words.iter().enumerate() {
        let actual = *ptr.add(offset + j);
        if actual != expected {
            *errors += 1;
            if *errors <= 10 {
                log::error!("{}, idx {}: expected {:#x}, got {:#x}", crate::error_context::found_by(test_name), offset + j, expected, actual);
            }
        }
    }
}

/// Verify for the per-word patterns (modes 0, 1, 10, 11, 13): XOR each word with
/// `expected(idx)` and OR into 4 accumulators, then count and log in a cold rescan only if one is
/// set. A per-word branch and `log::error!` kept the log's arguments spilled to the stack on every
/// word, and cost Bench-Verify-TM5-0 30% when the harness around it changed (TODO 76).
#[inline(always)]
unsafe fn verify_words(ptr: *const u64, start: usize, end: usize, test_name: &str, expected: impl Fn(usize) -> u64 + Copy) -> u64 {
    let mut acc = [0u64; 4];
    let mut idx = start;
    while idx + 4 <= end {
        for (k, a) in acc.iter_mut().enumerate() {
            *a |= *ptr.add(idx + k) ^ expected(idx + k);
        }
        idx += 4;
    }
    while idx < end {
        acc[0] |= *ptr.add(idx) ^ expected(idx);
        idx += 1;
    }
    if (acc[0] | acc[1]) | (acc[2] | acc[3]) == 0 {
        return 0;
    }
    rescan_words(ptr, start, end, test_name, expected)
}

/// ORs the differences of words `start, start + stride, ...` below `end` into the accumulators
/// (TODO 89): the strided twin of `verify_words`'s loop. The caller checks them once and rescans
/// on a hit.
///
/// One load per step, not 4 accumulators 4 strides apart (`accumulate_stride_4`): the strided
/// SimpleTest's walk over a big chunk is memory-bound, not bound by the OR chain, and at strides
/// of about 12-20 KB the 4-way walk is far slower. On 1usmus_v3 (2026-10-06, 4 threads) it took
/// Test 6 (a 15.9 KB stride over 432 MiB chunks) from 44 s to 78 s, worse than the per-word branch
/// it replaced (55 s). A sweep over 432 MiB chunks (2026-10-07, 4-way time / 1-way time): 2-8 KB
/// 0.97-1.03, 12.7 KB 1.58, 15.9 KB 1.82, 20 KB 1.34, 25.5 KB 0.95, 32-128 KB 0.90-0.94; over
/// 32 MiB chunks 15.9 KB was 0.97. So the slow band is where one sweep's lines (chunk / stride,
/// 1.4-2.2 MB here) are about the L2's 2 MiB; the prefetcher's part is unmeasured. A stopgap until
/// the strided SimpleTest moves whole lines per jump as TM5 does (TODO 93).
#[inline(always)]
unsafe fn accumulate_stride(acc: &mut [u64; 4], ptr: *const u64, start: usize, end: usize, stride: usize,
                            expected: impl Fn(usize) -> u64 + Copy) {
    let mut idx = start;
    while idx < end {
        acc[0] |= *ptr.add(idx) ^ expected(idx);
        idx += stride;
    }
}

/// `accumulate_stride` 4 strides per step into the 4 accumulators: for a walk over a chunk that
/// stays in cache (Mem-CacheBust's 1 MiB, Mem-Stride's 2 MiB), where the loop is the limit, and for
/// the strided SimpleTest at 24 KiB strides and up (`STRIDE_4_FROM`). Measured on the dev box
/// (2026-10-06): CacheBust 12% and Mem-Stride 5% faster than one load per step; 1usmus_v3's 32-100
/// KB strides 13-30%.
#[inline(always)]
unsafe fn accumulate_stride_4(acc: &mut [u64; 4], ptr: *const u64, start: usize, end: usize, stride: usize,
                              expected: impl Fn(usize) -> u64 + Copy) {
    let mut idx = start;
    while idx + 3 * stride < end {
        for (k, a) in acc.iter_mut().enumerate() {
            let at = idx + k * stride;
            *a |= *ptr.add(at) ^ expected(at);
        }
        idx += 4 * stride;
    }
    while idx < end {
        acc[0] |= *ptr.add(idx) ^ expected(idx);
        idx += stride;
    }
}

/// The strided SimpleTest's stride, in words, from which it walks 4 strides per step (24 KiB, above
/// the slow band): see `accumulate_stride`.
const STRIDE_4_FROM: usize = 3072;

/// Counts and logs the words `start, start + stride, ...` below `end` that differ from
/// `expected(idx)`, the cold path of `accumulate_stride` (the caller adds `transient_if_none`).
#[cold]
#[inline(never)]
unsafe fn rescan_stride(ptr: *const u64, start: usize, end: usize, stride: usize, test_name: &str,
                        expected: impl Fn(usize) -> u64) -> u64 {
    let mut errors = 0u64;
    let mut idx = start;
    while idx < end {
        let (want, actual) = (expected(idx), *ptr.add(idx));
        if actual != want {
            errors += 1;
            if errors <= 10 {
                log::error!("{}, idx {} (stride {}): expected {:#x}, got {:#x}", crate::error_context::found_by(test_name), idx, stride, want, actual);
            }
        }
        idx += stride;
    }
    errors
}

/// Counts and logs the words of `[start, end)` that differ from `expected(idx)`, once an
/// accumulator has seen one.
#[cold]
#[inline(never)]
unsafe fn rescan_words(ptr: *const u64, start: usize, end: usize, test_name: &str, expected: impl Fn(usize) -> u64) -> u64 {
    let mut errors = 0u64;
    for idx in start..end {
        let (want, actual) = (expected(idx), *ptr.add(idx));
        if actual != want {
            errors += 1;
            if errors <= 10 {
                log::error!("{}, idx {}: expected {:#x}, got {:#x}", crate::error_context::found_by(test_name), idx, want, actual);
            }
        }
    }
    transient_if_none(errors, test_name, start, end)
}

/// A rescan that finds nothing still reports the error the accumulator saw.
fn transient_if_none(errors: u64, test_name: &str, start: usize, end: usize) -> u64 {
    if errors > 0 {
        return errors;
    }
    log::error!("{} in idx {}..{} that a reread no longer shows (transient)", crate::error_context::found_by(test_name), start, end);
    1
}

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
                    verify_words(ctx.ptr, ctx.chunk_start, ctx.chunk_end, test_name, |idx| pattern_gen::pattern_mode0(idx as u64, seed))
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
                    verify_words(ctx.ptr, ctx.chunk_start, ctx.chunk_end, test_name, |idx| pattern_gen::pattern_mode1(idx as u64, seed, cl_shift))
                },
            )
        }
        2 => {
            // Mode 2 (TM5-faithful): TM5's evolving lines, the chain restarted per 4 KiB page
            let mode2 = |ctx: &ChunkCtx| Mode2 { thread_id: ctx.thread_id, cycle: ctx.cycle, param0, param1 };
            run_phased_test(
                blocks, thread_id, error_mode, timing, config, progress,
                test_name, TestAction::WriteVerify, 1, false, config.test_reps, config.verify_reps,
                |ctx: &ChunkCtx| mode2(ctx).fill(ctx.ptr, ctx.chunk_start, ctx.chunk_end),
                |ctx: &ChunkCtx| mode2(ctx).fill(ctx.ptr, ctx.chunk_start, ctx.chunk_end),
                |ctx: &ChunkCtx| -> u64 { mode2(ctx).verify(ctx.ptr, ctx.chunk_start, ctx.chunk_end, test_name) },
            )
        }
        12 => {
            // Mode 12 (TMR-native): mode 2's lines, seed and step hashed from each line's address
            let mode12 = |ctx: &ChunkCtx| Mode12::new(ctx.thread_id, ctx.cycle, param0, param1);
            run_phased_test(
                blocks, thread_id, error_mode, timing, config, progress,
                test_name, TestAction::WriteVerify, 1, false, config.test_reps, config.verify_reps,
                |ctx: &ChunkCtx| mode12(ctx).fill(ctx.ptr, ctx.chunk_start, ctx.chunk_end),
                |ctx: &ChunkCtx| mode12(ctx).fill(ctx.ptr, ctx.chunk_start, ctx.chunk_end),
                |ctx: &ChunkCtx| -> u64 { mode12(ctx).verify(ctx.ptr, ctx.chunk_start, ctx.chunk_end, test_name) },
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
                    verify_words(ctx.ptr, ctx.chunk_start, ctx.chunk_end, test_name, |idx| pattern_gen::pattern_mode11(idx as u64, combined))
                },
            )
        }
        13 => {
            // Mode 13 (TMR-native): a hash per word
            let seed = param0;
            run_phased_test(
                blocks, thread_id, error_mode, timing, config, progress,
                test_name, TestAction::WriteVerify, 1, false, config.test_reps, config.verify_reps,
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
                |ctx: &ChunkCtx| -> u64 {
                    verify_words(ctx.ptr, ctx.chunk_start, ctx.chunk_end, test_name, |idx| pattern_gen::pattern_mode13(idx as u64, seed))
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
                    verify_words(ctx.ptr, ctx.chunk_start, ctx.chunk_end, test_name, |idx| pattern_gen::pattern_mode10(idx as u64, base))
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
    // For strided access, the line patterns (2, 12) fall back to positional mode 10: they are
    // computed a line at a time, and a stride visits a word at a time. The per-word modes work.
    let effective_mode = match pattern_mode {
        0 | 1 | 10 | 11 | 13 => pattern_mode,
        _ => 10,
    };
    let base = match effective_mode {
        11 => param0 ^ param1,
        _ => param0,
    };
    let cl_shift = pattern_gen::cache_line_shift(config.cache_line_bytes);

    // One copy of the loops per pattern, so each inlines its pattern function. A `fn` pointer picked
    // at run time was called once per element (`callq *%reg` in the release asm, 2026-10-03).
    macro_rules! run_strided {
        ($gen:expr) => {{
            let gen_pattern = $gen;
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
                // Verify: same strided order, into 4 accumulators; the walk covers every word of the
                // chunk, so a hit rescans it in order (TODO 89)
                |ctx: &ChunkCtx| -> u64 {
                    let seed = if effective_mode <= 1 {
                        pattern_gen::block_seed(ctx.ptr as usize, ctx.thread_id, ctx.cycle)
                    } else { base };
                    let len = ctx.chunk_end - ctx.chunk_start;
                    if len == 0 { return 0; }
                    let expected = |idx: usize| gen_pattern(idx as u64, seed, cl_shift);
                    let mut acc = [0u64; 4];
                    for sub_offset in 0..stride.min(len) {
                        let from = ctx.chunk_start + sub_offset;
                        if stride >= STRIDE_4_FROM {
                            accumulate_stride_4(&mut acc, ctx.ptr, from, ctx.chunk_end, stride, expected);
                        } else {
                            accumulate_stride(&mut acc, ctx.ptr, from, ctx.chunk_end, stride, expected);
                        }
                    }
                    if (acc[0] | acc[1]) | (acc[2] | acc[3]) == 0 {
                        return 0;
                    }
                    rescan_words(ctx.ptr, ctx.chunk_start, ctx.chunk_end, test_name, expected)
                },
            )
        }};
    }
    match effective_mode {
        0 => run_strided!(|idx: u64, seed: u64, _cl: u32| pattern_gen::pattern_mode0(idx, seed)),
        1 => run_strided!(|idx: u64, seed: u64, cl: u32| pattern_gen::pattern_mode1(idx, seed, cl)),
        11 => run_strided!(|idx: u64, combined: u64, _cl: u32| pattern_gen::pattern_mode11(idx, combined)),
        13 => run_strided!(|idx: u64, seed: u64, _cl: u32| pattern_gen::pattern_mode13(idx, seed)),
        _ => run_strided!(|idx: u64, base: u64, _cl: u32| pattern_gen::pattern_mode10(idx, base)),
    }
}

// ─── MirrorMove v2 — u64 migration ──────────────────────────────────────────

/// The test's mirror mode, `Whole` when unset (TODO 85).
fn mirror_mode(config: &TestMemoryConfig) -> MirrorMode {
    config.parameter_context.as_ref().and_then(|c| c.mirror).unwrap_or_default()
}

// ─── MirrorMove v2: the swap kernels ─────────────────────────────────────────
//
// A mirror test's data is the seal (TODO 74, TM5's `Capable_UseTst0ForGenAndCheck`): it swaps the
// sealed words and puts them back, and the seal check after it is its verify. In an unsealed run
// it writes the seal first. These macros stamp the swaps per width; `mirror_move_v2_impl!` makes
// the test function.

/// Swap one `$t` at element `$a` with one at element `$b`.
macro_rules! mirror_swap_pair {
    ($t:ty, $ctx:expr, $a:expr, $b:expr) => {{
        let x = *($ctx.ptr.add($a) as *const $t);
        let y = *($ctx.ptr.add($b) as *const $t);
        *($ctx.ptr.add($a) as *mut $t) = y;
        *($ctx.ptr.add($b) as *mut $t) = x;
    }}
}

/// Whole mirror, one `$w`-element vector per step. The two ends walk the whole chunk and cross
/// the middle, so every pair is swapped twice and the chunk ends as it began: one round trip, as
/// one pass of TM5 MirrorMove (`mtests0.asm` ~1159-1218, 64 B per step).
macro_rules! mirror_swap_whole {
    ($t:ty, $w:expr, $ctx:expr) => {{
        let mut lo = $ctx.chunk_start;
        let mut hi = $ctx.chunk_end - $w;
        for _ in 0..($ctx.chunk_end - $ctx.chunk_start) / $w {
            mirror_swap_pair!($t, $ctx, lo, hi);
            lo += $w;
            hi = hi.wrapping_sub($w);
        }
    }}
}

/// Subblocks: the chunk in `$n` equal parts (a literal, 2 or 4), each mirrored as above, all `$n`
/// in lockstep (TM5 MirrorMove Parameter 2 and 4, ~1224-1286 and ~1360-1435). A chunk is a
/// multiple of 4 KiB, so a quarter is a whole number of vectors at any width.
macro_rules! mirror_swap_subblocks {
    ($t:ty, $w:expr, $ctx:expr, $n:literal) => {{
        let sub = ($ctx.chunk_end - $ctx.chunk_start) / $n;
        let mut lo = $ctx.chunk_start;
        let mut hi = $ctx.chunk_start + sub - $w;
        for _ in 0..sub / $w {
            for s in 0..$n {
                mirror_swap_pair!($t, $ctx, lo + s * sub, hi + s * sub);
            }
            lo += $w;
            hi = hi.wrapping_sub($w);
        }
    }}
}

/// Jump: 128 B swaps every (jump + 1) x 128 B, the jump capped at a quarter of the chunk. Each
/// pass starts 128 B further in, until every 128 B has been visited, last pass first; each pass
/// crosses the middle. TM5 MirrorMove128 (~1522-1676). A pass pairs 128 B units x and n-1-x,
/// so it either swaps its units with another pass's, which swaps them back, or with its own,
/// twice: one round trip over all passes.
macro_rules! mirror_swap_jump {
    ($t:ty, $w:expr, $ctx:expr, $jump:expr) => {{
        const UNIT: usize = 128 / std::mem::size_of::<u64>();
        let len = $ctx.chunk_end - $ctx.chunk_start;
        // A chunk is a multiple of 4 KiB, so a quarter of it is a whole number of units
        let step = ($jump as usize * UNIT).min(len / 4) + UNIT;
        for pass in (0..step / UNIT).rev() {
            let mut lo = $ctx.chunk_start + pass * UNIT;
            let mut hi = $ctx.chunk_end - pass * UNIT - UNIT;
            while lo < $ctx.chunk_end {
                for v in (0..UNIT).step_by($w) {
                    mirror_swap_pair!($t, $ctx, lo + v, hi + v);
                }
                lo += step;
                hi = hi.wrapping_sub(step);
            }
        }
    }}
}

/// One mirror round trip over the chunk in the test's `MirrorMode`.
macro_rules! mirror_swap {
    ($t:ty, $w:expr, $ctx:expr, $mode:expr) => {{
        match $mode {
            MirrorMode::Whole => mirror_swap_whole!($t, $w, $ctx),
            MirrorMode::Subblocks(2) => mirror_swap_subblocks!($t, $w, $ctx, 2),
            MirrorMode::Subblocks(n) => {
                debug_assert_eq!(n, 4, "MirrorMode parsing admits 2 or 4 subblocks");
                mirror_swap_subblocks!($t, $w, $ctx, 4)
            }
            MirrorMode::Jump(j) => mirror_swap_jump!($t, $w, $ctx, j),
        }
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
            let simd_elements: usize = $simd_w;
            let mirror = mirror_mode(config);
            let seal = config.seal;
            run_phased_test(
                blocks, thread_id, error_mode, timing, config, progress,
                test_name, TestAction::WriteWaitVerify,
                4,  // bytes_per_test_op: 2R + 2W per mirror round-trip
                seal.expects || config.skip_init,  // a sealed step's memory holds the seal already
                config.test_reps,  // test_reps: mirror round-trips before verify
                config.verify_reps,  // verify_reps: verification passes
                // Init: the seal, in an unsealed run
                |ctx: &ChunkCtx| seal.kernel.fill(ctx.ptr, ctx.chunk_start, ctx.chunk_end),
                // Test: one mirror round trip
                |ctx: &ChunkCtx| {
                    mirror_swap!($simd_type, simd_elements, ctx, mirror);
                },
                // Verify: the seal check, resealing the chunk if it failed (TM5's refill)
                |ctx: &ChunkCtx| -> u64 { mirror_seal_check(seal.kernel, ctx) },
            )
        }
    }
}

/// A mirror test's verify (TODO 74): the seal check after the mirror, its errors the test's own
/// (TM5 numbers them by the mirror step), and the chunk resealed if it failed.
#[inline(always)]
unsafe fn mirror_seal_check(kernel: crate::seal::SealKernel, ctx: &ChunkCtx) -> u64 {
    let errors = kernel.check(ctx.ptr, ctx.chunk_start, ctx.chunk_end, "seal check after");
    if errors > 0 {
        kernel.fill(ctx.ptr, ctx.chunk_start, ctx.chunk_end);
    }
    errors
}

/// MirrorMove v2 scalar: the mirror a u64 per step, on the seal (TODO 74).
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
    let mirror = mirror_mode(config);
    let seal = config.seal;

    run_phased_test(
        blocks, thread_id, error_mode, timing, config, progress,
        test_name, TestAction::WriteWaitVerify,
        4, // bytes_per_test_op: 2R + 2W per mirror round-trip
        seal.expects || config.skip_init,  // a sealed step's memory holds the seal already
        config.test_reps,  // test_reps: mirror round-trips before verify
        config.verify_reps,  // verify_reps: verification passes
        // Init: the seal, in an unsealed run
        |ctx: &ChunkCtx| seal.kernel.fill(ctx.ptr, ctx.chunk_start, ctx.chunk_end),
        // Test: one mirror round trip, a u64 per step
        |ctx: &ChunkCtx| {
            mirror_swap!(u64, 1, ctx, mirror);
        },
        // Verify: the seal check, resealing the chunk if it failed (TM5's refill)
        |ctx: &ChunkCtx| -> u64 { mirror_seal_check(seal.kernel, ctx) },
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

mirror_move_v2_impl!(mirror_move_v2_128_impl, "Mem-MirrorV2-128", u64x2, 2, "sse4.2,sse4.1,ssse3,sse3,sse2,popcnt");

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

mirror_move_v2_impl!(mirror_move_v2_256_impl, "Mem-MirrorV2-256", u64x4, 4, "avx2,avx,fma,bmi1,bmi2");

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

mirror_move_v2_impl!(mirror_move_v2_512_impl, "Mem-MirrorV2-512", u64x8, 8, "avx512f,avx512bw,avx512cd,avx512dq,avx512vl,avx2,avx,fma,bmi1,bmi2");

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
// Two code paths: positional (idx ^ base, every mode but 12) and mode 12 (hashed lines).
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

/// SIMD write for mode 12: each 64 B line is `seed + j * step`, with seed and step hashed from
/// the line's address (`pattern_gen::mode12_line`), stored W words at a time.
macro_rules! simple_write_mode12_simd {
    ($simd_type:ty, $simd_w:expr, $ctx:expr, $mode12:expr, $lane_offsets:expr) => {{
        for offset in ($ctx.chunk_start..$ctx.chunk_end).step_by(LINE_WORDS) {
            let line = $ctx.ptr.add(offset);
            let (seed, step) = pattern_gen::mode12_line(line as usize, $mode12.key);
            let advance = <$simd_type>::splat(step.wrapping_mul($simd_w as u64));
            let mut words = <$simd_type>::splat(seed) + <$simd_type>::splat(step) * $lane_offsets;
            for j in (0..LINE_WORDS).step_by($simd_w) {
                *(line.add(j) as *mut $simd_type) = words;
                words += advance;
            }
        }
    }}
}

/// SIMD verify for mode 12: XOR+OR into an accumulator; the scalar rescan counts and logs.
macro_rules! simple_verify_mode12_simd {
    ($simd_type:ty, $simd_w:expr, $ctx:expr, $mode12:expr, $lane_offsets:expr, $zero:expr, $test_name:expr) => {{
        let mut acc = $zero;
        for offset in ($ctx.chunk_start..$ctx.chunk_end).step_by(LINE_WORDS) {
            let line = $ctx.ptr.add(offset);
            let (seed, step) = pattern_gen::mode12_line(line as usize, $mode12.key);
            let advance = <$simd_type>::splat(step.wrapping_mul($simd_w as u64));
            let mut words = <$simd_type>::splat(seed) + <$simd_type>::splat(step) * $lane_offsets;
            for j in (0..LINE_WORDS).step_by($simd_w) {
                acc |= *(line.add(j) as *const $simd_type) ^ words;
                words += advance;
            }
        }
        if acc.simd_ne($zero).any() {
            $mode12.rescan($ctx.ptr, $ctx.chunk_start, $ctx.chunk_end, $test_name)
        } else {
            0
        }
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

/// SIMD write for mode 12 with stride: the same image as the sequential write, each W-word
/// block computed from its line's hash, in the block-strided order of the positional writer.
macro_rules! simple_write_strided_mode12_simd {
    ($simd_type:ty, $simd_w:expr, $ctx:expr, $mode12:expr, $lane_offsets:expr, $stride_param:expr) => {{
        let block_elements: usize = $simd_w;
        let total_stride: usize = $stride_param * block_elements + block_elements;
        let interleave_passes: usize = total_stride / block_elements;

        for pass in 0..interleave_passes {
            let mut pos = $ctx.chunk_start + pass * block_elements;
            while pos + block_elements <= $ctx.chunk_end {
                let line = pos & !(LINE_WORDS - 1);
                let (seed, step) = pattern_gen::mode12_line($ctx.ptr.add(line) as usize, $mode12.key);
                let lanes = $lane_offsets + <$simd_type>::splat((pos - line) as u64);
                *($ctx.ptr.add(pos) as *mut $simd_type) = <$simd_type>::splat(seed) + <$simd_type>::splat(step) * lanes;
                pos += total_stride;
            }
        }
    }}
}

/// SIMD verify for mode 12 with stride: XOR+OR accumulate; the scalar rescan counts and logs.
macro_rules! simple_verify_strided_mode12_simd {
    ($simd_type:ty, $simd_w:expr, $ctx:expr, $mode12:expr, $lane_offsets:expr, $zero:expr, $stride_param:expr, $test_name:expr) => {{
        let block_elements: usize = $simd_w;
        let total_stride: usize = $stride_param * block_elements + block_elements;
        let interleave_passes: usize = total_stride / block_elements;
        let mut acc = $zero;

        for pass in 0..interleave_passes {
            let mut pos = $ctx.chunk_start + pass * block_elements;
            while pos + block_elements <= $ctx.chunk_end {
                let line = pos & !(LINE_WORDS - 1);
                let (seed, step) = pattern_gen::mode12_line($ctx.ptr.add(line) as usize, $mode12.key);
                let lanes = $lane_offsets + <$simd_type>::splat((pos - line) as u64);
                let expected = <$simd_type>::splat(seed) + <$simd_type>::splat(step) * lanes;
                acc |= *($ctx.ptr.add(pos) as *const $simd_type) ^ expected;
                pos += total_stride;
            }
        }

        if acc.simd_ne($zero).any() {
            $mode12.rescan($ctx.ptr, $ctx.chunk_start, $ctx.chunk_end, $test_name)
        } else {
            0
        }
    }}
}

/// Master macro: stamps out a complete SIMD SimpleTest v2 function for a given width.
/// Two pattern paths: positional (idx^base, or idx^combined for modes 1/11) and mode 12.
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
        $m12_seq_fn:ident,
        $m12_str_fn:ident,
        $test_name:literal,
        $simd_type:ty,
        $simd_w:expr,
        $lane_offsets:expr,
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

        /// Mode 12 sequential — contiguous access, each line hashed from its address.
        #[target_feature(enable = $target_feature)]
        unsafe fn $m12_seq_fn(
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
            let param0 = config.pattern_param0.unwrap_or(0xDEADBEEFDEADBEEF);
            let param1 = config.pattern_param1.unwrap_or(0xCAFEBABECAFEBABE);

            run_phased_test(
                blocks, thread_id, error_mode, timing, config, progress,
                test_name, TestAction::WriteVerify, 1, false, config.test_reps, config.verify_reps,
                |ctx: &ChunkCtx| {
                    let mode12 = Mode12::new(ctx.thread_id, ctx.cycle, param0, param1);
                    simple_write_mode12_simd!($simd_type, simd_elements, ctx, mode12, lane_offsets);
                },
                |ctx: &ChunkCtx| {
                    let mode12 = Mode12::new(ctx.thread_id, ctx.cycle, param0, param1);
                    simple_write_mode12_simd!($simd_type, simd_elements, ctx, mode12, lane_offsets);
                },
                |ctx: &ChunkCtx| -> u64 {
                    let mode12 = Mode12::new(ctx.thread_id, ctx.cycle, param0, param1);
                    simple_verify_mode12_simd!($simd_type, simd_elements, ctx, mode12, lane_offsets, zero, test_name)
                },
            )
        }

        /// Mode 12 strided — block-strided access, the same image as sequential.
        #[target_feature(enable = $target_feature)]
        unsafe fn $m12_str_fn(
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
            let param0 = config.pattern_param0.unwrap_or(0xDEADBEEFDEADBEEF);
            let param1 = config.pattern_param1.unwrap_or(0xCAFEBABECAFEBABE);
            let stride_param = config.parameter_context.as_ref()
                .and_then(|ctx| ctx.stride_elements)
                .unwrap_or(0);

            run_phased_test(
                blocks, thread_id, error_mode, timing, config, progress,
                test_name, TestAction::WriteVerify, 1, false, config.test_reps, config.verify_reps,
                |ctx: &ChunkCtx| {
                    let mode12 = Mode12::new(ctx.thread_id, ctx.cycle, param0, param1);
                    simple_write_strided_mode12_simd!($simd_type, simd_elements, ctx, mode12, lane_offsets, stride_param);
                },
                |ctx: &ChunkCtx| {
                    let mode12 = Mode12::new(ctx.thread_id, ctx.cycle, param0, param1);
                    simple_write_strided_mode12_simd!($simd_type, simd_elements, ctx, mode12, lane_offsets, stride_param);
                },
                |ctx: &ChunkCtx| -> u64 {
                    let mode12 = Mode12::new(ctx.thread_id, ctx.cycle, param0, param1);
                    simple_verify_strided_mode12_simd!($simd_type, simd_elements, ctx, mode12, lane_offsets, zero, stride_param, test_name)
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
            let is_mode12 = config.pattern_mode.unwrap_or(0) == 12;
            let has_stride = config.parameter_context.as_ref()
                .and_then(|ctx| ctx.stride_elements)
                .map_or(false, |s| s > 0);

            match (is_mode12, has_stride) {
                (true, true)   => $m12_str_fn(blocks, thread_id, error_mode, timing, config, progress),
                (true, false)  => $m12_seq_fn(blocks, thread_id, error_mode, timing, config, progress),
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
    simple_test_v2_128_m12_seq, simple_test_v2_128_m12_str,
    "Mem-SimpleV2-128", u64x2, 2, [0, 1],
    "sse4.2,sse4.1,ssse3,sse3,sse2,popcnt");
simple_test_v2_impl!(simple_test_v2_256_impl,
    simple_test_v2_256_pos_seq, simple_test_v2_256_pos_str,
    simple_test_v2_256_m12_seq, simple_test_v2_256_m12_str,
    "Mem-SimpleV2-256", u64x4, 4, [0, 1, 2, 3],
    "avx2,avx,fma,bmi1,bmi2");
simple_test_v2_impl!(simple_test_v2_512_impl,
    simple_test_v2_512_pos_seq, simple_test_v2_512_pos_str,
    simple_test_v2_512_m12_seq, simple_test_v2_512_m12_str,
    "Mem-SimpleV2-512", u64x8, 8, [0, 1, 2, 3, 4, 5, 6, 7],
    "avx512f,avx512bw,avx512cd,avx512dq,avx512vl,avx2,avx,fma,bmi1,bmi2");

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

/// Seal-Check: TM5's test 0 as a step (TODO 74), the seal check of every chunk, resealing a chunk
/// that failed (TM5's refill). Its errors are its own, and TM5 numbers them 0. In an unsealed run
/// it seals its extent first.
#[doc = include_str!("test_fn_safety.md")]
pub unsafe fn seal_check_multi(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> TestStats {
    let seal = config.seal;
    run_phased_test(
        blocks, thread_id, error_mode, timing, config, progress,
        "Seal-Check", TestAction::Verify, 0, seal.expects || config.skip_init, 0, 1,
        |ctx: &ChunkCtx| seal.kernel.fill(ctx.ptr, ctx.chunk_start, ctx.chunk_end),
        |_ctx: &ChunkCtx| {},
        |ctx: &ChunkCtx| -> u64 {
            let errors = seal.kernel.check(ctx.ptr, ctx.chunk_start, ctx.chunk_end, "seal check at");
            if errors > 0 {
                seal.kernel.fill(ctx.ptr, ctx.chunk_start, ctx.chunk_end);
            }
            errors
        },
    )
}

/// The kernel a Bench-*-Seal-* test measures: its pattern at its width or, where the CPU lacks
/// that one (or the width is `Auto`), the widest it has.
fn bench_seal_kernel(pattern: crate::seal::SealPattern, width: crate::seal::SealWidth, test_name: &str) -> crate::seal::SealKernel {
    let width = width.resolve().unwrap_or_else(|e| {
        log::warn!("{test_name}: {e}; measuring the widest width this CPU has");
        crate::seal::SealKernel::detect().width
    });
    crate::seal::SealKernel { pattern, width }
}

/// Bench-Init-Seal-*: the seal's fill alone (TODO 74), one non-temporal write pass per chunk per
/// cycle.
#[allow(clippy::too_many_arguments)]
unsafe fn bench_init_seal(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
    test_name: &'static str,
    kernel: crate::seal::SealKernel,
) -> TestStats {
    run_phased_test(
        blocks, thread_id, error_mode, timing, config, progress,
        test_name, TestAction::Write, 1, config.skip_init, 1, 0,
        |ctx: &ChunkCtx| kernel.fill(ctx.ptr, ctx.chunk_start, ctx.chunk_end),
        |ctx: &ChunkCtx| kernel.fill(ctx.ptr, ctx.chunk_start, ctx.chunk_end),
        |_ctx: &ChunkCtx| -> u64 { 0 },
    )
}

/// Bench-Verify-Seal-*: the seal's check alone (TODO 74), one read pass per chunk per cycle, after
/// one fill unless it follows its Bench-Init-Seal-* (`skip_init`).
#[allow(clippy::too_many_arguments)]
unsafe fn bench_verify_seal(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
    test_name: &'static str,
    kernel: crate::seal::SealKernel,
) -> TestStats {
    run_phased_test(
        blocks, thread_id, error_mode, timing, config, progress,
        test_name, TestAction::Verify, 1, config.skip_init, 0, 1,
        |ctx: &ChunkCtx| kernel.fill(ctx.ptr, ctx.chunk_start, ctx.chunk_end),
        |_ctx: &ChunkCtx| {},
        |ctx: &ChunkCtx| -> u64 { kernel.check(ctx.ptr, ctx.chunk_start, ctx.chunk_end, "seal check of") },
    )
}

/// One Bench-Init-Seal-* and its Bench-Verify-Seal-* for a seal pattern and width.
macro_rules! bench_seal_fns {
    ($init:ident, $init_name:literal, $verify:ident, $verify_name:literal, $pattern:ident, $width:ident) => {
        #[doc = include_str!("test_fn_safety.md")]
        pub unsafe fn $init(blocks: &[crate::runner::AllocationBlock], thread_id: usize, error_mode: ErrorMode,
                            timing: &TestTiming, config: &TestMemoryConfig, progress: Option<&TestProgress>) -> TestStats {
            let kernel = bench_seal_kernel(crate::seal::SealPattern::$pattern, crate::seal::SealWidth::$width, $init_name);
            bench_init_seal(blocks, thread_id, error_mode, timing, config, progress, $init_name, kernel)
        }

        #[doc = include_str!("test_fn_safety.md")]
        pub unsafe fn $verify(blocks: &[crate::runner::AllocationBlock], thread_id: usize, error_mode: ErrorMode,
                              timing: &TestTiming, config: &TestMemoryConfig, progress: Option<&TestProgress>) -> TestStats {
            let kernel = bench_seal_kernel(crate::seal::SealPattern::$pattern, crate::seal::SealWidth::$width, $verify_name);
            bench_verify_seal(blocks, thread_id, error_mode, timing, config, progress, $verify_name, kernel)
        }
    };
}

bench_seal_fns!(bench_init_seal_tmr_128_multi, "Bench-Init-Seal-TMR-128", bench_verify_seal_tmr_128_multi, "Bench-Verify-Seal-TMR-128", Tmr, W128);
bench_seal_fns!(bench_init_seal_tmr_256_multi, "Bench-Init-Seal-TMR-256", bench_verify_seal_tmr_256_multi, "Bench-Verify-Seal-TMR-256", Tmr, W256);
bench_seal_fns!(bench_init_seal_tmr_512_multi, "Bench-Init-Seal-TMR-512", bench_verify_seal_tmr_512_multi, "Bench-Verify-Seal-TMR-512", Tmr, W512);
bench_seal_fns!(bench_init_seal_tmr_widest_multi, "Bench-Init-Seal-TMR", bench_verify_seal_tmr_widest_multi, "Bench-Verify-Seal-TMR", Tmr, Auto);
bench_seal_fns!(bench_init_seal_tm5_128_multi, "Bench-Init-Seal-TM5-128", bench_verify_seal_tm5_128_multi, "Bench-Verify-Seal-TM5-128", Tm5, W128);
bench_seal_fns!(bench_init_seal_tm5_256_multi, "Bench-Init-Seal-TM5-256", bench_verify_seal_tm5_256_multi, "Bench-Verify-Seal-TM5-256", Tm5, W256);
bench_seal_fns!(bench_init_seal_tm5_512_multi, "Bench-Init-Seal-TM5-512", bench_verify_seal_tm5_512_multi, "Bench-Verify-Seal-TM5-512", Tm5, W512);
bench_seal_fns!(bench_init_seal_tm5_widest_multi, "Bench-Init-Seal-TM5", bench_verify_seal_tm5_widest_multi, "Bench-Verify-Seal-TM5", Tm5, Auto);

/// The Bench-*-Seal-* tests: (name, function), Init before its Verify, per pattern and width.
pub fn bench_seal_tests() -> [(&'static str, crate::runner::TestFunction); 16] {
    use crate::runner::TestFunction::MultiBlock;
    [
        ("Bench-Init-Seal-TMR", MultiBlock(bench_init_seal_tmr_widest_multi)),
        ("Bench-Verify-Seal-TMR", MultiBlock(bench_verify_seal_tmr_widest_multi)),
        ("Bench-Init-Seal-TMR-128", MultiBlock(bench_init_seal_tmr_128_multi)),
        ("Bench-Verify-Seal-TMR-128", MultiBlock(bench_verify_seal_tmr_128_multi)),
        ("Bench-Init-Seal-TMR-256", MultiBlock(bench_init_seal_tmr_256_multi)),
        ("Bench-Verify-Seal-TMR-256", MultiBlock(bench_verify_seal_tmr_256_multi)),
        ("Bench-Init-Seal-TMR-512", MultiBlock(bench_init_seal_tmr_512_multi)),
        ("Bench-Verify-Seal-TMR-512", MultiBlock(bench_verify_seal_tmr_512_multi)),
        ("Bench-Init-Seal-TM5", MultiBlock(bench_init_seal_tm5_widest_multi)),
        ("Bench-Verify-Seal-TM5", MultiBlock(bench_verify_seal_tm5_widest_multi)),
        ("Bench-Init-Seal-TM5-128", MultiBlock(bench_init_seal_tm5_128_multi)),
        ("Bench-Verify-Seal-TM5-128", MultiBlock(bench_verify_seal_tm5_128_multi)),
        ("Bench-Init-Seal-TM5-256", MultiBlock(bench_init_seal_tm5_256_multi)),
        ("Bench-Verify-Seal-TM5-256", MultiBlock(bench_verify_seal_tm5_256_multi)),
        ("Bench-Init-Seal-TM5-512", MultiBlock(bench_init_seal_tm5_512_multi)),
        ("Bench-Verify-Seal-TM5-512", MultiBlock(bench_verify_seal_tm5_512_multi)),
    ]
}

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
            // Mode 2: TM5's evolving lines, the chain restarted per 4 KiB page
            let mode2 = Mode2 { thread_id, cycle: 0, param0, param1 };
            run_phased_test(
                blocks, thread_id, error_mode, timing, config, progress,
                test_name, TestAction::Write, 1, skip_init, 1, 0,
                |ctx: &ChunkCtx| mode2.fill(ctx.ptr, ctx.chunk_start, ctx.chunk_end),
                |ctx: &ChunkCtx| mode2.fill(ctx.ptr, ctx.chunk_start, ctx.chunk_end),
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
            // Mode 12 (and any unknown mode): mode 2's lines, hashed from each line's address
            let mode12 = Mode12::new(thread_id, 0, param0, param1);
            run_phased_test(
                blocks, thread_id, error_mode, timing, config, progress,
                test_name, TestAction::Write, 1, skip_init, 1, 0,
                |ctx: &ChunkCtx| mode12.fill(ctx.ptr, ctx.chunk_start, ctx.chunk_end),
                |ctx: &ChunkCtx| mode12.fill(ctx.ptr, ctx.chunk_start, ctx.chunk_end),
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
                    verify_words(ctx.ptr, ctx.chunk_start, ctx.chunk_end, test_name, |idx| pattern_gen::pattern_mode0(idx as u64, seed))
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
                    verify_words(ctx.ptr, ctx.chunk_start, ctx.chunk_end, test_name, |idx| pattern_gen::pattern_mode1(idx as u64, seed, cl_shift))
                },
            )
        }
        2 => {
            // Mode 2: TM5's evolving lines, the chain restarted per 4 KiB page
            let mode2 = Mode2 { thread_id, cycle: 0, param0, param1 };
            run_phased_test(
                blocks, thread_id, error_mode, timing, config, progress,
                test_name, TestAction::Verify, 1, skip_init, 0, 1,
                |ctx: &ChunkCtx| mode2.fill(ctx.ptr, ctx.chunk_start, ctx.chunk_end),
                |_ctx: &ChunkCtx| {},
                |ctx: &ChunkCtx| -> u64 { mode2.verify(ctx.ptr, ctx.chunk_start, ctx.chunk_end, test_name) },
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
                    verify_words(ctx.ptr, ctx.chunk_start, ctx.chunk_end, test_name, |idx| pattern_gen::pattern_mode10(idx as u64, base))
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
                    verify_words(ctx.ptr, ctx.chunk_start, ctx.chunk_end, test_name, |idx| pattern_gen::pattern_mode11(idx as u64, combined))
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
                    verify_words(ctx.ptr, ctx.chunk_start, ctx.chunk_end, test_name, |idx| pattern_gen::pattern_mode13(idx as u64, seed))
                },
            )
        }
        _ => {
            // Mode 12 (and any unknown mode): mode 2's lines, hashed from each line's address
            let mode12 = Mode12::new(thread_id, 0, param0, param1);
            run_phased_test(
                blocks, thread_id, error_mode, timing, config, progress,
                test_name, TestAction::Verify, 1, skip_init, 0, 1,
                |ctx: &ChunkCtx| mode12.fill(ctx.ptr, ctx.chunk_start, ctx.chunk_end),
                |_ctx: &ChunkCtx| {},
                |ctx: &ChunkCtx| -> u64 { mode12.verify(ctx.ptr, ctx.chunk_start, ctx.chunk_end, test_name) },
            )
        }
    }
}

// ============================================================================
// Test Registry - Maps config test names to actual function implementations
// ============================================================================

/// Whether the test runs through `run_phased_test`, so `write_read_cycles`, `test_reps` and
/// `verify_reps` shape its loop. Tier 2, bandwidth and latency tests ignore all three
/// (`doc/test_harness_tiers.md`). A new Tier 1 test family adds its prefix here.
pub fn reads_rep_knobs(actual_name: &str) -> bool {
    ["Mem-SimpleV2", "Mem-SimpleNT", "Mem-MirrorV2", "Bench-Init-", "Bench-Verify-"]
        .iter()
        .any(|prefix| actual_name.starts_with(prefix))
}

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
        // CLFLUSHOPT-verify variants (#59). These map to the SAME functions — flushing is
        // driven by `TestMemoryConfig.flush_before_verify`, not by the function. The hardcoded
        // registrations set it via `.with_flush_before_verify(true)`; JSON configs set the
        // per-test `flush_before_verify` field (so any test can opt in, not just these names).
        "Mem-StuckBit-Flush" => Some(TestFunction::MultiBlock(stuck_bit_test_multi)),
        "Mem-StuckBit-FlushAuto" => Some(TestFunction::MultiBlock(stuck_bit_test_auto_multi)),
        "Mem-StuckBit-Flush128" => Some(TestFunction::MultiBlock(stuck_bit_test_128_multi)),
        "Mem-StuckBit-Flush256" => Some(TestFunction::MultiBlock(stuck_bit_test_256_multi)),
        "Mem-StuckBit-Flush512" => Some(TestFunction::MultiBlock(stuck_bit_test_512_multi)),

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
        // Flush variant (#59): auto-dispatch fn; flushing is driven by
        // `flush_before_verify`, which the hardcoded registration sets.
        "Mem-Refresh-Flush" => Some(TestFunction::MultiBlock(refresh_stable_multi)),
        "Mem-Refresh-Flush128" => Some(TestFunction::MultiBlock(refresh_stable_128_multi)),
        "Mem-Refresh-Flush256" => Some(TestFunction::MultiBlock(refresh_stable_256_multi)),
        "Mem-Refresh-Flush512" => Some(TestFunction::MultiBlock(refresh_stable_512_multi)),
        "Mem-Refresh-FlushAuto" => Some(TestFunction::MultiBlock(refresh_stable_auto_multi)),
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

        // TM5's test 0 as a step: the seal check (TODO 74)
        "Seal-Check" => Some(TestFunction::MultiBlock(seal_check_multi)),

        // The seal's fill and check, per pattern and width (TODO 74)
        s if s.starts_with("Bench-Init-Seal-") || s.starts_with("Bench-Verify-Seal-")
            => bench_seal_tests().into_iter().find(|(n, _)| *n == s).map(|(_, f)| f),

        _ => None,
    }
}

#[cfg(test)]
mod random_replay_tests {
    use super::{accumulate_stride, accumulate_stride_4, random_replay, rescan_stride, rescan_words, verify_words, Mode12, Mode2};
    use crate::pattern_gen::PAGE_WORDS;

    /// TODO 89: the strided accumulate sees a bad word wherever its walk passes, at any stride
    /// and remainder; the rescans count each bad word exactly, the strided one only on its walk.
    #[test]
    fn strided_verify_finds_and_counts_each_bad_word() {
        let len = 1000usize;
        let expected = |i: usize| (i as u64).wrapping_mul(0x9E3779B97F4A7C15);
        let mut mem: Vec<u64> = (0..len).map(expected).collect();
        let p = mem.as_mut_ptr();
        let clean = |stride: usize| unsafe {
            let (mut acc, mut acc4) = ([0u64; 4], [0u64; 4]);
            for offset in 0..stride.min(len) {
                accumulate_stride(&mut acc, p, offset, len, stride, expected);
                accumulate_stride_4(&mut acc4, p, offset, len, stride, expected);
            }
            let (one, four) = ((acc[0] | acc[1]) | (acc[2] | acc[3]) == 0, (acc4[0] | acc4[1]) | (acc4[2] | acc4[3]) == 0);
            assert_eq!(one, four, "the two walks disagree at stride {stride}");
            one
        };
        unsafe {
            for stride in [1, 3, 7, 64, 999, 1000, 4096] {
                assert!(clean(stride), "stride {stride} on clean memory");
            }
            *p.add(0) ^= 1;
            *p.add(997) ^= 1 << 63;
            *p.add(500) = 0;
            for stride in [1, 3, 7, 64, 999, 1000, 4096] {
                assert!(!clean(stride), "stride {stride} missed a bad word");
            }
            assert_eq!(rescan_words(p, 0, len, "test", expected), 3);
            assert_eq!(verify_words(p, 0, len, "test", expected), 3);
            // A stride-3 walk from 1 visits 997 (= 1 + 3 x 332) and 500 is off it (500 % 3 = 2)
            assert_eq!(rescan_stride(p, 1, len, 3, "test", expected), 1);
            assert_eq!(rescan_stride(p, 2, len, 3, "test", expected), 1);
            assert_eq!(rescan_stride(p, 0, len, 3, "test", expected), 1);
        }
    }

    /// TODO 76: modes 2 and 12 depend only on the address, so writing a range in overlapping
    /// pieces, in any order, gives the image one pass gives. The verify finds nothing on it and
    /// counts each bad word exactly.
    #[test]
    fn line_patterns_are_position_pure_and_count_exactly() {
        let words = 6 * PAGE_WORDS;
        let mut buf = vec![std::simd::u64x8::splat(0); words / 8];
        let p = buf.as_mut_ptr() as *mut u64;
        let mode2 = Mode2 { thread_id: 3, cycle: 1, param0: 0x5DEECE66D, param1: 0xB };
        let mode12 = Mode12::new(3, 1, 0x5DEECE66D, 0xB);
        let image = |p: *mut u64| unsafe { std::slice::from_raw_parts(p, words).to_vec() };
        unsafe {
            mode2.fill(p, 0, words);
            let one_pass = image(p);
            mode12.fill(p, 0, words);
            for (s, e) in [(4, 6), (0, 3), (2, 5)] {
                mode2.fill(p, s * PAGE_WORDS, e * PAGE_WORDS);
            }
            assert_eq!(image(p), one_pass);
            assert_eq!(mode2.verify(p, 0, words, "test"), 0);
            *p.add(700) ^= 1;
            *p.add(2 * PAGE_WORDS + 9) ^= 1 << 63;
            assert_eq!(mode2.verify(p, 0, words, "test"), 2);
            assert_eq!(mode2.verify(p, 3 * PAGE_WORDS, words, "test"), 0);
            assert!(mode12.verify(p, 0, words, "test") > 0);

            mode12.fill(p, 0, words);
            let one_pass = image(p);
            mode2.fill(p, 0, words);
            for (s, e) in [(5 * PAGE_WORDS, words), (8, 3 * PAGE_WORDS + 64), (0, 5 * PAGE_WORDS + 8)] {
                mode12.fill(p, s, e);
            }
            assert_eq!(image(p), one_pass);
            assert_eq!(mode12.verify(p, 0, words, "test"), 0);
            *p.add(5 * PAGE_WORDS + 511) = 0;
            assert_eq!(mode12.verify(p, 0, words, "test"), 1);
        }
    }

    /// TODO 76: Mem-Random's replay counts exactly the mismatches the hot loop's accumulator saw,
    /// on a length that isn't a power of two, and every index stays below it.
    #[test]
    fn replay_counts_what_a_plain_loop_counts() {
        let len = 1000usize;
        let mut mem: Vec<u64> = (0..len as u64).collect();
        mem[123] ^= 1;
        mem[999] ^= 1 << 40;
        let start = 0x123456789ABCDEFu64;

        let (mut rng, mut expected) = (start, 0u64);
        for _ in 0..20_000 {
            rng ^= rng << 13;
            rng ^= rng >> 17;
            rng ^= rng << 5;
            let idx = ((rng as u128 * len as u128) >> 64) as usize;
            assert!(idx < len);
            expected += u64::from(mem[idx] != idx as u64);
        }
        assert!(expected > 0);
        let counted = unsafe { random_replay(mem.as_mut_ptr(), len, start, 0..20_000, 0, "test") };
        assert_eq!(counted, expected);

        // A fault gone by the reread still counts once
        mem[123] ^= 1;
        mem[999] ^= 1 << 40;
        assert_eq!(unsafe { random_replay(mem.as_mut_ptr(), len, start, 0..20_000, 0, "test") }, 1);
    }
}

#[cfg(test)]
mod mirror_tests {
    use super::MirrorMode;
    use std::simd::{u64x2, u64x4, u64x8};

    /// Stands in for `ChunkCtx::ptr`: records every element index a swap kernel touches.
    struct Recorder {
        base: *mut u64,
        seen: std::cell::RefCell<Vec<usize>>,
    }

    impl Recorder {
        fn add(&self, i: usize) -> *mut u64 {
            self.seen.borrow_mut().push(i);
            unsafe { self.base.add(i) }
        }
    }

    struct Probe<'a> {
        ptr: &'a Recorder,
        chunk_start: usize,
        chunk_end: usize,
    }

    /// TODO 85: one test op of every mirror mode, at every width, visits each vector of the
    /// chunk in exactly two swaps, swaps it only with its mirror image (in its own subblock;
    /// 128 B units for a jump), and leaves the chunk as it began.
    #[test]
    fn mirror_modes_swap_mirror_pairs_twice_and_round_trip() {
        let modes = [MirrorMode::Whole, MirrorMode::Subblocks(2), MirrorMode::Subblocks(4),
                     MirrorMode::Jump(0), MirrorMode::Jump(1), MirrorMode::Jump(2), MirrorMode::Jump(510)];
        // 4, 68 and 100 KiB chunks, at an offset into the buffer
        for pages in [1usize, 17, 25] {
            let (start, len) = (512, pages * 512);
            let mut buf = vec![u64x8::splat(0); (start + len + 512) / 8];
            let base = buf.as_mut_ptr() as *mut u64;
            for mode in modes {
                for w in [1usize, 2, 4, 8] {
                    for i in 0..start + len + 512 {
                        unsafe { *base.add(i) = i as u64 };
                    }
                    let rec = Recorder { base, seen: Default::default() };
                    let ctx = Probe { ptr: &rec, chunk_start: start, chunk_end: start + len };
                    unsafe {
                        match w {
                            1 => mirror_swap!(u64, 1, ctx, mode),
                            2 => mirror_swap!(u64x2, 2, ctx, mode),
                            4 => mirror_swap!(u64x4, 4, ctx, mode),
                            _ => mirror_swap!(u64x8, 8, ctx, mode),
                        }
                    }
                    let what = format!("{mode} at {w} words, {pages} pages");
                    for i in 0..start + len + 512 {
                        assert_eq!(unsafe { *base.add(i) }, i as u64, "{what}: word {i} moved");
                    }
                    let seen = rec.seen.into_inner();
                    assert_eq!(seen.len(), 4 * len / w, "{what}: not two swaps per vector");
                    let mut count = vec![0u8; len / w];
                    for swap in seen.chunks(4) {
                        let (a, b) = (swap[0] - start, swap[1] - start);
                        assert_eq!((swap[2], swap[3]), (swap[0], swap[1]), "{what}: not a swap");
                        assert!(a % w == 0 && b % w == 0, "{what}: misaligned {a} {b}");
                        let mirrored = match mode {
                            MirrorMode::Whole => a + b == len - w,
                            MirrorMode::Subblocks(n) => {
                                let sub = len / n as usize;
                                a / sub == b / sub && a % sub + b % sub == sub - w
                            }
                            MirrorMode::Jump(_) => a / 16 + b / 16 == len / 16 - 1 && a % 16 == b % 16,
                        };
                        assert!(mirrored, "{what}: swapped {a} with {b}");
                        count[a / w] += 1;
                        count[b / w] += 1;
                    }
                    assert!(count.iter().all(|&c| c == 2), "{what}: a vector not in exactly two swaps");
                }
            }
        }
    }
}
