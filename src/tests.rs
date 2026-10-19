use crate::ErrorMode;
use crate::cache::{CacheInfo, SystemInfo};
use crate::memory::buffer::MemoryType;
use crate::constants::{MB, MB_16, MB_32, MB_64, KB, PAGE_SIZE_4KB};
use std::simd::*; // Docs here https://doc.rust-lang.org/std/simd/index.html
use std::simd::cmp::SimdPartialEq;
use std::simd::num::SimdUint;
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
/// Only `PER_CHUNK` is constructed today, so the one `check_mask` path (the sequential SIMD
/// positional verify: SimpleTest's modes 10 and 11, and SimpleNT) never runs. Every other kernel,
/// the native ones among them, checks once a chunk whatever it says, so constructing another
/// interval needs them taught it first.
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
    /// The test's own time: its clock restarts after setup it leaves out, such as a Bench-Verify's
    /// fill (TODO 90). `None` when it didn't run.
    pub timed: Option<std::time::Duration>,
    pub thread_id: usize,
    pub error_count: u64,
    pub total_operations: u64,  // Total operation count (lightweight tracking)
    pub cycles_completed: u32,  // Actual cycles completed
    pub cycles_planned: Option<u32>, // Planned cycles (None = unlimited)
    pub stopped_by_time_limit: bool, // True if stopped due to time, false if stopped due to cycle limit
    /// Bad words the seal check before a chunk found (TODO 74): data the previous step left
    /// sealed went bad, so they aren't this test's errors. TM5 numbers them 0.
    pub seal_errors: u64,
    /// Checks that found a difference (TODO 27's first tier): each failed accumulator, the seal's
    /// included. `error_count` counts the bad words their rescans found instead.
    pub alarms: u64,
    /// Failed checks whose rescan found every word right (`alarms` minus the localised ones, when
    /// rescanning): each still counts one error.
    pub not_reproduced: u64,
    /// The work around the test sequence, by operation (TODO 90): the seal's fills and checks, a
    /// fill, a chain build. Left out of `timed` and `bytes_processed`.
    pub ops: crate::ops::OpTally,
}

impl TestStats {
    /// A test that didn't run: no memory, or a SIMD width the CPU lacks.
    pub fn not_run(name: &'static str, action: TestAction, thread_id: usize, cycles_planned: Option<u32>) -> Self {
        TestStats {
            name,
            action,
            bytes_processed: 0,
            timed: None,
            thread_id,
            error_count: 0,
            total_operations: 0,
            cycles_completed: 0,
            cycles_planned,
            stopped_by_time_limit: false,
            seal_errors: 0,
            alarms: 0,
            not_reproduced: 0,
            ops: Default::default(),
        }
    }
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
    /// The native program a SimpleTest runs (pattern spec v2, `crate::pattern`). `None` with no
    /// `pattern_mode` is the default program; a `pattern_mode` runs that TMR mode (spec 1).
    pub program: Option<crate::pattern::Program>,
    /// Mem-Sequence's passes (`crate::sequence`); `None` is its default, `W 5R`. Boxed: the
    /// registry builds a config per test by value, and 132 bytes more each made the release
    /// build 3x slower
    pub sequence: Option<Box<crate::sequence::Sequence>>,
    /// Where this step runs in the suite (run seed, suite cycle, step), for the native patterns'
    /// keys; set per dispatch by the runner, all zero outside a run and for the Bench pairs.
    pub epoch: crate::pattern::StepEpoch,
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
            program: None,  // The default program, or the pattern_mode if one is set
            sequence: None,  // Mem-Sequence's passes; its default when unset
            epoch: crate::pattern::StepEpoch::default(),  // Set per dispatch by the runner
        }
    }

    /// Builder: Mem-Sequence's passes.
    pub fn with_sequence(mut self, sequence: crate::sequence::Sequence) -> Self {
        self.sequence = Some(Box::new(sequence));
        self
    }

    /// Mem-Sequence's passes: its own, or the default.
    pub fn sequence(&self) -> crate::sequence::Sequence {
        self.sequence.as_deref().copied().unwrap_or_default()
    }

    /// Builder: the native program this SimpleTest runs.
    pub fn with_program(mut self, program: crate::pattern::Program) -> Self {
        self.program = Some(program);
        self
    }

    /// The native program a SimpleTest with this config runs, or `None` when a `pattern_mode`
    /// selects a TMR mode instead.
    pub fn native_program(&self) -> Option<crate::pattern::Program> {
        match (self.program, self.pattern_mode) {
            (Some(program), _) => Some(program),
            (None, None) => Some(crate::pattern::Program::default_simple()),
            (None, Some(_)) => None,
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

    /// Builder: rounds per chunk per cycle (`write_read_cycles`).
    pub fn with_write_read_cycles(mut self, rounds: u32) -> Self {
        self.write_read_cycles = rounds;
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
            "Mem-MovingInv" => 32,                  // u64, which the AVX2 baseline vectorises
            "Mem-ModuloX" => 64,                    // Whole lines
            "Mem-Sequence" => 64,                   // Native lines at 256 or 512 bits
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
            | "Bench-Init-TMR-10" | "Bench-Init-TMR-11" | "Bench-Init-TMR-13" | "Bench-Init-TMR-20" | "Bench-Init-TMR-30"
            | "Bench-Verify-TM5-0" | "Bench-Verify-TM5-1" | "Bench-Verify-TM5-2"
            | "Bench-Verify-TMR-10" | "Bench-Verify-TMR-11" | "Bench-Verify-TMR-13" | "Bench-Verify-TMR-20"
            | "Bench-Verify-TMR-30" => 8,
            // The seal kernels take 64 B-aligned ranges at every width (TODO 74)
            s if s.starts_with("Bench-Init-Seal-") || s.starts_with("Bench-Verify-Seal-") => 64,
            // The native Bench pairs run the widest native SimpleTest (pattern spec v2)
            s if native_bench_tests().iter().any(|(init, verify, _, _)| s == *init || s == *verify) => 64,
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

/// Under `errors=halt`, ends a loop-owning test where it is once it has found errors (its own, or
/// the seal check's before its chunk), before it writes over them: the bad words stay in memory
/// (TODO 27). `$ops` is the test's operation count so far. Expands to an early return.
macro_rules! halt_here {
    ($runner:expr, $cycle_errors:expr, $ops:expr) => {
        if $runner.should_halt($cycle_errors) {
            let total_operations = $ops;
            return $runner.finish_aborted($cycle_errors, total_operations);
        }
    };
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
                    halt_here!(runner, cycle_errors, (runner.bytes_processed() / lanes) as u64);
                    stuck_bit_write_verify!($simd_type, base, processed, chunk_end, p1, &mut cycle_errors, test_name, thread_id, 1, flush_before_verify, line_bytes);
                    halt_here!(runner, cycle_errors, (runner.bytes_processed() / lanes) as u64);
                    stuck_bit_write_verify!($simd_type, base, processed, chunk_end, p2, &mut cycle_errors, test_name, thread_id, 2, flush_before_verify, line_bytes);
                    halt_here!(runner, cycle_errors, (runner.bytes_processed() / lanes) as u64);
                    stuck_bit_write_verify!($simd_type, base, processed, chunk_end, p1, &mut cycle_errors, test_name, thread_id, 3, flush_before_verify, line_bytes);
                    // Count the chunk when it is done; overlaps count each time
                    runner.add_bytes(spread.chunk() * 6);
                    halt_here!(runner, cycle_errors, (runner.bytes_processed() / lanes) as u64);
                    runner.reseal(spread.start(k), spread.chunk());

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
        // No fence (TODO 16): this thread reads its own cached writes in order; the optional flush ends in its own MFENCE

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
            let phase = format!("phase {}", $phase);
            *$errs += rescan_words_in($base as *const u64, $start * words, $end * words, $test_name, acc.reduce_or(), &phase, |_| pat);
        }
    }};
}

/// One constant over a chunk, for the scalar Mem-StuckBit and Mem-Refresh: words `start..end`
/// from `base`, each `pattern`. The write and the verify are functions of their own so their loops
/// walk a pointer, each access at it plus a fixed displacement (TODO 99): in the test functions they
/// addressed every word through an index.
///
/// Both loops run while the pointer is below their last step's start, rather than for a count,
/// and that is what keeps their shape: with a count the compiler vectorizes them itself, the
/// verify into eight vector accumulators that spill. Compiled this way they get the shape the
/// test functions had (the source is scalar; the baseline's 256-bit registers take it): the write
/// four 32 B stores a step, the verify its four accumulators as the four lanes of one register.
#[derive(Clone, Copy)]
struct ConstChunk {
    base: *mut u64,
    start: usize,
    end: usize,
    pattern: u64,
}

impl ConstChunk {
    /// Fills the chunk with the pattern: 16 words a step, then a word at a time.
    #[inline(never)]
    unsafe fn write(self) {
        let ConstChunk { base, start, end, pattern } = self;
        let mut p = base.add(start);
        let stop = base.wrapping_add(end);
        let last = base.wrapping_add(end.saturating_sub(15));
        while p < last {
            for k in 0..16 {
                *p.add(k) = pattern;
            }
            p = p.wrapping_add(16);
        }
        while p < stop {
            *p = pattern;
            p = p.wrapping_add(1);
        }
    }

    /// The four accumulators ORed: nonzero when a word differs. Four words a step, word `k` of each
    /// into accumulator `k`, then the rest into the first, as `verify_words`.
    #[inline(never)]
    unsafe fn verify(self) -> u64 {
        let ConstChunk { base, start, end, pattern } = self;
        let mut p = base.add(start) as *const u64;
        let stop = base.wrapping_add(end) as *const u64;
        let last = base.wrapping_add(end.saturating_sub(3)) as *const u64;
        let mut acc = [0u64; 4];
        while p < last {
            for (k, a) in acc.iter_mut().enumerate() {
                *a |= *p.add(k) ^ pattern;
            }
            p = p.wrapping_add(4);
        }
        while p < stop {
            acc[0] |= *p ^ pattern;
            p = p.wrapping_add(1);
        }
        (acc[0] | acc[1]) | (acc[2] | acc[3])
    }

    /// The verify, then the cold rescan when it saw a difference: the errors, as `verify_words`.
    #[inline(always)]
    unsafe fn check(self, test_name: &str) -> u64 {
        let seen = self.verify();
        if seen == 0 {
            return 0;
        }
        let pattern = self.pattern;
        rescan_words(self.base, self.start, self.end, test_name, seen, |_| pattern)
    }
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
            halt_here!(runner, cycle_errors, (runner.bytes_processed() / std::mem::size_of::<u64>()) as u64);
            let phase = |pattern| ConstChunk { base, start: processed, end: chunk_end, pattern };

            // Phase 1: Write P1 (0xAA55...), verify
            phase(pattern1).write();

            flush_chunk_if_enabled!();

            cycle_errors += phase(pattern1).check(test_name);
            halt_here!(runner, cycle_errors, (runner.bytes_processed() / std::mem::size_of::<u64>()) as u64);

            // Phase 2: Write P2 (0x55AA..., = !P1), verify
            phase(pattern2).write();

            flush_chunk_if_enabled!();

            cycle_errors += phase(pattern2).check(test_name);
            halt_here!(runner, cycle_errors, (runner.bytes_processed() / std::mem::size_of::<u64>()) as u64);

            // Phase 3: Write back to P1, verify
            phase(pattern1).write();

            flush_chunk_if_enabled!();

            cycle_errors += phase(pattern1).check(test_name);
            // Count the chunk when it is done; overlaps count each time
            runner.add_bytes(spread.chunk() * 6);
            halt_here!(runner, cycle_errors, (runner.bytes_processed() / std::mem::size_of::<u64>()) as u64);
            runner.reseal(spread.start(k), spread.chunk());

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
        return TestStats::not_run(test_name, TestAction::StuckBitTest, thread_id, timing.cycles);
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
        return TestStats::not_run(test_name, TestAction::StuckBitTest, thread_id, timing.cycles);
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
        return TestStats::not_run(test_name, TestAction::StuckBitTest, thread_id, timing.cycles);
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
                    halt_here!(runner, cycle_errors, (runner.bytes_processed() / lanes) as u64);

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

                    // No fence (TODO 16): the flush, when on, ends in its own MFENCE; the sleep is the wait
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
                                                     test_name, acc.reduce_or(), |_| REFRESH_PATTERN);
                    }
                    // Count the chunk when it is done; overlaps count each time
                    runner.add_bytes(spread.chunk() * 2);
                    halt_here!(runner, cycle_errors, (runner.bytes_processed() / lanes) as u64);
                    runner.reseal(spread.start(k), spread.chunk());

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
            halt_here!(runner, cycle_errors, (runner.bytes_processed() / std::mem::size_of::<u64>()) as u64);

            // Non-byte-uniform (see REFRESH_PATTERN / #61): stays a real store loop,
            // not memset.
            let chunk = ConstChunk { base, start: processed, end: chunk_end, pattern: REFRESH_PATTERN };
            chunk.write();

            // Flush the written chunk to DRAM so the verify after the sleep
            // round-trips through DRAM instead of reading a cache-resident
            // copy. Without this the refresh/bit-fade test is silently inert:
            // the cached line masks any decay that happened in DRAM.
            let chunk_ptr = base.add(processed) as *const u8;
            let chunk_bytes = (chunk_end - processed) * std::mem::size_of::<u64>();
            flush_range_to_dram(chunk_ptr, chunk_bytes, config.cache_line_bytes);

            // No fence (TODO 16): the flush ends in its own MFENCE; the sleep is the wait
            std::thread::sleep(std::time::Duration::from_millis(64)); // DRAM refresh cycle timing

            // Verify phase
            cycle_errors += chunk.check(test_name);
            // Count the chunk when it is done; overlaps count each time
            runner.add_bytes(spread.chunk() * 2);
            halt_here!(runner, cycle_errors, (runner.bytes_processed() / std::mem::size_of::<u64>()) as u64);
            runner.reseal(spread.start(k), spread.chunk());

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
        return TestStats::not_run(test_name, TestAction::WriteWaitVerify, thread_id, timing.cycles);
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
        return TestStats::not_run(test_name, TestAction::WriteWaitVerify, thread_id, timing.cycles);
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
        return TestStats::not_run(test_name, TestAction::WriteWaitVerify, thread_id, timing.cycles);
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

/// Mem-CacheBust's walk: a page apart, in words.
const CACHE_BUST_WORDS: usize = CACHE_BUSTING_STRIDE / std::mem::size_of::<u64>();

/// One Mem-CacheBust pass over a chunk: from each offset in its first page, every word a page on,
/// each word its variant's pattern plus its index. The variant follows from the word's index, a
/// page round, so a word's value is position-pure. The write and the verify are functions of their
/// own so their loops keep every value in registers (TODO 99): in the test's function the write
/// reloaded its pattern and the verify a vector constant every iteration. A call a chunk.
///
/// Both walk a pointer, so each access is that register plus a fixed displacement. Every access
/// misses L1 (a page apart, all in one set), and there an index serving the address and the value
/// too, one instruction fewer, ran 6% slower (TODO 99 round 2).
#[derive(Clone, Copy)]
struct CacheBustChunk {
    base: *mut u64,
    start: usize,
    end: usize,
    pattern_base: u64,
    stride_patterns: usize,
}

impl CacheBustChunk {
    /// The pattern of the variant word `i` is in (with one pattern, the base).
    #[inline(always)]
    fn pattern_at(self, i: usize) -> u64 {
        let variant = ((i % CACHE_BUST_WORDS) % self.stride_patterns) as u64;
        self.pattern_base.wrapping_add(variant.wrapping_mul(0x1111111111111111))
    }

    /// The words from offset `first` to the chunk's end, a page apart.
    #[inline(always)]
    fn walk_len(self, first: usize) -> usize {
        (self.end - first).div_ceil(CACHE_BUST_WORDS)
    }

    #[inline(never)]
    unsafe fn write(self) {
        for offset in 0..CACHE_BUST_WORDS.min(self.end - self.start) {
            let first = self.start + offset;
            let mut value = self.pattern_at(first).wrapping_add(first as u64);
            let mut p = self.base.add(first);
            for _ in 0..self.walk_len(first) {
                *p = value;
                value = value.wrapping_add(CACHE_BUST_WORDS as u64);
                p = p.wrapping_add(CACHE_BUST_WORDS);
            }
        }
    }

    /// The accumulators ORed: nonzero when a word differs. Each step reads 16 words a page apart, as
    /// four vectors of four, into four vector accumulators, and steps a vector of their expected
    /// values: the shape the compiler gave the inlined loop. Written as vectors because from four
    /// scalar chains the compiler vectorizes it itself, eight accumulators wide, and spills five of
    /// its constants.
    #[inline(never)]
    unsafe fn verify(self) -> u64 {
        const W: usize = CACHE_BUST_WORDS;
        let lanes = u64x4::from_array([0, W as u64, 2 * W as u64, 3 * W as u64]);
        let (quarter, step) = (u64x4::splat(4 * W as u64), u64x4::splat(16 * W as u64));
        let mut acc = [u64x4::splat(0); 4];
        let mut tail = 0u64;
        for offset in 0..W.min(self.end - self.start) {
            let first = self.start + offset;
            let mut value = self.pattern_at(first).wrapping_add(first as u64);
            let mut expected = u64x4::splat(value) + lanes;
            let mut p = self.base.add(first) as *const u64;
            let len = self.walk_len(first);
            for _ in 0..len / 16 {
                let mut e = expected;
                for (k, a) in acc.iter_mut().enumerate() {
                    let q = p.add(4 * k * W);
                    let words = u64x4::from_array([*q, *q.add(W), *q.add(2 * W), *q.add(3 * W)]);
                    *a |= words ^ e;
                    e += quarter;
                }
                expected += step;
                p = p.wrapping_add(16 * W);
            }
            value = value.wrapping_add((len / 16 * 16 * W) as u64);
            for _ in 0..len % 16 {
                tail |= *p ^ value;
                value = value.wrapping_add(W as u64);
                p = p.wrapping_add(W);
            }
        }
        ((acc[0] | acc[1]) | (acc[2] | acc[3])).reduce_or() | tail
    }
}

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

    // Each word holds the constant plus its id (`pattern::word_base`), so no two threads' words
    // match (TODO 97)
    let thread_base = 0x0123456789ABCDEFu64.wrapping_add(crate::pattern::word_base(thread_id));

    let (mut runner, extent) = crate::test_scaffolding::TestRunner::new(
        blocks, thread_id, error_mode, timing, config, progress,
        test_name, TestAction::CacheBusting,
    );

    loop {
        let cycle = runner.begin_cycle();
        let mut cycle_errors = 0u64;
        // A cycle's values differ from the last's, so a write that didn't land shows even unsealed
        let pattern_base = thread_base.wrapping_add(u64::from(cycle));

        let base = extent.ptr as *mut u64;

        // Chunks of exactly the test's size, spread evenly over the extent (TODO 76)
        let spread = runner.chunks(extent.test_size);
        let chunk_size_operations = spread.chunk() / std::mem::size_of::<u64>();

        for k in 0..spread.count() {
            let processed = spread.start(k) / std::mem::size_of::<u64>();
            let chunk_end = processed + chunk_size_operations;

            runner.check_seal(spread.start(k), spread.chunk());
            halt_here!(runner, cycle_errors, (runner.bytes_processed() / std::mem::size_of::<u64>()) as u64);

            // Every word from each offset in the chunk's first page, a page apart, so each write
            // and read lands in a different page; the walks cover every word
            let chunk = CacheBustChunk { base, start: processed, end: chunk_end, pattern_base, stride_patterns };
            chunk.write();
            let seen = chunk.verify();
            if seen != 0 {
                let expected = |i: usize| chunk.pattern_at(i).wrapping_add(i as u64);
                cycle_errors += rescan_words(base, processed, chunk_end, test_name, seen, expected);
            }
            // Count the chunk when it is done; overlaps count each time
            runner.add_bytes(spread.chunk() * 2);
            halt_here!(runner, cycle_errors, (runner.bytes_processed() / std::mem::size_of::<u64>()) as u64);
            runner.reseal(spread.start(k), spread.chunk());

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

/// Mem-Random's extent as its reads see it: word `idx` holds `ids | idx`, its word id.
#[derive(Clone, Copy)]
struct RandomExtent {
    base: *const u64,
    len: usize,
    ids: u64,
}

/// Words a step of Mem-Random's sweep check: four vectors of four, one per accumulator.
const RANDOM_SWEEP_STEP: usize = 16;

impl RandomExtent {
    /// The RNG's next step and the index it picks: `rng * len >> 64`, uniform over any length, one
    /// multiply off the RNG's dependency chain (TODO 76; the old `& (len - 1)` needed a power of
    /// two). `random_replay` takes the same steps.
    #[inline(always)]
    fn pick(self, rng: &mut u64) -> usize {
        *rng ^= *rng << 13;
        *rng ^= *rng >> 17;
        *rng ^= *rng << 5;
        ((*rng as u128 * self.len as u128) >> 64) as usize
    }

    /// `n` random reads from RNG state `rng`, each word checked against its id. Returns the
    /// differences ORed, four reads a step into four accumulators, and the RNG's state after. A
    /// function of its own so its loop keeps every value in registers (TODO 99), called once a
    /// batch. The index is random, so each read is the base and an index, not a walked pointer.
    #[inline(never)]
    unsafe fn reads(self, mut rng: u64, n: usize) -> (u64, u64) {
        let RandomExtent { base, ids, .. } = self;
        let mut acc = [0u64; 4];
        for _ in 0..n / 4 {
            for a in acc.iter_mut() {
                let idx = self.pick(&mut rng);
                *a |= *base.add(idx) ^ (ids | idx as u64);
            }
        }
        for _ in 0..n % 4 {
            let idx = self.pick(&mut rng);
            acc[0] |= *base.add(idx) ^ (ids | idx as u64);
        }
        ((acc[0] | acc[1]) | (acc[2] | acc[3]), rng)
    }

    /// The final sweep's check of words `[from, to)`, in order: 16 words a step as four `u64x4`
    /// (the AVX2 baseline's width, which the compiler gave it) into four accumulators, on a walked
    /// pointer with the indices carried, then a word at a time. Returns the differences ORed. A
    /// function of its own (TODO 99): inlined, the compiler made five loops of it, reloading the
    /// lanes' offsets from the stack or shuffling the words into the accumulators' order.
    #[inline(never)]
    unsafe fn verify(self, from: usize, to: usize) -> u64 {
        let RandomExtent { base, ids, .. } = self;
        // The words past the whole steps are volatile, so they stay a word at a time: as a plain
        // loop the compiler folded it into masked vector loads (a sweep's pieces are whole steps)
        let (steps, rest) = ((to - from) / RANDOM_SWEEP_STEP, (to - from) % RANDOM_SWEEP_STEP);
        let ids_v = u64x4::splat(ids);
        let (q1, q2, q3, step) = (u64x4::splat(4), u64x4::splat(8), u64x4::splat(12), u64x4::splat(RANDOM_SWEEP_STEP as u64));
        let mut idx = u64x4::splat(from as u64) + u64x4::from_array([0, 1, 2, 3]);
        let mut acc = [u64x4::splat(0); 4];
        let mut p = base.add(from);
        for _ in 0..steps {
            let v = p as *const u64x4;
            let idxs = [idx, idx + q1, idx + q2, idx + q3];
            for (k, a) in acc.iter_mut().enumerate() {
                *a |= v.add(k).read_unaligned() ^ (ids_v | idxs[k]);
            }
            idx += step;
            p = p.wrapping_add(RANDOM_SWEEP_STEP);
        }
        let mut tail = 0u64;
        let whole = from + steps * RANDOM_SWEEP_STEP;
        for i in whole..whole + rest {
            tail |= p.read_volatile() ^ (ids | i as u64);
            p = p.wrapping_add(1);
        }
        ((acc[0] | acc[1]) | (acc[2] | acc[3])).reduce_or() | tail
    }
}

/// Cold path of Mem-Random: rerun one chunk's reads from its saved RNG state, counting and logging
/// each mismatch. A fault that was gone by the reread still counts once. Random reads can land on
/// a word twice: a bad word counts once, and its repeat reads are counted in the record.
#[cold]
#[inline(never)]
unsafe fn random_replay(extent: RandomExtent, mut rng_state: u64, iterations: std::ops::Range<usize>,
                        seq: u32, test_name: &str, seen: u64) -> u64 {
    let RandomExtent { base, ids, .. } = extent;
    let (first, last) = (iterations.start, iterations.end);
    if !crate::pattern::rescan_enabled() {
        return unlocalised(found_by(test_name), format_args!("in rng_seq {seq}, iterations {first}..{last}"), seen);
    }
    let mut scan = Rescan::for_test(base, test_name).seen(seen);
    // The words found bad so far in this replay, and the reads of them since
    let mut found = std::collections::HashSet::new();
    let mut repeats = 0u64;
    for _ in iterations {
        let idx = extent.pick(&mut rng_state);
        if found.contains(&idx) {
            repeats += 1;
            continue;
        }
        let before = scan.found();
        // SAFETY: idx < len, inside the extent
        unsafe { scan.word(idx, ids | idx as u64) };
        if scan.found() > before {
            found.insert(idx);
        }
    }
    let repeats = match repeats {
        0 => String::new(),
        n => format!(" ({n} more reads of its bad words, not counted again)"),
    };
    scan.finish(format_args!("in rng_seq {seq}, iterations {first}..{last}{repeats}"))
}

/// Mem-Random's final sweep (TODO 97): every word of the extent read once in order, `piece` words
/// a check, stopping when `stop(errors so far)` says to. Returns the errors and the words it got
/// through.
unsafe fn random_sweep(extent: RandomExtent, piece: usize, test_name: &str, mut stop: impl FnMut(u64) -> bool) -> (u64, usize) {
    let RandomExtent { base, len, ids } = extent;
    let mut errors = 0;
    let mut at = 0;
    while at < len {
        let end = (at + piece).min(len);
        let seen = extent.verify(at, end);
        if seen != 0 {
            errors += rescan_words_in(base, at, end, test_name, seen, "final sweep", |i| ids | i as u64);
        }
        at = end;
        if stop(errors) {
            break;
        }
    }
    (errors, at)
}

/// RandomTorture MultiBlock implementation - random access pattern testing.
///
/// Pattern per cycle:
/// - Initializes memory with each word's id (the thread's and its index, `pattern::word_base`)
/// - Performs random reads using XORshift RNG
/// - Verifies each random read matches expected value
/// - Supports multiple RNG sequences with different seeds
///
/// After the last cycle, a final sweep reads every word once in order (TODO 97).
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

    // Initialize the extent once, each word with its id: no two threads' words alike (TODO 97)
    let base = extent.ptr as *mut u64;
    let len = extent.test_size / std::mem::size_of::<u64>();
    let ids = crate::pattern::word_base(thread_id);
    let reads = RandomExtent { base, len, ids };

    for i in 0..len {
        *base.add(i) = ids | i as u64;
    }
    // Setup, not the test: its own record (TODO 90)
    runner.setup_done(crate::ops::Op::PatternFill, extent.test_size as u64);

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

            // Random read verification for this sequence - chunked for responsive shutdown
            // (`RandomExtent::pick` picks each index). Mismatches OR into the accumulators; a
            // nonzero one replays the chunk to count and log each bad word, so the loop has no
            // branch and no logging.
            for chunk_start in (0..iterations_per_seq).step_by(chunk_size_operations) {
                let chunk_end = (chunk_start + chunk_size_operations).min(iterations_per_seq);
                let chunk_rng = rng_state;
                let acc;
                (acc, rng_state) = reads.reads(rng_state, chunk_end - chunk_start);

                if acc != 0 {
                    cycle_errors += random_replay(reads, chunk_rng, chunk_start..chunk_end, seq, test_name, acc);
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

            // The final sweep (TODO 97): the reads sample a few thousand words a cycle, and the
            // fill has sat since the test began, so every word is read once, in order, a chunk at
            // a time: a bad word the reads missed shows, and one that decayed over the test
            let piece = (runner.chunk_bytes() / std::mem::size_of::<u64>()).max(1);
            let (sweep_errors, swept) = random_sweep(reads, piece, test_name, |errors| {
                runner.should_halt(errors) || runner.shutdown_requested()
            });
            if swept < len {
                return runner.finish_aborted(sweep_errors, total_operations + swept as u64);
            }
            runner.add_bytes(extent.test_size);
            runner.commit_cycle_errors(sweep_errors);
            return runner.finish_completed(total_operations + len as u64);
        }
    }
}

// ================================================================================================
// StrideAccess, BandwidthSat, BlockMove MultiBlock Implementations
// ================================================================================================

/// One Mem-Stride pass over a chunk: each subdivision from its start, every `stride` words, each
/// word the pattern plus its index. The write and the verify are functions of their own so their
/// loops keep every value in registers (TODO 99): in the test's function, with its passes and
/// chunks live too, the allocator kept the stride on the stack. A call a chunk, not a word.
///
/// Each loop walks a pointer, every access at it plus a fixed displacement, the expected value
/// carried beside it, and runs while the pointer is below its last step's start rather than for a
/// count (TODO 99 round 3). With a count the compiler vectorizes the strided verify itself, eight
/// vector accumulators wide with five constants spilled, and turns the walks back into indexes.
#[derive(Clone, Copy)]
struct StrideWalk {
    base: *mut u64,
    chunk_start: usize,
    subdivisions: usize,
    elements_per_subdiv: usize,
    stride: usize,
    pattern: u64,
}

impl StrideWalk {
    /// Each of the test's strides a compile-time constant, as `verify`.
    #[inline]
    unsafe fn write(self) {
        match self.stride {
            1 => self.write_strided::<1>(),
            16 => self.write_strided::<16>(),
            64 => self.write_strided::<64>(),
            256 => self.write_strided::<256>(),
            1024 => self.write_strided::<1024>(),
            4096 => self.write_strided::<4096>(),
            _ => self.write_strided::<0>(),
        }
    }

    /// `write` at stride `S`, or at `self.stride` when `S` is 0: a word a step.
    #[inline(never)]
    unsafe fn write_strided<const S: usize>(self) {
        let StrideWalk { base, chunk_start, subdivisions, elements_per_subdiv, stride, pattern } = self;
        let stride = if S == 0 { stride } else { S };
        for subdiv in 0..subdivisions {
            let start = chunk_start + subdiv * elements_per_subdiv;
            let mut p = base.add(start);
            let stop = base.wrapping_add(start + elements_per_subdiv);
            let mut value = pattern.wrapping_add(start as u64);
            while p < stop {
                *p = value;
                p = p.wrapping_add(stride);
                value = value.wrapping_add(stride as u64);
            }
        }
    }

    /// The four accumulators ORed: nonzero when a word differs. Each of the test's strides is a
    /// compile-time constant here, so the four loads a stride apart are one pointer and fixed
    /// displacements, and the four expected values one value and fixed offsets: with the four
    /// accumulators, every value fits in registers (at a runtime stride two didn't).
    #[inline]
    unsafe fn verify(self) -> u64 {
        match self.stride {
            1 => self.verify_unit(),
            16 => self.verify_strided::<16>(),
            64 => self.verify_strided::<64>(),
            256 => self.verify_strided::<256>(),
            1024 => self.verify_strided::<1024>(),
            4096 => self.verify_strided::<4096>(),
            _ => self.verify_any(),
        }
    }

    /// `verify` at a stride the test doesn't use, so a runtime one: the strided accumulate.
    #[inline(never)]
    unsafe fn verify_any(self) -> u64 {
        let StrideWalk { base, chunk_start, subdivisions, elements_per_subdiv, stride, pattern } = self;
        let expected = move |pos: usize| pattern.wrapping_add(pos as u64);
        let mut acc = [0u64; 4];
        for subdiv in 0..subdivisions {
            let start = chunk_start + subdiv * elements_per_subdiv;
            accumulate_stride_4(&mut acc, base, start, start + elements_per_subdiv, stride, expected);
        }
        (acc[0] | acc[1]) | (acc[2] | acc[3])
    }

    /// `verify` at stride `S`: four words a stride apart a step, word `k` into accumulator `k`,
    /// then the rest into the first, as `accumulate_stride_4`.
    #[inline(never)]
    unsafe fn verify_strided<const S: usize>(self) -> u64 {
        let StrideWalk { base, chunk_start, subdivisions, elements_per_subdiv, pattern, .. } = self;
        let stride = S;
        let mut acc = [0u64; 4];
        for subdiv in 0..subdivisions {
            let start = chunk_start + subdiv * elements_per_subdiv;
            let mut p = base.add(start) as *const u64;
            let stop = base.wrapping_add(start + elements_per_subdiv) as *const u64;
            let last = base.wrapping_add(start + elements_per_subdiv.saturating_sub(3 * stride)) as *const u64;
            let mut value = pattern.wrapping_add(start as u64);
            while p < last {
                for (k, a) in acc.iter_mut().enumerate() {
                    *a |= *p.add(k * stride) ^ value.wrapping_add((k * stride) as u64);
                }
                p = p.wrapping_add(4 * stride);
                value = value.wrapping_add(4 * stride as u64);
            }
            while p < stop {
                acc[0] |= *p ^ value;
                p = p.wrapping_add(stride);
                value = value.wrapping_add(stride as u64);
            }
        }
        (acc[0] | acc[1]) | (acc[2] | acc[3])
    }

    /// `verify` at stride 1, where the words are contiguous: 16 a step, as four vectors of four,
    /// one into each accumulator, each against its own vector of expected values (so no constant
    /// is reloaded); then the rest a word at a time. Explicit vectors, at the baseline's 256 bits
    /// (Mem-Stride has no width variants): from four scalar chains the compiler built a mix of
    /// scalar and 128-bit loads through an index, four words a step.
    #[inline(never)]
    unsafe fn verify_unit(self) -> u64 {
        let StrideWalk { base, chunk_start, subdivisions, elements_per_subdiv, pattern, .. } = self;
        let step = u64x4::splat(16);
        let mut acc = [u64x4::splat(0); 4];
        let mut tail = 0u64;
        for subdiv in 0..subdivisions {
            let start = chunk_start + subdiv * elements_per_subdiv;
            let mut p = base.add(start) as *const u64;
            let stop = base.wrapping_add(start + elements_per_subdiv) as *const u64;
            let last = base.wrapping_add(start + elements_per_subdiv.saturating_sub(15)) as *const u64;
            let first = u64x4::splat(pattern.wrapping_add(start as u64)) + u64x4::from_array([0, 1, 2, 3]);
            let mut expected: [u64x4; 4] = std::array::from_fn(|k| first + u64x4::splat(4 * k as u64));
            while p < last {
                for (k, a) in acc.iter_mut().enumerate() {
                    *a |= std::ptr::read_unaligned(p.add(4 * k) as *const u64x4) ^ expected[k];
                    expected[k] += step;
                }
                p = p.wrapping_add(16);
            }
            let mut value = expected[0][0];
            while p < stop {
                tail |= *p ^ value;
                value = value.wrapping_add(1);
                p = p.wrapping_add(1);
            }
        }
        ((acc[0] | acc[1]) | (acc[2] | acc[3])).reduce_or() | tail
    }
}

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
        // Each word holds the constant plus its id (`pattern::word_base`), so no two threads' words
        // match (TODO 97), plus the cycle
        let pattern_base = 0xFEDCBA9876543210u64.wrapping_add(crate::pattern::word_base(thread_id)).wrapping_add(cycle as u64);

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
            // A word's value depends on its index and the pass, not on the chunk: the pass in the
            // top byte, above every thread's ids
            let pattern = pattern_base.wrapping_add((pass as u64 + 1) << 56);
            let mut checked_to = 0;

            for k in 0..spread.count() {
                let chunk_start = spread.start(k) / std::mem::size_of::<u64>();
                if pass == 0 {
                    let from = spread.start(k).max(checked_to);
                    runner.check_seal(from, spread.start(k) + spread.chunk() - from);
                    halt_here!(runner, cycle_errors, (runner.bytes_processed() / (2 * std::mem::size_of::<u64>())) as u64);
                    checked_to = spread.start(k) + spread.chunk();
                }

                let walk = StrideWalk { base, chunk_start, subdivisions, elements_per_subdiv, stride, pattern };
                walk.write();
                let seen = walk.verify();
                let expected = |pos: usize| pattern.wrapping_add(pos as u64);
                if seen != 0 {
                    let chunk_end = chunk_start + subdivisions * elements_per_subdiv;
                    cycle_errors += if crate::pattern::rescan_enabled() {
                        let mut scan = Rescan::for_test(base, test_name).seen(seen);
                        for subdiv in 0..subdivisions {
                            let subdiv_start = chunk_start + subdiv * elements_per_subdiv;
                            scan.stride(subdiv_start, subdiv_start + elements_per_subdiv, stride, expected);
                        }
                        scan.finish(format_args!("in idx {chunk_start}..{chunk_end} (stride {stride})"))
                    } else {
                        unlocalised(found_by(test_name), format_args!("in idx {chunk_start}..{chunk_end} (stride {stride})"), seen)
                    };
                }
                // Count the chunk when it is done (one write and one read per word touched)
                runner.add_bytes(touched * std::mem::size_of::<u64>() * 2);
                halt_here!(runner, cycle_errors, (runner.bytes_processed() / (2 * std::mem::size_of::<u64>())) as u64);
                if pass + 1 == passes.len() {
                    runner.reseal(spread.start(k), spread.chunk());
                }

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

/// One Mem-BlockMove half-chunk: the source's words `[start, end)` copied to the same offsets of
/// the destination half, in the test's direction pattern, then checked there, each word the
/// pattern plus its index. The copies and the verify are functions of their own so their loops
/// keep every value in registers (TODO 99): in the test's function a backward copy reloaded its
/// bound, and the verify a vector constant, every iteration. A call a half-chunk.
///
/// Each walks a pointer, every access at it plus a fixed displacement. The vector copies and the
/// verify move 16 words a step as four 32 B vectors, the shape the compiler gave the inlined loops
/// at the `x86-64-v3` baseline; the strided copies move one word a step, as written.
#[derive(Clone, Copy)]
struct BlockMoveChunk {
    src: *mut u64,
    dst: *mut u64,
    start: usize,
    end: usize,
    pattern_base: u64,
}

impl BlockMoveChunk {
    /// Word `i`'s value in either half.
    #[inline(always)]
    fn expected(self, i: usize) -> u64 {
        self.pattern_base.wrapping_add(i as u64)
    }

    /// The copy of `[start, end)` for `dirs` directions: 1 up; 2 the first half up and the second
    /// down; 4 four quarters, up, down, even words then odd, up; any other count `dirs`
    /// interleaved strided passes, each up.
    #[inline(always)]
    unsafe fn copy(self, dirs: u32) {
        let (start, end) = (self.start, self.end);
        match dirs {
            1 => self.copy_up(start, end),
            2 => {
                let mid = start + (end - start) / 2;
                self.copy_up(start, mid);
                self.copy_down(mid, end);
            }
            4 => {
                let block = (end - start) / 4;
                for stream in 0..4 {
                    let from = start + stream * block;
                    let to = (start + (stream + 1) * block).min(end);
                    if from < to {
                        match stream {
                            1 => self.copy_down(from, to),
                            2 => self.copy_strided::<2>(from, to, 2),
                            _ => self.copy_up(from, to),
                        }
                    }
                }
            }
            dirs => self.copy_strided::<0>(start, end, dirs as usize),
        }
    }

    /// Words `[from, to)` copied up, 16 a step: four vectors read, then the four written.
    #[inline(never)]
    unsafe fn copy_up(self, from: usize, to: usize) {
        let (mut s, mut d) = (self.src.add(from) as *const u64, self.dst.add(from));
        for _ in 0..(to - from) / 16 {
            let v0 = s.cast::<u64x4>().read_unaligned();
            let v1 = s.add(4).cast::<u64x4>().read_unaligned();
            let v2 = s.add(8).cast::<u64x4>().read_unaligned();
            let v3 = s.add(12).cast::<u64x4>().read_unaligned();
            d.cast::<u64x4>().write_unaligned(v0);
            d.add(4).cast::<u64x4>().write_unaligned(v1);
            d.add(8).cast::<u64x4>().write_unaligned(v2);
            d.add(12).cast::<u64x4>().write_unaligned(v3);
            s = s.wrapping_add(16);
            d = d.wrapping_add(16);
        }
        for _ in 0..(to - from) % 16 {
            *d = *s;
            s = s.wrapping_add(1);
            d = d.wrapping_add(1);
        }
    }

    /// Words `[from, to)` copied down from the top, 16 a step, the step's four vectors from its
    /// top too; the words below the last whole step one at a time, down.
    #[inline(never)]
    unsafe fn copy_down(self, from: usize, to: usize) {
        let (mut s, mut d) = (self.src.add(to) as *const u64, self.dst.add(to));
        for _ in 0..(to - from) / 16 {
            s = s.wrapping_sub(16);
            d = d.wrapping_sub(16);
            let v3 = s.add(12).cast::<u64x4>().read_unaligned();
            let v2 = s.add(8).cast::<u64x4>().read_unaligned();
            let v1 = s.add(4).cast::<u64x4>().read_unaligned();
            let v0 = s.cast::<u64x4>().read_unaligned();
            d.add(12).cast::<u64x4>().write_unaligned(v3);
            d.add(8).cast::<u64x4>().write_unaligned(v2);
            d.add(4).cast::<u64x4>().write_unaligned(v1);
            d.cast::<u64x4>().write_unaligned(v0);
        }
        for _ in 0..(to - from) % 16 {
            s = s.wrapping_sub(1);
            d = d.wrapping_sub(1);
            *d = *s;
        }
    }

    /// Words `[from, to)` copied in `stride` passes, pass `k` the words `from + k`, `from + k +
    /// stride`, ..., each pass up. The stride is `S`, or `stride` when `S` is 0: the 4-way copy's
    /// even-then-odd quarter is a constant 2, so its accesses are fixed displacements.
    #[inline(never)]
    unsafe fn copy_strided<const S: usize>(self, from: usize, to: usize, stride: usize) {
        let stride = if S == 0 { stride } else { S };
        debug_assert!(stride > 0, "copy_directions is at least 1");
        for first in from..(from + stride).min(to) {
            let (mut s, mut d) = (self.src.add(first) as *const u64, self.dst.add(first));
            for _ in 0..(to - first).div_ceil(stride) {
                *d = *s;
                s = s.wrapping_add(stride);
                d = d.wrapping_add(stride);
            }
        }
    }

    /// The source's words written, each its value: a sealed step's setup for each half-chunk, an
    /// unsealed one's once for the half. Setup, so left to the vectorizer as written: in vectors
    /// of its own, the unroller gave each of 16 stores an offset and spilled two.
    #[inline(never)]
    unsafe fn fill(self) {
        let (mut p, mut value) = (self.src.add(self.start), self.expected(self.start));
        for _ in self.start..self.end {
            *p = value;
            value = value.wrapping_add(1);
            p = p.wrapping_add(1);
        }
    }

    /// The destination's words checked: the accumulators ORed, nonzero when a word differs. Each
    /// step reads 16 words as four vectors into four vector accumulators and steps a vector of
    /// their expected values; the words past the last whole step go to a scalar accumulator.
    #[inline(never)]
    unsafe fn verify(self) -> u64 {
        let lanes = u64x4::from_array([0, 1, 2, 3]);
        let (quarter, step) = (u64x4::splat(4), u64x4::splat(16));
        let mut acc = [u64x4::splat(0); 4];
        let len = self.end - self.start;
        let mut expected = u64x4::splat(self.expected(self.start)) + lanes;
        let mut p = self.dst.add(self.start) as *const u64;
        for _ in 0..len / 16 {
            let mut e = expected;
            for (k, a) in acc.iter_mut().enumerate() {
                *a |= p.add(4 * k).cast::<u64x4>().read_unaligned() ^ e;
                e += quarter;
            }
            expected += step;
            p = p.wrapping_add(16);
        }
        let mut value = self.expected(self.start + len / 16 * 16);
        let mut tail = 0u64;
        for _ in 0..len % 16 {
            tail |= *p ^ value;
            value = value.wrapping_add(1);
            p = p.wrapping_add(1);
        }
        ((acc[0] | acc[1]) | (acc[2] | acc[3])).reduce_or() | tail
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

    // Initialize the source half, each word the constant plus its id (`pattern::word_base`), so no
    // two threads' words match (TODO 97)
    let pattern_base = 0xDEADBEEFCAFEBABEu64.wrapping_add(crate::pattern::word_base(thread_id));
    // Divide the extent in half: first half = source, second half = destination
    let half_size = extent.test_size / 2;
    let src_base = extent.ptr as *mut u64;
    let len = half_size / std::mem::size_of::<u64>();

    // Initialize source with pattern; a sealed step fills each source piece after its seal check
    // instead (TODO 74)
    let sealed = runner.seal_wrap();
    if !sealed {
        BlockMoveChunk { src: src_base, dst: src_base.wrapping_add(len), start: 0, end: len, pattern_base }.fill();
        // Setup, not the test: its own record (TODO 90)
        runner.setup_done(crate::ops::Op::PatternFill, (len * std::mem::size_of::<u64>()) as u64);
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
            halt_here!(runner, cycle_errors, (runner.bytes_processed() / (3 * std::mem::size_of::<u64>())) as u64);
            let chunk = BlockMoveChunk { src: src_base, dst: dst_base, start: processed, end: chunk_end, pattern_base };
            if sealed {
                // Setup, as the unsealed fill (TODO 90)
                runner.untimed(crate::ops::Op::PatternFill, spread.chunk() as u64, || chunk.fill());
            }

            // Copy from source to destination in the direction pattern, then verify the copy
            // (indices from the destination half's start)
            chunk.copy(copy_dirs);
            let seen = chunk.verify();
            if seen != 0 {
                cycle_errors += rescan_words(dst_base, processed, chunk_end, test_name, seen, |i| chunk.expected(i));
            }
            // Count the half-chunk when it is done: copy (read + write) and verify (read)
            runner.add_bytes(spread.chunk() * 3);
            halt_here!(runner, cycle_errors, (runner.bytes_processed() / (3 * std::mem::size_of::<u64>())) as u64);
            runner.reseal(spread.start(k), spread.chunk());
            runner.reseal(half_size + spread.start(k), spread.chunk());

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

// ================================================================================================
// Moving inversions (TODO 97, TODO 19's order)
// ================================================================================================

/// `pattern::ident_word`'s `x` for word id `id` under `key`: the word is `ident_mix(x)`, and word
/// id `id + n`'s `x` is `x + n * G`, so a walk carries it and adds G a word instead of multiplying.
#[inline(always)]
fn ident_x(id: u64, key: u64) -> u64 {
    id.wrapping_mul(crate::pattern::G).wrapping_add(key)
}

/// `pattern::ident_word` from its `x`.
#[inline(always)]
fn ident_mix(x: u64) -> u64 {
    x ^ (x >> 32)
}

/// `ident_mix` on every lane.
#[inline(always)]
fn ident_mix_v(x: u64x4) -> u64x4 {
    x ^ (x >> u64x4::splat(32))
}

/// `n` steps of `ident_x`'s walk, `n * G`, in every lane.
#[inline(always)]
fn ident_steps(n: usize) -> u64x4 {
    u64x4::splat((n as u64).wrapping_mul(crate::pattern::G))
}

/// The `x`s of word ids `id .. id + 4` under `key`, a lane each.
#[inline(always)]
fn ident_x_v(id: u64, key: u64) -> u64x4 {
    let g = crate::pattern::G;
    u64x4::splat(ident_x(id, key)) + u64x4::from_array([0, g, g.wrapping_mul(2), g.wrapping_mul(3)])
}

/// Words a Mem-MovingInv step covers: four vectors of four, one per accumulator.
const MOVING_INV_STEP: usize = 16;

/// One Mem-MovingInv chunk, words `[start, end)` of `base`: word `i`'s P is `ident`'s word of its
/// id `ids + i` under `key`. Each pass is a function of its own, so its loop keeps every value in
/// registers (TODO 99): in the test's function each word reloaded `ids` and `key` through the
/// closures' references and multiplied by G. Here each pass walks a pointer, 16 words a step as
/// four `u64x4` (the AVX2 baseline's width, which the compiler gave the fill and the verify), with
/// `ident_x` carried, and a word at a time for the rest. A call a pass.
#[derive(Clone, Copy)]
struct MovingInvChunk {
    base: *mut u64,
    start: usize,
    end: usize,
    ids: u64,
    key: u64,
}

impl MovingInvChunk {
    /// The chunk's whole steps, and the words past them, which go a word at a time. Those are
    /// volatile so they stay that: as plain loops the compiler folded them into masked vector
    /// loads and stores. (A chunk is whole 4 KiB granules, so a test's never has them.)
    #[inline(always)]
    fn steps(self) -> (usize, usize) {
        let len = self.end - self.start;
        (len / MOVING_INV_STEP, len % MOVING_INV_STEP)
    }

    /// Writes P over the chunk, going up.
    #[inline(never)]
    unsafe fn fill(self) {
        let MovingInvChunk { base, start, ids, key, .. } = self;
        let (steps, rest) = self.steps();
        let (q1, q2, q3, step) = (ident_steps(4), ident_steps(8), ident_steps(12), ident_steps(MOVING_INV_STEP));
        let mut x = ident_x_v(ids.wrapping_add(start as u64), key);
        let mut p = base.add(start);
        for _ in 0..steps {
            let v = p as *mut u64x4;
            v.write_unaligned(ident_mix_v(x));
            v.add(1).write_unaligned(ident_mix_v(x + q1));
            v.add(2).write_unaligned(ident_mix_v(x + q2));
            v.add(3).write_unaligned(ident_mix_v(x + q3));
            x += step;
            p = p.wrapping_add(MOVING_INV_STEP);
        }
        let mut x = ident_x(ids.wrapping_add((start + steps * MOVING_INV_STEP) as u64), key);
        for _ in 0..rest {
            p.write_volatile(ident_mix(x));
            x = x.wrapping_add(crate::pattern::G);
            p = p.wrapping_add(1);
        }
    }

    /// A read-then-write pass: going up (`UP`), each word checked as P and written ~P; going down,
    /// checked as ~P and written P. Down starts with the words past the whole steps, from the top,
    /// then takes the steps from the top, their vectors from the top. Returns the differences ORed,
    /// in four accumulators as every verify keeps them.
    #[inline(never)]
    unsafe fn pass<const UP: bool>(self) -> u64 {
        let MovingInvChunk { base, start, end, ids, key } = self;
        let (steps, rest) = self.steps();
        let whole = start + steps * MOVING_INV_STEP;
        let (q1, q2, q3, step) = (ident_steps(4), ident_steps(8), ident_steps(12), ident_steps(MOVING_INV_STEP));
        let ones = u64x4::splat(!0);
        let mut acc = [u64x4::splat(0); 4];
        let mut tail = 0u64;
        if UP {
            let mut x = ident_x_v(ids.wrapping_add(start as u64), key);
            let mut p = base.add(start);
            for _ in 0..steps {
                let v = p as *mut u64x4;
                let xs = [x, x + q1, x + q2, x + q3];
                for (k, a) in acc.iter_mut().enumerate() {
                    let e = ident_mix_v(xs[k]);
                    *a |= v.add(k).read_unaligned() ^ e;
                    v.add(k).write_unaligned(e ^ ones);
                }
                x += step;
                p = p.wrapping_add(MOVING_INV_STEP);
            }
            let mut x = ident_x(ids.wrapping_add(whole as u64), key);
            for _ in 0..rest {
                let e = ident_mix(x);
                tail |= p.read_volatile() ^ e;
                p.write_volatile(!e);
                x = x.wrapping_add(crate::pattern::G);
                p = p.wrapping_add(1);
            }
        } else {
            let mut x = ident_x(ids.wrapping_add(end as u64), key);
            let mut p = base.wrapping_add(end);
            for _ in 0..rest {
                x = x.wrapping_sub(crate::pattern::G);
                p = p.wrapping_sub(1);
                let e = ident_mix(x);
                tail |= p.read_volatile() ^ !e;
                p.write_volatile(e);
            }
            let mut x = ident_x_v(ids.wrapping_add(whole as u64), key);
            for _ in 0..steps {
                x -= step;
                p = p.wrapping_sub(MOVING_INV_STEP);
                let v = p as *mut u64x4;
                let xs = [x, x + q1, x + q2, x + q3];
                for (k, a) in acc.iter_mut().enumerate().rev() {
                    let e = ident_mix_v(xs[k]);
                    *a |= v.add(k).read_unaligned() ^ (e ^ ones);
                    v.add(k).write_unaligned(e);
                }
            }
        }
        ((acc[0] | acc[1]) | (acc[2] | acc[3])).reduce_or() | tail
    }

    /// The last pass: each word checked as P, going up. Returns the differences ORed.
    #[inline(never)]
    unsafe fn verify(self) -> u64 {
        let MovingInvChunk { base, start, ids, key, .. } = self;
        let (steps, rest) = self.steps();
        let (q1, q2, q3, step) = (ident_steps(4), ident_steps(8), ident_steps(12), ident_steps(MOVING_INV_STEP));
        let mut acc = [u64x4::splat(0); 4];
        let mut x = ident_x_v(ids.wrapping_add(start as u64), key);
        let mut p = base.add(start) as *const u64;
        for _ in 0..steps {
            let v = p as *const u64x4;
            let xs = [x, x + q1, x + q2, x + q3];
            for (k, a) in acc.iter_mut().enumerate() {
                *a |= v.add(k).read_unaligned() ^ ident_mix_v(xs[k]);
            }
            x += step;
            p = p.wrapping_add(MOVING_INV_STEP);
        }
        let mut tail = 0u64;
        let mut x = ident_x(ids.wrapping_add((start + steps * MOVING_INV_STEP) as u64), key);
        for _ in 0..rest {
            tail |= p.read_volatile() ^ ident_mix(x);
            x = x.wrapping_add(crate::pattern::G);
            p = p.wrapping_add(1);
        }
        ((acc[0] | acc[1]) | (acc[2] | acc[3])).reduce_or() | tail
    }
}

/// The cold path of a read-then-write pass's failed check (Mem-Sequence's `RW`, Mem-MovingInv's
/// middle passes). The pass kept no copy of what it read, so first rescan what it wrote, `wrote`:
/// a cell that didn't take the write shows. If every word holds it, the word that read wrong may
/// be a cell stuck at the value the pass then wrote over it, which it holds fine (the review): so
/// write back what was read, flush it to DRAM, and rescan for that. A cell stuck shows; a read
/// that went wrong doesn't (not reproduced). Returns the errors, and whether the chunk now holds
/// `read`: the caller writes `wrote` again to go on, except under halt, which keeps what it found.
#[cold]
#[inline(never)]
unsafe fn rescan_read_write(ptr: *mut u64, range: std::ops::Range<usize>, test_name: &str, seen: u64, label: &str,
                            read: &dyn Fn(usize) -> u64, wrote: &dyn Fn(usize) -> u64) -> (u64, bool) {
    let (start, end) = (range.start, range.end);
    if !crate::pattern::rescan_enabled() {
        return (unlocalised(found_by(test_name), format_args!("in idx {start}..{end} ({label})"), seen), false);
    }
    let mut scan = Rescan::for_test(ptr, test_name).seen(seen);
    for idx in start..end {
        scan.word(idx, wrote(idx));
    }
    if scan.found() > 0 {
        return (scan.finish(format_args!("in idx {start}..{end} ({label}: what it wrote, rescanned)")), false);
    }
    for idx in start..end {
        std::ptr::write_volatile(ptr.add(idx), read(idx));
    }
    // SAFETY: the CPU gate requires CLFLUSHOPT, and the chunk is whole lines of the extent
    flush_range_to_dram(ptr.add(start) as *const u8, (end - start) * std::mem::size_of::<u64>(), 64);
    for idx in start..end {
        scan.word(idx, read(idx));
    }
    (scan.finish(format_args!("in idx {start}..{end} ({label}: what it wrote held, so what it read was written back, \
        flushed and rescanned)")), true)
}

/// Mem-MovingInv: memtest86's moving inversions, a March X over each chunk. The chunk is written
/// with P going up; then each word is read as P and written as ~P going up; then read as ~P and
/// written as P going down; then read as P. So every word is checked just before it is
/// overwritten, while the words below it hold one value and those above the other: a write that
/// disturbs other cells, or lands at another address, shows. Chunks are twice the L3 by default,
/// so each pass reaches DRAM and moves through its rows.
///
/// P is `ident`'s word of each word's id (`pattern::word_base`) under a key per step and cycle:
/// position-unique, and new every cycle. The read-then-write passes keep no copy of what a word
/// read, so their failed checks go to `rescan_read_write`. The last pass only reads, and its
/// rescan is every verify's.
#[doc = include_str!("test_fn_safety.md")]
pub unsafe fn moving_inversions_multi(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> TestStats {
    let test_name = "Mem-MovingInv";
    let (mut runner, extent) = crate::test_scaffolding::TestRunner::new(
        blocks, thread_id, error_mode, timing, config, progress,
        test_name, TestAction::ReadWrite,
    );
    let base = extent.ptr as *mut u64;
    // Word i's id is ids + i: the span holds fewer than 2^40 words, so the add is the OR
    let ids = crate::pattern::word_base(thread_id);
    // Words written or read a chunk: a write, two reads and writes, a read
    const PASSES: usize = 6;
    let ops = |runner: &crate::test_scaffolding::TestRunner| (runner.bytes_processed() / std::mem::size_of::<u64>()) as u64;

    loop {
        let cycle = runner.begin_cycle();
        let mut cycle_errors = 0u64;
        let key = crate::pattern::fmix(config.epoch.key().wrapping_add(u64::from(cycle)));
        let p = |i: usize| crate::pattern::ident_word(ids + i as u64, key);
        let not_p = |i: usize| !p(i);

        // Chunks of exactly the test's size, spread evenly over the extent (TODO 76)
        let spread = runner.chunks(extent.test_size);
        let words = spread.chunk() / std::mem::size_of::<u64>();
        for k in 0..spread.count() {
            let start = spread.start(k) / std::mem::size_of::<u64>();
            let end = start + words;
            runner.check_seal(spread.start(k), spread.chunk());
            halt_here!(runner, cycle_errors, ops(&runner));

            let chunk = MovingInvChunk { base, start, end, ids, key };
            chunk.fill();
            let seen = chunk.pass::<true>();
            if seen != 0 {
                let (errors, rewrote) = rescan_read_write(base, start..end, test_name, seen, "up: P read, ~P written", &p, &not_p);
                cycle_errors += errors;
                halt_here!(runner, cycle_errors, ops(&runner));
                if rewrote {
                    (start..end).for_each(|i| *base.add(i) = not_p(i));
                }
            }
            let seen = chunk.pass::<false>();
            if seen != 0 {
                let (errors, rewrote) = rescan_read_write(base, start..end, test_name, seen, "down: ~P read, P written", &not_p, &p);
                cycle_errors += errors;
                halt_here!(runner, cycle_errors, ops(&runner));
                if rewrote {
                    (start..end).for_each(|i| *base.add(i) = p(i));
                }
            }
            let seen = chunk.verify();
            if seen != 0 {
                cycle_errors += rescan_words_in(base, start, end, test_name, seen, "P read", p);
            }

            // Count the chunk when it is done; overlaps count each time
            runner.add_bytes(spread.chunk() * PASSES);
            halt_here!(runner, cycle_errors, ops(&runner));
            runner.reseal(spread.start(k), spread.chunk());
            if runner.should_halt(cycle_errors) || runner.shutdown_requested() {
                return runner.finish_aborted(cycle_errors, ops(&runner));
            }
        }

        runner.commit_cycle_errors(cycle_errors);
        runner.update_progress();
        if !runner.should_continue() {
            return runner.finish_completed(ops(&runner));
        }
    }
}

// ================================================================================================
// Protected victims: modulo-X (TODO 97)
// ================================================================================================

/// Mem-ModuloX's spacing: one victim line in every `MODULO_X`, memtest86's 20.
const MODULO_X: usize = 20;
/// The aggressor passes over a chunk between writing its victims and checking them.
const MODULO_PASSES: usize = 2;

/// Mem-ModuloX's victims and aggressors in lines `[start, end)` of the extent (line `l` from the
/// extent's start): line `l` is a victim when `l % MODULO_X == offset`. Calls `victim(line)` for
/// each victim and `aggressors(lines)` for each run of the lines between them, so neither has a
/// branch a line.
#[inline(always)]
fn modulo_lines(start: usize, end: usize, offset: usize, mut victim: impl FnMut(usize), mut aggressors: impl FnMut(std::ops::Range<usize>)) {
    let mut group = start / MODULO_X * MODULO_X;
    while group < end {
        let v = group + offset;
        let before = start.max(group)..end.min(v);
        let after = start.max(v + 1)..end.min(group + MODULO_X);
        if !before.is_empty() {
            aggressors(before);
        }
        if (start..end).contains(&v) {
            victim(v);
        }
        if !after.is_empty() {
            aggressors(after);
        }
        group += MODULO_X;
    }
}

/// Words from one Mem-ModuloX victim to the next.
const MODULO_APART: usize = MODULO_X * LINE_WORDS;

/// One Mem-ModuloX chunk, lines `[start, end)` of `base`: the victims and aggressors of
/// `modulo_lines`, word `i`'s P `ident`'s word of its id `ids + i` under `key`. The victims'
/// write, the aggressors' pass and the victims' check are functions of their own, so their loops
/// keep every value in registers (TODO 99): in the test's function each aggressor store redid its
/// run's bounds, and the victims' loops reloaded G and their offsets. Each walks a pointer, a line
/// as two `u64x4` (the AVX2 baseline's width, which the compiler gave them), with `ident_x`
/// carried. A call a pass.
#[derive(Clone, Copy)]
struct ModuloChunk {
    base: *mut u64,
    start: usize,
    end: usize,
    offset: usize,
    ids: u64,
    key: u64,
}

impl ModuloChunk {
    /// The first victim line at or after `start`, and the victims from it to `end`.
    #[inline(always)]
    fn victims(self) -> (usize, usize) {
        let first = self.start + (self.offset + MODULO_X - self.start % MODULO_X) % MODULO_X;
        (first, self.end.saturating_sub(first).div_ceil(MODULO_X))
    }

    /// The `x`s of line `line`'s first four words.
    #[inline(always)]
    fn line_x(self, line: usize) -> u64x4 {
        ident_x_v(self.ids.wrapping_add((line * LINE_WORDS) as u64), self.key)
    }

    /// Writes P over each victim line, going up: the first alone if their count is odd, then two a
    /// step.
    #[inline(never)]
    unsafe fn write_victims(self) {
        let (first, victims) = self.victims();
        let (half, apart, step) = (ident_steps(4), ident_steps(MODULO_APART), ident_steps(2 * MODULO_APART));
        let mut x = self.line_x(first);
        let mut p = self.base.wrapping_add(first * LINE_WORDS);
        if victims % 2 == 1 {
            let v = p as *mut u64x4;
            v.write_unaligned(ident_mix_v(x));
            v.add(1).write_unaligned(ident_mix_v(x + half));
            x += apart;
            p = p.wrapping_add(MODULO_APART);
        }
        for _ in 0..victims / 2 {
            let (v, w) = (p as *mut u64x4, p.add(MODULO_APART) as *mut u64x4);
            v.write_unaligned(ident_mix_v(x));
            v.add(1).write_unaligned(ident_mix_v(x + half));
            w.write_unaligned(ident_mix_v(x + apart));
            w.add(1).write_unaligned(ident_mix_v(x + apart + half));
            x += step;
            p = p.wrapping_add(2 * MODULO_APART);
        }
    }

    /// One aggressor pass: ~P over every other line, going up, a run of lines from each victim to
    /// the next, in `modulo_lines`'s order.
    #[inline(never)]
    unsafe fn aggress(self) {
        let ModuloChunk { base, start, end, .. } = self;
        let (half, line) = (ident_steps(4), ident_steps(LINE_WORDS));
        let ones = u64x4::splat(!0);
        let mut x = self.line_x(start);
        let mut p = base.wrapping_add(start * LINE_WORDS);
        let (mut at, mut victim) = (start, self.victims().0);
        while at < end {
            let run = victim.min(end) - at;
            for _ in 0..run {
                let v = p as *mut u64x4;
                v.write_unaligned(ident_mix_v(x) ^ ones);
                v.add(1).write_unaligned(ident_mix_v(x + half) ^ ones);
                x += line;
                p = p.wrapping_add(LINE_WORDS);
            }
            // Over the victim
            x += line;
            p = p.wrapping_add(LINE_WORDS);
            at += run + 1;
            victim += MODULO_X;
        }
    }

    /// Checks each victim line for P, going up: the first alone if their count is odd, then two
    /// a step, a vector each into the four accumulators. Returns the differences ORed.
    #[inline(never)]
    unsafe fn verify_victims(self) -> u64 {
        let (first, victims) = self.victims();
        let (half, apart, step) = (ident_steps(4), ident_steps(MODULO_APART), ident_steps(2 * MODULO_APART));
        let mut acc = [u64x4::splat(0); 4];
        let mut x = self.line_x(first);
        let mut p = self.base.wrapping_add(first * LINE_WORDS) as *const u64;
        if victims % 2 == 1 {
            let v = p as *const u64x4;
            acc[0] |= v.read_unaligned() ^ ident_mix_v(x);
            acc[1] |= v.add(1).read_unaligned() ^ ident_mix_v(x + half);
            x += apart;
            p = p.wrapping_add(MODULO_APART);
        }
        for _ in 0..victims / 2 {
            let (v, w) = (p as *const u64x4, p.add(MODULO_APART) as *const u64x4);
            acc[0] |= v.read_unaligned() ^ ident_mix_v(x);
            acc[1] |= v.add(1).read_unaligned() ^ ident_mix_v(x + half);
            acc[2] |= w.read_unaligned() ^ ident_mix_v(x + apart);
            acc[3] |= w.add(1).read_unaligned() ^ ident_mix_v(x + apart + half);
            x += step;
            p = p.wrapping_add(2 * MODULO_APART);
        }
        ((acc[0] | acc[1]) | (acc[2] | acc[3])).reduce_or()
    }
}

/// Mem-ModuloX: memtest86's modulo-20, protected victims. In each chunk one line in every
/// `MODULO_X` is a victim: it is written with P, then every other line is written with ~P,
/// `MODULO_PASSES` times over the chunk, then the victims are read. So a victim holds still while
/// the lines around it are written again and again: a write that disturbs other cells, or lands
/// at a victim's address, shows. Lines, not words: the cache writes whole lines back. The offset
/// moves on a line each cycle, from one each step picks, so every line is a victim in turn. P is
/// `ident`'s word of each word's id under a key per step and cycle, as in Mem-MovingInv.
#[doc = include_str!("test_fn_safety.md")]
pub unsafe fn modulo_x_multi(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> TestStats {
    let test_name = "Mem-ModuloX";
    let (mut runner, extent) = crate::test_scaffolding::TestRunner::new(
        blocks, thread_id, error_mode, timing, config, progress,
        test_name, TestAction::ReadWrite,
    );
    let base = extent.ptr as *mut u64;
    // Word i's id is ids + i: the span holds fewer than 2^40 words, so the add is the OR
    let ids = crate::pattern::word_base(thread_id);
    let first_offset = (crate::pattern::fmix(config.epoch.key()) % MODULO_X as u64) as usize;
    let ops = |runner: &crate::test_scaffolding::TestRunner| (runner.bytes_processed() / std::mem::size_of::<u64>()) as u64;

    loop {
        let cycle = runner.begin_cycle();
        let mut cycle_errors = 0u64;
        let key = crate::pattern::fmix(config.epoch.key().wrapping_add(u64::from(cycle)));
        let offset = (first_offset + cycle as usize - 1) % MODULO_X;
        let p = |i: usize| crate::pattern::ident_word(ids + i as u64, key);

        // Chunks of exactly the test's size, spread evenly over the extent (TODO 76)
        let spread = runner.chunks(extent.test_size);
        for k in 0..spread.count() {
            let (start, end) = (spread.start(k) / 64, (spread.start(k) + spread.chunk()) / 64);
            runner.check_seal(spread.start(k), spread.chunk());
            halt_here!(runner, cycle_errors, ops(&runner));

            let chunk = ModuloChunk { base, start, end, offset, ids, key };
            chunk.write_victims();
            for _ in 0..MODULO_PASSES {
                chunk.aggress();
            }
            let seen = chunk.verify_victims();
            if seen != 0 {
                cycle_errors += if crate::pattern::rescan_enabled() {
                    let mut scan = Rescan::for_test(base, test_name).seen(seen);
                    modulo_lines(start, end, offset, |v| {
                        let words: [u64; LINE_WORDS] = std::array::from_fn(|j| p(v * LINE_WORDS + j));
                        scan.line(v * LINE_WORDS, &words);
                    }, |_| {});
                    scan.finish(format_args!("in idx {}..{} (victim lines {offset} mod {MODULO_X})", start * LINE_WORDS, end * LINE_WORDS))
                } else {
                    unlocalised(found_by(test_name), format_args!("in idx {}..{} (victim lines {offset} mod {MODULO_X})",
                        start * LINE_WORDS, end * LINE_WORDS), seen)
                };
            }

            // Count the chunk when it is done: the victims written and read, the rest written
            // each pass; overlaps count each time
            let victims = spread.chunk() / MODULO_X;
            runner.add_bytes(2 * victims + MODULO_PASSES * (spread.chunk() - victims));
            halt_here!(runner, cycle_errors, ops(&runner));
            runner.reseal(spread.start(k), spread.chunk());
            if runner.should_halt(cycle_errors) || runner.shutdown_requested() {
                return runner.finish_aborted(cycle_errors, ops(&runner));
            }
        }

        runner.commit_cycle_errors(cycle_errors);
        runner.update_progress();
        if !runner.should_continue() {
            return runner.finish_completed(ops(&runner));
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

// ─── Line patterns: modes 2 and 20 (TODO 76) ─────────────────────────────────
//
// Both write each 64 B line as `seed + j * step`. Mode 2 takes seed and step from TM5's chain,
// restarted at every 4 KiB page; mode 20 hashes them from the line's address. Either way a
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
        let seen = (acc[0] | acc[1]) | (acc[2] | acc[3]);
        if seen == 0 {
            return 0;
        }
        self.rescan(ptr, start, end, test_name, seen)
    }

    #[cold]
    #[inline(never)]
    unsafe fn rescan(self, ptr: *const u64, start: usize, end: usize, test_name: &str, seen: u64) -> u64 {
        if !crate::pattern::rescan_enabled() {
            return unlocalised(found_by(test_name), format_args!("in idx {start}..{end}"), seen);
        }
        let mut scan = Rescan::for_test(ptr, test_name).seen(seen);
        self.walk(ptr, start, end, |offset, words| scan.line(offset, &words));
        scan.finish(format_args!("in idx {start}..{end}"))
    }
}

/// Mode 20 for one thread and cycle.
#[derive(Clone, Copy)]
struct Mode20 {
    key: u64,
}

impl Mode20 {
    fn new(thread_id: usize, cycle: u32, param0: u64, param1: u64) -> Self {
        Self { key: pattern_gen::mode20_key(thread_id, cycle, param0, param1) }
    }

    /// Calls `line(word_offset, expected)` for each line of words `[start, end)` of `ptr`.
    #[inline(always)]
    unsafe fn walk(self, ptr: *const u64, start: usize, end: usize, mut line: impl FnMut(usize, [u64; LINE_WORDS])) {
        debug_assert!(start.is_multiple_of(LINE_WORDS) && end.is_multiple_of(LINE_WORDS), "mode 20 needs whole lines: {start}..{end}");
        for offset in (start..end).step_by(LINE_WORDS) {
            let (seed, step) = pattern_gen::mode20_line(ptr.add(offset) as usize, self.key);
            line(offset, pattern_gen::line_words(seed, step));
        }
    }

    // Test-only since TODO 99 round 3 (the kernels have their own): the tests' reference
    #[cfg(test)]
    #[inline(always)]
    unsafe fn fill(self, ptr: *mut u64, start: usize, end: usize) {
        self.walk(ptr, start, end, |offset, words| *(ptr.add(offset) as *mut [u64; LINE_WORDS]) = words);
    }

    /// Errors in words `[start, end)`: 0 if all hold, else counted and logged by a rescan.
    // Test-only since TODO 99 round 3 (the kernels have their own): the tests' reference
    #[cfg(test)]
    #[inline(always)]
    unsafe fn verify(self, ptr: *const u64, start: usize, end: usize, test_name: &str) -> u64 {
        let mut acc = [0u64; 4];
        self.walk(ptr, start, end, |offset, words| accumulate_line(&mut acc, ptr.add(offset), &words));
        let seen = (acc[0] | acc[1]) | (acc[2] | acc[3]);
        if seen == 0 {
            return 0;
        }
        self.rescan(ptr, start, end, test_name, seen)
    }

    #[cold]
    #[inline(never)]
    unsafe fn rescan(self, ptr: *const u64, start: usize, end: usize, test_name: &str, seen: u64) -> u64 {
        if !crate::pattern::rescan_enabled() {
            return unlocalised(found_by(test_name), format_args!("in idx {start}..{end}"), seen);
        }
        let mut scan = Rescan::for_test(ptr, test_name).seen(seen);
        self.walk(ptr, start, end, |offset, words| scan.line(offset, &words));
        scan.finish(format_args!("in idx {start}..{end}"))
    }
}

/// The bus beat mode 30 flips at: the transfer width (`beat=`, else the DIMMs' SMBIOS memory
/// type, `crate::pattern::transfer_beat`), or DDR5's 32 bits when neither gives one, as mode 30
/// always has (the native `switch` alternates 64 and 32 there instead).
fn bus_beat() -> pattern_gen::BusBeat {
    static BEAT: std::sync::OnceLock<pattern_gen::BusBeat> = std::sync::OnceLock::new();
    *BEAT.get_or_init(|| {
        crate::pattern::transfer_beat().0.unwrap_or_else(|| {
            log::info!("Mode 30: no known transfer width; assuming DDR5's 32-bit beats");
            pattern_gen::BusBeat::Bits32
        })
    })
}

/// Mode 30 for one thread and cycle: the bus-flip lines (`pattern_gen::mode30_line`).
#[derive(Clone, Copy)]
struct Mode30 {
    key: u64,
    beat: pattern_gen::BusBeat,
}

/// Runs `$body` with `$line` the line function (`*const u64` to the line's words) for `$mode30`'s
/// beat, matched once outside the body, so a loop in it is compiled per beat with no branch on it.
macro_rules! with_mode30_line {
    ($mode30:expr, |$line:ident| $body:block) => {{
        let key = $mode30.key;
        match $mode30.beat {
            pattern_gen::BusBeat::Bits12 => {
                let $line = |line: *const u64| pattern_gen::mode30_line(line as usize, key, pattern_gen::BusBeat::Bits12);
                $body
            }
            pattern_gen::BusBeat::Bits16 => {
                let $line = |line: *const u64| pattern_gen::mode30_line(line as usize, key, pattern_gen::BusBeat::Bits16);
                $body
            }
            pattern_gen::BusBeat::Bits24 => {
                let $line = |line: *const u64| pattern_gen::mode30_line(line as usize, key, pattern_gen::BusBeat::Bits24);
                $body
            }
            pattern_gen::BusBeat::Bits32 => {
                let $line = |line: *const u64| pattern_gen::mode30_line(line as usize, key, pattern_gen::BusBeat::Bits32);
                $body
            }
            pattern_gen::BusBeat::Bits64 => {
                let $line = |line: *const u64| pattern_gen::mode30_line(line as usize, key, pattern_gen::BusBeat::Bits64);
                $body
            }
        }
    }};
}

impl Mode30 {
    fn new(thread_id: usize, cycle: u32, param0: u64, param1: u64) -> Self {
        Self { key: pattern_gen::mode20_key(thread_id, cycle, param0, param1), beat: bus_beat() }
    }

    /// Calls `line(word_offset, expected)` for each line of words `[start, end)` of `ptr`.
    #[inline(always)]
    unsafe fn walk(self, ptr: *const u64, start: usize, end: usize, mut line: impl FnMut(usize, [u64; LINE_WORDS])) {
        debug_assert!(start.is_multiple_of(LINE_WORDS) && end.is_multiple_of(LINE_WORDS), "mode 30 needs whole lines: {start}..{end}");
        with_mode30_line!(self, |expected| {
            for offset in (start..end).step_by(LINE_WORDS) {
                line(offset, expected(ptr.add(offset)));
            }
        })
    }

    // Test-only since TODO 99 round 3 (the kernels have their own): the tests' reference
    #[cfg(test)]
    #[inline(always)]
    unsafe fn fill(self, ptr: *mut u64, start: usize, end: usize) {
        self.walk(ptr, start, end, |offset, words| *(ptr.add(offset) as *mut [u64; LINE_WORDS]) = words);
    }

    /// Errors in words `[start, end)`: 0 if all hold, else counted and logged by a rescan.
    // Test-only since TODO 99 round 3 (the kernels have their own): the tests' reference
    #[cfg(test)]
    #[inline(always)]
    unsafe fn verify(self, ptr: *const u64, start: usize, end: usize, test_name: &str) -> u64 {
        let mut acc = [0u64; 4];
        self.walk(ptr, start, end, |offset, words| accumulate_line(&mut acc, ptr.add(offset), &words));
        let seen = (acc[0] | acc[1]) | (acc[2] | acc[3]);
        if seen == 0 {
            return 0;
        }
        self.rescan(ptr, start, end, test_name, seen)
    }

    #[cold]
    #[inline(never)]
    unsafe fn rescan(self, ptr: *const u64, start: usize, end: usize, test_name: &str, seen: u64) -> u64 {
        if !crate::pattern::rescan_enabled() {
            return unlocalised(found_by(test_name), format_args!("in idx {start}..{end}"), seen);
        }
        let mut scan = Rescan::for_test(ptr, test_name).seen(seen);
        self.walk(ptr, start, end, |offset, words| scan.line(offset, &words));
        scan.finish(format_args!("in idx {start}..{end}"))
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

/// Verify for the per-word patterns (modes 0, 1, 10, 11, 13): XOR each word with
/// `expected(idx)` and OR into 4 accumulators, then count and log in a cold rescan only if one is
/// set. A per-word branch and `log::error!` kept the log's arguments spilled to the stack on every
/// word, and cost Bench-Verify-TM5-0 30% when the harness around it changed (TODO 76).
// Test-only since TODO 99 round 3 (the kernels have their own): the tests' reference
#[cfg(test)]
#[inline(always)]
unsafe fn verify_words(ptr: *const u64, start: usize, end: usize, test_name: &str, expected: impl Fn(usize) -> u64 + Copy) -> u64 {
    verify_words_in(ptr, start, end, test_name, "", expected)
}

/// As `verify_words`, a failed check's record naming the region's `label` too (`rescan_words_in`).
#[cfg(test)]
#[inline(always)]
unsafe fn verify_words_in(ptr: *const u64, start: usize, end: usize, test_name: &str, label: &str,
                          expected: impl Fn(usize) -> u64 + Copy) -> u64 {
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
    let seen = (acc[0] | acc[1]) | (acc[2] | acc[3]);
    if seen == 0 {
        return 0;
    }
    rescan_words_in(ptr, start, end, test_name, seen, label, expected)
}

/// ORs the differences of words `start, start + stride, ...` below `end` into the 4 accumulators,
/// 4 strides per step (TODO 89): the strided twin of `verify_words`'s loop, for walks over a chunk
/// that stays in cache (Mem-CacheBust's 1 MiB, Mem-Stride's 2 MiB), where the loop is the limit.
/// The caller checks them once and rescans on a hit. Measured on the dev box (2026-10-06):
/// CacheBust 12% and Mem-Stride 5% faster than one load per step.
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

// ─── The cold path of every verify (TODO 27's second tier) ──────────────────────────────────────
//
// A verify ORs each word's difference into accumulators and branches once per chunk: the first
// tier, unchanged. When an accumulator is set, the region is rescanned before anything overwrites
// or reseals it. Each bad word is logged with its index, address, expected and actual values and
// their XOR, and the first few are reread: volatile loads, so each is a real load, then once more
// after flushing the word's line. A reread that still sees the wrong value may be reading a cached
// copy of a bad read, so neither result locates the fault; they are reported as seen. A rescan that
// finds every word right reports the check as not reproduced, and it still counts one error: the
// check saw a difference. `rescan=off` skips all of it and counts one error a failed check.

/// Bad words a rescan logs.
const RESCAN_LOGGED: u64 = 10;
/// Bad words a rescan rereads, and the rereads of each before the one after the flush.
const REREAD_WORDS: u64 = 4;
const REREADS: u32 = 4;
/// Rescans a test rereads in, so a run full of errors doesn't slow to the rereads' pace.
const REREAD_RESCANS: u32 = 16;

/// This thread's failed checks in the current test, for its `TestStats` (TODO 27).
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct RescanCounts {
    /// Checks that found a difference.
    pub alarms: u64,
    /// Of those, the ones whose rescan found every word right.
    pub not_reproduced: u64,
    /// Rescans that have reread, against `REREAD_RESCANS`.
    rereading: u32,
}

thread_local! {
    static RESCANS: std::cell::Cell<RescanCounts> =
        const { std::cell::Cell::new(RescanCounts { alarms: 0, not_reproduced: 0, rereading: 0 }) };
    /// This thread's failing bits since the last take: the worker sends them with each item's
    /// result (`thread_pool`)
    static BITS: std::cell::RefCell<crate::failing_bits::CheckBits> = std::cell::RefCell::default();
}

/// Starts a test's counts afresh (`TestRunner::new`), its failing bits too.
pub(crate) fn reset_rescan_counts() {
    RESCANS.with(|c| c.set(RescanCounts::default()));
    BITS.with_borrow_mut(|b| *b = Default::default());
}

/// This thread's failing bits since the last take or reset, leaving none.
pub(crate) fn take_failing_bits() -> crate::failing_bits::CheckBits {
    BITS.with_borrow_mut(std::mem::take)
}

/// A failed check's bits, the test's own (`count`) or the seal's.
fn note_bits(count: bool, bits: &crate::failing_bits::FailingBits) {
    BITS.with_borrow_mut(|b| if count { b.test.add(bits) } else { b.seal.add(bits) });
}

/// This thread's counts since the last reset.
pub(crate) fn rescan_counts() -> RescanCounts {
    RESCANS.with(|c| c.get())
}

fn found_by(test_name: &str) -> String {
    crate::error_context::found_by(test_name)
}

/// One rescan of a region a check found a difference in.
pub(crate) struct Rescan {
    ptr: *const u64,
    /// The start of each log line: who found it, and where in the run.
    lead: String,
    found: u64,
    reread: bool,
    /// The check's differences ORed over the region: which bits differed somewhere. The rescan's
    /// bad words are compared with it, so bits the check saw and the rescan didn't are kept.
    seen: u64,
    /// The rescan's bad words' differences, ORed
    found_xor: u64,
    /// In the test's alarms and not-reproduced counts (not the seal's own checks)
    count: bool,
    /// The first bad words, reread once the scan is done: a flush in mid-scan would make the
    /// rest of their line a fresh read
    suspects: [(usize, u64); REREAD_WORDS as usize],
    /// The bad words' bits, by the way each went
    words: crate::failing_bits::WordTally,
}

impl Rescan {
    /// A test's rescan, counted in its alarms.
    pub(crate) fn for_test(ptr: *const u64, test_name: &str) -> Self {
        Self::begin(ptr, found_by(test_name), true)
    }

    /// A rescan whose log lines start with `lead`; `count` puts it in the test's alarms (the seal's
    /// checks count apart, as seal errors).
    pub(crate) fn begin(ptr: *const u64, lead: String, count: bool) -> Self {
        let reread = RESCANS.with(|c| {
            let mut n = c.get();
            n.alarms += u64::from(count);
            c.set(n);
            n.rereading < REREAD_RESCANS
        });
        Rescan { ptr, lead, found: 0, reread, seen: 0, found_xor: 0, count, suspects: [(0, 0); REREAD_WORDS as usize],
                 words: Default::default() }
    }

    /// The bad words found so far.
    pub(crate) fn found(&self) -> u64 {
        self.found
    }

    /// The check's differences, ORed: 0 where the check didn't keep them.
    pub(crate) fn seen(mut self, mask: u64) -> Self {
        self.seen = mask;
        self
    }

    /// Checks word `idx` against `want`, with a volatile load.
    ///
    /// # Safety
    /// `ptr.add(idx)` must be valid for reads.
    #[inline(always)]
    pub(crate) unsafe fn word(&mut self, idx: usize, want: u64) {
        let actual = std::ptr::read_volatile(self.ptr.add(idx));
        if actual != want {
            self.bad(idx, want, actual);
        }
    }

    /// Checks the line of words `offset..offset + 8` against `words`.
    ///
    /// # Safety
    /// As `word`.
    #[inline(always)]
    pub(crate) unsafe fn line(&mut self, offset: usize, words: &[u64; LINE_WORDS]) {
        for (j, &want) in words.iter().enumerate() {
            self.word(offset + j, want);
        }
    }

    /// Checks the words `start, start + stride, ...` below `end` against `expected(idx)`.
    ///
    /// # Safety
    /// As `word`.
    pub(crate) unsafe fn stride(&mut self, start: usize, end: usize, stride: usize, expected: impl Fn(usize) -> u64) {
        let mut idx = start;
        while idx < end {
            self.word(idx, expected(idx));
            idx += stride;
        }
    }

    #[cold]
    #[inline(never)]
    unsafe fn bad(&mut self, idx: usize, want: u64, actual: u64) {
        self.found += 1;
        self.found_xor |= want ^ actual;
        self.words.add(want, actual);
        let at = self.ptr.add(idx);
        if self.found <= RESCAN_LOGGED {
            let diff = want ^ actual;
            log::error!("{}, idx {idx} (address {:#x}): expected {want:#x}, got {actual:#x}, XOR {diff:#018x} ({} bits)",
                        self.lead, at as usize, diff.count_ones());
        }
        if self.reread && self.found <= REREAD_WORDS {
            self.suspects[(self.found - 1) as usize] = (idx, want);
        }
    }

    /// Rereads the first bad words, after the scan: `REREADS` times each, then once after flushing
    /// its line. Neither tells where the fault is: a reread can hit a cached copy of a bad read, and
    /// the flush writes back a dirty line first.
    unsafe fn reread(&self) {
        let suspects = &self.suspects[..self.found.min(REREAD_WORDS) as usize];
        RESCANS.with(|c| {
            let mut n = c.get();
            n.rereading += 1;
            c.set(n);
        });
        // Every suspect's rereads before any flush: a flush evicts the whole line, so a second
        // suspect in it would be reread fresh from DRAM
        let still: Vec<usize> = suspects.iter()
            .map(|&(idx, want)| (0..REREADS).filter(|_| std::ptr::read_volatile(self.ptr.add(idx)) != want).count())
            .collect();
        for &(idx, _) in suspects {
            // SAFETY: the CPU gate requires CLFLUSHOPT, and the line holds a valid word
            flush_range_to_dram((self.ptr.add(idx) as usize & !63) as *const u8, 64, 64);
        }
        for (&(idx, want), still) in suspects.iter().zip(still) {
            let after = std::ptr::read_volatile(self.ptr.add(idx));
            let after = if after == want { "right".to_string() } else { format!("wrong ({after:#x})") };
            log::error!("{}, idx {idx}: {still} of {REREADS} rereads still wrong; after flushing its line, {after}", self.lead);
        }
    }

    /// The bad words found, which the caller counts as errors; or one if none was, since the check
    /// saw a difference. `what` names the region and the pattern (`in idx 0..512 (random-~P, ...)`).
    /// Every failed check leaves this one record, whatever the rescan found.
    pub(crate) fn finish(self, what: std::fmt::Arguments) -> u64 {
        if self.reread && self.found > 0 {
            // SAFETY: the suspects are words of the region the scan just read
            unsafe { self.reread() };
        }
        // The check's bits are those its differences had; where it kept none, the rescan's. A bit
        // only the rescan found (the word changed since) counts in its words alone.
        let mut bits = crate::failing_bits::FailingBits::default();
        bits.add_check(if self.seen != 0 { self.seen } else { self.found_xor });
        bits.add_words(&self.words);
        note_bits(self.count, &bits);
        log::error!("{} {what}: {}", self.lead, alarm_record(self.found, self.seen, self.found_xor));
        if self.found > 0 {
            return self.found;
        }
        RESCANS.with(|c| {
            let mut n = c.get();
            n.not_reproduced += u64::from(self.count);
            c.set(n);
        });
        1
    }
}

/// A failed check's record: what its rescan found, the check's ORed differences, and the bits the
/// check saw but no bad word of the rescan had (gone by the rescan). An ORed mask names bit
/// positions over the region, not which words, how many, or which way.
fn alarm_record(found: u64, seen: u64, found_xor: u64) -> String {
    let rescan = match found {
        0 => "not reproduced on rescan, every word read back right".to_string(),
        n if n > RESCAN_LOGGED => format!("{n} bad words on rescan, the first {RESCAN_LOGGED} logged, their differences ORed {found_xor:#018x}"),
        n => format!("{n} bad words on rescan, their differences ORed {found_xor:#018x}"),
    };
    let gone = seen & !found_xor;
    let gone = if found > 0 && gone != 0 { format!("; the rescan didn't see bits {gone:#018x} again") } else { String::new() };
    format!("{rescan}; the check's differences, ORed: {seen:#018x} ({} bits){gone}", seen.count_ones())
}

/// Under `rescan=off`, a failed check: one error, and one alarm when `count` (as `Rescan::begin`),
/// naming only the region.
#[cold]
#[inline(never)]
pub(crate) fn unlocalised_as(lead: String, what: std::fmt::Arguments, seen: u64, count: bool) -> u64 {
    RESCANS.with(|c| {
        let mut n = c.get();
        n.alarms += u64::from(count);
        c.set(n);
    });
    let mut bits = crate::failing_bits::FailingBits::default();
    bits.add_check(seen);
    note_bits(count, &bits);
    log::error!("{lead} {what} (rescan=off: not localised); the check's differences, ORed: {seen:#018x}");
    1
}

/// `unlocalised_as` for a test's own check: counted in its alarms.
pub(crate) fn unlocalised(lead: String, what: std::fmt::Arguments, seen: u64) -> u64 {
    unlocalised_as(lead, what, seen, true)
}

/// Counts and logs the words of `[start, end)` that differ from `expected(idx)`, once an
/// accumulator has seen one.
#[cold]
#[inline(never)]
unsafe fn rescan_words(ptr: *const u64, start: usize, end: usize, test_name: &str, seen: u64,
                       expected: impl Fn(usize) -> u64) -> u64 {
    rescan_words_in(ptr, start, end, test_name, seen, "", expected)
}

/// As `rescan_words`, the region also named by `label` when it isn't empty (StuckBit's phase), as
/// a native kernel names its phase: `in idx 0..512 (phase 2)`.
#[cold]
#[inline(never)]
unsafe fn rescan_words_in(ptr: *const u64, start: usize, end: usize, test_name: &str, seen: u64, label: &str,
                          expected: impl Fn(usize) -> u64) -> u64 {
    let what = |f: &mut std::fmt::Formatter| if label.is_empty() {
        write!(f, "in idx {start}..{end}")
    } else {
        write!(f, "in idx {start}..{end} ({label})")
    };
    let what = Region(&what);
    if !crate::pattern::rescan_enabled() {
        return unlocalised(found_by(test_name), format_args!("{what}"), seen);
    }
    let mut scan = Rescan::for_test(ptr, test_name).seen(seen);
    for idx in start..end {
        scan.word(idx, expected(idx));
    }
    scan.finish(format_args!("{what}"))
}

/// A region's name for a record line, written by a closure
struct Region<'a>(&'a dyn Fn(&mut std::fmt::Formatter) -> std::fmt::Result);

impl std::fmt::Display for Region<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        (self.0)(f)
    }
}

/// TM5's strided walk (`mtests0.asm` ~336-394, TODO 93): runs `$body` once for each 64 B line of
/// words `[$start, $end)` of `$ptr`, with `$offset` its first word. TM5 writes or reads a whole
/// line, then jumps `64 x (Channels x Parameter - 1)` bytes, so a period of `$period` words (a line
/// plus the jump) is touched one line at a time; each of its lines starts an interleave pass, the
/// last line first, and a pass steps a period at a time to the end. A macro, not a closure, so the
/// SIMD variants' bodies keep their `#[target_feature]`.
///
/// What the walk is for: every access opens a new DRAM row, as many per second as the timings
/// allow. So each step prefetches the line the walk reaches `STRIDED_AHEAD` steps on, a period or
/// more away and so in another row, keeping requests queued without fetching neighbours together.
/// TM5's descending order serves the same end: the L1's next-line prefetcher fetches the line above
/// each one, which this order has already done. With passes going up instead and the line above
/// prefetched, the walk ran 1.2x faster over 1usmus_v3's seven shapes and 1.9x at Test 13's, where
/// a pass has 57 lines; likely in part by letting the hardware fetch neighbours together, fewer row
/// openings per line, so its speed was no measure of the stress. See TODO 93's detail file.
// Test-only since TODO 99 round 3: the reference the kernels' own strided walks are tested against
#[cfg(test)]
macro_rules! for_each_strided_line {
    ($ptr:expr, $start:expr, $end:expr, $period:expr, |$offset:ident| $body:block) => {{
        let (ptr, start, end, period): (*const u64, usize, usize, usize) = ($ptr, $start, $end, $period);
        debug_assert!(period.is_multiple_of(LINE_WORDS) && start.is_multiple_of(LINE_WORDS) && end.is_multiple_of(LINE_WORDS),
                      "the strided walk needs whole lines: {start}..{end} by {period}");
        let ahead = strided_lookahead(start, end, period);
        let mut first = start + period;
        while first > start {
            first -= LINE_WORDS;
            let wrap = strided_wrap(first, end, period);
            let mut $offset = first;
            while $offset < end {
                // The walk's last steps prefetch off the chunk: a wasted prefetch, not a fault
                prefetch_line(ptr.wrapping_add(strided_ahead($offset, ahead, end, wrap)));
                $body
                $offset += period;
            }
        }
    }};
}

/// How many steps ahead the strided walk prefetches. TM5 prefetches 8 periods on in the same pass
/// (`prefetchnta [edi+esi]`), which lands outside the chunk once a pass has fewer than 8 lines. On
/// 1usmus_v3's seven shapes (2026-10-08) the walk sped up to 64 steps and was flat from there to
/// 256 (about level: two shapes moved 8% and 16%, opposite ways); 128 is the middle of it.
const STRIDED_AHEAD: usize = 128;

/// The strided walk's lookahead in words: `STRIDED_AHEAD` steps, but no more than the shortest
/// pass has lines, so a step past a pass's end lands in the next pass.
#[inline(always)]
fn strided_lookahead(start: usize, end: usize, period: usize) -> usize {
    STRIDED_AHEAD.min((end - start) / period) * period
}

/// For the pass whose first line is `first`: how far `strided_ahead` steps back to go from past
/// this pass's end into the next pass, a line lower. The pass's lines, in words, plus one line.
#[inline(always)]
fn strided_wrap(first: usize, end: usize, period: usize) -> usize {
    end.saturating_sub(first).div_ceil(period) * period + LINE_WORDS
}

/// The line `ahead` words of steps on from `offset` in the strided walk: on in this pass, or past
/// its end, on into the next.
#[cfg(test)]
#[inline(always)]
fn strided_ahead(offset: usize, ahead: usize, end: usize, wrap: usize) -> usize {
    let next = offset + ahead;
    if next < end { next } else { next.wrapping_sub(wrap) }
}

/// Prefetches the line at `line` into the L2 and below. `prefetcht1`, not `t0`: at 128 steps on,
/// `t0` was 0.79x `t1` at 1usmus Test 6's shape and 0.91x at Test 12's (likely the L1's few miss
/// slots, which `t0` holds until the line arrives), and within 10% at the other five.
#[inline(always)]
fn prefetch_line(line: *const u64) {
    // SAFETY: SSE is in the x86-64-v3 baseline, and a prefetch neither faults nor writes, at any address
    unsafe { core::arch::x86_64::_mm_prefetch::<{ core::arch::x86_64::_MM_HINT_T1 }>(line as *const i8) }
}

/// The strided walk's period in words: a line plus the jump, the stride rounded up to whole lines.
fn strided_period(stride: usize) -> usize {
    stride.next_multiple_of(LINE_WORDS) + LINE_WORDS
}

// ─── SimpleTest v2 ───────────────────────────────────────────────────────────

/// `for_each_strided_line!` on a walked pointer, for the numbered-mode SimpleTest's loops (TODO 99):
/// the same lines in the same order with the same prefetches, `$line` the line's first word
/// (`*mut u64`), `$offset`, when asked for, its index, and `$target` the line it prefetched (for the
/// tests). The pointer steps a period a line, so each access is a register and a fixed displacement
/// rather than a base and an index; every access here is a new row, as in Mem-CacheBust, where the
/// index cost 6% (TODO 99 round 2).
///
/// The prefetch target (`strided_ahead`) is `steps` lines on in the pass for the pass's first
/// `lines - steps` lines, and for the rest that less the pass's `wrap`, in the next pass. So each
/// pass is two loops, the target a fixed distance from the line in each and walked beside it: no
/// compare and select a line, and the registers they took free for the body.
macro_rules! for_each_strided_line_at {
    ($ptr:expr, $start:expr, $end:expr, $period:expr, |$line:ident, $offset:ident, $target:ident| $body:block) => {{
        let (ptr, start, end, period): (*mut u64, usize, usize, usize) = ($ptr, $start, $end, $period);
        debug_assert!(period.is_multiple_of(LINE_WORDS) && start.is_multiple_of(LINE_WORDS) && end.is_multiple_of(LINE_WORDS),
                      "the strided walk needs whole lines: {start}..{end} by {period}");
        // `strided_lookahead` in lines
        let steps = STRIDED_AHEAD.min((end - start) / period);
        let ahead = steps * period;
        let mut first = start + period;
        while first > start {
            first -= LINE_WORDS;
            // `strided_wrap`: the pass's lines, and a line
            let lines = end.saturating_sub(first).div_ceil(period);
            let wrap = lines * period + LINE_WORDS;
            let mut $line = ptr.wrapping_add(first);
            let mut $offset = first;
            let mut $target = $line.wrapping_add(ahead);
            // Each loop runs to a line, not a count: a counted loop the compiler unrolls, and its
            // copies' lines a run-time period apart take an index register each
            let (switch, stop) = ($line.wrapping_add(lines.saturating_sub(steps) * period), $line.wrapping_add(lines * period));
            while $line < switch {
                prefetch_line($target);
                $body
                $line = $line.wrapping_add(period);
                $offset += period;
                $target = $target.wrapping_add(period);
            }
            // The walk's last steps prefetch off the chunk: a wasted prefetch, not a fault
            $target = $target.wrapping_sub(wrap);
            while $line < stop {
                prefetch_line($target);
                $body
                $line = $line.wrapping_add(period);
                $offset += period;
                $target = $target.wrapping_add(period);
            }
        }
    }};
    ($ptr:expr, $start:expr, $end:expr, $period:expr, |$line:ident, $offset:ident| $body:block) => {
        for_each_strided_line_at!($ptr, $start, $end, $period, |$line, $offset, _target| $body)
    };
    ($ptr:expr, $start:expr, $end:expr, $period:expr, |$line:ident| $body:block) => {
        for_each_strided_line_at!($ptr, $start, $end, $period, |$line, _offset, _target| $body)
    };
}

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

// ─── SimpleTest v2's scalar loops (TODO 99) ──────────────────────────────────────────────────────
//
// The scalar build runs every numbered mode (the SIMD builds send modes 0, 1, 2, 13 and 30 here).
// Each pattern's write and verify, sequential and strided, is a function of its own in
// `simple_test_v2_loops`, taking its chunk as a `Copy` struct by value and called once a chunk:
// in the test function, with every mode's closures and the harness's chunks, seal and stats live
// too, the loops reloaded their constants and spilled their accumulators. Each walks a pointer, the
// expected value computed from the word's index (or a line's address) beside it.
//
// The build's width is the baseline's, 256 bits, which the compiler reached on its own before: it
// vectorized the loops over words and lines itself, four iterations wide, the verify's four
// accumulators transposed into lanes by shuffles and their constants spilled (Rule 6.5). So the
// loops here are written as `u64x4` vectors, which the compiler leaves in that shape: four words
// a vector, a line two, the expected words computed per word or per line as before.
//
// The shared `Mode2`/`Mode20`/`Mode30` walks stay as they are for Bench-Init/Verify and the
// rescans; these loops write the same words (`pattern_gen`'s line functions) in the same order.

/// SimpleTest's per-word patterns on words `[start, end)` of `ptr`, the scalar build: word `i`
/// holds `expected(i)`, mode `MODE` (0, 1, 11 and 13, and 10 for any other, as the test's match
/// has it) from `seed` (the chunk's seed for modes 0 and 1, the base for the others) and, for mode
/// 1, `cl_shift`. The mode is a constant so each mode's loops are compiled with its pattern inlined.
#[derive(Clone, Copy)]
struct SimpleWords<const MODE: u32> {
    ptr: *mut u64,
    start: usize,
    end: usize,
    seed: u64,
    cl_shift: u32,
}

impl<const MODE: u32> SimpleWords<MODE> {
    /// Word `idx`'s pattern.
    #[inline(always)]
    fn expected(self, idx: usize) -> u64 {
        match MODE {
            0 => pattern_gen::pattern_mode0(idx as u64, self.seed),
            1 => pattern_gen::pattern_mode1(idx as u64, self.seed, self.cl_shift),
            11 => pattern_gen::pattern_mode11(idx as u64, self.seed),
            13 => pattern_gen::pattern_mode13(idx as u64, self.seed),
            _ => pattern_gen::pattern_mode10(idx as u64, self.seed),
        }
    }

    /// Words `idx` to `idx + 3`'s patterns.
    #[inline(always)]
    fn expected4(self, idx: usize) -> u64x4 {
        u64x4::from_array([self.expected(idx), self.expected(idx + 1), self.expected(idx + 2), self.expected(idx + 3)])
    }

    /// The patterns of the four words whose indices are `idx`'s lanes. A vector of indices, so a
    /// loop can carry it beside its pointer: from one scalar index the compiler addressed the words
    /// through it, as `(%base,%index,8)`.
    #[inline(always)]
    fn expected_at(self, idx: u64x4) -> u64x4 {
        u64x4::from_array(idx.to_array().map(|i| self.expected(i as usize)))
    }

    /// The sequential verify and, on a hit, the cold rescan that counts and logs each bad word.
    #[inline(always)]
    unsafe fn check(self, test_name: &str) -> u64 {
        let seen = simple_test_v2_loops::words_verify(self);
        if seen == 0 {
            return 0;
        }
        rescan_words(self.ptr, self.start, self.end, test_name, seen, |idx| self.expected(idx))
    }

    /// The strided verify (the walk covers every word of the chunk, so a hit rescans it in order,
    /// TODO 89).
    #[inline(always)]
    unsafe fn check_strided(self, period: usize, test_name: &str) -> u64 {
        let seen = simple_test_v2_loops::words_verify_str(self, period);
        if seen == 0 {
            return 0;
        }
        rescan_words(self.ptr, self.start, self.end, test_name, seen, |idx| self.expected(idx))
    }
}

/// Mode 2 on the whole pages of words `[start, end)` of `ptr`, the scalar build: TM5's evolving
/// lines, the chain restarted at each page (`Mode2`).
#[derive(Clone, Copy)]
struct SimpleMode2 {
    ptr: *mut u64,
    start: usize,
    end: usize,
    mode2: Mode2,
}

/// Mode 30 on the whole lines of words `[start, end)` of `ptr`, the scalar build: each line the
/// bus-flip line of its address (`Mode30`).
#[derive(Clone, Copy)]
struct SimpleMode30 {
    ptr: *mut u64,
    start: usize,
    end: usize,
    mode30: Mode30,
}

/// The scalar build's loops (mode 20 takes `SimpleMode20`, as the SIMD builds do). Each verify
/// returns its accumulators ORed, nonzero when a word differs; the caller rescans. A sequential
/// verify takes four vectors a step, each into its own accumulator; a strided one a line at a time
/// into one, whose lanes are `accumulate_line`'s four (words `k` and `k + 4` into the `k`th).
mod simple_test_v2_loops {
    use super::*;

    /// A mode 2 or 20 line, `seed + j * step` (`pattern_gen::line_words`), as its two halves.
    #[inline(always)]
    fn line_vectors(seed: u64, step: u64) -> (u64x4, u64x4) {
        let lo = u64x4::splat(seed) + u64x4::splat(step) * u64x4::from_array([0, 1, 2, 3]);
        (lo, lo + u64x4::splat(step.wrapping_mul(4)))
    }

    /// A line's words as its two halves.
    #[inline(always)]
    fn halves(w: [u64; LINE_WORDS]) -> (u64x4, u64x4) {
        (u64x4::from_array([w[0], w[1], w[2], w[3]]), u64x4::from_array([w[4], w[5], w[6], w[7]]))
    }

    /// Stores a line's halves.
    #[inline(always)]
    unsafe fn store_line(line: *mut u64, (lo, hi): (u64x4, u64x4)) {
        *(line as *mut u64x4) = lo;
        *(line.add(4) as *mut u64x4) = hi;
    }

    /// A line's differences from its halves, ORed: words `k` and `k + 4` into lane `k`.
    #[inline(always)]
    unsafe fn line_diff(line: *const u64, (lo, hi): (u64x4, u64x4)) -> u64x4 {
        (*(line as *const u64x4) ^ lo) | (*(line.add(4) as *const u64x4) ^ hi)
    }

    /// The per-word write: each word its pattern, in order, four vectors a step, then the rest a
    /// vector at a time and the last words, under a vector, out of line (no chunk has any: chunks
    /// and extents are whole 4 KiB pages). The vector of indices steps beside the pointer.
    #[inline(never)]
    pub(super) unsafe fn words_write<const MODE: u32>(c: SimpleWords<MODE>) {
        debug_assert!((c.ptr as usize + 8 * c.start).is_multiple_of(32), "the words start on a vector");
        let words = c.end - c.start;
        let four = u64x4::splat(4);
        let mut idx = u64x4::splat(c.start as u64) + u64x4::from_array([0, 1, 2, 3]);
        let mut p = c.ptr.add(c.start) as *mut u64x4;
        for _ in 0..words / 16 {
            for k in 0..4 {
                *p.add(k) = c.expected_at(idx);
                idx += four;
            }
            p = p.wrapping_add(4);
        }
        // To a pointer, not a count: counted, at most three, the compiler unrolled these into
        // straight-line code laid out beside the epilogue, which the check then read as a loop
        let vectors_end = c.ptr.wrapping_add(c.end - words % 4) as *mut u64x4;
        while p < vectors_end {
            *p = c.expected_at(idx);
            idx += four;
            p = p.wrapping_add(1);
        }
        if !words.is_multiple_of(4) {
            words_write_rest(c);
        }
    }

    /// The per-word verify: four vectors a step into four accumulators, then the rest a vector at
    /// a time into the first, and the last words, under a vector, out of line (`verify_words`'s
    /// loop, at the width the compiler gave it).
    #[inline(never)]
    pub(super) unsafe fn words_verify<const MODE: u32>(c: SimpleWords<MODE>) -> u64 {
        debug_assert!((c.ptr as usize + 8 * c.start).is_multiple_of(32), "the words start on a vector");
        let words = c.end - c.start;
        let four = u64x4::splat(4);
        let mut idx = u64x4::splat(c.start as u64) + u64x4::from_array([0, 1, 2, 3]);
        let mut acc = [u64x4::splat(0); 4];
        let mut p = c.ptr.add(c.start) as *const u64x4;
        for _ in 0..words / 16 {
            for (k, a) in acc.iter_mut().enumerate() {
                *a |= *p.add(k) ^ c.expected_at(idx);
                idx += four;
            }
            p = p.wrapping_add(4);
        }
        let vectors_end = c.ptr.wrapping_add(c.end - words % 4) as *const u64x4;
        while p < vectors_end {
            acc[0] |= *p ^ c.expected_at(idx);
            idx += four;
            p = p.wrapping_add(1);
        }
        let seen = ((acc[0] | acc[1]) | (acc[2] | acc[3])).reduce_or();
        if words.is_multiple_of(4) { seen } else { seen | words_verify_rest(c) }
    }

    /// The per-word write's last words, under a vector: at most three, each on its own.
    #[cold]
    #[inline(never)]
    unsafe fn words_write_rest<const MODE: u32>(c: SimpleWords<MODE>) {
        let rest = (c.end - c.start) % 4;
        for k in 0..3 {
            if k < rest {
                let idx = c.end - rest + k;
                *c.ptr.add(idx) = c.expected(idx);
            }
        }
    }

    /// The per-word verify's last words, under a vector.
    #[cold]
    #[inline(never)]
    unsafe fn words_verify_rest<const MODE: u32>(c: SimpleWords<MODE>) -> u64 {
        let (rest, mut seen) = ((c.end - c.start) % 4, 0);
        for k in 0..3 {
            if k < rest {
                let idx = c.end - rest + k;
                seen |= *c.ptr.add(idx) ^ c.expected(idx);
            }
        }
        seen
    }

    /// The per-word write on TM5's strided walk (TODO 93): each line's words their patterns.
    #[inline(never)]
    pub(super) unsafe fn words_write_str<const MODE: u32>(c: SimpleWords<MODE>, period: usize) {
        for_each_strided_line_at!(c.ptr, c.start, c.end, period, |line, offset| {
            store_line(line, (c.expected4(offset), c.expected4(offset + 4)));
        });
    }

    /// The per-word verify on the strided walk, a line at a time.
    #[inline(never)]
    pub(super) unsafe fn words_verify_str<const MODE: u32>(c: SimpleWords<MODE>, period: usize) -> u64 {
        let mut acc = u64x4::splat(0);
        for_each_strided_line_at!(c.ptr, c.start, c.end, period, |line, offset| {
            acc |= line_diff(line, (c.expected4(offset), c.expected4(offset + 4)));
        });
        acc.reduce_or()
    }

    /// Mode 2's write: each page's chain from the page's address, a line a link.
    #[inline(never)]
    pub(super) unsafe fn mode2_write(c: SimpleMode2) {
        let SimpleMode2 { ptr, start, end, mode2 } = c;
        debug_assert!(start.is_multiple_of(PAGE_WORDS) && end.is_multiple_of(PAGE_WORDS), "mode 2 needs whole pages: {start}..{end}");
        let mut line = ptr.add(start);
        for _ in 0..(end - start) / PAGE_WORDS {
            let (mut seed, mut step) = pattern_gen::mode2_page_start(line as usize, mode2.thread_id, mode2.cycle);
            // To the page's end, not a count: from 64 lines the compiler addressed each from the
            // page through an offset register
            let page_end = line.wrapping_add(PAGE_WORDS);
            while line < page_end {
                store_line(line, line_vectors(seed, step));
                (seed, step) = pattern_gen::mode2_evolve(seed, step, mode2.param0, mode2.param1);
                line = line.wrapping_add(LINE_WORDS);
            }
        }
    }

    /// Mode 2's verify: the write's chain, four lines a step, each into its own accumulator.
    #[inline(never)]
    pub(super) unsafe fn mode2_verify(c: SimpleMode2) -> u64 {
        let SimpleMode2 { ptr, start, end, mode2 } = c;
        debug_assert!(start.is_multiple_of(PAGE_WORDS) && end.is_multiple_of(PAGE_WORDS), "mode 2 needs whole pages: {start}..{end}");
        let mut acc = [u64x4::splat(0); 4];
        let mut line = ptr.add(start) as *const u64;
        for _ in 0..(end - start) / PAGE_WORDS {
            let (mut seed, mut step) = pattern_gen::mode2_page_start(line as usize, mode2.thread_id, mode2.cycle);
            let page_end = line.wrapping_add(PAGE_WORDS);
            while line < page_end {
                for (k, a) in acc.iter_mut().enumerate() {
                    *a |= line_diff(line.add(k * LINE_WORDS), line_vectors(seed, step));
                    (seed, step) = pattern_gen::mode2_evolve(seed, step, mode2.param0, mode2.param1);
                }
                line = line.wrapping_add(4 * LINE_WORDS);
            }
        }
        ((acc[0] | acc[1]) | (acc[2] | acc[3])).reduce_or()
    }

    /// Mode 20's write: each line from its address's hash.
    #[inline(never)]
    pub(super) unsafe fn mode20_write(c: SimpleMode20) {
        let SimpleMode20 { ptr, start, end, key } = c;
        debug_assert!(start.is_multiple_of(LINE_WORDS) && end.is_multiple_of(LINE_WORDS), "mode 20 needs whole lines: {start}..{end}");
        let mut line = ptr.add(start);
        for _ in 0..(end - start) / LINE_WORDS {
            let (seed, step) = pattern_gen::mode20_line(line as usize, key);
            store_line(line, line_vectors(seed, step));
            line = line.wrapping_add(LINE_WORDS);
        }
    }

    /// Mode 20's verify: four lines a step, each into its own accumulator, then the rest into the
    /// first.
    #[inline(never)]
    pub(super) unsafe fn mode20_verify(c: SimpleMode20) -> u64 {
        let SimpleMode20 { ptr, start, end, key } = c;
        debug_assert!(start.is_multiple_of(LINE_WORDS) && end.is_multiple_of(LINE_WORDS), "mode 20 needs whole lines: {start}..{end}");
        let mut acc = [u64x4::splat(0); 4];
        let mut line = ptr.add(start) as *const u64;
        let lines = (end - start) / LINE_WORDS;
        for _ in 0..lines / 4 {
            for (k, a) in acc.iter_mut().enumerate() {
                let at = line.add(k * LINE_WORDS);
                let (seed, step) = pattern_gen::mode20_line(at as usize, key);
                *a |= line_diff(at, line_vectors(seed, step));
            }
            line = line.wrapping_add(4 * LINE_WORDS);
        }
        for _ in 0..lines % 4 {
            let (seed, step) = pattern_gen::mode20_line(line as usize, key);
            acc[0] |= line_diff(line, line_vectors(seed, step));
            line = line.wrapping_add(LINE_WORDS);
        }
        ((acc[0] | acc[1]) | (acc[2] | acc[3])).reduce_or()
    }

    /// Mode 20's write on TM5's strided walk: the sequential write's lines (and mode 2's strided
    /// image, as the test's doc says).
    #[inline(never)]
    pub(super) unsafe fn mode20_write_str(c: SimpleMode20, period: usize) {
        let SimpleMode20 { ptr, start, end, key } = c;
        for_each_strided_line_at!(ptr, start, end, period, |line| {
            let (seed, step) = pattern_gen::mode20_line(line as usize, key);
            store_line(line, line_vectors(seed, step));
        });
    }

    /// Mode 20's verify on the strided walk.
    #[inline(never)]
    pub(super) unsafe fn mode20_verify_str(c: SimpleMode20, period: usize) -> u64 {
        let SimpleMode20 { ptr, start, end, key } = c;
        let mut acc = u64x4::splat(0);
        for_each_strided_line_at!(ptr, start, end, period, |line| {
            let (seed, step) = pattern_gen::mode20_line(line as usize, key);
            acc |= line_diff(line, line_vectors(seed, step));
        });
        acc.reduce_or()
    }

    /// Mode 30's write: each line the bus-flip line of its address, the beat matched once outside
    /// the loop (`with_mode30_line!`).
    #[inline(never)]
    pub(super) unsafe fn mode30_write(c: SimpleMode30) {
        let SimpleMode30 { ptr, start, end, mode30 } = c;
        debug_assert!(start.is_multiple_of(LINE_WORDS) && end.is_multiple_of(LINE_WORDS), "mode 30 needs whole lines: {start}..{end}");
        with_mode30_line!(mode30, |expected| {
            let mut line = ptr.add(start);
            for _ in 0..(end - start) / LINE_WORDS {
                store_line(line, halves(expected(line)));
                line = line.wrapping_add(LINE_WORDS);
            }
        })
    }

    /// Mode 30's verify: four lines a step, each into its own accumulator, then the rest into the
    /// first.
    #[inline(never)]
    pub(super) unsafe fn mode30_verify(c: SimpleMode30) -> u64 {
        let SimpleMode30 { ptr, start, end, mode30 } = c;
        debug_assert!(start.is_multiple_of(LINE_WORDS) && end.is_multiple_of(LINE_WORDS), "mode 30 needs whole lines: {start}..{end}");
        let mut acc = [u64x4::splat(0); 4];
        let lines = (end - start) / LINE_WORDS;
        with_mode30_line!(mode30, |expected| {
            let mut line = ptr.add(start) as *const u64;
            for _ in 0..lines / 4 {
                for (k, a) in acc.iter_mut().enumerate() {
                    let at = line.add(k * LINE_WORDS);
                    *a |= line_diff(at, halves(expected(at)));
                }
                line = line.wrapping_add(4 * LINE_WORDS);
            }
            for _ in 0..lines % 4 {
                acc[0] |= line_diff(line, halves(expected(line)));
                line = line.wrapping_add(LINE_WORDS);
            }
        });
        ((acc[0] | acc[1]) | (acc[2] | acc[3])).reduce_or()
    }

    /// Mode 30's write on TM5's strided walk.
    #[inline(never)]
    pub(super) unsafe fn mode30_write_str(c: SimpleMode30, period: usize) {
        let SimpleMode30 { ptr, start, end, mode30 } = c;
        with_mode30_line!(mode30, |expected| {
            for_each_strided_line_at!(ptr, start, end, period, |line| {
                store_line(line, halves(expected(line)));
            });
        })
    }

    /// Mode 30's verify on the strided walk.
    #[inline(never)]
    pub(super) unsafe fn mode30_verify_str(c: SimpleMode30, period: usize) -> u64 {
        let SimpleMode30 { ptr, start, end, mode30 } = c;
        let mut acc = u64x4::splat(0);
        with_mode30_line!(mode30, |expected| {
            for_each_strided_line_at!(ptr, start, end, period, |line| {
                acc |= line_diff(line, halves(expected(line)));
            });
        });
        acc.reduce_or()
    }
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
    // No pattern_mode: a native program (pattern spec v2)
    if let Some(program) = config.native_program() {
        return simple_native_scalar(blocks, thread_id, error_mode, timing, config, progress, NativeRun::simple(program, config));
    }
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
    let chunk_seed = |ctx: &ChunkCtx| pattern_gen::block_seed(ctx.ptr as usize, ctx.thread_id, ctx.cycle);

    // A per-word mode `$mode` (`SimpleWords`), its seed per chunk from `$seed`
    macro_rules! run_words {
        ($mode:literal, $seed:expr) => {{
            let seed_of = $seed;
            let words = |ctx: &ChunkCtx| SimpleWords::<$mode> { ptr: ctx.ptr, start: ctx.chunk_start, end: ctx.chunk_end, seed: seed_of(ctx), cl_shift };
            run_phased_test(
                blocks, thread_id, error_mode, timing, config, progress,
                test_name, TestAction::WriteVerify, 1, false, config.test_reps, config.verify_reps,
                // Init: first write
                |ctx: &ChunkCtx| simple_test_v2_loops::words_write(words(ctx)),
                // Test: write patterns (every cycle)
                |ctx: &ChunkCtx| simple_test_v2_loops::words_write(words(ctx)),
                // Verify: four accumulators, then a rescan on a hit
                |ctx: &ChunkCtx| -> u64 { words(ctx).check(test_name) },
            )
        }};
    }

    match pattern_mode {
        // Mode 0 (TM5-faithful): bit dispersion + branchless 4KB page complement
        0 => run_words!(0, chunk_seed),
        // Mode 1 (TM5-faithful): linear step + cache-line complement
        1 => run_words!(1, chunk_seed),
        2 => {
            // Mode 2 (TM5-faithful): TM5's evolving lines, the chain restarted per 4 KiB page
            let mode2 = |ctx: &ChunkCtx| Mode2 { thread_id: ctx.thread_id, cycle: ctx.cycle, param0, param1 };
            let chunk = |ctx: &ChunkCtx| SimpleMode2 { ptr: ctx.ptr, start: ctx.chunk_start, end: ctx.chunk_end, mode2: mode2(ctx) };
            run_phased_test(
                blocks, thread_id, error_mode, timing, config, progress,
                test_name, TestAction::WriteVerify, 1, false, config.test_reps, config.verify_reps,
                |ctx: &ChunkCtx| simple_test_v2_loops::mode2_write(chunk(ctx)),
                |ctx: &ChunkCtx| simple_test_v2_loops::mode2_write(chunk(ctx)),
                |ctx: &ChunkCtx| -> u64 {
                    let seen = simple_test_v2_loops::mode2_verify(chunk(ctx));
                    if seen == 0 {
                        return 0;
                    }
                    mode2(ctx).rescan(ctx.ptr, ctx.chunk_start, ctx.chunk_end, test_name, seen)
                },
            )
        }
        20 => {
            // Mode 20 (TMR-native): mode 2's lines, seed and step hashed from each line's address
            let mode20 = |ctx: &ChunkCtx| Mode20::new(ctx.thread_id, ctx.cycle, param0, param1);
            let chunk = |ctx: &ChunkCtx| SimpleMode20 { ptr: ctx.ptr, start: ctx.chunk_start, end: ctx.chunk_end, key: mode20(ctx).key };
            run_phased_test(
                blocks, thread_id, error_mode, timing, config, progress,
                test_name, TestAction::WriteVerify, 1, false, config.test_reps, config.verify_reps,
                |ctx: &ChunkCtx| simple_test_v2_loops::mode20_write(chunk(ctx)),
                |ctx: &ChunkCtx| simple_test_v2_loops::mode20_write(chunk(ctx)),
                |ctx: &ChunkCtx| -> u64 {
                    let seen = simple_test_v2_loops::mode20_verify(chunk(ctx));
                    if seen == 0 {
                        return 0;
                    }
                    mode20(ctx).rescan(ctx.ptr, ctx.chunk_start, ctx.chunk_end, test_name, seen)
                },
            )
        }
        30 => {
            // Mode 30 (TMR-native): each beat of a line the inverse of the one before
            let mode30 = |ctx: &ChunkCtx| Mode30::new(ctx.thread_id, ctx.cycle, param0, param1);
            let chunk = |ctx: &ChunkCtx| SimpleMode30 { ptr: ctx.ptr, start: ctx.chunk_start, end: ctx.chunk_end, mode30: mode30(ctx) };
            run_phased_test(
                blocks, thread_id, error_mode, timing, config, progress,
                test_name, TestAction::WriteVerify, 1, false, config.test_reps, config.verify_reps,
                |ctx: &ChunkCtx| simple_test_v2_loops::mode30_write(chunk(ctx)),
                |ctx: &ChunkCtx| simple_test_v2_loops::mode30_write(chunk(ctx)),
                |ctx: &ChunkCtx| -> u64 {
                    let c = chunk(ctx);
                    let seen = simple_test_v2_loops::mode30_verify(c);
                    if seen == 0 {
                        return 0;
                    }
                    c.mode30.rescan(ctx.ptr, ctx.chunk_start, ctx.chunk_end, test_name, seen)
                },
            )
        }
        // Mode 11 (TMR-native): inverted constant
        11 => run_words!(11, |_: &ChunkCtx| param0 ^ param1),
        // Mode 13 (TMR-native): a hash per word
        13 => run_words!(13, |_: &ChunkCtx| param0),
        // Mode 10 (TMR-native): address-derived unique (default fallback)
        _ => run_words!(10, |_: &ChunkCtx| param0),
    }
}

/// Strided SimpleTest v2 (TODO 93): TM5's strided SimpleTest (`mtests0.asm` ~336-394), the
/// `for_each_strided_line!` walk. Each step writes or reads a whole 64 B line, then jumps the
/// stride: TM5 Parameter 8 on 2 channels jumps 15 lines, so the period is 16 lines (1 KiB) and
/// each chunk takes 16 interleave passes. The image is the sequential test's: the per-word modes
/// (0, 1, 10, 11, 13) compute each word from its index, and the line modes (2, 20) both write
/// mode 20's lines, hashed from each line's address, as mode 2's chain runs a page in order.
#[doc = include_str!("test_fn_safety.md")]
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
    let period = strided_period(stride);
    let cl_shift = pattern_gen::cache_line_shift(config.cache_line_bytes);

    if matches!(pattern_mode, 2 | 20) {
        let mode20 = |ctx: &ChunkCtx| Mode20::new(ctx.thread_id, ctx.cycle, param0, param1);
        let chunk = |ctx: &ChunkCtx| SimpleMode20 { ptr: ctx.ptr, start: ctx.chunk_start, end: ctx.chunk_end, key: mode20(ctx).key };
        return run_phased_test(
            blocks, thread_id, error_mode, timing, config, progress,
            test_name, TestAction::WriteVerify, 1, false, config.test_reps, config.verify_reps,
            |ctx: &ChunkCtx| simple_test_v2_loops::mode20_write_str(chunk(ctx), period),
            |ctx: &ChunkCtx| simple_test_v2_loops::mode20_write_str(chunk(ctx), period),
            |ctx: &ChunkCtx| -> u64 {
                let seen = simple_test_v2_loops::mode20_verify_str(chunk(ctx), period);
                if seen == 0 {
                    return 0;
                }
                mode20(ctx).rescan(ctx.ptr, ctx.chunk_start, ctx.chunk_end, test_name, seen)
            },
        );
    }
    if pattern_mode == 30 {
        let mode30 = |ctx: &ChunkCtx| Mode30::new(ctx.thread_id, ctx.cycle, param0, param1);
        let chunk = |ctx: &ChunkCtx| SimpleMode30 { ptr: ctx.ptr, start: ctx.chunk_start, end: ctx.chunk_end, mode30: mode30(ctx) };
        return run_phased_test(
            blocks, thread_id, error_mode, timing, config, progress,
            test_name, TestAction::WriteVerify, 1, false, config.test_reps, config.verify_reps,
            |ctx: &ChunkCtx| simple_test_v2_loops::mode30_write_str(chunk(ctx), period),
            |ctx: &ChunkCtx| simple_test_v2_loops::mode30_write_str(chunk(ctx), period),
            |ctx: &ChunkCtx| -> u64 {
                let c = chunk(ctx);
                let seen = simple_test_v2_loops::mode30_verify_str(c, period);
                if seen == 0 {
                    return 0;
                }
                c.mode30.rescan(ctx.ptr, ctx.chunk_start, ctx.chunk_end, test_name, seen)
            },
        );
    }

    // One copy of the loops per pattern (`SimpleWords`'s mode is a constant), so each inlines its
    // pattern function. A `fn` pointer picked at run time was called once per element (`callq
    // *%reg` in the release asm, 2026-10-03). `$seed` gives the chunk's seed for modes 0 and 1,
    // the base for the others.
    macro_rules! run_strided {
        ($mode:literal, $seed:expr) => {{
            let seed_of = $seed;
            let words = |ctx: &ChunkCtx| SimpleWords::<$mode> { ptr: ctx.ptr, start: ctx.chunk_start, end: ctx.chunk_end, seed: seed_of(ctx), cl_shift };
            run_phased_test(
                blocks, thread_id, error_mode, timing, config, progress,
                test_name, TestAction::WriteVerify, 1, false, config.test_reps, config.verify_reps,
                |ctx: &ChunkCtx| simple_test_v2_loops::words_write_str(words(ctx), period),
                |ctx: &ChunkCtx| simple_test_v2_loops::words_write_str(words(ctx), period),
                // Verify: the same walk, a line at a time into 4 accumulators; the walk covers every
                // word of the chunk, so a hit rescans it in order (TODO 89)
                |ctx: &ChunkCtx| -> u64 { words(ctx).check_strided(period, test_name) },
            )
        }};
    }
    let chunk_seed = |ctx: &ChunkCtx| pattern_gen::block_seed(ctx.ptr as usize, ctx.thread_id, ctx.cycle);
    match pattern_mode {
        0 => run_strided!(0, chunk_seed),
        1 => run_strided!(1, chunk_seed),
        11 => run_strided!(11, |_: &ChunkCtx| param0 ^ param1),
        13 => run_strided!(13, |_: &ChunkCtx| param0),
        _ => run_strided!(10, |_: &ChunkCtx| param0),
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
//
// Each width's swaps are a function of their own (`mirror_swap_fn!`, TODO 99 round 3). The whole
// and jump mirrors walk two pointers, each access at one plus a fixed displacement, while the low
// end is below its last step's start: inlined into the test function they addressed every access
// through an index, and with a count instead of the bound the compiler unrolls them and addresses
// the leftover steps through one. The swap itself is a macro passed in (`$swap`), so the unit test
// can record each swap of the same walks.

/// The chunk a mirror swaps: words `start..end` from `ptr`, a test op's `ChunkCtx`.
#[derive(Clone, Copy)]
struct MirrorChunk {
    ptr: *mut u64,
    start: usize,
    end: usize,
}

/// Swap one `$t` at `$a` with one at `$b` (both `*mut u64`).
macro_rules! mirror_swap_pair {
    ($t:ty, $a:expr, $b:expr) => {{
        let (a, b) = ($a as *mut $t, $b as *mut $t);
        let x = *a;
        let y = *b;
        *a = y;
        *b = x;
    }}
}

/// Whole mirror, one `$w`-element vector per swap, four swaps a step. The two ends walk the whole
/// chunk and cross the middle, so every pair is swapped twice and the chunk ends as it began: one
/// round trip, as one pass of TM5 MirrorMove (`mtests0.asm` ~1159-1218, 64 B per step).
macro_rules! mirror_swap_whole {
    ($t:ty, $w:expr, $chunk:expr, $swap:ident) => {{
        let MirrorChunk { ptr, start, end } = $chunk;
        let mut lo = ptr.add(start);
        let mut hi = ptr.add(end - $w);
        let stop = ptr.wrapping_add(end);
        let last = ptr.wrapping_add(end.saturating_sub(3 * $w));
        while lo < last {
            $swap!($t, lo, hi);
            $swap!($t, lo.add($w), hi.sub($w));
            $swap!($t, lo.add(2 * $w), hi.sub(2 * $w));
            $swap!($t, lo.add(3 * $w), hi.sub(3 * $w));
            lo = lo.wrapping_add(4 * $w);
            hi = hi.wrapping_sub(4 * $w);
        }
        while lo < stop {
            $swap!($t, lo, hi);
            lo = lo.wrapping_add($w);
            hi = hi.wrapping_sub($w);
        }
    }}
}

/// Subblocks: the chunk in `$n` equal parts (a literal, 2 or 4), each mirrored as above, all `$n`
/// in lockstep (TM5 MirrorMove Parameter 2 and 4, ~1224-1286 and ~1360-1435). A chunk is a
/// multiple of 4 KiB, so a quarter is a whole number of vectors at any width. Still indexed: the
/// parts are a runtime length apart, and from a pointer for each end of each part the compiler
/// makes bases and shared offsets anyway, in more instructions than from these indexes.
macro_rules! mirror_swap_subblocks {
    ($t:ty, $w:expr, $chunk:expr, $n:literal, $swap:ident) => {{
        let MirrorChunk { ptr, start, end } = $chunk;
        let sub = (end - start) / $n;
        let mut lo = start;
        let mut hi = start + sub - $w;
        for _ in 0..sub / $w {
            for s in 0..$n {
                $swap!($t, ptr.add(lo + s * sub), ptr.add(hi + s * sub));
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
    ($t:ty, $w:expr, $chunk:expr, $jump:expr, $swap:ident) => {{
        const UNIT: usize = 128 / std::mem::size_of::<u64>();
        let MirrorChunk { ptr, start, end } = $chunk;
        let len = end - start;
        // A chunk is a multiple of 4 KiB, so a quarter of it is a whole number of units
        let step = ($jump as usize * UNIT).min(len / 4) + UNIT;
        let stop = ptr.wrapping_add(end);
        for pass in (0..step / UNIT).rev() {
            let mut lo = ptr.add(start + pass * UNIT);
            let mut hi = ptr.add(end - pass * UNIT - UNIT);
            while lo < stop {
                for v in (0..UNIT).step_by($w) {
                    $swap!($t, lo.add(v), hi.add(v));
                }
                lo = lo.wrapping_add(step);
                hi = hi.wrapping_sub(step);
            }
        }
    }}
}

/// One mirror round trip over the chunk in the test's `MirrorMode`, each swap by `$swap`.
macro_rules! mirror_swap {
    ($t:ty, $w:expr, $chunk:expr, $mode:expr, $swap:ident) => {{
        match $mode {
            MirrorMode::Whole => mirror_swap_whole!($t, $w, $chunk, $swap),
            MirrorMode::Subblocks(2) => mirror_swap_subblocks!($t, $w, $chunk, 2, $swap),
            MirrorMode::Subblocks(n) => {
                debug_assert_eq!(n, 4, "MirrorMode parsing admits 2 or 4 subblocks");
                mirror_swap_subblocks!($t, $w, $chunk, 4, $swap)
            }
            MirrorMode::Jump(j) => mirror_swap_jump!($t, $w, $chunk, j, $swap),
        }
    }}
}

/// The function a mirror test calls for one round trip over a chunk: `mirror_swap!` at `$t`,
/// `#[inline(never)]` so its loops are allocated registers on their own, with the variant's
/// target features (`$tf`) so the width holds.
macro_rules! mirror_swap_fn {
    ($name:ident, $t:ty, $w:expr $(, $tf:literal)?) => {
        #[inline(never)]
        $(#[target_feature(enable = $tf)])?
        unsafe fn $name(chunk: MirrorChunk, mode: MirrorMode) {
            mirror_swap!($t, $w, chunk, mode, mirror_swap_pair);
        }
    };
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
            mirror_swap_fn!(swap, $simd_type, $simd_w, $target_feature);
            let test_name = $test_name;
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
                |ctx: &ChunkCtx| swap(MirrorChunk { ptr: ctx.ptr, start: ctx.chunk_start, end: ctx.chunk_end }, mirror),
                // Verify: the seal check, resealing the chunk if it failed (TM5's refill)
                |ctx: &ChunkCtx| -> u64 { mirror_seal_check(seal.kernel, ctx, error_mode) },
            )
        }
    }
}

/// A mirror test's verify (TODO 74): the seal check after the mirror, its errors the test's own
/// (TM5 numbers them by the mirror step), and the chunk resealed if it failed, unless the error
/// halts the run: then the bad words stay in memory.
#[inline(always)]
unsafe fn mirror_seal_check(kernel: crate::seal::SealKernel, ctx: &ChunkCtx, error_mode: ErrorMode) -> u64 {
    let errors = kernel.verify(ctx.ptr, ctx.chunk_start, ctx.chunk_end, "seal check after");
    if errors > 0 && !matches!(error_mode, ErrorMode::Halt) {
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
    mirror_swap_fn!(swap, u64, 1);
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
        // Test: one mirror round trip, a u64 per swap
        |ctx: &ChunkCtx| swap(MirrorChunk { ptr: ctx.ptr, start: ctx.chunk_start, end: ctx.chunk_end }, mirror),
        // Verify: the seal check, resealing the chunk if it failed (TM5's refill)
        |ctx: &ChunkCtx| -> u64 { mirror_seal_check(seal.kernel, ctx, error_mode) },
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

// ─── SIMD SimpleTest v2: the loops, a module a width (TODO 99) ──────────────────────────────────
//
// SimpleTest's SIMD paths are positional (modes 10 and 11, and SimpleNT) and mode 20, each
// sequential and strided. Each width's writes and verifies are functions of their own, stamped per
// width by `simple_simd_loops!` with the width's `#[target_feature]` (Rule 4), each taking its
// chunk as a `Copy` struct by value and called once a chunk. In the test functions, with the
// harness's chunks, seal and stats live too, the loops were indexed (`(%base,%index,8)`), and
// their verifies had one accumulator. Here each walks a pointer, addressing at fixed
// displacements from it, and each sequential verify keeps four accumulators. The master macro
// `simple_test_v2_impl!` makes the test functions.

/// SimpleTest's positional pattern on words `[start, end)` of `ptr` at a SIMD width (modes 10 and
/// 11, and SimpleNT): word `i` holds `i ^ base`. The words are whole vectors.
#[derive(Clone, Copy)]
struct SimplePositional {
    ptr: *mut u64,
    start: usize,
    end: usize,
    base: u64,
}

/// Mode 20 on the whole lines of words `[start, end)` of `ptr`, in the SIMD builds and the scalar
/// one: a line holds `seed + j * step` in word `j`, both hashed from its address and `key`
/// (`pattern_gen::mode20_line`).
#[derive(Clone, Copy)]
struct SimpleMode20 {
    ptr: *mut u64,
    start: usize,
    end: usize,
    key: u64,
}

/// Stamps a width's SimpleTest loops into module `$module`: `$simd_type` vectors of `$w` words,
/// lanes `$lane_offsets`, NT stores through `$stream_fn` on `$arch_type`. Each verify returns its
/// accumulators ORed, nonzero when a word differs; the caller rescans.
macro_rules! simple_simd_loops {
    ($module:ident, $simd_type:ty, $w:expr, $lane_offsets:expr, $arch_type:ty, $stream_fn:path, $target_feature:literal) => {
        mod $module {
            use super::*;

            type V = $simd_type;
            const W: usize = $w;

            /// The positional write: `i ^ base` to each word, four vectors a step, then the rest.
            #[target_feature(enable = $target_feature)]
            #[inline(never)]
            pub(super) unsafe fn pos_write(c: SimplePositional) {
                let SimplePositional { ptr, start, end, base } = c;
                debug_assert!((end - start).is_multiple_of(W), "chunk not aligned to SIMD width");
                let base = V::splat(base);
                let (w1, w2, w3, w4) = (V::splat(W as u64), V::splat(2 * W as u64), V::splat(3 * W as u64), V::splat(4 * W as u64));
                let mut idx = V::splat(start as u64) + V::from_array($lane_offsets);
                let mut p = ptr.add(start) as *mut V;
                let vectors = (end - start) / W;
                // Four a step and the rest, so the compiler's own unroll leaves no indexed remainder
                for _ in 0..vectors / 4 {
                    *p = idx ^ base;
                    *p.add(1) = (idx + w1) ^ base;
                    *p.add(2) = (idx + w2) ^ base;
                    *p.add(3) = (idx + w3) ^ base;
                    idx += w4;
                    p = p.wrapping_add(4);
                }
                for _ in 0..vectors % 4 {
                    *p = idx ^ base;
                    idx += w1;
                    p = p.wrapping_add(1);
                }
            }

            /// The positional write in non-temporal stores (SimpleNT), four a step (the manual
            /// unroll, `doc/nt_stores.md`), then the rest one at a time, and an SFENCE.
            #[target_feature(enable = $target_feature)]
            #[inline(never)]
            pub(super) unsafe fn pos_write_nt(c: SimplePositional) {
                let SimplePositional { ptr, start, end, base } = c;
                let base = V::splat(base);
                let (w1, w2, w3, w4) = (V::splat(W as u64), V::splat(2 * W as u64), V::splat(3 * W as u64), V::splat(4 * W as u64));
                let mut idx = V::splat(start as u64) + V::from_array($lane_offsets);
                let mut p = ptr.add(start);
                let vectors = (end - start) / W;
                for _ in 0..vectors / 4 {
                    let (v0, v1, v2, v3) = (idx ^ base, (idx + w1) ^ base, (idx + w2) ^ base, (idx + w3) ^ base);
                    $stream_fn(p as *mut $arch_type, std::mem::transmute::<V, $arch_type>(v0));
                    $stream_fn(p.add(W) as *mut $arch_type, std::mem::transmute::<V, $arch_type>(v1));
                    $stream_fn(p.add(2 * W) as *mut $arch_type, std::mem::transmute::<V, $arch_type>(v2));
                    $stream_fn(p.add(3 * W) as *mut $arch_type, std::mem::transmute::<V, $arch_type>(v3));
                    idx += w4;
                    p = p.wrapping_add(4 * W);
                }
                for _ in 0..vectors % 4 {
                    $stream_fn(p as *mut $arch_type, std::mem::transmute::<V, $arch_type>(idx ^ base));
                    idx += w1;
                    p = p.wrapping_add(W);
                }
                std::arch::x86_64::_mm_sfence();
            }

            /// The positional verify: four vectors a step into four accumulators, then the rest
            /// into the first.
            #[target_feature(enable = $target_feature)]
            #[inline(never)]
            pub(super) unsafe fn pos_verify(c: SimplePositional) -> u64 {
                let SimplePositional { ptr, start, end, base } = c;
                debug_assert!((end - start).is_multiple_of(W), "chunk not aligned to SIMD width");
                let base = V::splat(base);
                let (w1, w2, w3, w4) = (V::splat(W as u64), V::splat(2 * W as u64), V::splat(3 * W as u64), V::splat(4 * W as u64));
                let mut idx = V::splat(start as u64) + V::from_array($lane_offsets);
                let mut p = ptr.add(start) as *const V;
                let mut acc = [V::splat(0); 4];
                let vectors = (end - start) / W;
                for _ in 0..vectors / 4 {
                    acc[0] |= *p ^ (idx ^ base);
                    acc[1] |= *p.add(1) ^ ((idx + w1) ^ base);
                    acc[2] |= *p.add(2) ^ ((idx + w2) ^ base);
                    acc[3] |= *p.add(3) ^ ((idx + w3) ^ base);
                    idx += w4;
                    p = p.wrapping_add(4);
                }
                for _ in 0..vectors % 4 {
                    acc[0] |= *p ^ (idx ^ base);
                    idx += w1;
                    p = p.wrapping_add(1);
                }
                ((acc[0] | acc[1]) | (acc[2] | acc[3])).reduce_or()
            }

            /// The positional write on TM5's strided walk (TODO 93): each line `i ^ base` in
            /// `W`-word stores.
            #[target_feature(enable = $target_feature)]
            #[inline(never)]
            pub(super) unsafe fn pos_write_str(c: SimplePositional, period: usize) {
                let SimplePositional { ptr, start, end, base } = c;
                let base = V::splat(base);
                // Each vector's lanes, from the line's first word
                let lanes: [V; LINE_WORDS / W] = std::array::from_fn(|j| V::from_array($lane_offsets) + V::splat((j * W) as u64));
                for_each_strided_line_at!(ptr, start, end, period, |line, offset| {
                    let at = V::splat(offset as u64);
                    for (j, &lane) in lanes.iter().enumerate() {
                        *(line.add(j * W) as *mut V) = (at + lane) ^ base;
                    }
                });
            }

            /// The positional verify on the strided walk: a line a step, in the walk's order, its
            /// vectors into the accumulators in turn (four at 128 bits, two at 256, one at 512,
            /// where a line is one vector). Four lines a step would take the period, a run-time
            /// value, as an index register.
            #[target_feature(enable = $target_feature)]
            #[inline(never)]
            pub(super) unsafe fn pos_verify_str(c: SimplePositional, period: usize) -> u64 {
                let SimplePositional { ptr, start, end, base } = c;
                let base = V::splat(base);
                let lanes: [V; LINE_WORDS / W] = std::array::from_fn(|j| V::from_array($lane_offsets) + V::splat((j * W) as u64));
                let mut acc = [V::splat(0); 4];
                for_each_strided_line_at!(ptr, start, end, period, |line, offset| {
                    let at = V::splat(offset as u64);
                    for (j, &lane) in lanes.iter().enumerate() {
                        acc[j % 4] |= *(line.add(j * W) as *const V) ^ ((at + lane) ^ base);
                    }
                });
                ((acc[0] | acc[1]) | (acc[2] | acc[3])).reduce_or()
            }

            /// The mode 20 write: each line from its address's hash, in `W`-word stores.
            #[target_feature(enable = $target_feature)]
            #[inline(never)]
            pub(super) unsafe fn m20_write(c: SimpleMode20) {
                let SimpleMode20 { ptr, start, end, key } = c;
                let lanes = V::from_array($lane_offsets);
                let mut line = ptr.add(start);
                for _ in 0..(end - start) / LINE_WORDS {
                    let (seed, step) = pattern_gen::mode20_line(line as usize, key);
                    let advance = V::splat(step.wrapping_mul(W as u64));
                    let mut words = V::splat(seed) + V::splat(step) * lanes;
                    for j in 0..LINE_WORDS / W {
                        *(line.add(j * W) as *mut V) = words;
                        words += advance;
                    }
                    line = line.wrapping_add(LINE_WORDS);
                }
            }

            /// The mode 20 verify: four lines a step, each into its own accumulator, then the rest
            /// into the first.
            #[target_feature(enable = $target_feature)]
            #[inline(never)]
            pub(super) unsafe fn m20_verify(c: SimpleMode20) -> u64 {
                let SimpleMode20 { ptr, start, end, key } = c;
                let lanes = V::from_array($lane_offsets);
                let mut acc = [V::splat(0); 4];
                let mut line = ptr.add(start) as *const u64;
                let lines = (end - start) / LINE_WORDS;
                for _ in 0..lines / 4 {
                    for (k, a) in acc.iter_mut().enumerate() {
                        let at = line.add(k * LINE_WORDS);
                        let (seed, step) = pattern_gen::mode20_line(at as usize, key);
                        let advance = V::splat(step.wrapping_mul(W as u64));
                        let mut words = V::splat(seed) + V::splat(step) * lanes;
                        for j in 0..LINE_WORDS / W {
                            *a |= *(at.add(j * W) as *const V) ^ words;
                            words += advance;
                        }
                    }
                    line = line.wrapping_add(4 * LINE_WORDS);
                }
                for _ in 0..lines % 4 {
                    let (seed, step) = pattern_gen::mode20_line(line as usize, key);
                    let advance = V::splat(step.wrapping_mul(W as u64));
                    let mut words = V::splat(seed) + V::splat(step) * lanes;
                    for j in 0..LINE_WORDS / W {
                        acc[0] |= *(line.add(j * W) as *const V) ^ words;
                        words += advance;
                    }
                    line = line.wrapping_add(LINE_WORDS);
                }
                ((acc[0] | acc[1]) | (acc[2] | acc[3])).reduce_or()
            }

            /// The mode 20 write on TM5's strided walk: the sequential write's lines.
            #[target_feature(enable = $target_feature)]
            #[inline(never)]
            pub(super) unsafe fn m20_write_str(c: SimpleMode20, period: usize) {
                let SimpleMode20 { ptr, start, end, key } = c;
                let lanes = V::from_array($lane_offsets);
                for_each_strided_line_at!(ptr, start, end, period, |line| {
                    let (seed, step) = pattern_gen::mode20_line(line as usize, key);
                    let advance = V::splat(step.wrapping_mul(W as u64));
                    let mut words = V::splat(seed) + V::splat(step) * lanes;
                    for j in 0..LINE_WORDS / W {
                        *(line.add(j * W) as *mut V) = words;
                        words += advance;
                    }
                });
            }

            /// The mode 20 verify on the strided walk: a line a step, its vectors into the
            /// accumulators in turn, as `pos_verify_str`.
            #[target_feature(enable = $target_feature)]
            #[inline(never)]
            pub(super) unsafe fn m20_verify_str(c: SimpleMode20, period: usize) -> u64 {
                let SimpleMode20 { ptr, start, end, key } = c;
                let lanes = V::from_array($lane_offsets);
                let mut acc = [V::splat(0); 4];
                for_each_strided_line_at!(ptr, start, end, period, |line| {
                    let (seed, step) = pattern_gen::mode20_line(line as usize, key);
                    let advance = V::splat(step.wrapping_mul(W as u64));
                    let mut words = V::splat(seed) + V::splat(step) * lanes;
                    for j in 0..LINE_WORDS / W {
                        acc[j % 4] |= *(line.add(j * W) as *const V) ^ words;
                        words += advance;
                    }
                });
                ((acc[0] | acc[1]) | (acc[2] | acc[3])).reduce_or()
            }
        }
    };
}

simple_simd_loops!(simple_test_v2_128_loops, u64x2, 2, [0, 1],
    std::arch::x86_64::__m128i, std::arch::x86_64::_mm_stream_si128,
    "sse4.2,sse4.1,ssse3,sse3,sse2,popcnt");
simple_simd_loops!(simple_test_v2_256_loops, u64x4, 4, [0, 1, 2, 3],
    std::arch::x86_64::__m256i, std::arch::x86_64::_mm256_stream_si256,
    "avx2,avx,fma,bmi1,bmi2");
simple_simd_loops!(simple_test_v2_512_loops, u64x8, 8, [0, 1, 2, 3, 4, 5, 6, 7],
    std::arch::x86_64::__m512i, std::arch::x86_64::_mm512_stream_si512,
    "avx512f,avx512bw,avx512cd,avx512dq,avx512vl,avx2,avx,fma,bmi1,bmi2");

/// A positional verify of a chunk (modes 10 and 11, SimpleNT): the width's `$verify` on the chunk,
/// or on each batch of `check_mask + 1` vectors when an `ErrorCheckInterval` asks for one (none
/// does yet), and the cold rescan of each chunk or batch whose accumulators saw a difference.
macro_rules! simple_verify_positional {
    ($verify:path, $simd_w:expr, $ctx:expr, $base:expr, $test_name:expr) => {{
        let ctx: &ChunkCtx = $ctx;
        let base: u64 = $base;
        let mut total_errors = 0u64;
        let mut check = |start: usize, end: usize| {
            let seen = $verify(SimplePositional { ptr: ctx.ptr, start, end, base });
            if seen != 0 {
                total_errors += rescan_words(ctx.ptr, start, end, $test_name, seen, |idx| idx as u64 ^ base);
            }
        };
        match ctx.check_mask {
            Some(check_mask) => {
                let batch_elements = (check_mask as usize + 1) * $simd_w;
                let mut pos = ctx.chunk_start;
                let aligned_end = ctx.chunk_end - (ctx.chunk_end - ctx.chunk_start) % batch_elements;
                while pos < aligned_end {
                    check(pos, pos + batch_elements);
                    pos += batch_elements;
                }
                if pos < ctx.chunk_end {
                    check(pos, ctx.chunk_end);
                }
            }
            None => check(ctx.chunk_start, ctx.chunk_end),
        }
        total_errors
    }}
}

/// Master macro: stamps out a complete SIMD SimpleTest v2 function for a given width.
/// Two pattern paths: positional (idx^base, or idx^combined for modes 1/11) and mode 20.
/// Generates 4 specialized `#[target_feature]` functions (one per pattern/stride combo)
/// plus a lightweight dispatch function. This prevents LLVM from inlining all 4 code paths
/// into one mega-function, which caused 9K+ lines of assembly, 46 `vzeroupper` calls,
/// and instruction cache pressure that made wider SIMD slower than narrower.
///
/// Each specialized function contains exactly ONE run_phased_test call with its closures,
/// producing tight, focused assembly, and is `#[inline(never)]`: the 128-bit ones' features are
/// the baseline's, so the compiler inlined all four into the dispatch's caller. The dispatch
/// function has NO `#[target_feature]` so it compiles to a simple branch without pulling in SIMD
/// register state.
macro_rules! simple_test_v2_impl {
    (
        $dispatch_fn:ident,
        $pos_seq_fn:ident,
        $pos_str_fn:ident,
        $m20_seq_fn:ident,
        $m20_str_fn:ident,
        $native_fn:ident,
        $test_name:literal,
        $simd_w:expr,
        $loops:ident,
        $target_feature:literal
    ) => {
        /// Positional sequential — contiguous access with idx^base pattern.
        #[target_feature(enable = $target_feature)]
        #[inline(never)]
        unsafe fn $pos_seq_fn(
            blocks: &[crate::runner::AllocationBlock],
            thread_id: usize,
            error_mode: ErrorMode,
            timing: &TestTiming,
            config: &TestMemoryConfig,
            progress: Option<&TestProgress>,
        ) -> TestStats {
            let test_name = $test_name;
            let pattern_mode = config.pattern_mode.unwrap_or(0);
            let param0 = config.pattern_param0.unwrap_or(0xDEADBEEFDEADBEEF);
            let param1 = config.pattern_param1.unwrap_or(0xCAFEBABECAFEBABE);
            let base = match pattern_mode {
                1 | 11 => param0 ^ param1,
                _ => param0,
            };
            let chunk = |ctx: &ChunkCtx| SimplePositional { ptr: ctx.ptr, start: ctx.chunk_start, end: ctx.chunk_end, base };

            run_phased_test(
                blocks, thread_id, error_mode, timing, config, progress,
                test_name, TestAction::WriteVerify, 1, false, config.test_reps, config.verify_reps,
                |ctx: &ChunkCtx| $loops::pos_write(chunk(ctx)),
                |ctx: &ChunkCtx| $loops::pos_write(chunk(ctx)),
                |ctx: &ChunkCtx| -> u64 { simple_verify_positional!($loops::pos_verify, $simd_w, ctx, base, test_name) },
            )
        }

        /// Positional strided — TM5's strided walk with the idx^base pattern (TODO 93).
        #[target_feature(enable = $target_feature)]
        #[inline(never)]
        unsafe fn $pos_str_fn(
            blocks: &[crate::runner::AllocationBlock],
            thread_id: usize,
            error_mode: ErrorMode,
            timing: &TestTiming,
            config: &TestMemoryConfig,
            progress: Option<&TestProgress>,
        ) -> TestStats {
            let test_name = $test_name;
            let pattern_mode = config.pattern_mode.unwrap_or(0);
            let param0 = config.pattern_param0.unwrap_or(0xDEADBEEFDEADBEEF);
            let param1 = config.pattern_param1.unwrap_or(0xCAFEBABECAFEBABE);
            let period = strided_period(config.parameter_context.as_ref()
                .and_then(|ctx| ctx.stride_elements)
                .unwrap_or(0));
            let base = match pattern_mode {
                1 | 11 => param0 ^ param1,
                _ => param0,
            };
            let chunk = |ctx: &ChunkCtx| SimplePositional { ptr: ctx.ptr, start: ctx.chunk_start, end: ctx.chunk_end, base };

            run_phased_test(
                blocks, thread_id, error_mode, timing, config, progress,
                test_name, TestAction::WriteVerify, 1, false, config.test_reps, config.verify_reps,
                |ctx: &ChunkCtx| $loops::pos_write_str(chunk(ctx), period),
                |ctx: &ChunkCtx| $loops::pos_write_str(chunk(ctx), period),
                |ctx: &ChunkCtx| -> u64 {
                    let seen = $loops::pos_verify_str(chunk(ctx), period);
                    if seen == 0 {
                        return 0;
                    }
                    rescan_words(ctx.ptr, ctx.chunk_start, ctx.chunk_end, test_name, seen, |idx| idx as u64 ^ base)
                },
            )
        }

        /// Mode 20 sequential — contiguous access, each line hashed from its address.
        #[target_feature(enable = $target_feature)]
        #[inline(never)]
        unsafe fn $m20_seq_fn(
            blocks: &[crate::runner::AllocationBlock],
            thread_id: usize,
            error_mode: ErrorMode,
            timing: &TestTiming,
            config: &TestMemoryConfig,
            progress: Option<&TestProgress>,
        ) -> TestStats {
            let test_name = $test_name;
            let param0 = config.pattern_param0.unwrap_or(0xDEADBEEFDEADBEEF);
            let param1 = config.pattern_param1.unwrap_or(0xCAFEBABECAFEBABE);
            let mode20 = |ctx: &ChunkCtx| Mode20::new(ctx.thread_id, ctx.cycle, param0, param1);
            let chunk = |ctx: &ChunkCtx| SimpleMode20 { ptr: ctx.ptr, start: ctx.chunk_start, end: ctx.chunk_end, key: mode20(ctx).key };

            run_phased_test(
                blocks, thread_id, error_mode, timing, config, progress,
                test_name, TestAction::WriteVerify, 1, false, config.test_reps, config.verify_reps,
                |ctx: &ChunkCtx| $loops::m20_write(chunk(ctx)),
                |ctx: &ChunkCtx| $loops::m20_write(chunk(ctx)),
                |ctx: &ChunkCtx| -> u64 {
                    let seen = $loops::m20_verify(chunk(ctx));
                    if seen == 0 {
                        return 0;
                    }
                    mode20(ctx).rescan(ctx.ptr, ctx.chunk_start, ctx.chunk_end, test_name, seen)
                },
            )
        }

        /// Mode 20 strided — TM5's strided walk, the same image as sequential (TODO 93).
        #[target_feature(enable = $target_feature)]
        #[inline(never)]
        unsafe fn $m20_str_fn(
            blocks: &[crate::runner::AllocationBlock],
            thread_id: usize,
            error_mode: ErrorMode,
            timing: &TestTiming,
            config: &TestMemoryConfig,
            progress: Option<&TestProgress>,
        ) -> TestStats {
            let test_name = $test_name;
            let param0 = config.pattern_param0.unwrap_or(0xDEADBEEFDEADBEEF);
            let param1 = config.pattern_param1.unwrap_or(0xCAFEBABECAFEBABE);
            let period = strided_period(config.parameter_context.as_ref()
                .and_then(|ctx| ctx.stride_elements)
                .unwrap_or(0));
            let mode20 = |ctx: &ChunkCtx| Mode20::new(ctx.thread_id, ctx.cycle, param0, param1);
            let chunk = |ctx: &ChunkCtx| SimpleMode20 { ptr: ctx.ptr, start: ctx.chunk_start, end: ctx.chunk_end, key: mode20(ctx).key };

            run_phased_test(
                blocks, thread_id, error_mode, timing, config, progress,
                test_name, TestAction::WriteVerify, 1, false, config.test_reps, config.verify_reps,
                |ctx: &ChunkCtx| $loops::m20_write_str(chunk(ctx), period),
                |ctx: &ChunkCtx| $loops::m20_write_str(chunk(ctx), period),
                |ctx: &ChunkCtx| -> u64 {
                    let seen = $loops::m20_verify_str(chunk(ctx), period);
                    if seen == 0 {
                        return 0;
                    }
                    mode20(ctx).rescan(ctx.ptr, ctx.chunk_start, ctx.chunk_end, test_name, seen)
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
            // No pattern_mode: a native program, at this width
            if let Some(program) = config.native_program() {
                return $native_fn(blocks, thread_id, error_mode, timing, config, progress, NativeRun::simple(program, config));
            }
            // The SIMD paths here are positional (modes 10 and 11) and mode 20. Every other mode
            // runs the scalar build, which writes that mode's own image: modes 0, 1, 2 and 13 used
            // to write mode 10's here (TODO 3), and mode 30's lines are a few values (one and its
            // inverse at 16, 32 and 64 bits, three at 12 and 24), which the scalar build already
            // stores at the baseline's full width.
            if matches!(config.pattern_mode, Some(0 | 1 | 2 | 13 | 30)) {
                return simple_test_v2_multi(blocks, thread_id, error_mode, timing, config, progress);
            }
            let is_mode20 = config.pattern_mode.unwrap_or(0) == 20;
            let has_stride = config.parameter_context.as_ref()
                .and_then(|ctx| ctx.stride_elements)
                .map_or(false, |s| s > 0);

            match (is_mode20, has_stride) {
                (true, true)   => $m20_str_fn(blocks, thread_id, error_mode, timing, config, progress),
                (true, false)  => $m20_seq_fn(blocks, thread_id, error_mode, timing, config, progress),
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
    simple_test_v2_128_m20_seq, simple_test_v2_128_m20_str, simple_native_128,
    "Mem-SimpleV2-128", 2, simple_test_v2_128_loops,
    "sse4.2,sse4.1,ssse3,sse3,sse2,popcnt");
simple_test_v2_impl!(simple_test_v2_256_impl,
    simple_test_v2_256_pos_seq, simple_test_v2_256_pos_str,
    simple_test_v2_256_m20_seq, simple_test_v2_256_m20_str, simple_native_256,
    "Mem-SimpleV2-256", 4, simple_test_v2_256_loops,
    "avx2,avx,fma,bmi1,bmi2");
simple_test_v2_impl!(simple_test_v2_512_impl,
    simple_test_v2_512_pos_seq, simple_test_v2_512_pos_str,
    simple_test_v2_512_m20_seq, simple_test_v2_512_m20_str, simple_native_512,
    "Mem-SimpleV2-512", 8, simple_test_v2_512_loops,
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
// SIMD variants handle both sequential and strided access (TM5's strided walk, TODO 93).
crate::auto_dispatch!(
    pub simple_test_v2_auto_multi,
    simple_test_v2_multi,
    simple_test_v2_128_multi,
    simple_test_v2_256_multi,
    simple_test_v2_512_multi
);

// ─── SimpleTest on the native patterns (pattern spec v2, `crate::pattern`) ──────────────────────
//
// A SimpleTest whose config names no `pattern_mode` runs a native program: each write round
// writes one phase of it (a family, its pair's key, the polarity) and its verifies check that
// phase. The scalar kernel takes each line from the line functions (`with_phase_line!`), the SIMD
// ones from their vector forms at the variant's width (`with_phase_vectors!`, so the 128-bit
// variant stays 128-bit); tests check every width writes the same image. Per chunk the phase is
// computed once and its family matched once, outside the loops, and the accumulators are checked
// once, at the chunk's end: `ErrorCheckInterval` isn't read.

/// How a native SimpleTest runs: its program, and SimpleTest's write-and-verify or one side of a
/// Bench pair.
#[derive(Clone, Copy)]
struct NativeRun {
    program: crate::pattern::Program,
    action: TestAction,
    test_reps: u32,
    verify_reps: u32,
    /// Every round is round 0 (pair 0's P), whatever the cycle: a Bench pair's image
    pinned: bool,
}

impl NativeRun {
    fn simple(program: crate::pattern::Program, config: &TestMemoryConfig) -> Self {
        NativeRun { program, action: TestAction::WriteVerify, test_reps: config.test_reps, verify_reps: config.verify_reps, pinned: false }
    }
}

/// One native SimpleTest walk over a chunk: words `[start, end)` of `base`, in whole lines, its
/// first line line id `lid0`, each line `phase`'s. Sequential, or TM5's strided walk
/// (`native_strided_walk!`) when `period`, its period in words, isn't 0. Each walk at each width
/// is a function of its own (`native_simple_impl!`'s walks module), called once a chunk, so its
/// loops keep every value in registers (TODO 99): in the per-chunk closures, with both walks of
/// every family and beat in one function, values spilled to the stack in loops of a dozen
/// instructions.
#[derive(Clone, Copy)]
struct NativeChunk {
    base: *mut u64,
    start: usize,
    end: usize,
    lid0: u64,
    period: usize,
    phase: crate::pattern::Phase,
}

/// The walked pointer of a native walk's next step, hidden from the compiler's loop strength
/// reduction (TODO 99). In view, it rewrote the strided walk's pointer and line id as four
/// induction variables (a prefetch pointer, an offset from a base, the line id and the id times
/// 8), each with its own step in a register, and the lines that hash with 64-bit constants
/// spilled; and it made the 512-bit `random` verify's pointer and word id one index, every load
/// `(%base,%index,8)`. An empty `asm!` with the pointer in a register: no instruction, no flags,
/// and it writes no memory (it says it may read it, as an asm given a pointer must).
#[inline(always)]
fn native_opaque_step(mut p: *mut u64) -> *mut u64 {
    // SAFETY: the template is a comment: the register comes out as it went in
    unsafe { std::arch::asm!("/* {0} */", inout(reg) p, options(pure, readonly, nostack, preserves_flags)) };
    p
}

/// TM5's strided walk as `for_each_strided_line!` takes it, the same lines in the same order and
/// the same line prefetched at each step (by `$prefetch`, `prefetch_line` but in a test), on a
/// walked pointer: `$body` runs with `$p` the line's first word and `$lid` its line id, from
/// `$lid0` at `$start`. Each pass is two loops, split at the step where its prefetches go from on
/// in the pass to on into the next, so no step selects: in one loop with an index, its offset,
/// end, step and the prefetch's two offsets didn't all fit in registers (TODO 99). Each loop runs
/// to a pointer bound, as the old one ran to its end: with a count, the compiler unrolled them,
/// and the unrolled loops spilled. The line id comes from the pointer, so a step carries one value.
macro_rules! native_strided_walk {
    ($prefetch:expr, $base:expr, $start:expr, $end:expr, $period:expr, $lid0:expr, |$p:ident, $lid:ident| $body:block) => {{
        let (base, start, end, period, lid0): (*mut u64, usize, usize, usize, u64) = ($base, $start, $end, $period, $lid0);
        debug_assert!(period.is_multiple_of(LINE_WORDS) && start.is_multiple_of(LINE_WORDS) && end.is_multiple_of(LINE_WORDS),
                      "the strided walk needs whole lines: {start}..{end} by {period}");
        // A line's id is its address's distance from `at` in lines: `lid0` at `start`, exact while
        // ids are under 2^58 (thread ids under 2^21, `pattern::line_base`)
        debug_assert!(lid0 + ((end - start) / LINE_WORDS) as u64 <= 1 << 58, "line ids from {lid0} past 2^58");
        let at = (base.wrapping_add(start) as u64).wrapping_sub(lid0 << 6);
        let ahead = strided_lookahead(start, end, period);
        let stop = base.wrapping_add(end);
        // Each pass's first line, from the last line of the first period down
        let mut first = base.wrapping_add(start + period);
        for _ in 0..period / LINE_WORDS {
            first = first.wrapping_sub(LINE_WORDS);
            let mut $p = first;
            // While the line `ahead` words on is in the chunk, `strided_ahead` takes it; from there it
            // steps back `strided_wrap` words from it, into the next pass
            while $p.wrapping_add(ahead) < stop {
                $prefetch($p.wrapping_add(ahead));
                let $lid = ($p as u64).wrapping_sub(at) >> 6;
                $body
                $p = native_opaque_step($p.wrapping_add(period));
            }
            // The pass's words to the chunk's end, as `strided_wrap` takes them
            let left = (stop as usize).saturating_sub(first as usize) / std::mem::size_of::<u64>();
            let back = ahead.wrapping_sub(strided_wrap(0, left, period));
            while $p < stop {
                // The walk's last steps prefetch off the chunk: a wasted prefetch, not a fault
                $prefetch($p.wrapping_add(back));
                let $lid = ($p as u64).wrapping_sub(at) >> 6;
                $body
                $p = native_opaque_step($p.wrapping_add(period));
            }
        }
    }};
}

/// The line type of a native walk at a width: `[scalar]` one array of words, `[u64xN, N]` vectors.
macro_rules! native_line_ty {
    ([scalar]) => { [u64; LINE_WORDS] };
    ([$simd:ty, $w:tt]) => { [$simd; LINE_WORDS / $w] };
}

/// Runs `$body` with `$line` the line function of `$phase` at a width (`with_phase_line!` for
/// `[scalar]`, `with_phase_vectors!` at `N` lanes for `[u64xN, N]`): its family and beat matched
/// once, outside the walk.
macro_rules! native_with_line {
    ([scalar], $phase:expr, |$line:ident| $body:block) => {
        crate::pattern::with_phase_line!($phase, |$line| $body)
    };
    ([$simd:ty, $w:tt], $phase:expr, |$line:ident| $body:block) => {
        crate::pattern::with_phase_vectors!($w, $phase, |$line| $body)
    };
}

/// The sequential write of `$c`, a `NativeChunk`, each line `$line`'s: from the chunk's start, a
/// pointer walked a line a step, to the chunk's end as the old loop ran (with a count, the
/// compiler vectorized the scalar walks across lines, and spilled their constants). `scalar`
/// stores each line as one array; `[u64xN, N]` stores it as vectors of that width.
macro_rules! native_write_seq {
    ([scalar], $c:expr, $line:expr) => {{
        let (NativeChunk { base, start, end, lid0, .. }, line) = ($c, $line);
        let (mut p, stop) = (base.add(start), base.wrapping_add(end));
        let mut lid = lid0;
        while p < stop {
            *(p as *mut [u64; LINE_WORDS]) = line(lid);
            lid += 1;
            p = native_opaque_step(p.wrapping_add(LINE_WORDS));
        }
    }};
    ([$simd:ty, $w:tt], $c:expr, $line:expr) => {{
        let (NativeChunk { base, start, end, lid0, .. }, line) = ($c, $line);
        let (mut p, stop) = (base.add(start), base.wrapping_add(end));
        let mut lid = lid0;
        while p < stop {
            for (v, x) in line(lid).into_iter().enumerate() {
                *(p.add(v * $w) as *mut $simd) = x;
            }
            lid += 1;
            p = native_opaque_step(p.wrapping_add(LINE_WORDS));
        }
    }};
}

/// The strided write of `$c` (see `native_write_seq`): TM5's walk, `native_strided_walk!`.
macro_rules! native_write_strided {
    ([scalar], $c:expr, $line:expr) => {{
        let (NativeChunk { base, start, end, lid0, period, .. }, line) = ($c, $line);
        native_strided_walk!(prefetch_line, base, start, end, period, lid0, |p, lid| {
            *(p as *mut [u64; LINE_WORDS]) = line(lid);
        });
    }};
    ([$simd:ty, $w:tt], $c:expr, $line:expr) => {{
        let (NativeChunk { base, start, end, lid0, period, .. }, line) = ($c, $line);
        native_strided_walk!(prefetch_line, base, start, end, period, lid0, |p, lid| {
            for (v, x) in line(lid).into_iter().enumerate() {
                *(p.add(v * $w) as *mut $simd) = x;
            }
        });
    }};
}

/// The differences of `$c`'s words from `$line`'s on the sequential walk (see `native_write_seq`),
/// ORed: 0 when every word holds. Each word's difference goes into 4 accumulators, one branch at
/// the end. The SIMD walk takes 4 vectors a step (a group of lines), so the 4 accumulators are
/// independent chains at every width, and the lines past the last whole group one at a time.
macro_rules! native_differs_seq {
    ([scalar], $c:expr, $line:expr) => {{
        let (NativeChunk { base, start, end, lid0, .. }, line) = ($c, $line);
        let mut acc = [0u64; 4];
        let (mut p, stop) = (base.add(start), base.wrapping_add(end));
        let mut lid = lid0;
        while p < stop {
            accumulate_line(&mut acc, p, &line(lid));
            lid += 1;
            p = native_opaque_step(p.wrapping_add(LINE_WORDS));
        }
        (acc[0] | acc[1]) | (acc[2] | acc[3])
    }};
    ([$simd:ty, $w:tt], $c:expr, $line:expr) => {{
        // Vectors a line, and lines a group of 4 vectors
        const VPL: usize = LINE_WORDS / $w;
        const LPG: usize = if VPL >= 4 { 1 } else { 4 / VPL };
        let (NativeChunk { base, start, end, lid0, .. }, line) = ($c, $line);
        let mut acc = [<$simd>::splat(0); 4];
        let (mut p, stop) = (base.add(start), base.wrapping_add(end));
        let mut lid = lid0;
        while p.wrapping_add(LPG * LINE_WORDS) <= stop {
            // Each line computed and checked before the next, the group's vector `k` into
            // accumulator `k`. Computing the group's lines first and checking them after had the
            // same instructions and ran 2.1x slower for `random` at 512 bits (Rule 6.12, TODO 99)
            for l in 0..LPG {
                let words = line(lid + l as u64);
                for v in 0..VPL {
                    let k = l * VPL + v;
                    acc[k] |= *(p.add(k * $w) as *const $simd) ^ words[v];
                }
            }
            lid += LPG as u64;
            p = native_opaque_step(p.wrapping_add(LPG * LINE_WORDS));
        }
        while p < stop {
            let words = line(lid);
            for v in 0..VPL {
                acc[v % 4] |= *(p.add(v * $w) as *const $simd) ^ words[v];
            }
            lid += 1;
            p = native_opaque_step(p.wrapping_add(LINE_WORDS));
        }
        ((acc[0] | acc[1]) | (acc[2] | acc[3])).reduce_or()
    }};
}

/// The differences of `$c`'s words from `$line`'s on the strided walk (see `native_differs_seq`).
macro_rules! native_differs_strided {
    ([scalar], $c:expr, $line:expr) => {{
        let (NativeChunk { base, start, end, lid0, period, .. }, line) = ($c, $line);
        let mut acc = [0u64; 4];
        native_strided_walk!(prefetch_line, base, start, end, period, lid0, |p, lid| {
            accumulate_line(&mut acc, p, &line(lid));
        });
        (acc[0] | acc[1]) | (acc[2] | acc[3])
    }};
    ([$simd:ty, $w:tt], $c:expr, $line:expr) => {{
        const VPL: usize = LINE_WORDS / $w;
        let (NativeChunk { base, start, end, lid0, period, .. }, line) = ($c, $line);
        let mut acc = [<$simd>::splat(0); 4];
        native_strided_walk!(prefetch_line, base, start, end, period, lid0, |p, lid| {
            let words = line(lid);
            for v in 0..VPL {
                acc[v % 4] |= *(p.add(v * $w) as *const $simd) ^ words[v];
            }
        });
        ((acc[0] | acc[1]) | (acc[2] | acc[3])).reduce_or()
    }};
}

/// The cold path of a native verify: rescan the chunk against its phase (TODO 27).
#[cold]
#[inline(never)]
unsafe fn native_rescan(ctx: &ChunkCtx, lid0: u64, phase: crate::pattern::Phase, test_name: &str, seen: u64) -> u64 {
    let (ptr, start, end) = (ctx.ptr as *const u64, ctx.chunk_start, ctx.chunk_end);
    let label = format!("{}, key {:#018x}, round {}", phase.label(), phase.key, ctx.round);
    if !crate::pattern::rescan_enabled() {
        return unlocalised(found_by(test_name), format_args!("in idx {start}..{end} ({label})"), seen);
    }
    let mut scan = Rescan::for_test(ptr, test_name).seen(seen);
    let mut off = start;
    while off < end {
        scan.line(off, &phase.line(lid0 + ((off - start) / LINE_WORDS) as u64));
        off += LINE_WORDS;
    }
    scan.finish(format_args!("in idx {start}..{end} ({label})"))
}

/// A native SimpleTest at one width: `[scalar]` or `[u64xN, N]`, and its walks module `$walks`.
macro_rules! native_simple_impl {
    ($fn_name:ident, $walks:ident, $test_name:literal, [$($kind:tt)*], $tf:literal) => {
        /// The native SimpleTest's walks at this width, called once a chunk (`NativeChunk`). The
        /// phase's family and beat are matched once a chunk, here, and each walk is generic over
        /// the line function it gets, so each family and beat has its own function of each walk,
        /// `#[inline(never)]` with the width's target features: in one function holding every
        /// family's walk, what the walks share was kept across all of them, and values spilled in
        /// walks that had registers free (TODO 99).
        mod $walks {
            use super::*;

            /// Writes `c`'s phase over its words, on its walk.
            #[inline]
            #[target_feature(enable = $tf)]
            pub(super) unsafe fn write(c: NativeChunk) {
                native_with_line!([$($kind)*], c.phase, |line| {
                    if c.period == 0 { write_seq(c, line) } else { write_strided(c, line) }
                })
            }

            /// The differences of `c`'s words from its phase, ORed: 0 when every word holds.
            #[inline]
            #[target_feature(enable = $tf)]
            pub(super) unsafe fn differs(c: NativeChunk) -> u64 {
                native_with_line!([$($kind)*], c.phase, |line| {
                    if c.period == 0 { differs_seq(c, line) } else { differs_strided(c, line) }
                })
            }

            #[inline(never)]
            #[target_feature(enable = $tf)]
            unsafe fn write_seq<L: Fn(u64) -> native_line_ty!([$($kind)*])>(c: NativeChunk, line: L) {
                native_write_seq!([$($kind)*], c, line)
            }

            #[inline(never)]
            #[target_feature(enable = $tf)]
            unsafe fn write_strided<L: Fn(u64) -> native_line_ty!([$($kind)*])>(c: NativeChunk, line: L) {
                native_write_strided!([$($kind)*], c, line)
            }

            #[inline(never)]
            #[target_feature(enable = $tf)]
            unsafe fn differs_seq<L: Fn(u64) -> native_line_ty!([$($kind)*])>(c: NativeChunk, line: L) -> u64 {
                native_differs_seq!([$($kind)*], c, line)
            }

            #[inline(never)]
            #[target_feature(enable = $tf)]
            unsafe fn differs_strided<L: Fn(u64) -> native_line_ty!([$($kind)*])>(c: NativeChunk, line: L) -> u64 {
                native_differs_strided!([$($kind)*], c, line)
            }
        }

        /// A native SimpleTest (pattern spec v2) at this width; see the section comment.
        #[target_feature(enable = $tf)]
        unsafe fn $fn_name(
            blocks: &[crate::runner::AllocationBlock],
            thread_id: usize,
            error_mode: ErrorMode,
            timing: &TestTiming,
            config: &TestMemoryConfig,
            progress: Option<&TestProgress>,
            run: NativeRun,
        ) -> TestStats {
            let test_name = $test_name;
            let program = run.program;
            let (epoch, wrc) = (config.epoch, config.write_read_cycles);
            let beat = crate::pattern::transfer_beat().0;
            // Line ids count from the thread's span: the extent starts there (TODO 76)
            let lid_base = crate::pattern::line_base(thread_id);
            let stride = config.parameter_context.as_ref().and_then(|c| c.stride_elements).unwrap_or(0);
            // TM5's strided walk's period, or 0 for the sequential walk
            let period = if stride > 0 { strided_period(stride) } else { 0 };
            let pinned = run.pinned;
            // A step that verifies its fill writes one image a suite cycle, so its pairs span two;
            // a Bench pair is pinned to pair 0's P, its Init's and its Verify's alike
            let schedule = match (pinned, run.test_reps) {
                (false, 0) => crate::pattern::Schedule::OncePerRun,
                _ => crate::pattern::Schedule::Rounds,
            };
            let phase_of = move |ctx: &ChunkCtx| program.phase(&epoch, wrc, if pinned { 0 } else { ctx.round }, schedule, beat);
            let chunk = move |ctx: &ChunkCtx| NativeChunk {
                base: ctx.ptr,
                start: ctx.chunk_start,
                end: ctx.chunk_end,
                lid0: lid_base + (ctx.chunk_start / LINE_WORDS) as u64,
                period,
                phase: phase_of(ctx),
            };
            let write = |ctx: &ChunkCtx| $walks::write(chunk(ctx));
            run_phased_test(
                blocks, thread_id, error_mode, timing, config, progress,
                test_name, run.action, 1, config.skip_init, run.test_reps, run.verify_reps,
                write, write,
                |ctx: &ChunkCtx| -> u64 {
                    let c = chunk(ctx);
                    let seen = $walks::differs(c);
                    if seen != 0 {
                        native_rescan(ctx, c.lid0, c.phase, test_name, seen)
                    } else {
                        0
                    }
                },
            )
        }
    };
}

native_simple_impl!(simple_native_scalar, simple_native_scalar_walks, "Mem-SimpleV2", [scalar], "avx2,avx,fma,bmi1,bmi2");
native_simple_impl!(simple_native_128, simple_native_128_walks, "Mem-SimpleV2-128", [u64x2, 2], "sse4.2,sse4.1,ssse3,sse3,sse2,popcnt");
native_simple_impl!(simple_native_256, simple_native_256_walks, "Mem-SimpleV2-256", [u64x4, 4], "avx2,avx,fma,bmi1,bmi2");
native_simple_impl!(simple_native_512, simple_native_512_walks, "Mem-SimpleV2-512", [u64x8, 8], "avx512f,avx512bw,avx512cd,avx512dq,avx512vl,avx2,avx,fma,bmi1,bmi2");

// ─── Mem-Sequence (TODO 97) ──────────────────────────────────────────────────────────────────────
//
// A configurable sequence of passes over each chunk (`crate::sequence`): writes, reads and
// read-then-writes, each in an order. A write writes the program's next phase; a read checks the
// phase last written; a read-then-write checks it and writes its inverse in the same visit, so
// reads and writes alternate line by line. Each pass is one loop, its family, beat, operation and
// order matched once outside it. A separate test from SimpleTest, to bench against it before it
// replaces anything (the owner's call, 2026-10-16).

/// One vector of a Mem-Sequence visit at `$at`: `write` stores `$want`, `read` ORs its difference
/// into `$acc`, `rw` does both and stores the inverse.
macro_rules! sequence_visit {
    (write, $at:expr, $want:expr, $acc:expr, $ones:expr) => {{
        *$at = $want;
    }};
    (read, $at:expr, $want:expr, $acc:expr, $ones:expr) => {{
        $acc |= *$at ^ $want;
    }};
    (rw, $at:expr, $want:expr, $acc:expr, $ones:expr) => {{
        let (at, want) = ($at, $want);
        $acc |= *at ^ want;
        *at = want ^ $ones;
    }};
}

/// A Mem-Sequence line `$lid` under `$key` as vectors of `$n` lanes, before the phase's polarity:
/// `pattern::with_phase_vectors!`'s lines, each named by its family and beat (`[$kind]`, from
/// `sequence_families!`), so each family's walks can be functions of their own (TODO 99).
macro_rules! sequence_line {
    ($n:tt, [ident], $lid:expr, $key:expr) => {
        crate::pattern::ident_line_v::<$n, { LINE_WORDS / $n }>($lid, $key)
    };
    ($n:tt, [random], $lid:expr, $key:expr) => {
        crate::pattern::random_line_v::<$n, { LINE_WORDS / $n }>($lid, $key)
    };
    ($n:tt, [lines], $lid:expr, $key:expr) => {
        crate::pattern::lines_line_v::<$n, { LINE_WORDS / $n }>($lid, $key)
    };
    ($n:tt, [linepair], $lid:expr, $key:expr) => {
        crate::pattern::linepair_line_v::<$n, { LINE_WORDS / $n }>($lid, $key)
    };
    ($n:tt, [switch Bits24], $lid:expr, $key:expr) => {{
        let hash = crate::pattern::fmix(($lid).wrapping_add($key));
        crate::pattern_switch24::vectors!($n, hash)
    }};
    ($n:tt, [switch $beat:ident], $lid:expr, $key:expr) => {
        crate::pattern::switch_line_v::<$n, { LINE_WORDS / $n }>($lid, $key, crate::pattern_gen::BusBeat::$beat)
    };
    ($n:tt, [quiet $beat:ident], $lid:expr, $key:expr) => {
        crate::pattern::quiet_line_v::<$n, { LINE_WORDS / $n }>($lid, $key, crate::pattern_gen::BusBeat::$beat)
    };
    ($n:tt, [header $beat:ident], $lid:expr, $key:expr) => {
        crate::pattern::header_line_v::<$n, { LINE_WORDS / $n }>($lid, $key, crate::pattern_gen::BusBeat::$beat)
    };
}

/// `$m!($($args)* [kind])` for the family and beat of `$phase`, `kind` its line's name in
/// `sequence_line!`: the arms of `pattern::with_phase_vectors!`, one each.
macro_rules! sequence_families {
    ($phase:expr, $m:ident!($($args:tt)*)) => {{
        use crate::pattern::Family;
        use crate::pattern_gen::BusBeat;
        match ($phase.family, $phase.beat) {
            (Family::Ident, _) => $m!($($args)* [ident]),
            (Family::Random, _) => $m!($($args)* [random]),
            (Family::Lines, _) => $m!($($args)* [lines]),
            (Family::Linepair, _) => $m!($($args)* [linepair]),
            (Family::Switch, BusBeat::Bits12) => $m!($($args)* [switch Bits12]),
            (Family::Switch, BusBeat::Bits16) => $m!($($args)* [switch Bits16]),
            (Family::Switch, BusBeat::Bits24) => $m!($($args)* [switch Bits24]),
            (Family::Switch, BusBeat::Bits32) => $m!($($args)* [switch Bits32]),
            (Family::Switch, BusBeat::Bits64) => $m!($($args)* [switch Bits64]),
            (Family::Quiet, BusBeat::Bits12) => $m!($($args)* [quiet Bits12]),
            (Family::Quiet, BusBeat::Bits16) => $m!($($args)* [quiet Bits16]),
            (Family::Quiet, BusBeat::Bits24) => $m!($($args)* [quiet Bits24]),
            (Family::Quiet, BusBeat::Bits32) => $m!($($args)* [quiet Bits32]),
            (Family::Quiet, BusBeat::Bits64) => $m!($($args)* [quiet Bits64]),
            (Family::Header, BusBeat::Bits12) => $m!($($args)* [header Bits12]),
            (Family::Header, BusBeat::Bits16) => $m!($($args)* [header Bits16]),
            (Family::Header, BusBeat::Bits24) => $m!($($args)* [header Bits24]),
            (Family::Header, BusBeat::Bits32) => $m!($($args)* [header Bits32]),
            (Family::Header, BusBeat::Bits64) => $m!($($args)* [header Bits64]),
        }
    }};
}

/// A Mem-Sequence walk's accumulators (`init`), for the operations that read, and its value
/// (`result`): the differences the reads found, ORed (0 for a write). A read-then-write's `ones`
/// inverts what it writes.
macro_rules! sequence_acc {
    (init write, $acc:ident, $ones:ident, $simd:ty) => {};
    (init read, $acc:ident, $ones:ident, $simd:ty) => {
        let mut $acc = [<$simd>::splat(0); 4];
    };
    (init rw, $acc:ident, $ones:ident, $simd:ty) => {
        let mut $acc = [<$simd>::splat(0); 4];
        let $ones = <$simd>::splat(!0);
    };
    (result write, $acc:ident) => { 0u64 };
    (result $op:tt, $acc:ident) => { (($acc[0] | $acc[1]) | ($acc[2] | $acc[3])).reduce_or() };
    // The next line's vectors to the next accumulators: line `j`'s vector `v` is always
    // accumulator `(j * VPL + v) % 4`'s, as in a step of lines
    (rotate write, $acc:ident, $n:expr) => {};
    (rotate $op:tt, $acc:ident, $n:expr) => {
        $acc = std::array::from_fn(|k| $acc[(k + $n) % 4]);
    };
}

/// Mem-Sequence's step: `[line: [vector accumulator, ...], ...]`, the lines it visits, in order,
/// each vector of each and the accumulator its difference goes to. A step is four vectors, one an
/// accumulator, as every verify's: two lines at 256 bits, four at 512. Checked against the old
/// walk's `(l * VPL + v) % 4` when it compiles.
macro_rules! sequence_step_check {
    ($w:tt, [$($l:literal: [$($v:literal $a:literal),*]),*]) => {
        const _: () = {
            const VPL: usize = LINE_WORDS / $w;
            assert!([$($l),*].len() * VPL == 4, "a step is four vectors");
            $($(assert!($a == ($l * VPL + $v) % 4 && $v < VPL, "each vector its accumulator");)*)*
        };
    };
}

/// Mem-Sequence's forward walk (`$rev` false) or reverse (true) of `$op` over the chunk of `$walk`,
/// its family's line `[$kind]`: a pointer walked from the chunk's first line or its last, each line
/// and vector at a fixed displacement from it, the line's id beside it. One loop to a function
/// (`sequence_orders!`), so its values stay in registers. In the width's form (`sequence_random!`):
/// `steps` a step's lines an iteration (at 512 bits), `rotate` a line an iteration with the
/// accumulators rotated (at 256, where two lines' words at once spilled for the heavier families).
macro_rules! sequence_linear {
    (rotate, $op:tt, $simd:ty, $w:tt, [$l0:literal: [$($v:literal $a:literal),*] $(, $l:literal: $rest:tt)*], $kind:tt, $walk:ident, $rev:literal) => {{
        sequence_step_check!($w, [$l0: [$($v $a),*] $(, $l: $rest)*]);
        let SequenceWalk { ptr, start, end, lid0, pkey, mask, .. } = $walk;
        let mask = <$simd>::splat(mask);
        sequence_acc!(init $op, acc, ones, $simd);
        let lines = (end - start) / LINE_WORDS;
        let first = if $rev { lines - 1 } else { 0 };
        let mut p = ptr.add(start + first * LINE_WORDS);
        let mut lid = lid0 + first as u64;
        for _ in 0..lines {
            let words = sequence_line!($w, $kind, lid, pkey).map(|x| x ^ mask);
            $(sequence_visit!($op, p.add($v * $w) as *mut $simd, words[$v], acc[$a], ones);)*
            sequence_acc!(rotate $op, acc, LINE_WORDS / $w);
            // The last step lands off the chunk
            p = if $rev { p.wrapping_sub(LINE_WORDS) } else { p.wrapping_add(LINE_WORDS) };
            lid = if $rev { lid.wrapping_sub(1) } else { lid.wrapping_add(1) };
        }
        sequence_acc!(result $op, acc)
    }};
    (steps, $op:tt, $simd:ty, $w:tt, [$($l:literal: [$($v:literal $a:literal),*]),*], $kind:tt, $walk:ident, $rev:literal) => {{
        sequence_step_check!($w, [$($l: [$($v $a),*]),*]);
        const LPG: usize = [$($l),*].len();
        let SequenceWalk { ptr, start, end, lid0, pkey, mask, .. } = $walk;
        let mask = <$simd>::splat(mask);
        sequence_acc!(init $op, acc, ones, $simd);
        let lines = (end - start) / LINE_WORDS;
        // A chunk is whole 4 KiB pages: whole steps
        debug_assert!(lines.is_multiple_of(LPG), "{lines} lines");
        let first = if $rev { lines - 1 } else { 0 };
        let mut p = ptr.add(start + first * LINE_WORDS);
        let mut lid = lid0 + first as u64;
        for _ in 0..lines / LPG {
            $({
                let (at, line) = if $rev { (p.sub($l * LINE_WORDS), lid.wrapping_sub($l)) } else { (p.add($l * LINE_WORDS), lid.wrapping_add($l)) };
                let words = sequence_line!($w, $kind, line, pkey).map(|x| x ^ mask);
                $(sequence_visit!($op, at.add($v * $w) as *mut $simd, words[$v], acc[$a], ones);)*
            })*
            // The last step lands off the chunk
            p = if $rev { p.wrapping_sub(LPG * LINE_WORDS) } else { p.wrapping_add(LPG * LINE_WORDS) };
            lid = if $rev { lid.wrapping_sub(LPG as u64) } else { lid.wrapping_add(LPG as u64) };
        }
        sequence_acc!(result $op, acc)
    }};
}

/// Mem-Sequence's random walk of `$op` over the chunk of `$walk`: each power-of-two block of the
/// chunk's lines in its own scrambled order (`sequence::random_line`; a chunk is whole 4 KiB pages,
/// so every block is 64 lines or more). Its lines are where the scramble puts them, so each is
/// addressed from the block's first by its index. `j` counts from `-n` up to 0, so the loop needs
/// no bound: `random_line` masks `j` to the block first, and `-n` is 0 there.
///
/// `steps` visits a step's lines an iteration (`$step`): at 512 bits the compiler vectorizes their
/// four scrambles. `rotate` visits a line an iteration and rotates the accumulators by its vectors,
/// so each vector's difference goes to the accumulator it would in a step: at 256 bits the
/// scrambles are scalar, and two lines' at once (and their hashes) didn't fit in the registers.
macro_rules! sequence_random {
    (steps, $op:tt, $simd:ty, $w:tt, [$($l:literal: [$($v:literal $a:literal),*]),*], $kind:tt, $walk:ident) => {{
        sequence_step_check!($w, [$($l: [$($v $a),*]),*]);
        const LPG: usize = [$($l),*].len();
        let SequenceWalk { ptr, start, end, lid0, key, pkey, mask, .. } = $walk;
        let mask = <$simd>::splat(mask);
        sequence_acc!(init $op, acc, ones, $simd);
        let lines = (end - start) / LINE_WORDS;
        // Whole 4 KiB pages (`test_memory::GRANULE`): every block whole steps, so `j` lands on 0
        debug_assert!(lines.is_multiple_of(64), "{lines} lines");
        let mut s = 0;
        while s < lines {
            let bits = (lines - s).ilog2();
            let n = 1usize << bits;
            let (block, block_lid) = (ptr.add(start + s * LINE_WORDS), lid0 + s as u64);
            let mut j = n.wrapping_neg();
            while j != 0 {
                $({
                    let i = crate::sequence::random_line(j.wrapping_add($l) as u64, bits, key) as usize;
                    let words = sequence_line!($w, $kind, block_lid + i as u64, pkey).map(|x| x ^ mask);
                    let at = block.add(i * LINE_WORDS);
                    $(sequence_visit!($op, at.add($v * $w) as *mut $simd, words[$v], acc[$a], ones);)*
                })*
                j = j.wrapping_add(LPG);
            }
            s += n;
        }
        sequence_acc!(result $op, acc)
    }};
    (rotate, $op:tt, $simd:ty, $w:tt, [$l0:literal: [$($v:literal $a:literal),*] $(, $l:literal: $rest:tt)*], $kind:tt, $walk:ident) => {{
        sequence_step_check!($w, [$l0: [$($v $a),*] $(, $l: $rest)*]);
        let SequenceWalk { ptr, start, end, lid0, key, pkey, mask, .. } = $walk;
        let mask = <$simd>::splat(mask);
        sequence_acc!(init $op, acc, ones, $simd);
        let lines = (end - start) / LINE_WORDS;
        let mut s = 0;
        while s < lines {
            let bits = (lines - s).ilog2();
            let n = 1usize << bits;
            let (block, block_lid) = (ptr.add(start + s * LINE_WORDS), lid0 + s as u64);
            let mut j = n.wrapping_neg();
            while j != 0 {
                let i = crate::sequence::random_line(j as u64, bits, key) as usize;
                let words = sequence_line!($w, $kind, block_lid + i as u64, pkey).map(|x| x ^ mask);
                let at = block.add(i * LINE_WORDS);
                $(sequence_visit!($op, at.add($v * $w) as *mut $simd, words[$v], acc[$a], ones);)*
                sequence_acc!(rotate $op, acc, LINE_WORDS / $w);
                j = j.wrapping_add(1);
            }
            s += n;
        }
        sequence_acc!(result $op, acc)
    }};
}

/// Mem-Sequence's strided walk of `$op` over the chunk of `$walk`: `for_each_strided_line!`'s
/// lines and prefetches, a line a step, on a walked pointer: each pass from its first line, a
/// period at a time, each vector of the line `acc[v % 4]`'s as the old walk's (at 512 bits one).
/// The pointer goes from pass to pass (back `wrap`), so a pass keeps nothing else live: at TM5's
/// large strides a pass is a line or two. Each pass is two loops, the steps whose prefetch lands in
/// the pass and those whose lands in the next, so neither picks its target, and each line's id
/// goes through `opaque_line_id`.
macro_rules! sequence_strided {
    ($op:tt, $simd:ty, $w:tt, [$($l:literal: [$($v:literal $a:literal),*]),*], $kind:tt, $walk:ident) => {
        sequence_strided!(@line $op, $simd, $w, $kind, $walk, [$($l: [$($v $a),*]),*])
    };
    (@line $op:tt, $simd:ty, $w:tt, $kind:tt, $walk:ident, [$l0:literal: [$($v:literal $a:literal),*] $(, $l:literal: $rest:tt)*]) => {{
        let SequenceWalk { ptr, start, end, lid0, period, pkey, mask, .. } = $walk;
        debug_assert!(period.is_multiple_of(LINE_WORDS) && start.is_multiple_of(LINE_WORDS) && end.is_multiple_of(LINE_WORDS),
                      "the strided walk needs whole lines: {start}..{end} by {period}");
        let mask = <$simd>::splat(mask);
        sequence_acc!(init $op, acc, ones, $simd);
        let ahead = strided_lookahead(start, end, period);
        let (bottom, stop, lid_step) = (ptr.wrapping_add(start), ptr.add(end), (period / LINE_WORDS) as u64);
        // The first pass's first line: the last of the chunk's first period
        let mut p = bottom.wrapping_add(period - LINE_WORDS);
        loop {
            // `strided_wrap` in bytes, from the pointers: a pass can be a line or two, so its
            // start keeps no more than the walk does
            let wrap = sequence_wrap(p, stop, period);
            let mut lid = lid0 + ((p.addr() - bottom.addr()) / 64) as u64;
            // `strided_ahead`'s targets, a loop each: the steps whose line `ahead` on is in this
            // pass, then the rest, whose is in the next
            let near = stop.wrapping_sub(ahead);
            while p < near {
                prefetch_line(p.wrapping_add(ahead));
                let words = sequence_line!($w, $kind, opaque_line_id(lid), pkey).map(|x| x ^ mask);
                $(sequence_visit!($op, p.add($v * $w) as *mut $simd, words[$v], acc[$v % 4], ones);)*
                p = p.wrapping_add(period);
                lid = lid.wrapping_add(lid_step);
            }
            while p < stop {
                // The walk's last steps prefetch off the chunk: a wasted prefetch, not a fault
                prefetch_line(p.wrapping_add(ahead).wrapping_byte_sub(wrap));
                let words = sequence_line!($w, $kind, opaque_line_id(lid), pkey).map(|x| x ^ mask);
                $(sequence_visit!($op, p.add($v * $w) as *mut $simd, words[$v], acc[$v % 4], ones);)*
                p = p.wrapping_add(period);
                lid = lid.wrapping_add(lid_step);
            }
            // Back `wrap` from past the pass's end is the next pass's first line, a line below
            p = p.wrapping_byte_sub(wrap);
            if p < bottom {
                break;
            }
        }
        sequence_acc!(result $op, acc)
    }};
}

/// `lid`, through an empty `asm!` the compiler can't see through: the strided walk computes each
/// line's values from its id, rather than stepping each beside the id. At a period known only at
/// run time, each value a family takes from the id (`lid + key`, `ident`'s `(lid << 3) * G + key`,
/// ...) stepped on its own took two registers, itself and its step, and the walks of `header` and
/// `quiet` spilled (TODO 99: 25 hot loops reloading from the stack, 13 with this). No instruction:
/// the block is a comment, its operand the register `lid` is already in.
#[inline(always)]
fn opaque_line_id(mut lid: u64) -> u64 {
    // SAFETY: an empty block that reads and writes one register and touches nothing else
    unsafe { core::arch::asm!("/* {0} */", inout(reg) lid, options(pure, nomem, nostack, preserves_flags)) };
    lid
}

/// `strided_wrap` for the pass from `p` in bytes, from the pointers: the pass's lines rounded up to
/// whole periods, plus a line. Back this from past the pass's end is the next pass's first line.
#[inline(always)]
fn sequence_wrap(p: *const u64, stop: *const u64, period: usize) -> usize {
    let step = period * std::mem::size_of::<u64>();
    stop.addr().saturating_sub(p.addr()).div_ceil(step) * step + LINE_WORDS * std::mem::size_of::<u64>()
}

/// One Mem-Sequence pass of `$op` (`write`, `read` or `rw`) over `$walk`'s chunk in `$order`, its
/// family's line `[$kind]`; returns the differences the reads found, ORed (0 for a write). Each
/// order's loop is a function of its own, with the width's features, taking the walk by value
/// (TODO 99): in one function for every family and order, the loops kept their bounds, keys and
/// at 512 bits their accumulators on the stack.
macro_rules! sequence_orders {
    ($op:tt, $simd:ty, $w:tt, $tf:literal, $step:tt, $form:ident, $walk:ident, $order:expr, $kind:tt) => {
        match $order {
            crate::sequence::Order::Forward => {
                #[target_feature(enable = $tf)]
                #[inline(never)]
                unsafe fn forward(walk: SequenceWalk) -> u64 {
                    sequence_linear!($form, $op, $simd, $w, $step, $kind, walk, false)
                }
                forward($walk)
            }
            crate::sequence::Order::Reverse => {
                #[target_feature(enable = $tf)]
                #[inline(never)]
                unsafe fn reverse(walk: SequenceWalk) -> u64 {
                    sequence_linear!($form, $op, $simd, $w, $step, $kind, walk, true)
                }
                reverse($walk)
            }
            crate::sequence::Order::Random => {
                #[target_feature(enable = $tf)]
                #[inline(never)]
                unsafe fn random(walk: SequenceWalk) -> u64 {
                    sequence_random!($form, $op, $simd, $w, $step, $kind, walk)
                }
                random($walk)
            }
            crate::sequence::Order::Stride => {
                #[target_feature(enable = $tf)]
                #[inline(never)]
                unsafe fn stride(walk: SequenceWalk) -> u64 {
                    sequence_strided!($op, $simd, $w, $step, $kind, walk)
                }
                stride($walk)
            }
        }
    };
}

/// The cold path of a Mem-Sequence read: rescan the chunk against `phase` (TODO 27). A
/// read-then-write's goes to `rescan_read_write`.
#[cold]
#[inline(never)]
unsafe fn sequence_rescan(ptr: *const u64, range: std::ops::Range<usize>, lid0: u64, phase: crate::pattern::Phase, pass: &str,
                          test_name: &str, seen: u64) -> u64 {
    let (start, end) = (range.start, range.end);
    let label = format!("{pass}, {}, key {:#018x}", phase.label(), phase.key);
    if !crate::pattern::rescan_enabled() {
        return unlocalised(found_by(test_name), format_args!("in idx {start}..{end} ({label})"), seen);
    }
    let mut scan = Rescan::for_test(ptr, test_name).seen(seen);
    let mut off = start;
    while off < end {
        scan.line(off, &phase.line(lid0 + ((off - start) / LINE_WORDS) as u64));
        off += LINE_WORDS;
    }
    scan.finish(format_args!("in idx {start}..{end} ({label})"))
}

/// Where a Mem-Sequence pass runs: words `[start, end)` of the extent at `ptr`, the first line's
/// id, and TM5's stride period in words (0 when the test has none).
#[derive(Clone, Copy)]
struct SequenceChunk {
    ptr: *mut u64,
    start: usize,
    end: usize,
    lid0: u64,
    period: usize,
}

/// What one Mem-Sequence pass's walk takes, by value (TODO 99): its chunk, the random order's key,
/// and the phase's key and polarity (its family and beat pick the walk).
#[derive(Clone, Copy)]
struct SequenceWalk {
    ptr: *mut u64,
    start: usize,
    end: usize,
    lid0: u64,
    period: usize,
    key: u64,
    pkey: u64,
    mask: u64,
}

impl SequenceWalk {
    fn new(c: SequenceChunk, key: u64, phase: crate::pattern::Phase) -> Self {
        let SequenceChunk { ptr, start, end, lid0, period } = c;
        SequenceWalk { ptr, start, end, lid0, period, key, pkey: phase.key, mask: phase.mask }
    }
}

/// Mem-Sequence at one width: the test, and a function per operation for its passes, each
/// picking its family's walk in its order (`sequence_orders!`). `$step` is the width's step
/// (`sequence_step_check!`), `$form` its linear and random walks' form (`sequence_random!`).
macro_rules! sequence_impl {
    ($fn_name:ident, $write:ident, $read:ident, $rw:ident, $simd:ty, $w:tt, $step:tt, $form:ident, $tf:literal) => {
        #[target_feature(enable = $tf)]
        #[inline(never)]
        unsafe fn $write(c: SequenceChunk, order: crate::sequence::Order, key: u64, phase: crate::pattern::Phase) -> u64 {
            let walk = SequenceWalk::new(c, key, phase);
            sequence_families!(phase, sequence_orders!(write, $simd, $w, $tf, $step, $form, walk, order,))
        }

        #[target_feature(enable = $tf)]
        #[inline(never)]
        unsafe fn $read(c: SequenceChunk, order: crate::sequence::Order, key: u64, phase: crate::pattern::Phase) -> u64 {
            let walk = SequenceWalk::new(c, key, phase);
            sequence_families!(phase, sequence_orders!(read, $simd, $w, $tf, $step, $form, walk, order,))
        }

        #[target_feature(enable = $tf)]
        #[inline(never)]
        unsafe fn $rw(c: SequenceChunk, order: crate::sequence::Order, key: u64, phase: crate::pattern::Phase) -> u64 {
            let walk = SequenceWalk::new(c, key, phase);
            sequence_families!(phase, sequence_orders!(rw, $simd, $w, $tf, $step, $form, walk, order,))
        }

        /// Mem-Sequence at this width; see the section comment.
        #[target_feature(enable = $tf)]
        unsafe fn $fn_name(
            blocks: &[crate::runner::AllocationBlock],
            thread_id: usize,
            error_mode: ErrorMode,
            timing: &TestTiming,
            config: &TestMemoryConfig,
            progress: Option<&TestProgress>,
        ) -> TestStats {
            use crate::sequence::Op;
            let test_name = "Mem-Sequence";
            debug_assert!(config.pattern_mode.is_none(), "Mem-Sequence runs native patterns only (the config check)");
            let program = config.native_program().unwrap_or_else(crate::pattern::Program::default_simple);
            let seq = config.sequence();
            let reps = config.write_read_cycles.max(1);
            // Program rounds a chunk writes a cycle: the phases go by them, as SimpleTest's
            let writes = seq.rounds(reps);
            let (reads, written) = seq.visits(reps);
            let (epoch, beat) = (config.epoch, crate::pattern::transfer_beat().0);
            let schedule = crate::pattern::Schedule::Rounds;
            // Line ids count from the thread's span: the extent starts there (TODO 76)
            let lid_base = crate::pattern::line_base(thread_id);
            // A stride of 0 (a parameter of 1 on one channel) is TM5's jump 0: in order, as SimpleTest
            let stride = config.parameter_context.as_ref().and_then(|c| c.stride_elements).unwrap_or(0);
            let period = if stride > 0 { strided_period(stride) } else { 0 };
            let (mut runner, extent) = crate::test_scaffolding::TestRunner::new(
                blocks, thread_id, error_mode, timing, config, progress,
                test_name, TestAction::WriteVerify,
            );
            let base = extent.ptr as *mut u64;
            let ops = |runner: &crate::test_scaffolding::TestRunner| (runner.bytes_processed() / std::mem::size_of::<u64>()) as u64;

            loop {
                let cycle = runner.begin_cycle();
                let mut cycle_errors = 0u64;
                // Chunks of exactly the test's size, spread evenly over the extent (TODO 76)
                let spread = runner.chunks(extent.test_size);
                for k in 0..spread.count() {
                    let start = spread.start(k) / std::mem::size_of::<u64>();
                    let end = start + spread.chunk() / std::mem::size_of::<u64>();
                    let lid0 = lid_base + (start / LINE_WORDS) as u64;
                    let chunk = SequenceChunk { ptr: base, start, end, lid0, period };
                    runner.check_seal(spread.start(k), spread.chunk());
                    halt_here!(runner, cycle_errors, ops(&runner));

                    // The round the chunk holds, and the next a W writes (`sequence::round_after`)
                    let first = u64::from(cycle - 1) * u64::from(writes);
                    let (mut current, mut next) = (first, first);
                    let mut phase = program.phase(&epoch, writes, current, schedule, beat);
                    // A sequence that reads first reads a fill: one write pass before its steps,
                    // through the same loops (each pass's loops are compiled once a function)
                    let fill = crate::sequence::Step { op: Op::Write, count: 1, order: Some(crate::sequence::Order::Forward) };
                    let fill = if seq.reads_first() { std::slice::from_ref(&fill) } else { &[] };
                    // Each pass's random order its own: from the step's key, the cycle, the chunk
                    // and the pass
                    let mut pass = 0u64;
                    for rep in 0..reps {
                        let steps = if rep == 0 { fill } else { &[] }.iter().chain(seq.steps());
                        for step in steps {
                            let order = match seq.order_of(step) {
                                crate::sequence::Order::Stride if period == 0 => crate::sequence::Order::Forward,
                                order => order,
                            };
                            for _ in 0..step.count {
                                pass += 1;
                                let key = crate::pattern::fmix(epoch.key() ^ (u64::from(cycle) << 44) ^ ((k as u64) << 24) ^ pass);
                                let read = phase;
                                (current, next) = crate::sequence::round_after(step.op, current, next);
                                phase = program.phase(&epoch, writes, current, schedule, beat);
                                let (seen, name) = match step.op {
                                    Op::Write => ($write(chunk, order, key, phase), "W"),
                                    Op::Read => ($read(chunk, order, key, phase), "R"),
                                    Op::ReadWrite => {
                                        // It writes the inverse of what it read, round `current`
                                        debug_assert_eq!(phase, crate::pattern::Phase { mask: !read.mask, ..read });
                                        ($rw(chunk, order, key, read), "RW")
                                    }
                                };
                                if seen != 0 {
                                    let what = format!("pass {pass}: {name}@{}", order.name());
                                    if step.op == Op::ReadWrite {
                                        let line = |p: crate::pattern::Phase| move |idx: usize| p.line(lid0 + ((idx - start) / LINE_WORDS) as u64)[idx % LINE_WORDS];
                                        let (errors, rewrote) = rescan_read_write(base, start..end, test_name, seen, &what, &line(read), &line(phase));
                                        cycle_errors += errors;
                                        halt_here!(runner, cycle_errors, ops(&runner));
                                        if rewrote {
                                            $write(chunk, crate::sequence::Order::Forward, key, phase);
                                        }
                                    } else {
                                        cycle_errors += sequence_rescan(base, start..end, lid0, phase, &what, test_name, seen);
                                        halt_here!(runner, cycle_errors, ops(&runner));
                                    }
                                }
                            }
                        }
                    }

                    // Count the chunk when it is done: each pass's reads and writes, the fill's
                    runner.add_bytes(spread.chunk() * (reads + written) as usize);
                    halt_here!(runner, cycle_errors, ops(&runner));
                    runner.reseal(spread.start(k), spread.chunk());
                    if runner.shutdown_requested() {
                        return runner.finish_aborted(cycle_errors, ops(&runner));
                    }
                }

                runner.commit_cycle_errors(cycle_errors);
                runner.update_progress();
                if !runner.should_continue() {
                    return runner.finish_completed(ops(&runner));
                }
            }
        }
    };
}

sequence_impl!(sequence_256, sequence_write_256, sequence_read_256, sequence_rw_256, u64x4, 4, [0: [0 0, 1 1], 1: [0 2, 1 3]], rotate,
               "avx2,avx,fma,bmi1,bmi2");
sequence_impl!(sequence_512, sequence_write_512, sequence_read_512, sequence_rw_512, u64x8, 8, [0: [0 0], 1: [0 1], 2: [0 2], 3: [0 3]], steps,
               "avx512f,avx512bw,avx512cd,avx512dq,avx512vl,avx2,avx,fma,bmi1,bmi2");

/// Mem-Sequence at the widest width this CPU has: 512 bits, else 256 (the CPU gate requires AVX2).
#[doc = include_str!("test_fn_safety.md")]
pub unsafe fn sequence_auto_multi(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> TestStats {
    if is_x86_feature_detected!("avx512f") {
        return sequence_512(blocks, thread_id, error_mode, timing, config, progress);
    }
    sequence_256(blocks, thread_id, error_mode, timing, config, progress)
}

/// A native SimpleTest at the widest width this CPU has.
///
/// # Safety
/// As every test function: `blocks` hold valid, aligned, writable memory.
unsafe fn simple_native_widest(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
    run: NativeRun,
) -> TestStats {
    if is_x86_feature_detected!("avx512f") {
        return simple_native_512(blocks, thread_id, error_mode, timing, config, progress, run);
    }
    if is_x86_feature_detected!("avx2") {
        return simple_native_256(blocks, thread_id, error_mode, timing, config, progress, run);
    }
    simple_native_128(blocks, thread_id, error_mode, timing, config, progress, run)
}

/// Bench-Init-<family> and Bench-Verify-<family>: one native family's write or verify alone, at the
/// widest width, every cycle on its own (`write_read_cycles` 1). Both are pinned to pair 0's P
/// image under a zero epoch, so the Init writes what the Verify expects; the Verify checks its own
/// fill and doesn't count it in its time or bytes (TODO 90).
macro_rules! native_bench {
    ($init:ident, $verify:ident, $family:expr) => {
        #[doc = include_str!("test_fn_safety.md")]
        pub unsafe fn $init(
            blocks: &[crate::runner::AllocationBlock],
            thread_id: usize,
            error_mode: ErrorMode,
            timing: &TestTiming,
            config: &TestMemoryConfig,
            progress: Option<&TestProgress>,
        ) -> TestStats {
            let mut config = config.clone();
            (config.epoch, config.write_read_cycles) = (crate::pattern::StepEpoch::default(), 1);
            let run = NativeRun { program: crate::pattern::Program::single($family), action: TestAction::Write, test_reps: 1, verify_reps: 0, pinned: true };
            simple_native_widest(blocks, thread_id, error_mode, timing, &config, progress, run)
        }

        #[doc = include_str!("test_fn_safety.md")]
        pub unsafe fn $verify(
            blocks: &[crate::runner::AllocationBlock],
            thread_id: usize,
            error_mode: ErrorMode,
            timing: &TestTiming,
            config: &TestMemoryConfig,
            progress: Option<&TestProgress>,
        ) -> TestStats {
            let mut config = config.clone();
            (config.epoch, config.write_read_cycles) = (crate::pattern::StepEpoch::default(), 1);
            let run = NativeRun { program: crate::pattern::Program::single($family), action: TestAction::Verify, test_reps: 0, verify_reps: 1, pinned: true };
            simple_native_widest(blocks, thread_id, error_mode, timing, &config, progress, run)
        }
    };
}

native_bench!(bench_init_ident, bench_verify_ident, crate::pattern::Family::Ident);
native_bench!(bench_init_random, bench_verify_random, crate::pattern::Family::Random);
native_bench!(bench_init_switch, bench_verify_switch, crate::pattern::Family::Switch);
native_bench!(bench_init_lines, bench_verify_lines, crate::pattern::Family::Lines);
native_bench!(bench_init_linepair, bench_verify_linepair, crate::pattern::Family::Linepair);
native_bench!(bench_init_quiet, bench_verify_quiet, crate::pattern::Family::Quiet);
native_bench!(bench_init_header, bench_verify_header, crate::pattern::Family::Header);

/// The native Bench pairs' names and functions: `Bench-Init-Ident`, `Bench-Verify-Ident`, ...
pub(crate) fn native_bench_tests() -> [(&'static str, &'static str, crate::runner::TestFunctionMultiBlock, crate::runner::TestFunctionMultiBlock); 7] {
    [
        ("Bench-Init-Ident", "Bench-Verify-Ident", bench_init_ident, bench_verify_ident),
        ("Bench-Init-Random", "Bench-Verify-Random", bench_init_random, bench_verify_random),
        ("Bench-Init-Switch", "Bench-Verify-Switch", bench_init_switch, bench_verify_switch),
        ("Bench-Init-Lines", "Bench-Verify-Lines", bench_init_lines, bench_verify_lines),
        ("Bench-Init-Linepair", "Bench-Verify-Linepair", bench_init_linepair, bench_verify_linepair),
        ("Bench-Init-Quiet", "Bench-Verify-Quiet", bench_init_quiet, bench_verify_quiet),
        ("Bench-Init-Header", "Bench-Verify-Header", bench_init_header, bench_verify_header),
    ]
}
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
// SOLUTION: Manual 4x unroll using std::arch intrinsics (each width's `pos_write_nt`,
// `simple_simd_loops!`). This gives us:
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

/// Stamps out a complete SimpleTestNT implementation for a given SIMD width.
/// Generates one #[target_feature] impl function + one public wrapper. The NT write and the verify
/// are the width's `pos_write_nt` and `pos_verify` (`simple_simd_loops!`, TODO 99).
macro_rules! simple_test_nt_impl {
    (
        $impl_fn:ident,
        $pub_fn:ident,
        $test_name:literal,
        $simd_w:expr,
        $loops:ident,
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
            let chunk = |ctx: &ChunkCtx| SimplePositional { ptr: ctx.ptr, start: ctx.chunk_start, end: ctx.chunk_end, base };

            run_phased_test(
                blocks, thread_id, error_mode, timing, config, progress,
                test_name, TestAction::WriteVerify, 1, false, 1, 1,
                // Init: NT write (fill memory with pattern, bypassing cache)
                |ctx: &ChunkCtx| $loops::pos_write_nt(chunk(ctx)),
                // Test: NT write (re-write same pattern via streaming stores)
                |ctx: &ChunkCtx| $loops::pos_write_nt(chunk(ctx)),
                // Verify: regular loads into four accumulators
                |ctx: &ChunkCtx| -> u64 { simple_verify_positional!($loops::pos_verify, $simd_w, ctx, base, test_name) },
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
    "Mem-SimpleNT-128", 2, simple_test_v2_128_loops,
    "sse4.2,sse4.1,ssse3,sse3,sse2,popcnt"
);

simple_test_nt_impl!(
    simple_test_nt_256_impl, simple_test_nt_256_multi,
    "Mem-SimpleNT-256", 4, simple_test_v2_256_loops,
    "avx2,avx,fma,bmi1,bmi2"
);

simple_test_nt_impl!(
    simple_test_nt_512_impl, simple_test_nt_512_multi,
    "Mem-SimpleNT-512", 8, simple_test_v2_512_loops,
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

// ─── SimpleTest v2's loops: tests (TODO 99) ──────────────────────────────────────────────────────

#[cfg(test)]
mod simple_loops_tests {
    use super::*;

    /// What a word outside the loops' range holds, and must still hold after a write.
    const GUARD: u64 = 0x6A09_E667_F3BC_C908;
    /// The bit each check flips.
    const BIT: u64 = 1 << 40;

    /// 64-byte-aligned words, every one `GUARD`, as the extent's are aligned.
    struct Mem(Vec<u64x8>);

    impl Mem {
        fn new(words: usize) -> Self {
            Mem(vec![u64x8::splat(GUARD); words.div_ceil(8)])
        }
        fn ptr(&mut self) -> *mut u64 {
            self.0.as_mut_ptr() as *mut u64
        }
        fn words(&mut self) -> &mut [u64] {
            let n = self.0.len() * 8;
            // SAFETY: the vectors' words, as long as the borrow of `self`
            unsafe { std::slice::from_raw_parts_mut(self.ptr(), n) }
        }
        fn reset(&mut self) {
            self.words().fill(GUARD);
        }
        /// Every word of `[start, end)` is `expected(i)` and every other one is `GUARD`.
        fn assert_image(&mut self, start: usize, end: usize, expected: impl Fn(usize) -> u64, what: &str) {
            for (i, &word) in self.words().iter().enumerate() {
                let want = if (start..end).contains(&i) { expected(i) } else { GUARD };
                assert_eq!(word, want, "{what}: word {i} of {start}..{end}");
            }
        }
        /// `verify` sees `BIT` flipped in each of `words`, and nothing once it's back.
        fn assert_sees(&mut self, words: impl IntoIterator<Item = usize>, verify: impl Fn() -> u64, what: &str) {
            assert_eq!(verify(), 0, "{what}: clean");
            for i in words {
                self.words()[i] ^= BIT;
                assert_ne!(verify() & BIT, 0, "{what}: missed word {i}");
                self.words()[i] ^= BIT;
            }
            assert_eq!(verify(), 0, "{what}: clean again");
        }
    }

    /// The first `head` words of `[start, end)`, its last `tail`, and every 13th between.
    fn flips(start: usize, end: usize, head: usize, tail: usize) -> Vec<usize> {
        (start..start + head).chain((start..end).step_by(13)).chain(end - tail..end).collect()
    }

    /// The strided chunks the walk tests take: a pass shorter than the lookahead (TM5 Parameter 8
    /// on 2 channels, its period 16 lines), one longer (two-line periods, 300 of them), and a
    /// period of 64 lines off a page.
    const WALKS: [(usize, usize, usize); 3] = [(0, 4096, 15 * 8), (0, 300 * 16, 8), (64, 64 + 4096 + 40, 63 * 8)];

    /// TODO 99: the walk on a pointer visits `for_each_strided_line!`'s lines in its order, each at
    /// its offset, and prefetches each step what `strided_ahead` gives, where a pass is shorter
    /// than the lookahead, longer, and empty (a period longer than the chunk).
    #[test]
    fn the_pointer_walk_is_the_strided_walk() {
        let mem = [0u64; 1];
        let ptr = mem.as_ptr() as *mut u64;
        let shapes = WALKS.iter().copied().chain([(512, 4096 + 512, 15 * 8), (0, 512, 4095 * 8), (64, 128, 0), (8, 8 + 200 * 24, 16)]);
        for (start, end, stride) in shapes {
            let period = strided_period(stride);
            let mut old = Vec::new();
            for_each_strided_line!(ptr, start, end, period, |offset| { old.push(offset); });
            // The old walk's prefetch targets, from its loops and helpers
            let ahead = strided_lookahead(start, end, period);
            let mut targets = Vec::new();
            let mut first = start + period;
            while first > start {
                first -= LINE_WORDS;
                let wrap = strided_wrap(first, end, period);
                targets.extend((first..end).step_by(period).map(|offset| strided_ahead(offset, ahead, end, wrap)));
            }
            let mut new = Vec::new();
            for_each_strided_line_at!(ptr, start, end, period, |line, offset, target| {
                new.push((line as usize, offset, target as usize));
            });
            assert_eq!(new.len(), old.len(), "{start}..{end} by {stride}: the lines");
            for (k, (&(line, offset, target), (&want, &ahead))) in new.iter().zip(old.iter().zip(&targets)).enumerate() {
                assert_eq!(offset, want, "{start}..{end} by {stride}: line {k}");
                assert_eq!(line, ptr.wrapping_add(offset) as usize, "{start}..{end} by {stride}: line {k}'s pointer");
                assert_eq!(target, ptr.wrapping_add(ahead) as usize, "{start}..{end} by {stride}: line {k}'s prefetch");
            }
        }
    }

    /// Mode 20's words, from the definition: a line `seed + j * step`, both hashed from its address.
    fn mode20_word(ptr: *mut u64, key: u64, i: usize) -> u64 {
        let line = ptr.wrapping_add(i - i % LINE_WORDS);
        let (seed, step) = pattern_gen::mode20_line(line as usize, key);
        pattern_gen::line_words(seed, step)[i % LINE_WORDS]
    }

    /// One width's loops (`simple_simd_loops!`): each write leaves its pattern on its words and no
    /// other, the NT write the cached one's image and each strided write the sequential one's; each
    /// verify reads clean and sees a bad word in every lane of every accumulator and in each tail.
    macro_rules! check_simd_loops {
        ($loops:ident, $w:expr) => {{
            const W: usize = $w;
            let base = 0x5A5A_0000_0000_1234u64;
            let mut mem = Mem::new(8192);
            let ptr = mem.ptr();
            let what = stringify!($loops);
            // Four steps of four vectors and three more: the first step's words are every lane of
            // every accumulator, the last three vectors the tail
            let (start, end) = (64, 64 + 19 * W);
            let pos = SimplePositional { ptr, start, end, base };
            unsafe { $loops::pos_write(pos) };
            mem.assert_image(start, end, |i| i as u64 ^ base, what);
            mem.reset();
            unsafe { $loops::pos_write_nt(pos) };
            mem.assert_image(start, end, |i| i as u64 ^ base, what);
            mem.assert_sees(flips(start, end, 16 * W, 3 * W), || unsafe { $loops::pos_verify(pos) }, what);
            // Fifteen lines: three steps of four lines and a tail of three
            let (start, end) = (64, 64 + 15 * LINE_WORDS);
            let key = Mode20::new(3, 1, 0xDEADBEEFDEADBEEF, 0xCAFEBABECAFEBABE).key;
            let m20 = SimpleMode20 { ptr, start, end, key };
            mem.reset();
            unsafe { $loops::m20_write(m20) };
            mem.assert_image(start, end, |i| mode20_word(ptr, key, i), what);
            mem.assert_sees(flips(start, end, 4 * LINE_WORDS, 3 * LINE_WORDS), || unsafe { $loops::m20_verify(m20) }, what);
            for (start, end, stride) in WALKS {
                let period = strided_period(stride);
                let pos = SimplePositional { ptr, start, end, base };
                mem.reset();
                unsafe { $loops::pos_write_str(pos, period) };
                mem.assert_image(start, end, |i| i as u64 ^ base, what);
                mem.assert_sees(flips(start, end, 2 * LINE_WORDS, 2 * LINE_WORDS), || unsafe { $loops::pos_verify_str(pos, period) }, what);
                let m20 = SimpleMode20 { ptr, start, end, key };
                mem.reset();
                unsafe { $loops::m20_write_str(m20, period) };
                mem.assert_image(start, end, |i| mode20_word(ptr, key, i), what);
                mem.assert_sees(flips(start, end, 2 * LINE_WORDS, 2 * LINE_WORDS), || unsafe { $loops::m20_verify_str(m20, period) }, what);
            }
        }};
    }

    /// TODO 99: the SIMD loops at each width the CPU has (`check_simd_loops!`).
    #[test]
    fn simd_loops_write_their_patterns_and_see_every_bad_word() {
        check_simd_loops!(simple_test_v2_128_loops, 2);
        check_simd_loops!(simple_test_v2_256_loops, 4);
        if is_x86_feature_detected!("avx512f") && is_x86_feature_detected!("avx512bw") && is_x86_feature_detected!("avx512cd")
            && is_x86_feature_detected!("avx512dq") && is_x86_feature_detected!("avx512vl") {
            check_simd_loops!(simple_test_v2_512_loops, 8);
        }
    }

    /// One per-word mode's scalar loops (`SimpleWords`): the image, sequential and strided, at
    /// lengths that end on a step, a vector and each word under a vector; the verifies see a bad
    /// word in every lane of every accumulator and in the tails.
    macro_rules! check_words {
        ($mode:literal, $definition:expr) => {{
            let definition = $definition;
            let what = concat!("mode ", $mode);
            let seed = 0x0123_4567_89AB_CDEFu64;
            let mut mem = Mem::new(8192);
            let ptr = mem.ptr();
            for rest in 0..4 {
                // Five steps of four vectors, three vectors and `rest` words
                let (start, end) = (64, 64 + 5 * 16 + 3 * 4 + rest);
                let c = SimpleWords::<$mode> { ptr, start, end, seed, cl_shift: 3 };
                mem.reset();
                unsafe { simple_test_v2_loops::words_write(c) };
                mem.assert_image(start, end, |i| definition(i as u64, seed), what);
                mem.assert_sees(flips(start, end, 16, 12 + rest), || unsafe { simple_test_v2_loops::words_verify(c) }, what);
            }
            for (start, end, stride) in WALKS {
                let period = strided_period(stride);
                let c = SimpleWords::<$mode> { ptr, start, end, seed, cl_shift: 3 };
                mem.reset();
                unsafe { simple_test_v2_loops::words_write_str(c, period) };
                mem.assert_image(start, end, |i| definition(i as u64, seed), what);
                mem.assert_sees(flips(start, end, 2 * LINE_WORDS, 2 * LINE_WORDS), || unsafe { simple_test_v2_loops::words_verify_str(c, period) }, what);
            }
        }};
    }

    /// TODO 99: the scalar build's per-word modes, each from its `pattern_gen` definition (mode 1's
    /// lines are 64 B, `cl_shift` 3).
    #[test]
    fn scalar_word_loops_write_their_patterns_and_see_every_bad_word() {
        check_words!(0, pattern_gen::pattern_mode0);
        check_words!(1, |i, seed| pattern_gen::pattern_mode1(i, seed, 3));
        check_words!(10, pattern_gen::pattern_mode10);
        check_words!(11, pattern_gen::pattern_mode11);
        check_words!(13, pattern_gen::pattern_mode13);
    }

    /// TODO 99: the scalar build's line modes write what the shared walks (`Mode2`, `Mode20`,
    /// `Mode30`) write and what the definitions give, sequential and strided, mode 30 at every beat;
    /// the verifies see a bad word in every lane of every accumulator and in the tails.
    #[test]
    fn scalar_line_loops_write_their_patterns_and_see_every_bad_word() {
        let mut mem = Mem::new(8192);
        let ptr = mem.ptr();
        // Mode 2: three pages, each chain from its address
        let (start, end) = (512, 512 + 3 * PAGE_WORDS);
        let mode2 = Mode2 { thread_id: 3, cycle: 1, param0: 0x5DEECE66D, param1: 0xB };
        let mut chain = vec![0u64; end];
        for page in (start..end).step_by(PAGE_WORDS) {
            let (mut seed, mut step) = pattern_gen::mode2_page_start(ptr.wrapping_add(page) as usize, 3, 1);
            for line in (page..page + PAGE_WORDS).step_by(LINE_WORDS) {
                chain[line..line + LINE_WORDS].copy_from_slice(&pattern_gen::line_words(seed, step));
                (seed, step) = pattern_gen::mode2_evolve(seed, step, 0x5DEECE66D, 0xB);
            }
        }
        unsafe { mode2.fill(ptr, start, end) };
        let old = mem.words().to_vec();
        mem.reset();
        let c = SimpleMode2 { ptr, start, end, mode2 };
        unsafe { simple_test_v2_loops::mode2_write(c) };
        assert!(mem.words() == old.as_slice(), "mode 2: not the shared walk's image");
        mem.assert_image(start, end, |i| chain[i], "mode 2");
        mem.assert_sees(flips(start, end, 4 * LINE_WORDS, 4 * LINE_WORDS), || unsafe { simple_test_v2_loops::mode2_verify(c) }, "mode 2");

        // Mode 20: fifteen lines (steps of four and a tail of three), then the walks
        let mode20 = Mode20::new(3, 1, 0xDEADBEEFDEADBEEF, 0xCAFEBABECAFEBABE);
        let (start, end) = (64, 64 + 15 * LINE_WORDS);
        mem.reset();
        unsafe { mode20.fill(ptr, start, end) };
        let old = mem.words().to_vec();
        mem.reset();
        let c = SimpleMode20 { ptr, start, end, key: mode20.key };
        unsafe { simple_test_v2_loops::mode20_write(c) };
        assert!(mem.words() == old.as_slice(), "mode 20: not the shared walk's image");
        mem.assert_image(start, end, |i| mode20_word(ptr, mode20.key, i), "mode 20");
        mem.assert_sees(flips(start, end, 4 * LINE_WORDS, 3 * LINE_WORDS), || unsafe { simple_test_v2_loops::mode20_verify(c) }, "mode 20");
        for (start, end, stride) in WALKS {
            let period = strided_period(stride);
            let c = SimpleMode20 { ptr, start, end, key: mode20.key };
            mem.reset();
            unsafe { simple_test_v2_loops::mode20_write_str(c, period) };
            mem.assert_image(start, end, |i| mode20_word(ptr, mode20.key, i), "mode 20 strided");
            mem.assert_sees(flips(start, end, 2 * LINE_WORDS, 2 * LINE_WORDS), || unsafe { simple_test_v2_loops::mode20_verify_str(c, period) }, "mode 20 strided");
        }

        // Mode 30 at each beat
        use pattern_gen::BusBeat;
        for beat in [BusBeat::Bits12, BusBeat::Bits16, BusBeat::Bits24, BusBeat::Bits32, BusBeat::Bits64] {
            let mode30 = Mode30 { key: mode20.key, beat };
            let word = |i: usize| pattern_gen::mode30_line(ptr.wrapping_add(i - i % LINE_WORDS) as usize, mode30.key, beat)[i % LINE_WORDS];
            let what = format!("mode 30 at {} bits", beat.bits());
            let (start, end) = (64, 64 + 15 * LINE_WORDS);
            mem.reset();
            unsafe { mode30.fill(ptr, start, end) };
            let old = mem.words().to_vec();
            mem.reset();
            let c = SimpleMode30 { ptr, start, end, mode30 };
            unsafe { simple_test_v2_loops::mode30_write(c) };
            assert!(mem.words() == old.as_slice(), "{what}: not the shared walk's image");
            mem.assert_image(start, end, word, &what);
            mem.assert_sees(flips(start, end, 4 * LINE_WORDS, 3 * LINE_WORDS), || unsafe { simple_test_v2_loops::mode30_verify(c) }, &what);
            for (start, end, stride) in WALKS {
                let period = strided_period(stride);
                let c = SimpleMode30 { ptr, start, end, mode30 };
                mem.reset();
                unsafe { simple_test_v2_loops::mode30_write_str(c, period) };
                mem.assert_image(start, end, word, &what);
                mem.assert_sees(flips(start, end, 2 * LINE_WORDS, 2 * LINE_WORDS), || unsafe { simple_test_v2_loops::mode30_verify_str(c, period) }, &what);
            }
        }
    }
}

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
            let errors = seal.kernel.verify(ctx.ptr, ctx.chunk_start, ctx.chunk_end, "seal check at");
            // The refill, unless the error halts the run: then the bad words stay in memory
            if errors > 0 && !matches!(error_mode, ErrorMode::Halt) {
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
    crate::seal::SealKernel::new(pattern, width)
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
    // The fill is setup: the checks alone are timed and counted (TODO 90, the harness)
    run_phased_test(
        blocks, thread_id, error_mode, timing, config, progress,
        test_name, TestAction::Verify, 1, config.skip_init, 0, 1,
        |ctx: &ChunkCtx| kernel.fill(ctx.ptr, ctx.chunk_start, ctx.chunk_end),
        |_ctx: &ChunkCtx| {},
        |ctx: &ChunkCtx| -> u64 { kernel.verify(ctx.ptr, ctx.chunk_start, ctx.chunk_end, "seal check of") },
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

// ─── Bench-Init and Bench-Verify kernels (TODO 99) ──────────────────────────────────────────────
//
// Each mode's write and check of one chunk is a function of its own, called once a chunk, so its
// loop keeps its values in registers: inlined into the dispatchers, with every mode's loops and the
// harness live, the checks reloaded constants from the stack every iteration and the line modes
// were vectorized across lines. The per-word modes (0, 1, 10, 11, 13) walk a pointer, 16 words a
// step as four 32 B vectors at fixed displacements (the width the compiler gave the inlined loops
// at the `x86-64-v3` baseline), the expected values a vector stepped beside it, the checks into
// four vector accumulators. Bench-Init's write and Bench-Verify's fill are one function, so the
// verify checks what the init wrote.

/// Writes words `[start, end)` of `ptr`: 16 a step as four vectors, each call of `next` the next
/// four words' values, then the words past the last whole step from `word`.
#[inline(always)]
unsafe fn fill_words_by_16(ptr: *mut u64, start: usize, end: usize, mut next: impl FnMut() -> u64x4,
                           word: impl Fn(usize) -> u64) {
    let len = end - start;
    let mut p = ptr.add(start);
    for _ in 0..len / 16 {
        for k in 0..4 {
            p.add(4 * k).cast::<u64x4>().write_unaligned(next());
        }
        p = p.wrapping_add(16);
    }
    let rest = start + len / 16 * 16;
    for j in 0..len % 16 {
        *p = word(rest + j);
        p = p.wrapping_add(1);
    }
}

/// Checks words `[start, end)` of `ptr` against the same values: each step's four vectors into
/// four vector accumulators, the words past the last whole step into a scalar one. The
/// accumulators ORed: nonzero when a word differs.
#[inline(always)]
unsafe fn verify_words_by_16(ptr: *const u64, start: usize, end: usize, mut next: impl FnMut() -> u64x4,
                             word: impl Fn(usize) -> u64) -> u64 {
    let len = end - start;
    let mut acc = [u64x4::splat(0); 4];
    let mut p = ptr.add(start);
    for _ in 0..len / 16 {
        for (k, a) in acc.iter_mut().enumerate() {
            *a |= p.add(4 * k).cast::<u64x4>().read_unaligned() ^ next();
        }
        p = p.wrapping_add(16);
    }
    let (rest, mut tail) = (start + len / 16 * 16, 0u64);
    for j in 0..len % 16 {
        tail |= *p ^ word(rest + j);
        p = p.wrapping_add(1);
    }
    ((acc[0] | acc[1]) | (acc[2] | acc[3])).reduce_or() | tail
}

/// Calls `body(from, to)` for the pieces of `[start, end)` split at the multiples of `segment`
/// words, or once for the whole range when `segment` is 0.
#[inline(always)]
fn for_segments(start: usize, end: usize, segment: usize, mut body: impl FnMut(usize, usize)) {
    if segment == 0 {
        return body(start, end);
    }
    let mut from = start;
    while from < end {
        let to = (from - from % segment + segment).min(end);
        body(from, to);
        from = to;
    }
}

/// A per-word pattern's chunk kernels, from its `word(i)` and `lanes(start)` (the generator of the
/// values of each next four words from word `start` on, within a piece of `$segment` words, 0 for
/// no pieces): `fill`, `verify` (the accumulators ORed) and `check` (the errors, counted and
/// logged by a rescan when the verify sees one).
macro_rules! word_kernels {
    ($segment:expr) => {
        #[inline(never)]
        unsafe fn fill(self, ptr: *mut u64, start: usize, end: usize) {
            for_segments(start, end, $segment, |from, to| fill_words_by_16(ptr, from, to, self.lanes(from), |i| self.word(i)));
        }

        #[inline(never)]
        unsafe fn verify(self, ptr: *const u64, start: usize, end: usize) -> u64 {
            let mut seen = 0;
            for_segments(start, end, $segment, |from, to| seen |= verify_words_by_16(ptr, from, to, self.lanes(from), |i| self.word(i)));
            seen
        }

        #[inline(always)]
        unsafe fn check(self, ptr: *const u64, start: usize, end: usize, test_name: &str) -> u64 {
            match self.verify(ptr, start, end) {
                0 => 0,
                seen => rescan_words(ptr, start, end, test_name, seen, |i| self.word(i)),
            }
        }
    };
}

/// The indices of four words from `start`, a vector.
#[inline(always)]
fn word_lanes(start: usize) -> u64x4 {
    u64x4::splat(start as u64) + u64x4::from_array([0, 1, 2, 3])
}

/// Mode 0 (`pattern_gen::pattern_mode0`): the seed XOR the word's index times the golden-ratio
/// constant, inverted on odd 4 KiB pages. The kernels go a page at a time, the page's inversion
/// folded into the seed, and step each vector's product, an add a vector: with the index stepped
/// too for the inversion, the step's offsets spilled to the stack.
#[derive(Clone, Copy)]
struct Mode0Words {
    seed: u64,
}

impl Mode0Words {
    #[inline(always)]
    fn word(self, i: usize) -> u64 {
        pattern_gen::pattern_mode0(i as u64, self.seed)
    }

    /// The values from word `start` on, to the end of its page.
    #[inline(always)]
    fn lanes(self, start: usize) -> impl FnMut() -> u64x4 {
        // The multiplier: word 1's value under a zero seed (an even page)
        let golden = pattern_gen::pattern_mode0(1, 0);
        // The page's inversion: word `start`'s value under a zero seed, less its product
        let inversion = pattern_gen::pattern_mode0(start as u64, 0) ^ (start as u64).wrapping_mul(golden);
        let seed = u64x4::splat(self.seed ^ inversion);
        // A product per vector of the step, each stepped a whole step on: one product and an
        // offset per vector became a constant per vector once the loop was unrolled, and those
        // were reloaded from memory every iteration
        let mut product: [u64x4; 4] = std::array::from_fn(|k| word_lanes(start + 4 * k) * u64x4::splat(golden));
        let step = u64x4::splat(golden.wrapping_mul(16));
        move || {
            let words = seed ^ product[0];
            product = [product[1], product[2], product[3], product[0] + step];
            words
        }
    }

    word_kernels!(PAGE_WORDS);
}

/// Mode 1 (`pattern_gen::pattern_mode1`): the seed less three times the word's index, inverted
/// on odd cache lines (of `1 << cl_shift` words). Both from one stepped vector of indices: with the
/// difference stepped too, the step's offsets spilled to the stack.
#[derive(Clone, Copy)]
struct Mode1Words {
    seed: u64,
    cl_shift: u32,
}

impl Mode1Words {
    #[inline(always)]
    fn word(self, i: usize) -> u64 {
        pattern_gen::pattern_mode1(i as u64, self.seed, self.cl_shift)
    }

    #[inline(always)]
    fn lanes(self, start: usize) -> impl FnMut() -> u64x4 {
        let mut idx = word_lanes(start);
        let (seed, three, four) = (u64x4::splat(self.seed), u64x4::splat(3), u64x4::splat(4));
        let (shift, zero, one) = (u64x4::splat(u64::from(self.cl_shift)), u64x4::splat(0), u64x4::splat(1));
        move || {
            let line_mask = zero - ((idx >> shift) & one);
            let words = (seed - idx * three) ^ line_mask;
            idx += four;
            words
        }
    }

    word_kernels!(0);
}

/// Modes 10 and 11 (`pattern_gen::pattern_mode10` and `pattern_mode11`, one function): the word's
/// index XOR a key, mode 10's base or mode 11's two parameters XORed.
#[derive(Clone, Copy)]
struct XorWords {
    key: u64,
}

impl XorWords {
    #[inline(always)]
    fn word(self, i: usize) -> u64 {
        pattern_gen::pattern_mode10(i as u64, self.key)
    }

    #[inline(always)]
    fn lanes(self, start: usize) -> impl FnMut() -> u64x4 {
        let mut idx = word_lanes(start);
        let (key, four) = (u64x4::splat(self.key), u64x4::splat(4));
        move || {
            let words = idx ^ key;
            idx += four;
            words
        }
    }

    word_kernels!(0);
}

/// Mode 13 (`pattern_gen::pattern_mode13`): the splitmix64 finalizer of the word's index plus the
/// seed, four lanes at once. The constants are `pattern_mode13`'s; the unit test holds the two
/// equal.
#[derive(Clone, Copy)]
struct Mode13Words {
    seed: u64,
}

impl Mode13Words {
    #[inline(always)]
    fn word(self, i: usize) -> u64 {
        pattern_gen::pattern_mode13(i as u64, self.seed)
    }

    #[inline(always)]
    fn lanes(self, start: usize) -> impl FnMut() -> u64x4 {
        let mut sum = word_lanes(start) + u64x4::splat(self.seed);
        let (m1, m2, four) = (u64x4::splat(0xbf58476d1ce4e5b9), u64x4::splat(0x94d049bb133111eb), u64x4::splat(4));
        move || {
            let mut h = sum;
            h ^= h >> 30;
            h *= m1;
            h ^= h >> 27;
            h *= m2;
            h ^= h >> 31;
            sum += four;
            h
        }
    }

    word_kernels!(0);
}

/// Mode 2's fill and check of one chunk as functions of their own, for the same reason. `Mode2`
/// holds the loop, which SimpleV2 shares: its chain keeps the compiler a line at a time.
#[inline(never)]
unsafe fn mode2_fill_chunk(mode: Mode2, ptr: *mut u64, start: usize, end: usize) {
    mode.fill(ptr, start, end)
}

#[inline(never)]
unsafe fn mode2_check_chunk(mode: Mode2, ptr: *const u64, start: usize, end: usize, test_name: &str) -> u64 {
    mode.verify(ptr, start, end, test_name)
}

/// Writes the lines of words `[start, end)` of `ptr` (whole lines), each the words `words(line)`
/// gives it, as two 32 B vectors from a walked pointer. Explicit vectors keep the walk a line at a
/// time: from the 8-word arrays the compiler vectorized the walk across four lines, transposing
/// them, and spilled (modes 20 and 30).
#[inline(always)]
unsafe fn fill_lines(ptr: *mut u64, start: usize, end: usize, mut words: impl FnMut(*const u64) -> [u64; LINE_WORDS]) {
    debug_assert!(start.is_multiple_of(LINE_WORDS) && end.is_multiple_of(LINE_WORDS), "whole lines: {start}..{end}");
    let mut p = ptr.add(start);
    for _ in 0..(end - start) / LINE_WORDS {
        let w = words(p);
        p.cast::<u64x4>().write_unaligned(u64x4::from_slice(&w[..4]));
        p.add(4).cast::<u64x4>().write_unaligned(u64x4::from_slice(&w[4..]));
        p = p.wrapping_add(LINE_WORDS);
    }
}

/// Checks the same lines: the differences ORed into the four accumulators `accumulate_line` keeps,
/// words `k` and `k + 4` into accumulator `k`, here the lanes of one vector. Nonzero when a word
/// differs.
#[inline(always)]
unsafe fn verify_lines(ptr: *const u64, start: usize, end: usize, mut words: impl FnMut(*const u64) -> [u64; LINE_WORDS]) -> u64 {
    debug_assert!(start.is_multiple_of(LINE_WORDS) && end.is_multiple_of(LINE_WORDS), "whole lines: {start}..{end}");
    let mut acc = u64x4::splat(0);
    let mut p = ptr.add(start);
    for _ in 0..(end - start) / LINE_WORDS {
        let w = words(p);
        let low = p.cast::<u64x4>().read_unaligned() ^ u64x4::from_slice(&w[..4]);
        let high = p.add(4).cast::<u64x4>().read_unaligned() ^ u64x4::from_slice(&w[4..]);
        acc |= low | high;
        p = p.wrapping_add(LINE_WORDS);
    }
    acc.reduce_or()
}

/// Mode 20's fill of one chunk (`Mode20::fill`'s lines).
#[inline(never)]
unsafe fn mode20_fill_chunk(mode: Mode20, ptr: *mut u64, start: usize, end: usize) {
    fill_lines(ptr, start, end, |line| {
        let (seed, step) = pattern_gen::mode20_line(line as usize, mode.key);
        pattern_gen::line_words(seed, step)
    })
}

/// Mode 20's check of one chunk: `Mode20::verify`'s, its rescan on a hit.
#[inline(never)]
unsafe fn mode20_check_chunk(mode: Mode20, ptr: *const u64, start: usize, end: usize, test_name: &str) -> u64 {
    let seen = verify_lines(ptr, start, end, |line| {
        let (seed, step) = pattern_gen::mode20_line(line as usize, mode.key);
        pattern_gen::line_words(seed, step)
    });
    if seen == 0 {
        return 0;
    }
    mode.rescan(ptr, start, end, test_name, seen)
}

/// Mode 30's fill of one chunk (`Mode30::fill`'s lines), a loop per beat.
#[inline(never)]
unsafe fn mode30_fill_chunk(mode: Mode30, ptr: *mut u64, start: usize, end: usize) {
    with_mode30_line!(mode, |expected| {
        fill_lines(ptr, start, end, expected)
    })
}

/// Mode 30's check of one chunk: `Mode30::verify`'s, its rescan on a hit.
#[inline(never)]
unsafe fn mode30_check_chunk(mode: Mode30, ptr: *const u64, start: usize, end: usize, test_name: &str) -> u64 {
    let seen = with_mode30_line!(mode, |expected| {
        verify_lines(ptr, start, end, expected)
    });
    if seen == 0 {
        return 0;
    }
    mode.rescan(ptr, start, end, test_name, seen)
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
    let mode0 = |ctx: &ChunkCtx| Mode0Words { seed: pattern_gen::block_seed(ctx.ptr as usize, ctx.thread_id, 0) };
    let mode1 = |ctx: &ChunkCtx| Mode1Words { seed: pattern_gen::block_seed(ctx.ptr as usize, ctx.thread_id, 0), cl_shift };

    let test_name = match mode {
        0 => "Bench-Init-TM5-0",
        1 => "Bench-Init-TM5-1",
        2 => "Bench-Init-TM5-2",
        10 => "Bench-Init-TMR-10",
        11 => "Bench-Init-TMR-11",
        13 => "Bench-Init-TMR-13",
        20 => "Bench-Init-TMR-20",
        30 => "Bench-Init-TMR-30",
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
                |ctx: &ChunkCtx| mode0(ctx).fill(ctx.ptr, ctx.chunk_start, ctx.chunk_end),
                |ctx: &ChunkCtx| mode0(ctx).fill(ctx.ptr, ctx.chunk_start, ctx.chunk_end),
                |_ctx: &ChunkCtx| -> u64 { 0 },
            )
        }
        1 => {
            // Mode 1: linear step + cache-line complement
            run_phased_test(
                blocks, thread_id, error_mode, timing, config, progress,
                test_name, TestAction::Write, 1, skip_init, 1, 0,
                |ctx: &ChunkCtx| mode1(ctx).fill(ctx.ptr, ctx.chunk_start, ctx.chunk_end),
                |ctx: &ChunkCtx| mode1(ctx).fill(ctx.ptr, ctx.chunk_start, ctx.chunk_end),
                |_ctx: &ChunkCtx| -> u64 { 0 },
            )
        }
        2 => {
            // Mode 2: TM5's evolving lines, the chain restarted per 4 KiB page
            let mode2 = Mode2 { thread_id, cycle: 0, param0, param1 };
            run_phased_test(
                blocks, thread_id, error_mode, timing, config, progress,
                test_name, TestAction::Write, 1, skip_init, 1, 0,
                |ctx: &ChunkCtx| mode2_fill_chunk(mode2, ctx.ptr, ctx.chunk_start, ctx.chunk_end),
                |ctx: &ChunkCtx| mode2_fill_chunk(mode2, ctx.ptr, ctx.chunk_start, ctx.chunk_end),
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
                |ctx: &ChunkCtx| XorWords { key: base }.fill(ctx.ptr, ctx.chunk_start, ctx.chunk_end),
                |ctx: &ChunkCtx| XorWords { key: base }.fill(ctx.ptr, ctx.chunk_start, ctx.chunk_end),
                |_ctx: &ChunkCtx| -> u64 { 0 },
            )
        }
        11 => {
            // Mode 11: idx ^ combined (param0 ^ param1)
            let combined = param0 ^ param1;
            run_phased_test(
                blocks, thread_id, error_mode, timing, config, progress,
                test_name, TestAction::Write, 1, skip_init, 1, 0,
                |ctx: &ChunkCtx| XorWords { key: combined }.fill(ctx.ptr, ctx.chunk_start, ctx.chunk_end),
                |ctx: &ChunkCtx| XorWords { key: combined }.fill(ctx.ptr, ctx.chunk_start, ctx.chunk_end),
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
                |ctx: &ChunkCtx| Mode13Words { seed }.fill(ctx.ptr, ctx.chunk_start, ctx.chunk_end),
                |ctx: &ChunkCtx| Mode13Words { seed }.fill(ctx.ptr, ctx.chunk_start, ctx.chunk_end),
                |_ctx: &ChunkCtx| -> u64 { 0 },
            )
        }
        30 => {
            // Mode 30: the bus-flip lines
            let mode30 = Mode30::new(thread_id, 0, param0, param1);
            run_phased_test(
                blocks, thread_id, error_mode, timing, config, progress,
                test_name, TestAction::Write, 1, skip_init, 1, 0,
                |ctx: &ChunkCtx| mode30_fill_chunk(mode30, ctx.ptr, ctx.chunk_start, ctx.chunk_end),
                |ctx: &ChunkCtx| mode30_fill_chunk(mode30, ctx.ptr, ctx.chunk_start, ctx.chunk_end),
                |_ctx: &ChunkCtx| -> u64 { 0 },
            )
        }
        _ => {
            // Mode 20 (and any unknown mode): mode 2's lines, hashed from each line's address
            let mode20 = Mode20::new(thread_id, 0, param0, param1);
            run_phased_test(
                blocks, thread_id, error_mode, timing, config, progress,
                test_name, TestAction::Write, 1, skip_init, 1, 0,
                |ctx: &ChunkCtx| mode20_fill_chunk(mode20, ctx.ptr, ctx.chunk_start, ctx.chunk_end),
                |ctx: &ChunkCtx| mode20_fill_chunk(mode20, ctx.ptr, ctx.chunk_start, ctx.chunk_end),
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
    // The fill is setup: the verifies alone are timed and counted (TODO 90, the harness)
    let mode = config.pattern_mode.unwrap_or(10);
    let param0 = config.pattern_param0.unwrap_or(0xDEADBEEFDEADBEEF);
    let param1 = config.pattern_param1.unwrap_or(0xCAFEBABECAFEBABE);
    let cl_shift = pattern_gen::cache_line_shift(config.cache_line_bytes);
    let skip_init = config.skip_init;
    let mode0 = |ctx: &ChunkCtx| Mode0Words { seed: pattern_gen::block_seed(ctx.ptr as usize, ctx.thread_id, 0) };
    let mode1 = |ctx: &ChunkCtx| Mode1Words { seed: pattern_gen::block_seed(ctx.ptr as usize, ctx.thread_id, 0), cl_shift };

    let test_name = match mode {
        0 => "Bench-Verify-TM5-0",
        1 => "Bench-Verify-TM5-1",
        2 => "Bench-Verify-TM5-2",
        10 => "Bench-Verify-TMR-10",
        11 => "Bench-Verify-TMR-11",
        13 => "Bench-Verify-TMR-13",
        20 => "Bench-Verify-TMR-20",
        30 => "Bench-Verify-TMR-30",
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
                |ctx: &ChunkCtx| mode0(ctx).fill(ctx.ptr, ctx.chunk_start, ctx.chunk_end),
                |_ctx: &ChunkCtx| {},
                |ctx: &ChunkCtx| -> u64 { mode0(ctx).check(ctx.ptr, ctx.chunk_start, ctx.chunk_end, test_name) },
            )
        }
        1 => {
            // Mode 1: linear step + cache-line complement
            run_phased_test(
                blocks, thread_id, error_mode, timing, config, progress,
                test_name, TestAction::Verify, 1, skip_init, 0, 1,
                |ctx: &ChunkCtx| mode1(ctx).fill(ctx.ptr, ctx.chunk_start, ctx.chunk_end),
                |_ctx: &ChunkCtx| {},
                |ctx: &ChunkCtx| -> u64 { mode1(ctx).check(ctx.ptr, ctx.chunk_start, ctx.chunk_end, test_name) },
            )
        }
        2 => {
            // Mode 2: TM5's evolving lines, the chain restarted per 4 KiB page
            let mode2 = Mode2 { thread_id, cycle: 0, param0, param1 };
            run_phased_test(
                blocks, thread_id, error_mode, timing, config, progress,
                test_name, TestAction::Verify, 1, skip_init, 0, 1,
                |ctx: &ChunkCtx| mode2_fill_chunk(mode2, ctx.ptr, ctx.chunk_start, ctx.chunk_end),
                |_ctx: &ChunkCtx| {},
                |ctx: &ChunkCtx| -> u64 { mode2_check_chunk(mode2, ctx.ptr, ctx.chunk_start, ctx.chunk_end, test_name) },
            )
        }
        // --- TMR-native modes ---
        10 => {
            // Mode 10: idx ^ base (address-derived unique)
            let base = 0xDEADBEEFDEADBEEF_u64;
            run_phased_test(
                blocks, thread_id, error_mode, timing, config, progress,
                test_name, TestAction::Verify, 1, skip_init, 0, 1,
                |ctx: &ChunkCtx| XorWords { key: base }.fill(ctx.ptr, ctx.chunk_start, ctx.chunk_end),
                |_ctx: &ChunkCtx| {},
                |ctx: &ChunkCtx| -> u64 { XorWords { key: base }.check(ctx.ptr, ctx.chunk_start, ctx.chunk_end, test_name) },
            )
        }
        11 => {
            // Mode 11: idx ^ combined (param0 ^ param1)
            let combined = param0 ^ param1;
            run_phased_test(
                blocks, thread_id, error_mode, timing, config, progress,
                test_name, TestAction::Verify, 1, skip_init, 0, 1,
                |ctx: &ChunkCtx| XorWords { key: combined }.fill(ctx.ptr, ctx.chunk_start, ctx.chunk_end),
                |_ctx: &ChunkCtx| {},
                |ctx: &ChunkCtx| -> u64 { XorWords { key: combined }.check(ctx.ptr, ctx.chunk_start, ctx.chunk_end, test_name) },
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
                |ctx: &ChunkCtx| Mode13Words { seed }.fill(ctx.ptr, ctx.chunk_start, ctx.chunk_end),
                |_ctx: &ChunkCtx| {},
                |ctx: &ChunkCtx| -> u64 { Mode13Words { seed }.check(ctx.ptr, ctx.chunk_start, ctx.chunk_end, test_name) },
            )
        }
        30 => {
            // Mode 30: the bus-flip lines
            let mode30 = Mode30::new(thread_id, 0, param0, param1);
            run_phased_test(
                blocks, thread_id, error_mode, timing, config, progress,
                test_name, TestAction::Verify, 1, skip_init, 0, 1,
                |ctx: &ChunkCtx| mode30_fill_chunk(mode30, ctx.ptr, ctx.chunk_start, ctx.chunk_end),
                |_ctx: &ChunkCtx| {},
                |ctx: &ChunkCtx| -> u64 { mode30_check_chunk(mode30, ctx.ptr, ctx.chunk_start, ctx.chunk_end, test_name) },
            )
        }
        _ => {
            // Mode 20 (and any unknown mode): mode 2's lines, hashed from each line's address
            let mode20 = Mode20::new(thread_id, 0, param0, param1);
            run_phased_test(
                blocks, thread_id, error_mode, timing, config, progress,
                test_name, TestAction::Verify, 1, skip_init, 0, 1,
                |ctx: &ChunkCtx| mode20_fill_chunk(mode20, ctx.ptr, ctx.chunk_start, ctx.chunk_end),
                |_ctx: &ChunkCtx| {},
                |ctx: &ChunkCtx| -> u64 { mode20_check_chunk(mode20, ctx.ptr, ctx.chunk_start, ctx.chunk_end, test_name) },
            )
        }
    }
}

#[cfg(test)]
mod bench_move_tests {
    use super::{BlockMoveChunk, Mode0Words, Mode1Words, Mode13Words, Mode2, Mode20, Mode30, XorWords, mode2_check_chunk,
                mode2_fill_chunk, mode20_check_chunk, mode20_fill_chunk, mode30_check_chunk, mode30_fill_chunk};
    use crate::pattern_gen::{self, BusBeat, LINE_WORDS, PAGE_WORDS};

    /// Mem-BlockMove's copy loops before TODO 99, as they were: the image the new copies must leave.
    unsafe fn old_copy(src_base: *const u64, dst_base: *mut u64, processed: usize, chunk_end: usize, copy_dirs: u32) {
        match copy_dirs {
            1 => {
                for i in processed..chunk_end {
                    *dst_base.add(i) = *src_base.add(i);
                }
            }
            2 => {
                let mid = processed + (chunk_end - processed) / 2;
                for i in processed..mid {
                    *dst_base.add(i) = *src_base.add(i);
                }
                for i in 0..(chunk_end - mid) {
                    let idx = chunk_end - 1 - i;
                    *dst_base.add(idx) = *src_base.add(idx);
                }
            }
            4 => {
                let block_size = (chunk_end - processed) / 4;
                for stream in 0..4 {
                    let start_idx = processed + stream * block_size;
                    let end_idx = (processed + (stream + 1) * block_size).min(chunk_end);
                    if start_idx < end_idx {
                        match stream & 3 {
                            0 | 3 => {
                                for i in start_idx..end_idx {
                                    *dst_base.add(i) = *src_base.add(i);
                                }
                            }
                            1 => {
                                for i in 0..(end_idx - start_idx) {
                                    let idx = end_idx - 1 - i;
                                    *dst_base.add(idx) = *src_base.add(idx);
                                }
                            }
                            _ => {
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
                let dirs = copy_dirs as usize;
                for dir in 0..dirs {
                    for i in (processed + dir..chunk_end).step_by(dirs) {
                        *dst_base.add(i) = *src_base.add(i);
                    }
                }
            }
        }
    }

    /// TODO 99: Mem-BlockMove's half-chunk kernels. The fill writes each source word the pattern
    /// plus its index and nothing else; each direction pattern's copy leaves the image the old
    /// loops left (the 4-way's quarters too, which miss the last words of a half-chunk that isn't
    /// a multiple of 4); a whole copy reads clean, and a bad word is seen in each lane of each
    /// accumulator and in the tail.
    #[test]
    fn block_move_copies_as_before_and_sees_every_bad_word() {
        let pattern_base = 0xDEADBEEFCAFEBABEu64;
        // Whole steps; a start off the step with a tail and quarters of 85 words and 3 over; a short one
        for (start, len) in [(256usize, 512usize), (300, 16 * 21 + 7), (40, 16 * 3 + 2)] {
            for dirs in [1u32, 2, 4, 3, 5, 8] {
                let half = start + len + 64;
                let mut mem = vec![!0u64; 2 * half];
                let base = mem.as_mut_ptr();
                let chunk = BlockMoveChunk { src: base, dst: base.wrapping_add(half), start, end: start + len, pattern_base };
                unsafe { chunk.fill() };
                for (i, &word) in mem.iter().enumerate() {
                    let want = if (start..start + len).contains(&i) { pattern_base.wrapping_add(i as u64) } else { !0 };
                    assert_eq!(word, want, "fill: word {i}");
                }
                let mut old = mem.clone();
                let old_base = old.as_mut_ptr();
                unsafe {
                    old_copy(old_base, old_base.add(half), start, start + len, dirs);
                    chunk.copy(dirs);
                }
                assert_eq!(mem, old, "{dirs} directions, words {start} + {len}: the copy's image");
                let whole = (start..start + len).all(|i| mem[half + i] == pattern_base.wrapping_add(i as u64));
                assert_eq!(unsafe { chunk.verify() } == 0, whole, "{dirs} directions, words {start} + {len}: the verify");
                assert!(whole || dirs == 4, "{dirs} directions copy every word");
            }
            // The check, on a whole copy: each word of the first step (every lane of the four
            // accumulators), each of the tail, and a spread
            let half = start + len + 64;
            let mut mem = vec![!0u64; 2 * half];
            let base = mem.as_mut_ptr();
            let chunk = BlockMoveChunk { src: base, dst: base.wrapping_add(half), start, end: start + len, pattern_base };
            unsafe {
                chunk.fill();
                chunk.copy(1);
            }
            assert_eq!(unsafe { chunk.verify() }, 0, "words {start} + {len}: clean");
            let end = start + len;
            let words = (start..start + 16).chain(end - len % 16..end).chain((start..end).step_by(37));
            for word in words {
                mem[half + word] ^= 1 << 40;
                assert_ne!(unsafe { chunk.verify() }, 0, "words {start} + {len}: missed word {word}");
                mem[half + word] ^= 1 << 40;
            }
        }
    }

    /// TODO 99: the per-word Bench kernels (modes 0, 1 at four line sizes, 10, 11 and 13) write each
    /// word its mode's value, `pattern_gen`'s word by word, and nothing else; read clean; and see a
    /// bad word in each lane of each accumulator, at each page edge and in the tails, which the
    /// check counts once.
    #[test]
    fn bench_word_kernels_write_their_pattern_and_see_every_bad_word() {
        let seed = 0x0123_4567_89AB_CDEFu64;
        // Whole pages; a start off the step, across pages, with a tail; pieces shorter than a step
        // either side of a page edge
        let ranges = [(0usize, 2 * PAGE_WORDS), (300, 300 + 16 * 45 + 11), (3 * PAGE_WORDS - 5, 3 * PAGE_WORDS + 40)];
        macro_rules! case {
            ($words:expr, $want:expr) => {{
                let (words, want) = ($words, $want);
                for (start, end) in ranges {
                    let mut mem = vec![!0u64; end + 64];
                    let p = mem.as_mut_ptr();
                    unsafe { words.fill(p, start, end) };
                    for (i, &word) in mem.iter().enumerate() {
                        let expected = if (start..end).contains(&i) { want(i as u64) } else { !0 };
                        assert_eq!(word, expected, "{}: word {i} of {start}..{end}", stringify!($words));
                    }
                    assert_eq!(unsafe { words.verify(p, start, end) }, 0, "{}: {start}..{end} clean", stringify!($words));
                    assert_eq!(unsafe { words.check(p, start, end, "test") }, 0);
                    let len = end - start;
                    let page_edges = (start..end).filter(|i| i % PAGE_WORDS == 0 || i % PAGE_WORDS == PAGE_WORDS - 1);
                    let bad = (start..start + 16).chain(end - len % 16..end).chain(page_edges).chain((start..end).step_by(97));
                    for word in bad {
                        mem[word] ^= 1 << 40;
                        assert_ne!(unsafe { words.verify(p, start, end) }, 0, "{}: {start}..{end} missed word {word}", stringify!($words));
                        assert_eq!(unsafe { words.check(p, start, end, "test") }, 1, "{}: word {word} counted once", stringify!($words));
                        mem[word] ^= 1 << 40;
                    }
                }
            }};
        }
        case!(Mode0Words { seed }, |i| pattern_gen::pattern_mode0(i, seed));
        for cl_shift in [3, 0, 2, 4] {
            case!(Mode1Words { seed, cl_shift }, |i| pattern_gen::pattern_mode1(i, seed, cl_shift));
        }
        let base = 0xDEADBEEFDEADBEEFu64;
        case!(XorWords { key: base }, |i| pattern_gen::pattern_mode10(i, base));
        let combined = base ^ 0xCAFEBABECAFEBABE;
        case!(XorWords { key: combined }, |i| pattern_gen::pattern_mode11(i, combined));
        case!(Mode13Words { seed }, |i| pattern_gen::pattern_mode13(i, seed));
    }

    /// TODO 99: the line Bench kernels (modes 20 and 30 at every beat, and mode 2 out of line)
    /// write each line its mode's words, `pattern_gen`'s line by line, and nothing else; read
    /// clean; and see a bad word in each lane of the accumulator, in either half of a line, and in
    /// the last line, which the check counts once.
    #[test]
    fn bench_line_kernels_write_their_lines_and_see_every_bad_word() {
        let total = 4 * PAGE_WORDS;
        // 64-byte aligned, so each line is a cache line, as in a test's memory
        let mut buf = vec![std::simd::u64x8::splat(0); total / LINE_WORDS];
        let p = buf.as_mut_ptr() as *mut u64;
        let run = |name: &str, start: usize, end: usize, fill: &dyn Fn(*mut u64, usize, usize),
                   check: &dyn Fn(*const u64, usize, usize) -> u64, want: &dyn Fn(usize) -> [u64; LINE_WORDS]| unsafe {
            std::slice::from_raw_parts_mut(p, total).fill(!0);
            fill(p, start, end);
            for i in 0..total {
                let expected = if (start..end).contains(&i) { want(p.add(i - i % LINE_WORDS) as usize)[i % LINE_WORDS] } else { !0 };
                assert_eq!(*p.add(i), expected, "{name}: word {i} of {start}..{end}");
            }
            assert_eq!(check(p, start, end), 0, "{name}: {start}..{end} clean");
            let bad = (start..start + LINE_WORDS).chain(end - LINE_WORDS..end).chain((start..end).step_by(61));
            for word in bad {
                *p.add(word) ^= 1 << 40;
                assert_eq!(check(p, start, end), 1, "{name}: {start}..{end}, word {word} counted once");
                *p.add(word) ^= 1 << 40;
            }
        };

        let mode20 = Mode20::new(3, 0, 0x5DEECE66D, 0xB);
        let lines20 = |line: usize| {
            let (seed, step) = pattern_gen::mode20_line(line, mode20.key);
            pattern_gen::line_words(seed, step)
        };
        for (start, end) in [(0, total), (3 * LINE_WORDS, total - 5 * LINE_WORDS)] {
            run("mode 20", start, end, &|p, s, e| unsafe { mode20_fill_chunk(mode20, p, s, e) },
                &|p, s, e| unsafe { mode20_check_chunk(mode20, p, s, e, "test") }, &lines20);
        }

        let key = pattern_gen::mode20_key(3, 0, 0x5DEECE66D, 0xB);
        for beat in [BusBeat::Bits12, BusBeat::Bits16, BusBeat::Bits24, BusBeat::Bits32, BusBeat::Bits64] {
            let mode30 = Mode30 { key, beat };
            for (start, end) in [(0, total), (3 * LINE_WORDS, total - 5 * LINE_WORDS)] {
                run(&format!("mode 30 at {} bits", beat.bits()), start, end, &|p, s, e| unsafe { mode30_fill_chunk(mode30, p, s, e) },
                    &|p, s, e| unsafe { mode30_check_chunk(mode30, p, s, e, "test") }, &|line| pattern_gen::mode30_line(line, key, beat));
            }
        }

        // Mode 2's chain, from the start of each page of the range (pages count from the pointer)
        let mode2 = Mode2 { thread_id: 3, cycle: 0, param0: 0x5DEECE66D, param1: 0xB };
        let base = p as usize;
        let lines2 = |line: usize| {
            let word = (line - base) / 8;
            let page = word - word % PAGE_WORDS;
            let (mut seed, mut step) = pattern_gen::mode2_page_start(base + page * 8, 3, 0);
            for _ in 0..(word - page) / LINE_WORDS {
                (seed, step) = pattern_gen::mode2_evolve(seed, step, 0x5DEECE66D, 0xB);
            }
            pattern_gen::line_words(seed, step)
        };
        run("mode 2", PAGE_WORDS, total, &|p, s, e| unsafe { mode2_fill_chunk(mode2, p, s, e) },
            &|p, s, e| unsafe { mode2_check_chunk(mode2, p, s, e, "test") }, &lines2);
    }
}

// ============================================================================
// Test Registry - Maps config test names to actual function implementations
// ============================================================================

/// The per-chunk loop a test that runs through `run_phased_test` actually runs, as
/// (`write_read_cycles`, `test_reps`, `verify_reps`): the config's where the test reads them, its
/// own where it fixes them. `None` for Tier 2, bandwidth and latency tests, which ignore all three
/// (`doc/test_harness_tiers.md`). A new Tier 1 test family adds itself here.
pub fn effective_reps(actual_name: &str, config: &TestMemoryConfig) -> Option<(u32, u32, u32)> {
    let wrc = config.write_read_cycles;
    let native_bench = |name: &str| native_bench_tests().iter().any(|(i, v, _, _)| *i == name || *v == name);
    match actual_name {
        n if n.starts_with("Mem-SimpleV2") || n.starts_with("Mem-MirrorV2") => Some((wrc, config.test_reps, config.verify_reps)),
        n if n.starts_with("Mem-SimpleNT") => Some((wrc, 1, 1)),
        "Seal-Check" => Some((wrc, 0, 1)),
        n if native_bench(n) => Some((1, u32::from(n.starts_with("Bench-Init-")), u32::from(n.starts_with("Bench-Verify-")))),
        n if n.starts_with("Bench-Init-") => Some((wrc, 1, 0)),
        n if n.starts_with("Bench-Verify-") => Some((wrc, 0, 1)),
        _ => None,
    }
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
        "Mem-MovingInv" => Some(TestFunction::MultiBlock(moving_inversions_multi)),
        "Mem-ModuloX" => Some(TestFunction::MultiBlock(modulo_x_multi)),
        "Mem-Sequence" => Some(TestFunction::MultiBlock(sequence_auto_multi)),

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

        // The same at one width (TODO 94), as the -Auto names resolve: Spd-{tier}-{op}-{128,256,512}
        s if s.starts_with("Spd-") && (s.ends_with("-128") || s.ends_with("-256") || s.ends_with("-512")) => {
            use crate::bandwidth_tests::*;
            let dram = s.starts_with("Spd-DRAM");
            let width = &s[s.len() - 3..];
            // These have no runtime feature check of their own: a width the CPU lacks isn't a test
            if (width == "512" && !is_x86_feature_detected!("avx512f")) || (width == "256" && !is_x86_feature_detected!("avx2")) {
                return None;
            }
            let op = s.trim_end_matches(&s[s.len() - 4..]).rsplit('-').next()?;
            let f: crate::runner::TestFunctionMultiBlock = match (op, dram, width) {
                ("Read", _, "128") => spd_read_128_multi,
                ("Read", _, "256") => spd_read_256_multi,
                ("Read", _, _) => spd_read_512_multi,
                ("Write", false, "128") => spd_write_128_multi,
                ("Write", false, "256") => spd_write_256_multi,
                ("Write", false, _) => spd_write_512_multi,
                ("Write", true, "128") => spd_write_nt_128_multi,
                ("Write", true, "256") => spd_write_nt_256_multi,
                ("Write", true, _) => spd_write_nt_512_multi,
                ("Copy", false, "128") => spd_copy_128_multi,
                ("Copy", false, "256") => spd_copy_256_multi,
                ("Copy", false, _) => spd_copy_512_multi,
                ("Copy", true, "128") => spd_copy_nt_128_multi,
                ("Copy", true, "256") => spd_copy_nt_256_multi,
                ("Copy", true, _) => spd_copy_nt_512_multi,
                _ => return None,
            };
            Some(TestFunction::MultiBlock(f))
        }

        // Bench-Init: Pattern generation throughput benchmarks
        "Bench-Init-TM5-0" | "Bench-Init-TM5-1" | "Bench-Init-TM5-2"
        | "Bench-Init-TMR-10" | "Bench-Init-TMR-11" | "Bench-Init-TMR-13"
        | "Bench-Init-TMR-20" | "Bench-Init-TMR-30"
            => Some(TestFunction::MultiBlock(bench_init_multi)),

        // Bench-Verify: Pattern verification throughput benchmarks
        // Can run independently (writes patterns first) or dependently (skip_init in config)
        "Bench-Verify-TM5-0" | "Bench-Verify-TM5-1" | "Bench-Verify-TM5-2"
        | "Bench-Verify-TMR-10" | "Bench-Verify-TMR-11" | "Bench-Verify-TMR-13"
        | "Bench-Verify-TMR-20" | "Bench-Verify-TMR-30"
            => Some(TestFunction::MultiBlock(bench_verify_multi)),

        // The native families' write and verify alone (pattern spec v2)
        s if native_bench_tests().iter().any(|(init, verify, _, _)| s == *init || s == *verify)
            => native_bench_tests().into_iter().find_map(|(init, verify, fi, fv)| {
                if s == init { Some(TestFunction::MultiBlock(fi)) } else if s == verify { Some(TestFunction::MultiBlock(fv)) } else { None }
            }),

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
    use super::{accumulate_stride_4, prefetch_line, random_replay, Rescan, rescan_words, strided_ahead, strided_lookahead, strided_period, StrideWalk, CacheBustChunk, CACHE_BUST_WORDS,
                strided_wrap, verify_words, Mode20, Mode2, STRIDED_AHEAD};
    use crate::pattern_gen::{LINE_WORDS, PAGE_WORDS};

    /// TODO 93: TM5's strided walk touches each line once, in TM5's order: the period's last line
    /// first, each pass a period at a time. Parameter 8 on 2 channels is 15 lines of stride. Each
    /// step prefetches the line the walk reaches `STRIDED_AHEAD` steps on, or as many as the
    /// shortest pass has lines.
    #[test]
    fn the_strided_walk_is_tm5s() {
        let mem = [0u64; 1];
        let walk = |start: usize, end: usize, stride: usize| {
            let mut lines = Vec::new();
            for_each_strided_line!(mem.as_ptr(), start, end, strided_period(stride), |offset| { lines.push(offset); });
            lines
        };
        // The prefetch targets, by the walk's own loops
        let targets = |start: usize, end: usize, stride: usize| {
            let period = strided_period(stride);
            let ahead = strided_lookahead(start, end, period);
            let mut targets = Vec::new();
            let mut first = start + period;
            while first > start {
                first -= LINE_WORDS;
                let wrap = strided_wrap(first, end, period);
                let mut offset = first;
                while offset < end {
                    targets.push(strided_ahead(offset, ahead, end, wrap));
                    offset += period;
                }
            }
            (targets, ahead / period)
        };
        assert_eq!(walk(0, 6 * LINE_WORDS, 2 * LINE_WORDS), [16, 40, 8, 32, 0, 24]);
        for (start, end, stride) in [(0, 4096, 15 * 8), (512, 4096 + 512, 15 * 8), (0, 4096 + 40, 15 * 8), (0, 4096, 63 * 8), (0, 512, 4095 * 8), (64, 128, 0)] {
            let lines = walk(start, end, stride);
            let period = strided_period(stride);
            let mut sorted = lines.clone();
            sorted.sort();
            assert_eq!(sorted, (start..end).step_by(LINE_WORDS).collect::<Vec<_>>(), "{start}..{end} by {stride}: each line once");
            // The last line of the first period, or of the chunk when the period is longer
            assert_eq!(lines[0], (start + period - LINE_WORDS).min(end - LINE_WORDS), "{start}..{end} by {stride}: the first line");
            let (targets, steps) = targets(start, end, stride);
            assert_eq!(steps, STRIDED_AHEAD.min((end - start) / period), "{start}..{end} by {stride}: the lookahead");
            for i in 0..lines.len().saturating_sub(steps) {
                assert_eq!(targets[i], lines[i + steps], "{start}..{end} by {stride}: step {i} prefetches {steps} on");
            }
        }
    }

    /// A failed check's record keeps the bits the check saw that the rescan didn't see again (Astra's
    /// review: a partly vanished failure lost its bit-47 evidence).
    #[test]
    fn the_alarm_record_keeps_what_the_rescan_lost() {
        let partly = super::alarm_record(1, 0x0000_8000_0000_0001, 0x1);
        assert!(partly.contains("1 bad words on rescan") && partly.contains("0x0000800000000001")
                && partly.contains("didn't see bits 0x0000800000000000 again"), "{partly}");
        let all = super::alarm_record(2, 0x3, 0x3);
        assert!(!all.contains("didn't see"), "{all}");
        let none = super::alarm_record(0, 0x80, 0);
        assert!(none.contains("not reproduced") && none.contains("0x0000000000000080"), "{none}");
    }

    /// TODO 99: Mem-Stride's write and verify at each of its strides as a constant, and at one
    /// that isn't (the runtime fallback): a clean pass reads clean, and a bad word is seen in each
    /// of the four accumulators' lanes and in the tail.
    #[test]
    fn stride_walks_see_a_bad_word_in_every_lane_at_every_stride() {
        let (subdivisions, elements_per_subdiv) = (4usize, 4 * 4096 + 5);
        let mut mem = vec![0u64; subdivisions * elements_per_subdiv];
        for stride in [1, 16, 64, 256, 1024, 4096, 7] {
            let walk = StrideWalk { base: mem.as_mut_ptr(), chunk_start: 0, subdivisions, elements_per_subdiv, stride, pattern: 0x5A5A_0000_0000_1234 };
            unsafe { walk.write() };
            assert_eq!(unsafe { walk.verify() }, 0, "stride {stride}: clean");
            // The last subdivision's walk: its first four words (one per accumulator) and its last
            let start = 3 * elements_per_subdiv;
            let visits: Vec<usize> = (start..start + elements_per_subdiv).step_by(stride).collect();
            for &word in visits.iter().take(4).chain(visits.last()) {
                mem[word] ^= 1 << 40;
                assert_ne!(unsafe { walk.verify() }, 0, "stride {stride}: missed word {word}");
                mem[word] ^= 1 << 40;
            }
        }
    }

    /// TODO 99 round 3: Mem-Stride's walked pointers write each subdivision's words from its start,
    /// every stride, the pattern plus the index, and nothing else; read clean; and see a bad word in
    /// each lane of each accumulator (the first 16 words a subdivision visits) and in the tails (its
    /// last 20), in every subdivision, with a tail and without, at each stride and the fallback's.
    #[test]
    fn stride_walks_write_their_words_and_see_every_lane_and_tail() {
        const UNTOUCHED: u64 = 0xDEAD_0000_0000_0000;
        let pattern = 0x5A5A_0000_0000_1234u64;
        for (chunk_start, elements_per_subdiv) in [(64usize, 4 * 4096 + 5), (0, 4 * 4096)] {
            let subdivisions = 4;
            let mut mem = vec![UNTOUCHED; chunk_start + subdivisions * elements_per_subdiv + 64];
            for stride in [1usize, 16, 64, 256, 1024, 4096, 7] {
                mem.fill(UNTOUCHED);
                let walk = StrideWalk { base: mem.as_mut_ptr(), chunk_start, subdivisions, elements_per_subdiv, stride, pattern };
                unsafe { walk.write() };
                let visits: Vec<Vec<usize>> = (0..subdivisions).map(|s| {
                    let start = chunk_start + s * elements_per_subdiv;
                    (start..start + elements_per_subdiv).step_by(stride).collect()
                }).collect();
                let mut want = vec![UNTOUCHED; mem.len()];
                for &i in visits.iter().flatten() {
                    want[i] = pattern.wrapping_add(i as u64);
                }
                assert!(mem == want, "stride {stride}, {elements_per_subdiv} a subdivision: not the walk's image");
                assert_eq!(unsafe { walk.verify() }, 0, "stride {stride}: clean");
                // Through the walk's own pointer, which a write through `mem` would invalidate
                for visits in &visits {
                    let ends = visits.iter().take(16).chain(visits.iter().rev().take(20));
                    for &word in ends {
                        unsafe { *walk.base.add(word) ^= 1 << 40 };
                        assert_eq!(unsafe { walk.verify() }, 1 << 40, "stride {stride}: missed word {word}");
                        unsafe { *walk.base.add(word) ^= 1 << 40 };
                    }
                }
            }
        }
    }

    /// TODO 99 round 3: the scalar Mem-StuckBit and Mem-Refresh chunk fills its words with the
    /// pattern and nothing else, from any start, with every length of tail; reads clean; sees a bad
    /// word anywhere in it (each accumulator's lane and the tail); and counts it once.
    #[test]
    fn const_chunks_fill_their_words_and_see_every_bad_word() {
        const UNTOUCHED: u64 = 0xDEAD_0000_0000_0000;
        for (start, len) in [(0usize, 64usize), (3, 16 * 5 + 15), (5, 16 * 2 + 7), (8, 3), (1, 0)] {
            let mut mem = vec![UNTOUCHED; start + len + 32];
            let chunk = super::ConstChunk { base: mem.as_mut_ptr(), start, end: start + len, pattern: super::STUCKBIT_P1 };
            unsafe { chunk.write() };
            for (i, &word) in mem.iter().enumerate() {
                let want = if (start..start + len).contains(&i) { super::STUCKBIT_P1 } else { UNTOUCHED };
                assert_eq!(word, want, "{start}..{}: word {i}", start + len);
            }
            assert_eq!(unsafe { chunk.verify() }, 0, "{start}..{}: clean", start + len);
            assert_eq!(unsafe { chunk.check("test") }, 0, "{start}..{}: clean check", start + len);
            // Through the chunk's own pointer, which a write through `mem` would invalidate
            for word in start..start + len {
                unsafe { *chunk.base.add(word) ^= 1 << 40 };
                assert_eq!(unsafe { chunk.verify() }, 1 << 40, "{start}..{}: missed word {word}", start + len);
                assert_eq!(unsafe { chunk.check("test") }, 1, "{start}..{}: word {word} not counted once", start + len);
                unsafe { *chunk.base.add(word) ^= 1 << 40 };
            }
        }
    }

    /// TODO 99: Mem-CacheBust's chunk walks write each word its variant's pattern plus its index and
    /// nothing outside the chunk, read clean, and see a bad word in each lane of each accumulator
    /// and in the tails, with one pattern and with four.
    #[test]
    fn cache_bust_walks_write_their_pattern_and_see_every_bad_word() {
        // Walks of 22 and 21 words (a vector step and a tail), then of 32 (two steps, no tail)
        for (start, len) in [(300usize, CACHE_BUST_WORDS * 21 + 37), (0, CACHE_BUST_WORDS * 32)] {
            for stride_patterns in [1usize, 4] {
                let mut mem = vec![0u64; start + len + 64];
                let chunk = CacheBustChunk { base: mem.as_mut_ptr(), start, end: start + len, pattern_base: 0x5A5A_0000_0000_1234, stride_patterns };
                unsafe { chunk.write() };
                for (i, &word) in mem.iter().enumerate() {
                    let want = if (start..start + len).contains(&i) { chunk.pattern_at(i).wrapping_add(i as u64) } else { 0 };
                    assert_eq!(word, want, "{stride_patterns} patterns: word {i}");
                }
                assert_eq!(unsafe { chunk.verify() }, 0, "{stride_patterns} patterns: clean");
                // Every word of the first and the last offset's walks, and a spread over the rest
                let last = start + CACHE_BUST_WORDS - 1;
                let words = (start..start + len).step_by(CACHE_BUST_WORDS)
                    .chain((last..start + len).step_by(CACHE_BUST_WORDS))
                    .chain((start..start + len).step_by(97));
                for word in words {
                    mem[word] ^= 1 << 40;
                    assert_ne!(unsafe { chunk.verify() }, 0, "{stride_patterns} patterns: missed word {word}");
                    mem[word] ^= 1 << 40;
                }
            }
        }
    }

    /// TODO 99: the native SimpleTest's strided walk is `for_each_strided_line!`'s: the same lines
    /// in the same order, each with its line id, and the same line prefetched at each step, in both
    /// of a pass's loops (on in the pass, then on into the next).
    #[test]
    fn the_native_strided_walk_is_tm5s() {
        use super::native_opaque_step;
        let mut mem = [0u64; 1];
        let base = mem.as_mut_ptr();
        // A pointer's word offset from `base`; a prefetch's may be off the chunk, either side
        let at = |q: *const u64| ((q as usize).wrapping_sub(base as usize) as isize) / 8;
        let shapes = [(0, 4096, 15 * 8), (512, 4096 + 512, 15 * 8), (0, 4096 + 40, 15 * 8), (0, 4096, 63 * 8),
                      (0, 512, 4095 * 8), (64, 128, 0), (0, 6 * LINE_WORDS, 2 * LINE_WORDS), (0, 344, 15 * 8)];
        for (start, end, stride) in shapes {
            let period = strided_period(stride);
            let mut old = Vec::new();
            for_each_strided_line!(base as *const u64, start, end, period, |offset| { old.push(offset); });
            // The old walk's prefetch targets, by its own loops (`the_strided_walk_is_tm5s`)
            let ahead = strided_lookahead(start, end, period);
            let mut targets = Vec::new();
            let mut first = start + period;
            while first > start {
                first -= LINE_WORDS;
                let wrap = strided_wrap(first, end, period);
                let mut offset = first;
                while offset < end {
                    targets.push(strided_ahead(offset, ahead, end, wrap) as isize);
                    offset += period;
                }
            }
            let lid0 = (7u64 << 37) + (start / LINE_WORDS) as u64;
            let (mut lines, mut prefetched) = (Vec::new(), Vec::new());
            let mut prefetch = |q: *mut u64| prefetched.push(at(q));
            native_strided_walk!(prefetch, base, start, end, period, lid0, |p, lid| {
                lines.push((at(p) as usize, lid));
            });
            let what = format!("{start}..{end} by {stride}");
            assert_eq!(lines.iter().map(|&(offset, _)| offset).collect::<Vec<_>>(), old, "{what}: the lines");
            for &(offset, lid) in &lines {
                assert_eq!(lid, lid0 + ((offset - start) / LINE_WORDS) as u64, "{what}: line {offset}'s id");
            }
            assert_eq!(prefetched, targets, "{what}: the prefetches");
        }
    }

    /// TODO 99: the native SimpleTest's walks at every width this CPU has, for every family and
    /// beat and both polarities, sequential and strided (passes of 2 or 3 lines, and passes of one
    /// line, the period longer than the chunk): the write leaves each word of the chunk its
    /// phase's (`Phase::line`, the spec's) and nothing outside it, a verify reads that clean, and
    /// a flipped bit is seen in each word of the first 4 lines (every lane of every accumulator),
    /// of the last 3 (the lines past the last whole group: 3 at 512 bits, 1 at 256) and in a spread.
    #[test]
    fn native_walks_write_their_phase_and_see_every_bad_word() {
        use crate::pattern::{Family, Phase};
        use crate::pattern_gen::BusBeat;
        type Walks = (unsafe fn(super::NativeChunk), unsafe fn(super::NativeChunk) -> u64);
        let mut widths: Vec<(&str, Walks)> = vec![
            ("scalar", (super::simple_native_scalar_walks::write, super::simple_native_scalar_walks::differs)),
            ("128", (super::simple_native_128_walks::write, super::simple_native_128_walks::differs)),
            ("256", (super::simple_native_256_walks::write, super::simple_native_256_walks::differs)),
        ];
        if is_x86_feature_detected!("avx512f") && is_x86_feature_detected!("avx512bw") && is_x86_feature_detected!("avx512cd")
            && is_x86_feature_detected!("avx512dq") && is_x86_feature_detected!("avx512vl") {
            widths.push(("512", (super::simple_native_512_walks::write, super::simple_native_512_walks::differs)));
        }
        const UNTOUCHED: u64 = 0x0DD0_F00D_0DD0_F00D;
        // 43 lines from line 8 of the buffer (64 B aligned), so whole groups and a tail at 512
        // and 256 bits
        let (start, lines) = (64usize, 43usize);
        let end = start + lines * LINE_WORDS;
        let mut buf = vec![std::simd::u64x8::splat(UNTOUCHED); end / 8 + 8];
        let base = buf.as_mut_ptr() as *mut u64;
        let total = buf.len() * 8;
        let lid0 = (3u64 << 37) + 1000;
        let flips: Vec<usize> = (start..start + 4 * LINE_WORDS).chain(end - 3 * LINE_WORDS..end).chain((start..end).step_by(37)).collect();
        let mut ran = 0;
        for (width, (write, differs)) in &widths {
            for family in Family::ALL {
                let beats = if family.uses_beat() { &[BusBeat::Bits12, BusBeat::Bits16, BusBeat::Bits24, BusBeat::Bits32, BusBeat::Bits64][..] } else { &[BusBeat::Bits64][..] };
                for &beat in beats {
                    for mask in [0, !0] {
                        let phase = Phase { family, key: 0x5A5A_1234_C0FF_EE00, mask, beat };
                        for period in [0, strided_period(15 * 8), strided_period(4095 * 8)] {
                            let what = format!("{width} {} period {period}", phase.label());
                            let c = super::NativeChunk { base, start, end, lid0, period, phase };
                            for i in 0..total {
                                unsafe { *base.add(i) = UNTOUCHED };
                            }
                            unsafe { write(c) };
                            for i in 0..total {
                                let want = if (start..end).contains(&i) {
                                    phase.line(lid0 + ((i - start) / LINE_WORDS) as u64)[(i - start) % LINE_WORDS]
                                } else {
                                    UNTOUCHED
                                };
                                assert_eq!(unsafe { *base.add(i) }, want, "{what}: word {i}");
                            }
                            assert_eq!(unsafe { differs(c) }, 0, "{what}: clean");
                            for &word in &flips {
                                let bit = 1u64 << (word % 64);
                                unsafe { *base.add(word) ^= bit };
                                assert_ne!(unsafe { differs(c) }, 0, "{what}: missed word {word}");
                                unsafe { *base.add(word) ^= bit };
                            }
                            ran += 1;
                        }
                    }
                }
            }
        }
        assert_eq!(ran, widths.len() * 19 * 2 * 3);
    }

    /// TODO 89: the strided accumulate sees a bad word wherever its walk passes, at any stride
    /// and remainder; the rescans count each bad word exactly, the strided one only on its walk.
    #[test]
    fn strided_verify_finds_and_counts_each_bad_word() {
        let len = 1000usize;
        let expected = |i: usize| (i as u64).wrapping_mul(0x9E3779B97F4A7C15);
        let mut mem: Vec<u64> = (0..len).map(expected).collect();
        let p = mem.as_mut_ptr();
        let clean = |stride: usize| unsafe {
            let mut acc = [0u64; 4];
            for offset in 0..stride.min(len) {
                accumulate_stride_4(&mut acc, p, offset, len, stride, expected);
            }
            (acc[0] | acc[1]) | (acc[2] | acc[3]) == 0
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
            assert_eq!(rescan_words(p, 0, len, "test", 1, expected), 3);
            assert_eq!(verify_words(p, 0, len, "test", expected), 3);
            // A stride-3 walk from 1 visits 997 (= 1 + 3 x 332) and 500 is off it (500 % 3 = 2)
            let strided = |from: usize| {
                let mut scan = Rescan::for_test(p, "test");
                scan.stride(from, len, 3, expected);
                scan.found
            };
            assert_eq!([strided(1), strided(2), strided(0)], [1, 1, 1]);
        }
    }

    /// TODO 76: modes 2 and 20 depend only on the address, so writing a range in overlapping
    /// pieces, in any order, gives the image one pass gives. The verify finds nothing on it and
    /// counts each bad word exactly.
    #[test]
    fn line_patterns_are_position_pure_and_count_exactly() {
        let words = 6 * PAGE_WORDS;
        let mut buf = vec![std::simd::u64x8::splat(0); words / 8];
        let p = buf.as_mut_ptr() as *mut u64;
        let mode2 = Mode2 { thread_id: 3, cycle: 1, param0: 0x5DEECE66D, param1: 0xB };
        let mode20 = Mode20::new(3, 1, 0x5DEECE66D, 0xB);
        let image = |p: *mut u64| unsafe { std::slice::from_raw_parts(p, words).to_vec() };
        unsafe {
            mode2.fill(p, 0, words);
            let one_pass = image(p);
            mode20.fill(p, 0, words);
            for (s, e) in [(4, 6), (0, 3), (2, 5)] {
                mode2.fill(p, s * PAGE_WORDS, e * PAGE_WORDS);
            }
            assert_eq!(image(p), one_pass);
            assert_eq!(mode2.verify(p, 0, words, "test"), 0);
            *p.add(700) ^= 1;
            *p.add(2 * PAGE_WORDS + 9) ^= 1 << 63;
            assert_eq!(mode2.verify(p, 0, words, "test"), 2);
            assert_eq!(mode2.verify(p, 3 * PAGE_WORDS, words, "test"), 0);
            assert!(mode20.verify(p, 0, words, "test") > 0);

            mode20.fill(p, 0, words);
            let one_pass = image(p);
            mode2.fill(p, 0, words);
            for (s, e) in [(5 * PAGE_WORDS, words), (8, 3 * PAGE_WORDS + 64), (0, 5 * PAGE_WORDS + 8)] {
                mode20.fill(p, s, e);
            }
            assert_eq!(image(p), one_pass);
            assert_eq!(mode20.verify(p, 0, words, "test"), 0);
            *p.add(5 * PAGE_WORDS + 511) = 0;
            assert_eq!(mode20.verify(p, 0, words, "test"), 1);
        }
    }

    /// A failed check counts the bits its differences had; a bit only the rescan found (the word
    /// changed since) counts in the words alone. Under rescan=off a check counts its bits, with no
    /// words (TODO 97, the second review).
    #[test]
    fn failing_bits_count_what_the_check_saw() {
        let mut mem = [0u64; 8];
        mem[3] = 1 << 7 | 1 << 9;
        crate::tests::reset_rescan_counts();
        let mut scan = Rescan::for_test(mem.as_ptr(), "test").seen(1 << 7);
        unsafe { scan.line(0, &[0; 8]) };
        assert_eq!(scan.finish(format_args!("in idx 0..8")), 1);
        assert_eq!(crate::tests::take_failing_bits().test.detail(), "7: 1 / 1 (1 + 0), 9: 0 / 1 (1 + 0)");
        assert_eq!(super::unlocalised_as(String::from("test"), format_args!("in idx 0..8"), 1 << 9, false), 1);
        let bits = crate::tests::take_failing_bits();
        assert!(bits.test.is_empty() && bits.seal.detail() == "9: 1 / 0", "{bits:?}");
    }

    /// TODO 99 (and TODO 97): Mem-MovingInv's passes over a chunk write `ident`'s word of each
    /// word's id (P, then ~P going up, then P going down) and nothing outside the chunk, read
    /// clean, and see a bad word wherever it is: in each lane of each accumulator and in the words
    /// past the whole steps, going up and going down.
    #[test]
    fn moving_inversions_passes_write_their_images_and_see_every_bad_word() {
        const UNTOUCHED: u64 = 0x0BAD_F00D_DEAD_BEEF;
        let (ids, key) = (crate::pattern::word_base(3), 0x1234_5678_9ABC_DEF1u64);
        let p = |i: usize| crate::pattern::ident_word(ids + i as u64, key);
        // Steps and the rest, steps alone, the rest alone, nothing
        for (start, len) in [(0usize, 16 * 5 + 7), (37, 16 * 3), (3, 5), (8, 0)] {
            let mut mem = vec![UNTOUCHED; start + len + 8];
            let chunk = super::MovingInvChunk { base: mem.as_mut_ptr(), start, end: start + len, ids, key };
            let image = |mem: &[u64], inverse: bool| mem.iter().enumerate().all(|(i, &w)| w == if (start..start + len).contains(&i) {
                if inverse { !p(i) } else { p(i) }
            } else { UNTOUCHED });
            unsafe {
                chunk.fill();
                assert!(image(&mem, false), "{start}+{len}: the fill wrote P");
                assert_eq!(chunk.verify(), 0, "{start}+{len}: P reads clean");
                assert_eq!(chunk.pass::<true>(), 0, "{start}+{len}: up reads P clean");
                assert!(image(&mem, true), "{start}+{len}: up wrote ~P");
                assert_eq!(chunk.pass::<false>(), 0, "{start}+{len}: down reads ~P clean");
                assert!(image(&mem, false), "{start}+{len}: down wrote P");
                for word in start..start + len {
                    let bit = 1u64 << (word % 64);
                    mem[word] ^= bit;
                    assert_eq!(chunk.verify(), bit, "{start}+{len}: the verify missed word {word}");
                    assert_eq!(chunk.pass::<true>(), bit, "{start}+{len}: up missed word {word}");
                    assert!(image(&mem, true), "{start}+{len}: up wrote ~P over word {word} too");
                    mem[word] ^= bit;
                    assert_eq!(chunk.pass::<false>(), bit, "{start}+{len}: down missed word {word}");
                    assert!(image(&mem, false), "{start}+{len}: down wrote P over word {word} too");
                }
            }
        }
    }

    /// Modulo-X's lines: in any range, one victim in every `MODULO_X` at the offset, and the runs
    /// between them the aggressors, each line exactly once (TODO 97).
    #[test]
    fn modulo_lines_split_victims_from_aggressors() {
        for (start, end) in [(0usize, 100usize), (7, 93), (40, 41), (19, 21), (0, 0)] {
            for offset in [0, 5, super::MODULO_X - 1] {
                let seen = std::cell::RefCell::new(vec![0u8; end]);
                let mut victims = Vec::new();
                let mut runs = 0;
                super::modulo_lines(start, end, offset, |v| { victims.push(v); seen.borrow_mut()[v] += 1 }, |lines| {
                    runs += 1;
                    lines.for_each(|l| seen.borrow_mut()[l] += 1);
                });
                let seen = seen.into_inner();
                assert!(seen[start..].iter().all(|&n| n == 1) && seen[..start].iter().all(|&n| n == 0), "{start}..{end} at {offset}");
                assert!(victims.iter().all(|v| v % super::MODULO_X == offset));
                assert_eq!(victims.len(), (start..end).filter(|l| l % super::MODULO_X == offset).count());
                assert!(runs <= 2 * end.div_ceil(super::MODULO_X).max(1), "{runs} runs");
            }
        }
    }

    /// TODO 99: Mem-ModuloX's passes over a chunk write P over its victim lines and ~P over the
    /// rest (`ident`'s word of each word's id), and nothing outside the chunk; the victims' check
    /// reads clean and sees a bad word in every victim line, an even count of them and an odd.
    #[test]
    fn modulo_x_passes_write_their_images_and_see_every_bad_victim_word() {
        use super::{LINE_WORDS, MODULO_X};
        const UNTOUCHED: u64 = 0x0BAD_F00D_DEAD_BEEF;
        let (ids, key) = (crate::pattern::word_base(2), 0x0FED_CBA9_8765_4321u64);
        let p = |i: usize| crate::pattern::ident_word(ids + i as u64, key);
        for (start, end) in [(0usize, 100usize), (7, 93), (7, 113), (40, 41), (19, 21), (5, 5), (0, 3 * MODULO_X)] {
            for offset in [0, 5, MODULO_X - 1] {
                let mut mem = vec![UNTOUCHED; (end + 2) * LINE_WORDS];
                let chunk = super::ModuloChunk { base: mem.as_mut_ptr(), start, end, offset, ids, key };
                let victim = |i: usize| (i / LINE_WORDS) % MODULO_X == offset;
                let inside = |i: usize| (start * LINE_WORDS..end * LINE_WORDS).contains(&i);
                unsafe {
                    chunk.write_victims();
                    for (i, &w) in mem.iter().enumerate() {
                        assert_eq!(w, if inside(i) && victim(i) { p(i) } else { UNTOUCHED }, "{start}..{end} at {offset}: victims, word {i}");
                    }
                    for _ in 0..super::MODULO_PASSES {
                        chunk.aggress();
                    }
                    for (i, &w) in mem.iter().enumerate() {
                        let want = if !inside(i) { UNTOUCHED } else if victim(i) { p(i) } else { !p(i) };
                        assert_eq!(w, want, "{start}..{end} at {offset}: word {i}");
                    }
                    assert_eq!(chunk.verify_victims(), 0, "{start}..{end} at {offset}: clean");
                    for word in (0..mem.len()).filter(|&i| inside(i) && victim(i)) {
                        let bit = 1u64 << (word % 64);
                        mem[word] ^= bit;
                        assert_eq!(chunk.verify_victims(), bit, "{start}..{end} at {offset}: missed word {word}");
                        mem[word] ^= bit;
                    }
                }
            }
        }
    }

    /// Mem-Sequence's passes at 256 bits: a write in any order and a read in any order agree; a
    /// read in each order sees a flipped bit at a block's edges and inside them; a read-then-write
    /// sees it and leaves the inverse in every line, the bad word's too. The walk is shared by every
    /// operation, so the write's and the read-then-write's full coverage is the read's (TODO 97).
    #[test]
    fn sequence_passes_find_a_bad_word_in_every_order() {
        use crate::sequence::Order;
        #[target_feature(enable = "avx2,avx,fma,bmi1,bmi2")]
        unsafe fn pass(op: u8, ptr: *mut u64, words: usize, order: Order, key: u64, phase: crate::pattern::Phase) -> u64 {
            let chunk = super::SequenceChunk { ptr, start: 0, end: words, lid0: 0, period: super::strided_period(15 * 8) };
            match op {
                0 => super::sequence_write_256(chunk, order, key, phase),
                1 => super::sequence_read_256(chunk, order, key, phase),
                _ => super::sequence_rw_256(chunk, order, key, phase),
            }
        }
        let words = 768 * LINE_WORDS;  // 768 lines: blocks of 512 and 256 in random order
        let mut mem = vec![std::simd::u64x8::splat(0); words / 8];
        let p = mem.as_mut_ptr() as *mut u64;
        let program = crate::pattern::Program::single(crate::pattern::Family::Random);
        let phase = program.phase(&crate::pattern::StepEpoch { seed: 4, suite_cycle: 1, step: 0 }, 2, 0, crate::pattern::Schedule::Rounds, None);
        let orders = [Order::Forward, Order::Reverse, Order::Random, Order::Stride];
        for write_order in orders {
            for read_order in orders {
                unsafe {
                    std::slice::from_raw_parts_mut(p, words).fill(0x5A5A);
                    pass(0, p, words, write_order, 11, phase);
                    assert_eq!(pass(1, p, words, read_order, 12, phase), 0, "{write_order:?} then {read_order:?}");
                    for (n, bad) in [0usize, 3001, 512 * LINE_WORDS - 1, 512 * LINE_WORDS, words - 1].into_iter().enumerate() {
                        pass(0, p, words, write_order, 11, phase);
                        *p.add(bad) ^= 1 << (21 + n);
                        assert_eq!(pass(1, p, words, read_order, 13, phase), 1 << (21 + n), "{read_order:?} read, word {bad}");
                        assert_eq!(pass(2, p, words, read_order, 14, phase), 1 << (21 + n), "{read_order:?} read-then-write");
                        // It wrote the inverse over every word, the bad one's too: all ~P now
                        let inverse = crate::pattern::Phase { mask: !phase.mask, ..phase };
                        assert_eq!(pass(1, p, words, Order::Forward, 15, inverse), 0, "{read_order:?}, word {bad}");
                    }
                }
            }
        }
    }

    /// The Mem-Sequence widths this CPU runs: 256 bits, and 512 with AVX-512.
    fn sequence_widths() -> Vec<usize> {
        let avx512 = std::arch::is_x86_feature_detected!("avx512f") && std::arch::is_x86_feature_detected!("avx512bw")
            && std::arch::is_x86_feature_detected!("avx512cd") && std::arch::is_x86_feature_detected!("avx512dq")
            && std::arch::is_x86_feature_detected!("avx512vl");
        if avx512 { vec![256, 512] } else { vec![256] }
    }

    /// One Mem-Sequence pass at `width` bits: `op` 0 a write, 1 a read, 2 a read-then-write.
    fn sequence_walk(width: usize, op: u8, chunk: super::SequenceChunk, order: crate::sequence::Order, key: u64,
                     phase: crate::pattern::Phase) -> u64 {
        // SAFETY: the chunk is in a live, 64-byte aligned buffer, and 512 bits run only on a CPU
        // with AVX-512 (`sequence_widths`)
        unsafe {
            match (width, op) {
                (256, 0) => super::sequence_write_256(chunk, order, key, phase),
                (256, 1) => super::sequence_read_256(chunk, order, key, phase),
                (256, _) => super::sequence_rw_256(chunk, order, key, phase),
                (_, 0) => super::sequence_write_512(chunk, order, key, phase),
                (_, 1) => super::sequence_read_512(chunk, order, key, phase),
                _ => super::sequence_rw_512(chunk, order, key, phase),
            }
        }
    }

    /// A chunk's lines in the order a Mem-Sequence pass visits them, from each order's definition:
    /// up; down; each power-of-two block in `sequence::random_line`'s order, `j` from 0; TM5's
    /// strided passes, the period's last line first, each a period at a time.
    fn sequence_visits(order: crate::sequence::Order, lines: usize, period_lines: usize, key: u64) -> Vec<usize> {
        use crate::sequence::Order;
        match order {
            Order::Forward => (0..lines).collect(),
            Order::Reverse => (0..lines).rev().collect(),
            Order::Random => {
                let (mut visits, mut s) = (Vec::new(), 0);
                while s < lines {
                    let bits = (lines - s).ilog2();
                    visits.extend((0..1u64 << bits).map(|j| s + crate::sequence::random_line(j, bits, key) as usize));
                    s += 1 << bits;
                }
                visits
            }
            Order::Stride => (0..period_lines).rev().flat_map(|first| (first..lines).step_by(period_lines)).collect(),
        }
    }

    /// The orders a Mem-Sequence test runs, each with a stride: the strided walk at a period shorter
    /// than the 192 lines of these tests' chunks (16 lines, passes of 12) and at one longer (4096,
    /// passes of one line or none).
    const SEQUENCE_ORDERS: [(crate::sequence::Order, usize); 5] = [(crate::sequence::Order::Forward, 15 * 8),
        (crate::sequence::Order::Reverse, 15 * 8), (crate::sequence::Order::Random, 15 * 8),
        (crate::sequence::Order::Stride, 15 * 8), (crate::sequence::Order::Stride, 4095 * 8)];

    /// TODO 99: Mem-Sequence's walks, a function each per width, operation, order and family. At
    /// each width, in every order, for every family and beat in both polarities: a write leaves each
    /// line of the chunk its words by the scalar definition (`Phase::line`) and nothing outside it; a
    /// read of that is clean; a read-then-write is clean and leaves the inverse in every word, so it
    /// visited each once.
    #[test]
    fn sequence_walks_write_every_family_in_every_order() {
        use crate::pattern::{Family, Phase};
        use crate::pattern_gen::BusBeat;
        const SENTINEL: u64 = 0x5A5A_0F0F_C3C3_9696;
        // The chunk 64 lines into its buffer, 192 lines: random blocks of 128 and 64
        let (pad, lines) = (64usize, 192usize);
        let lid_base = crate::pattern::line_base(3);
        let beats = [BusBeat::Bits12, BusBeat::Bits16, BusBeat::Bits24, BusBeat::Bits32, BusBeat::Bits64];
        let mut kinds = 0;
        for family in Family::ALL {
            for &beat in if family.uses_beat() { &beats[..] } else { &beats[..1] } {
                kinds += 1;
                for mask in [0, !0] {
                    let phase = Phase { family, key: 0x0123_4567_89AB_CDEF, mask, beat };
                    let inverse = Phase { mask: !mask, ..phase };
                    for width in sequence_widths() {
                        for (order, stride) in SEQUENCE_ORDERS {
                            let mut mem = vec![std::simd::u64x8::splat(SENTINEL); 2 * pad + lines];
                            let ptr = mem.as_mut_ptr() as *mut u64;
                            let chunk = super::SequenceChunk { ptr, start: pad * LINE_WORDS, end: (pad + lines) * LINE_WORDS,
                                                               lid0: lid_base + pad as u64, period: strided_period(stride) };
                            let what = format!("{width} bits, {} {order:?} by {}", phase.label(), chunk.period);
                            // The first word that differs from the image of `p`: its index, what it holds, what it should
                            let differs = |p: Phase| {
                                // SAFETY: the buffer is live, and no pass runs while this reads it
                                let words = unsafe { std::slice::from_raw_parts(ptr, (2 * pad + lines) * LINE_WORDS) };
                                (0..words.len()).find_map(|i| {
                                    let line = i / LINE_WORDS;
                                    let want = if (pad..pad + lines).contains(&line) { p.line(lid_base + line as u64)[i % LINE_WORDS] } else { SENTINEL };
                                    (words[i] != want).then_some((i, words[i], want))
                                })
                            };
                            sequence_walk(width, 0, chunk, order, 77, phase);
                            assert_eq!(differs(phase), None, "{what}: the write");
                            assert_eq!(sequence_walk(width, 1, chunk, order, 78, phase), 0, "{what}: a clean read");
                            assert_eq!(sequence_walk(width, 2, chunk, order, 79, phase), 0, "{what}: a clean read-then-write");
                            assert_eq!(differs(inverse), None, "{what}: the read-then-write's inverse");
                        }
                    }
                }
            }
        }
        assert_eq!(kinds, 19, "every family and beat `sequence_families!` has");
    }

    /// TODO 99: Mem-Sequence's reads and read-then-writes, at each width and in every order, report
    /// exactly a bit flipped in any lane of any accumulator and at each walk's edges: every word of
    /// the first and last four lines visited (a step is two lines at 256 bits, four at 512, each
    /// vector its own accumulator), of each random block's first and last, and of each strided
    /// pass's. Four families: `ident`'s lines are vector arithmetic, `switch`'s scalar, `quiet`'s at
    /// 24 bits and `header`'s at 12 the heaviest.
    #[test]
    fn sequence_walks_see_a_bad_word_in_every_lane() {
        use crate::pattern::{Family, Phase};
        use crate::pattern_gen::BusBeat;
        use crate::sequence::Order;
        let (pad, lines) = (64usize, 192usize);
        let lid_base = crate::pattern::line_base(5);
        for (family, beat, mask) in [(Family::Ident, BusBeat::Bits64, 0), (Family::Switch, BusBeat::Bits32, !0),
                                     (Family::Quiet, BusBeat::Bits24, 0), (Family::Header, BusBeat::Bits12, !0)] {
            let phase = Phase { family, key: 0x7E57_0000_0000_0099, mask, beat };
            for width in sequence_widths() {
                for (order, stride) in SEQUENCE_ORDERS {
                    let mut mem = vec![std::simd::u64x8::splat(0); 2 * pad + lines];
                    let ptr = mem.as_mut_ptr() as *mut u64;
                    let chunk = super::SequenceChunk { ptr, start: pad * LINE_WORDS, end: (pad + lines) * LINE_WORDS,
                                                       lid0: lid_base + pad as u64, period: strided_period(stride) };
                    let period_lines = chunk.period / LINE_WORDS;
                    let visits = sequence_visits(order, lines, period_lines, 77);
                    let mut edges: Vec<usize> = visits[..4].iter().chain(&visits[lines - 4..]).copied().collect();
                    match order {
                        // Each block's first and last: the blocks are 128 and 64 lines
                        Order::Random => edges.extend([visits[127], visits[128]]),
                        // Each pass's first and last line
                        Order::Stride if period_lines < lines => edges.extend((0..period_lines).flat_map(|first| {
                            [first, first + (lines - 1 - first) / period_lines * period_lines]
                        })),
                        _ => {}
                    }
                    let what = format!("{width} bits, {} {order:?} by {}", phase.label(), chunk.period);
                    sequence_walk(width, 0, chunk, order, 77, phase);
                    for (n, &line) in edges.iter().enumerate() {
                        for word in 0..LINE_WORDS {
                            let bit = 1u64 << ((n * LINE_WORDS + word) % 64);
                            // SAFETY: the word is in the chunk, and no pass runs while this writes it
                            let at = unsafe { ptr.add((pad + line) * LINE_WORDS + word) };
                            unsafe { *at ^= bit };
                            assert_eq!(sequence_walk(width, 1, chunk, order, 77, phase), bit, "{what}: a read, line {line} word {word}");
                            if n < 4 {
                                // It writes the inverse over every word, the bad one's too; then the line again
                                assert_eq!(sequence_walk(width, 2, chunk, order, 77, phase), bit, "{what}: a read-then-write, line {line} word {word}");
                                assert_eq!(sequence_walk(width, 1, chunk, Order::Forward, 77, Phase { mask: !mask, ..phase }), 0, "{what}: inverted");
                                sequence_walk(width, 0, chunk, order, 77, phase);
                            } else {
                                unsafe { *at ^= bit };
                            }
                        }
                    }
                    assert_eq!(sequence_walk(width, 1, chunk, order, 77, phase), 0, "{what}: clean again");
                }
            }
        }
    }

    /// A read-then-write's rescan: a word that didn't take the write is found against what was
    /// written; if every word took it, what was read is written back, flushed and rescanned, so a
    /// read that went wrong is not reproduced and the chunk is left holding what was read (TODO 97,
    /// the review: a cell stuck at the written value read back right).
    #[test]
    fn a_read_then_writes_rescan_writes_back_what_it_read() {
        let mut mem = vec![std::simd::u64x8::splat(0); 16];
        let p = mem.as_mut_ptr() as *mut u64;
        let (read, wrote) = (|i: usize| i as u64, |i: usize| !(i as u64));
        crate::tests::reset_rescan_counts();
        unsafe {
            (0..128).for_each(|i| *p.add(i) = wrote(i));
            *p.add(70) ^= 1 << 9;
            assert_eq!(super::rescan_read_write(p, 0..128, "test", 1 << 9, "RW", &read, &wrote), (1, false));
            assert_eq!(*p.add(70), wrote(70) ^ (1 << 9), "found on the first rescan: left as found");
            *p.add(70) = wrote(70);
            assert_eq!(super::rescan_read_write(p, 0..128, "test", 1 << 9, "RW", &read, &wrote), (1, true));
            assert!((0..128).all(|i| *p.add(i) == read(i)), "left holding what was read");
        }
        let counts = crate::tests::rescan_counts();
        assert_eq!((counts.alarms, counts.not_reproduced), (2, 1));
    }

    /// Mem-Random's final sweep reads every word, finds a bad one the random reads would miss,
    /// and stops where it is told (TODO 97).
    #[test]
    fn the_final_sweep_reads_every_word() {
        let ids = crate::pattern::word_base(2);
        let mut mem: Vec<u64> = (0..1000u64).map(|i| ids | i).collect();
        let extent = super::RandomExtent { base: mem.as_ptr(), len: mem.len(), ids };
        assert_eq!(unsafe { super::random_sweep(extent, 64, "test", |_| false) }, (0, 1000));
        mem[999] ^= 1 << 63;
        let extent = super::RandomExtent { base: mem.as_ptr(), ..extent };
        assert_eq!(unsafe { super::random_sweep(extent, 64, "test", |_| false) }, (1, 1000));
        // Under halt it stops after the piece that failed
        assert_eq!(unsafe { super::random_sweep(extent, 64, "test", |errors| errors > 0) }, (1, 1000));
        mem[100] ^= 1;
        let extent = super::RandomExtent { base: mem.as_ptr(), ..extent };
        assert_eq!(unsafe { super::random_sweep(extent, 64, "test", |errors| errors > 0) }, (1, 128));
    }

    /// TODO 99: Mem-Random's reads take xorshift's steps and `rng * len >> 64`'s indices, as a
    /// plain loop does, and leave the RNG where it does; they read clean, and see a bad word at
    /// the read of each accumulator and of the reads past the last step of four.
    #[test]
    fn random_reads_take_the_rngs_steps_and_see_every_bad_word() {
        let (ids, len) = (crate::pattern::word_base(5), 1000usize);
        let mut mem: Vec<u64> = (0..len as u64).map(|i| ids | i).collect();
        let start = 0x123456789ABCDEFu64;
        // The plain loop's indices and state after each read
        let plain = |n: usize| {
            let (mut rng, mut picks) = (start, Vec::new());
            for _ in 0..n {
                rng ^= rng << 13;
                rng ^= rng >> 17;
                rng ^= rng << 5;
                picks.push(((rng as u128 * len as u128) >> 64) as usize);
            }
            (picks, rng)
        };
        for n in [0usize, 1, 3, 4, 5, 8, 1003] {
            let extent = super::RandomExtent { base: mem.as_ptr(), len, ids };
            let (picks, rng) = plain(n);
            assert_eq!(unsafe { extent.reads(start, n) }, (0, rng), "{n} reads: clean, and the RNG after");
            // The reads of each accumulator in the first step, the second step's first, and the last
            for k in [0, 1, 2, 3, 4, n.saturating_sub(1)].into_iter().filter(|&k| k < n) {
                let (word, bit) = (picks[k], 1u64 << (k % 64));
                mem[word] ^= bit;
                let extent = super::RandomExtent { base: mem.as_ptr(), len, ids };
                assert_eq!(unsafe { extent.reads(start, n) }, (bit, rng), "{n} reads: missed read {k}, word {word}");
                mem[word] ^= bit;
            }
        }
    }

    /// TODO 99: Mem-Random's sweep check reads each word of its range against its id and nothing
    /// outside it: clean, it reads clean, and it sees a bad word in each lane of each accumulator
    /// and in the words past the whole steps.
    #[test]
    fn the_sweep_check_sees_every_bad_word_of_its_range() {
        let ids = crate::pattern::word_base(7);
        let mut mem: Vec<u64> = (0..300u64).map(|i| ids | i).collect();
        // Steps and the rest, steps alone, the rest alone, nothing
        for (from, to) in [(0usize, 16 * 5 + 7), (37, 37 + 16 * 3), (3, 10), (9, 9)] {
            let extent = super::RandomExtent { base: mem.as_ptr(), len: mem.len(), ids };
            assert_eq!(unsafe { extent.verify(from, to) }, 0, "{from}..{to}: clean");
            for word in from.saturating_sub(2)..to + 2 {
                let bit = 1u64 << (word % 64);
                mem[word] ^= bit;
                let extent = super::RandomExtent { base: mem.as_ptr(), len: mem.len(), ids };
                let want = if (from..to).contains(&word) { bit } else { 0 };
                assert_eq!(unsafe { extent.verify(from, to) }, want, "{from}..{to}: word {word}");
                mem[word] ^= bit;
            }
        }
    }

    /// TODO 76: Mem-Random's replay counts the bad words the hot loop's reads saw, each once however
    /// often they were read (TODO 97), on a length that isn't a power of two, and every index stays
    /// below it. Their bits are counted by the way each went.
    #[test]
    fn replay_counts_each_bad_word_a_plain_loop_reads_once() {
        use crate::failing_bits::BitFailures;
        let len = 1000usize;
        let mut mem: Vec<u64> = (0..len as u64).collect();
        mem[123] ^= 1;
        mem[999] ^= 1 << 40;
        let start = 0x123456789ABCDEFu64;

        let (mut rng, mut bad, mut reads, mut seen) = (start, std::collections::BTreeSet::new(), 0, 0);
        for _ in 0..20_000 {
            rng ^= rng << 13;
            rng ^= rng >> 17;
            rng ^= rng << 5;
            let idx = ((rng as u128 * len as u128) >> 64) as usize;
            assert!(idx < len);
            if mem[idx] != idx as u64 {
                bad.insert(idx);
                reads += 1;
            }
            seen |= mem[idx] ^ idx as u64;
        }
        assert!(bad.len() == 2 && reads > 2, "each bad word read, and read again");
        crate::tests::reset_rescan_counts();
        let extent = super::RandomExtent { base: mem.as_ptr(), len, ids: 0 };
        let counted = unsafe { random_replay(extent, start, 0..20_000, 0, "test", seen) };
        assert_eq!(counted, 2);
        // 123 is odd, so its bit 0 went 1 to 0; 999's bit 40 went 0 to 1
        let bits = crate::tests::take_failing_bits();
        assert!(bits.seal.is_empty());
        assert_eq!(serde_json::to_value(&bits.test).unwrap(), serde_json::to_value([
            BitFailures { bit: 0, checks: 1, words_0to1: 0, words_1to0: 1 },
            BitFailures { bit: 40, checks: 1, words_0to1: 1, words_1to0: 0 },
        ]).unwrap());

        // A fault gone by the reread still counts once
        mem[123] ^= 1;
        mem[999] ^= 1 << 40;
        assert_eq!(unsafe { random_replay(extent, start, 0..20_000, 0, "test", 1) }, 1);
    }
}

#[cfg(test)]
mod mirror_tests {
    use super::{MirrorChunk, MirrorMode};
    use std::simd::{u64x2, u64x4, u64x8};

    /// The swaps of one round trip as the index loops before TODO 99 made them, in their order:
    /// element pairs from the chunk's start, from each mode's definition.
    fn index_swaps(mode: MirrorMode, w: usize, start: usize, end: usize) -> Vec<(usize, usize)> {
        let (len, mut swaps) = (end - start, Vec::new());
        match mode {
            MirrorMode::Whole => swaps.extend((0..len / w).map(|i| (start + i * w, end - w - i * w))),
            MirrorMode::Subblocks(n) => {
                let sub = len / n as usize;
                for i in 0..sub / w {
                    swaps.extend((0..n as usize).map(|s| (start + s * sub + i * w, start + s * sub + sub - w - i * w)));
                }
            }
            MirrorMode::Jump(j) => {
                let step = (j as usize * 16).min(len / 4) + 16;
                for pass in (0..step / 16).rev() {
                    let (mut lo, mut hi) = (start + pass * 16, end - pass * 16 - 16);
                    while lo < end {
                        swaps.extend((0..16).step_by(w).map(|v| (lo + v, hi + v)));
                        lo += step;
                        hi = hi.wrapping_sub(step);
                    }
                }
            }
        }
        swaps
    }

    /// TODO 99: a swap exchanges two vectors, at every width (a round trip would hide one that
    /// didn't), and touches nothing else.
    #[test]
    fn a_mirror_swap_exchanges_its_two_vectors() {
        let mut buf = vec![u64x8::splat(0); 6];
        let base = buf.as_mut_ptr() as *mut u64;
        for w in [1usize, 2, 4, 8] {
            for i in 0..48 {
                unsafe { *base.add(i) = i as u64 };
            }
            let (a, b) = unsafe { (base.add(8), base.add(32)) };
            unsafe {
                match w {
                    1 => mirror_swap_pair!(u64, a, b),
                    2 => mirror_swap_pair!(u64x2, a, b),
                    4 => mirror_swap_pair!(u64x4, a, b),
                    _ => mirror_swap_pair!(u64x8, a, b),
                }
            }
            for i in 0..48 {
                let want = match i {
                    8..=15 if i - 8 < w => i + 24,
                    32..=39 if i - 32 < w => i - 24,
                    _ => i,
                };
                assert_eq!(unsafe { *base.add(i) }, want as u64, "{w} words: word {i}");
            }
        }
    }

    /// TODO 85: one test op of every mirror mode, at every width, visits each vector of the
    /// chunk in exactly two swaps, swaps it only with its mirror image (in its own subblock;
    /// 128 B units for a jump), and leaves the chunk as it began. TODO 99: the walked pointers swap
    /// the pairs the index loops did, in their order.
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
                    // Each swap the walks make, by element, then made
                    let seen = std::cell::RefCell::new(Vec::new());
                    macro_rules! record_swap {
                        ($t:ty, $a:expr, $b:expr) => {{
                            let (a, b): (*mut u64, *mut u64) = ($a, $b);
                            seen.borrow_mut().push((a.offset_from(base) as usize, b.offset_from(base) as usize));
                            mirror_swap_pair!($t, a, b);
                        }}
                    }
                    let chunk = MirrorChunk { ptr: base, start, end: start + len };
                    unsafe {
                        match w {
                            1 => mirror_swap!(u64, 1, chunk, mode, record_swap),
                            2 => mirror_swap!(u64x2, 2, chunk, mode, record_swap),
                            4 => mirror_swap!(u64x4, 4, chunk, mode, record_swap),
                            _ => mirror_swap!(u64x8, 8, chunk, mode, record_swap),
                        }
                    }
                    let what = format!("{mode} at {w} words, {pages} pages");
                    for i in 0..start + len + 512 {
                        assert_eq!(unsafe { *base.add(i) }, i as u64, "{what}: word {i} moved");
                    }
                    let seen = seen.into_inner();
                    assert_eq!(seen, index_swaps(mode, w, start, start + len), "{what}: not the index loops' swaps");
                    assert_eq!(seen.len(), len / w, "{what}: not two swaps per vector");
                    let mut count = vec![0u8; len / w];
                    for &(a, b) in &seen {
                        let (a, b) = (a - start, b - start);
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
