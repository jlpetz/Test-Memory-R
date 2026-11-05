//! Adaptive Cache Calibration Module
//!
//! Provides automatic discovery of optimal workload sizes for each memory tier (L1, L2, L3, DRAM, FullDRAM) using statistical convergence
//! detection.
//!
//! The calibration uses a "start high, search down" strategy: begin probing just above each tier's detected size to trigger spills, then
//! reduce workload size until measurements converge to stable values.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::cache::CacheInfo;
use crate::formatting::{serialize_round_2dp, serialize_round_3dp};
use crate::logfile::SHOWN_TARGET;

/// Accept any f64 on deserialize (don't enforce rounding on load)
fn deserialize_f64_lenient<'de, D: serde::Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
    f64::deserialize(d)
}

// ============================================================================
// Helper Functions
// ============================================================================

/// Format size in human-readable form (KB, MB, GB)
fn format_size(bytes: usize) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{} B", bytes)
    }
}

// ============================================================================
// Cache Tier Enum
// ============================================================================

/// Memory hierarchy tier being calibrated
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CacheTier {
    /// L1 Data Cache
    L1,
    /// L2 Cache
    L2,
    /// L3 Cache (Last Level Cache)
    L3,
    /// DRAM (fits in memory, exceeds L3)
    Dram,
    /// Full DRAM (larger working set for TLB pressure)
    FullDram,
}

impl CacheTier {
    /// Returns all tiers in calibration order
    pub fn all_tiers() -> &'static [CacheTier] {
        &[CacheTier::L1, CacheTier::L2, CacheTier::L3, CacheTier::Dram, CacheTier::FullDram]
    }

    /// Display name for the tier
    pub fn name(&self) -> &'static str {
        match self {
            CacheTier::L1 => "L1",
            CacheTier::L2 => "L2",
            CacheTier::L3 => "L3",
            CacheTier::Dram => "DRAM",
            CacheTier::FullDram => "FullDRAM",
        }
    }
}

impl std::fmt::Display for CacheTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name())
    }
}

// ============================================================================
// Configuration
// ============================================================================

/// Configuration parameters for the calibration process
#[derive(Debug, Clone)]
pub struct CalibrationConfig {
    /// Relative error thresholdfor convergence (default: 0.001 = 0.1%)
    pub convergence_threshold: f64,
    /// Maximum time per probe in milliseconds (default: 2000)
    pub max_probe_time_ms: u64,
    /// Minimum samplesbefore checking convergence (default: 50)
    pub min_samples: u32,
    /// Page size for calibration allocation: "regular"/"4kb", "large"/"2mb", "huge"/"1gb"
    pub page_size: String,
}

impl Default for CalibrationConfig {
    fn default() -> Self {
        Self {
            convergence_threshold: 0.001,   // 0.1% relative error for convergence
            max_probe_time_ms: 2000,        // 2 seconds per probe max
            min_samples: 50,                // Minimum samples before convergence check
            page_size: "large".to_string(), // Default: 2MB large pages (matches main tests)
        }
    }
}

// ============================================================================
// CPU Signature (for validation)
// ============================================================================

/// CPU signature for validating calibration results match current system
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct CpuSignature {
    /// CPU vendor string (e.g., "AuthenticAMD", "GenuineIntel")
    pub vendor: String,
    /// CPU brand string (e.g., "AMD Ryzen 9 7950X")
    pub brand: String,
    /// L1 Data cache size per core in bytes
    pub l1d_size: usize,
    /// Human-readable L1D size (e.g. "48.0 KB")
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub l1d_human: String,
    /// L2 cache size per core in bytes
    pub l2_size: usize,
    /// Human-readable L2 size (e.g. "2.0 MB")
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub l2_human: String,
    /// L3 cache size (shared) in bytes
    pub l3_size: usize,
    /// Human-readable L3 size (e.g. "480.0 MB")
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub l3_human: String,
}

impl CpuSignature {
    /// Create a CPU signature from current CacheInfo
    pub fn from_cache_info(cache_info: &CacheInfo) -> Self {
        // Get CPU vendor and brand from raw_cpuid
        let cpuid = raw_cpuid::CpuId::new();
        let vendor = cpuid.get_vendor_info().map(|v| v.as_str().to_string()).unwrap_or_else(|| "Unknown".to_string());
        let brand = cpuid.get_processor_brand_string().map(|b| b.as_str().trim().to_string()).unwrap_or_else(|| "Unknown".to_string());

        Self {
            vendor,
            brand,
            l1d_size: cache_info.per_core_l1d,
            l1d_human: format_size(cache_info.per_core_l1d),
            l2_size: cache_info.per_core_l2,
            l2_human: format_size(cache_info.per_core_l2),
            l3_size: cache_info.l3_cache,
            l3_human: format_size(cache_info.l3_cache),
        }
    }
}

// ============================================================================
// Calibration Results
// ============================================================================

/// Result of calibrating a single tier
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TierCalibrationResult {
    /// The tier that was calibrated
    pub tier: CacheTier,
    /// Optimal workload size in bytes
    pub optimal_size: usize,
    /// Median latency at optimal size in nanoseconds (rounded to 2dp for config readability)
    #[serde(serialize_with = "serialize_round_2dp", deserialize_with = "deserialize_f64_lenient")]
    pub median_latency_ns: f64,
    /// Spread ratio (P95/P5) at optimal size (rounded to 3dp for config readability)
    #[serde(serialize_with = "serialize_round_3dp", deserialize_with = "deserialize_f64_lenient")]
    pub spread_ratio: f64,
    /// Number of probes attempted
    pub probes_attempted: u32,
    /// Whether the probe converged (low relative error)
    pub converged: bool,
    /// Whether fallback selection was used (no stable size found)
    pub fallback_used: bool,
}

/// What a thread reaches with others running too (TODO 22), measured after the single thread's (anchor A): anchor B, L1 and L2 with its
/// SMT sibling chasing on the same core; anchor C, the L3 with every logical CPU chasing, each group's L3 split among its CPUs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SharedReach {
    /// L1 a thread with its sibling running (B); none without SMT
    pub l1_smt: Option<usize>,
    /// L2 a thread with its sibling running (B)
    pub l2_smt: Option<usize>,
    /// The L3 a thread reaches with every logical CPU running (C); none when its sweep found no edge
    pub l3_loaded: Option<usize>,
    /// Its latency there, ns
    #[serde(serialize_with = "serialize_round_2dp", deserialize_with = "deserialize_f64_lenient")]
    pub l3_loaded_ns: f64,
    /// The CPUs sharing an L3 group then: the most one group has
    pub l3_sharers: usize,
}

/// Complete calibration results for all tiers
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalibrationResults {
    /// Timestamp when calibration was performed
    pub timestamp: DateTime<Utc>,
    /// CPU signature for validation
    pub cpu_signature: CpuSignature,
    /// Results for each tier
    pub tiers: HashMap<CacheTier, TierCalibrationResult>,
    /// Total calibration time in milliseconds (runtime only, not persisted to config)
    #[serde(skip)]
    pub total_time_ms: u64,
    /// Page size used for calibration allocation
    #[serde(default = "default_page_size")]
    pub page_size: String,
    /// The tiers with other threads running (TODO 22, anchors B and C); none in a calibration measured before them
    #[serde(default)]
    pub shared: Option<SharedReach>,
}

fn default_page_size() -> String {
    "large".to_string()
}

impl CalibrationResults {
    /// Create new empty results
    pub fn new(cpu_signature: CpuSignature) -> Self {
        Self { timestamp: Utc::now(), cpu_signature, tiers: HashMap::new(), total_time_ms: 0, page_size: "large".to_string(), shared: None }
    }

    /// Add a tier result
    pub fn add_tier_result(&mut self, result: TierCalibrationResult) {
        self.tiers.insert(result.tier, result);
    }

    /// What a thread reaches of its L3 with `k` threads in its group running (TODO 22): the curve through the single thread's reach R1
    /// (anchor A) and the loaded one RN with N sharing (anchor C), reach(k) = own + split / k, so split = (R1 - RN) x N / (N - 1), at most
    /// R1, and own = R1 - split. Without C, R1 / k: the threads split it all. None without an L3. On the dev box (a VM) 8 threads held 19
    /// MB each, 154 MB in all, where one held 67 MB: the L3 doesn't split evenly, which the own part takes up (12 MB there).
    pub fn l3_reach(&self, k: usize) -> Option<f64> {
        let r1 = self.tiers.get(&CacheTier::L3)?.optimal_size as f64;
        let k = k.max(1) as f64;
        Some(match &self.shared {
            Some(SharedReach { l3_loaded: Some(rn), l3_sharers: n, .. }) if *n > 1 => {
                let (rn, n) = (*rn as f64, *n as f64);
                let split = ((r1 - rn) * n / (n - 1.0)).clamp(0.0, r1);
                (r1 - split) + split / k
            }
            _ => r1 / k,
        })
    }

    /// When it was measured and on which pages, as its reports give it: "2026-11-03 14:22 UTC on 2MB large pages", its timestamp's zone
    /// named, where the window's info line gives the same calibration in local time
    pub fn when_and_pages(&self) -> String {
        format!("{} on {} pages", self.timestamp.format("%Y-%m-%d %H:%M UTC"), page_size_name(&self.page_size))
    }

    /// Anchor B's line (TODO 22), L1 and L2 a thread with its SMT sibling chasing too, each size as `size` gives it; none without B
    pub fn two_threads_a_core(&self, size: &dyn Fn(usize) -> String) -> Option<String> {
        let shared = self.shared.as_ref().filter(|shared| shared.l1_smt.is_some() || shared.l2_smt.is_some())?;
        let named = |bytes: Option<usize>| bytes.map_or("none steady".to_string(), size);
        Some(format!("Two threads on one core: L1 {}, L2 {} a thread", named(shared.l1_smt), named(shared.l2_smt)))
    }

    /// The L3 a thread reaches alone and with its group's CPUs sharing it (`l3_reach`), which a run's L3 tests size from, each size as
    /// `size` gives it; none without an L3, or with no CPUs to share one (anchor C's `l3_sharers`)
    pub fn l3_a_thread(&self, size: &dyn Fn(usize) -> String) -> Option<String> {
        let (one, shared) = (self.l3_reach(1)?, self.shared.as_ref().filter(|shared| shared.l3_sharers > 1)?);
        let all = self.l3_reach(shared.l3_sharers).unwrap_or(one);
        Some(format!(
            "L3 a thread: {} alone, {} with {} sharing; a run's L3 tests size from the curve through them",
            size(one as usize),
            size(all as usize),
            shared.l3_sharers
        ))
    }
}

// ============================================================================
// Online Statistics (Welford's Algorithm)
// ============================================================================

/// Online statistics accumulator using Welford's algorithm
/// Computes running mean and variance in a single pass with numerical stability
#[derive(Debug, Clone)]
pub struct OnlineStats {
    /// Number of samples collected
    count: u64,
    /// Running mean
    mean: f64,
    /// Running M2 for variance calculation (Welford's algorithm)
    m2: f64,
    /// All samples(kept for percentile calculation)
    samples: Vec<f64>,
}

impl Default for OnlineStats {
    fn default() -> Self {
        Self::new()
    }
}

impl OnlineStats {
    /// Create a new empty statistics accumulator
    pub fn new() -> Self {
        Self { count: 0, mean: 0.0, m2: 0.0, samples: Vec::new() }
    }

    /// Create with pre-allocated capacity for samples
    pub fn with_capacity(capacity: usize) -> Self {
        Self { count: 0, mean: 0.0, m2: 0.0, samples: Vec::with_capacity(capacity) }
    }

    /// Add a new sample using Welford's online algorithm
    pub fn add_sample(&mut self, value: f64) {
        self.count += 1;
        self.samples.push(value);

        // Welford's algorithm for numerically stable mean/variance
        let delta = value - self.mean;
        self.mean += delta / self.count as f64;
        let delta2 = value - self.mean;
        self.m2 += delta * delta2;
    }

    /// Get the number of samples
    pub fn count(&self) -> u64 {
        self.count
    }

    /// Get the sample variance (using n-1 denominator)
    pub fn variance(&self) -> f64 {
        if self.count < 2 {
            return 0.0;
        }
        self.m2 / (self.count - 1) as f64
    }

    /// Get the standard deviation
    pub fn std_dev(&self) -> f64 {
        self.variance().sqrt()
    }

    /// Get the standard error of the mean
    pub fn std_error(&self) -> f64 {
        if self.count < 2 {
            return f64::MAX;
        }
        self.std_dev() / (self.count as f64).sqrt()
    }

    /// Get the relative error (standard error / mean)
    pub fn relative_error(&self) -> f64 {
        if self.mean == 0.0 || self.count < 2 {
            return f64::MAX;
        }
        self.std_error() / self.mean
    }

    /// Get a reference to all samples
    pub fn samples(&self) -> &[f64] {
        &self.samples
    }
}

// ============================================================================
// Probe Statistics
// ============================================================================

/// Statistical summary of probe measurements
#[derive(Debug, Clone)]
pub struct ProbeStatistics {
    /// Median (P50) latency in nanoseconds
    pub median_ns: f64,
    /// Spread ratio (P95/P5) - high values indicate bimodal/unstable measurements
    pub spread_ratio: f64,
}

// ============================================================================
// Statistics Analyzer
// ============================================================================

/// Analyzer for computing statistics from probe samples
pub struct StatisticsAnalyzer;

impl StatisticsAnalyzer {
    /// Compute statistics from a slice of samples
    pub fn analyze(samples: &[f64]) -> ProbeStatistics {
        if samples.is_empty() {
            return ProbeStatistics { median_ns: 0.0, spread_ratio: 0.0 };
        }

        // Sort samples for percentile calculation
        let mut sorted = samples.to_vec();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

        // Calculate percentiles
        let p5_ns = Self::percentile(&sorted, 5.0);
        let median_ns = Self::percentile(&sorted, 50.0);
        let p95_ns = Self::percentile(&sorted, 95.0);

        // Calculate spread ratio (P95/P5)
        let spread_ratio = if p5_ns > 0.0 { p95_ns / p5_ns } else { 0.0 };

        ProbeStatistics { median_ns, spread_ratio }
    }

    /// Calculate percentile using linear interpolation
    fn percentile(sorted: &[f64], p: f64) -> f64 {
        if sorted.is_empty() {
            return 0.0;
        }
        if sorted.len() == 1 {
            return sorted[0];
        }

        let n = sorted.len();
        let rank = (p / 100.0) * (n - 1) as f64;
        let lower = rank.floor() as usize;
        let upper = rank.ceil() as usize;
        let frac = rank - lower as f64;

        if upper >= n {
            sorted[n - 1]
        } else if lower == upper {
            sorted[lower]
        } else {
            sorted[lower] * (1.0 - frac) + sorted[upper] * frac
        }
    }
}

// ============================================================================
// Probe Engine
// ============================================================================

#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::__rdtscp;
use std::time::Instant;

use crate::memory::allocator::{AllocationConfig, PageSizeLevel, PageSizePreference};
use crate::memory::backend::BackendType;
use crate::memory::buffer::MemoryType;
use crate::memory::{MemoryAllocator, MemoryBuffer};

/// The page size a calibration asks for, from its `page_size` (`maxpage=`): "regular"/"4kb", "huge"/"1gb", else 2 MiB ("large"/"2mb", the
/// default).
fn calibration_page_size(page_size_str: &str) -> PageSizeLevel {
    match page_size_str.to_lowercase().as_str() {
        "regular" | "4kb" => PageSizeLevel::Regular,
        "huge" | "1gb" => PageSizeLevel::Huge,
        _ => PageSizeLevel::Large,
    }
}

/// A page size as a calibration's `page_size` names it (`page_size_name` reads it back).
fn page_size_str(page: PageSizeLevel) -> &'static str {
    match page {
        PageSizeLevel::Regular => "regular",
        PageSizeLevel::Large => "large",
        PageSizeLevel::Huge => "huge",
    }
}

/// A calibration's own memory: `size` bytes at `page`, else at the next smaller page size where it can't have them, down to 4 KiB; with the
/// page size it got; on NUMA node `node` where given (strictly for large pages, preferred for 4 KiB), and from any node before 4 KiB only
/// where its 4 KiB pages would be another node's too: of more than one node, a node with less than `size` free (none of its own on a
/// Threadripper 2990WX in NUMA mode or a vNUMA VM). The cache calibration's probes and the seal's calibration both take theirs from here.
fn allocate_calibration_buffer(size: usize, page: PageSizeLevel, node: Option<u32>) -> Result<(MemoryBuffer, PageSizeLevel), String> {
    let allocator = MemoryAllocator::new(BackendType::Auto)?;
    let elsewhere =
        node.is_some_and(|node| crate::memory::fill::numa_node_count() > 1 && free_on_node(node).is_some_and(|free| free < size));
    allocate_stepping_down(size, page, node, elsewhere, |config| allocator.allocate(config))
}

/// Bytes free on NUMA node `node` (`GetNumaAvailableMemoryNodeEx`), none where it can't say
fn free_on_node(node: u32) -> Option<usize> {
    let mut free = 0u64;
    // SAFETY: writes one u64, `free`, which outlives the call
    unsafe { windows::Win32::System::Threading::GetNumaAvailableMemoryNodeEx(u16::try_from(node).ok()?, &mut free) }.ok()?;
    usize::try_from(free).ok()
}

/// `allocate_calibration_buffer`'s requests, each made by `allocate`. Each is strict (`Require`), so the one granted proves its page size:
/// a `Prefer` request falls back inside the backend, which doesn't say to what, and a page size can't be read back after the fact. Each
/// large-page request carries its page's alignment, which is how it gets that page size (without it the kernel silently gives smaller
/// pages, TODO 87); 4 KiB needs none. A `node` is named strictly (the backend leaves 4 KiB preferred): a smaller page on the node is
/// worth more to a calibration than a larger one on another, whose latencies are a remote node's. Only `elsewhere`, where the node's 4 KiB
/// pages would be another node's too (`allocate_calibration_buffer`), do the same sizes from any node come before 4 KiB, a warning saying
/// whose latencies the calibration then reads.
fn allocate_stepping_down<T>(
    size: usize, page: PageSizeLevel, node: Option<u32>, elsewhere: bool,
    mut allocate: impl FnMut(&AllocationConfig) -> Result<T, crate::memory::backend::AllocError>,
) -> Result<(T, PageSizeLevel), String> {
    let large: Vec<PageSizeLevel> = [PageSizeLevel::Huge, PageSizeLevel::Large].into_iter().filter(|&l| l <= page).collect();
    let mut requests: Vec<(PageSizeLevel, Option<u32>)> = large.iter().map(|&level| (level, node)).collect();
    if node.is_some() && elsewhere {
        requests.extend(large.iter().map(|&level| (level, None)));
    }
    requests.push((PageSizeLevel::Regular, node));
    // Where a request's pages come from, where it says: its node, when strict, or any node, when it dropped the one given
    let place = |level: PageSizeLevel, at: Option<u32>| match (node, at) {
        (Some(_), None) => " from any NUMA node".to_string(),
        (_, Some(n)) if level != PageSizeLevel::Regular => format!(" on NUMA node {n}"),
        _ => String::new(),
    };
    let mut refused = Vec::new();
    for (level, at) in requests {
        let config = AllocationConfig {
            size,
            numa_node: at,
            page_size: PageSizePreference::Require(level),
            memory_type: MemoryType::WriteBack,
            zero_memory: false,
            alignment: (level != PageSizeLevel::Regular).then(|| level.bytes()),
            numa_strict: at.is_some(),
        };
        let name = page_size_name(page_size_str(level));
        let pages = format!("{name} pages{}", place(level, at));
        match allocate(&config) {
            Ok(buffer) => {
                // Another node's: the node hasn't the free memory, so its own 4 KiB pages would be another's too
                let remote = match (node, at) {
                    (Some(node), None) => format!(", whose latencies are another node's: NUMA node {node} hasn't that much free"),
                    _ => String::new(),
                };
                if refused.is_empty() {
                    log::info!(target: SHOWN_TARGET, "Calibration buffer: {} bytes on {name} pages", size);
                } else {
                    log::warn!("Calibration buffer: {} bytes on {pages}{remote}, after {} refused", size, refused.join("; "));
                }
                return Ok((buffer, level));
            }
            Err(e) => refused.push(format!("{pages} ({e})")),
        }
    }
    Err(format!("Failed to allocate calibration buffer: {}", refused.join("; ")))
}

/// Engine for executing latency probes at specific workload sizes
pub struct ProbeEngine {
    /// TSC frequency in GHz (cycles per nanosecond)
    tsc_frequency_ghz: f64,
    /// Memory buffer for pointer-chase pattern (owned — used for standalone calibration)
    memory_buffer: Option<MemoryBuffer>,
    /// External buffer pointer (borrowed — used when calibrating with test plan memory)
    /// When set, probe() uses this instead of memory_buffer. Caller must ensure the buffer outlives the ProbeEngine and is not concurrently
    /// modified.
    external_ptr: Option<*mut u64>,
    /// Maximum working set size (buffer size)
    max_working_set: usize,
    /// Whether the engine has been initialized
    initialized: bool,
    /// The page size its own buffer got (`None` with a borrowed buffer, or before it has one)
    page_obtained: Option<PageSizeLevel>,
}

// SAFETY: the engine's memory is its own buffer or one its caller keeps alive (`initialize_from_region`), and one thread at a time
// uses it: `CalibrationTest::run` moves the engine to anchor A's thread and back
unsafe impl Send for ProbeEngine {}

impl ProbeEngine {
    /// Create a new probe engine
    pub fn new(tsc_frequency_ghz: f64) -> Self {
        Self { tsc_frequency_ghz, memory_buffer: None, external_ptr: None, max_working_set: 0, initialized: false, page_obtained: None }
    }

    /// The page size its own buffer got, where it allocated one.
    pub fn page_obtained(&self) -> Option<PageSizeLevel> {
        self.page_obtained
    }

    /// Initialize with a specific page size preference, on NUMA node `node` (`allocate_calibration_buffer`)
    /// page_size_str: "regular"/"4kb", "large"/"2mb", "huge"/"1gb"
    pub fn initialize_with_page_size(&mut self, max_size: usize, page_size_str: &str, node: u32) -> Result<(), String> {
        if self.initialized {
            return Ok(());
        }

        match allocate_calibration_buffer(max_size, calibration_page_size(page_size_str), Some(node)) {
            Ok((buffer, obtained)) => {
                self.page_obtained = Some(obtained);
                // No chain over the whole buffer: each probe lays its own, its size's (`probe`). One here cost about 6 s on the dev box's
                // 1.9 GB buffer, and the first probe replaced it.
                self.memory_buffer = Some(buffer);
                self.max_working_set = max_size;
                self.initialized = true;

                log::info!("ProbeEngine: Initialized with {} byte buffer", max_size);
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// Initialize on memory the caller owns (no allocation): a run's own span (TODO 22).
    ///
    /// # Safety
    /// `bytes` at `base` are 64-byte aligned, writable, the caller's alone, and outlive the engine.
    pub unsafe fn initialize_from_region(&mut self, base: *mut u64, bytes: usize) -> Result<(), String> {
        if self.initialized {
            return Ok(());
        }
        if bytes < 64 {
            return Err(format!("Buffer too small for calibration: {bytes} bytes"));
        }
        // Each probe lays its own chain (`probe`), as `initialize_with_page_size` says
        log::info!("ProbeEngine: Using external buffer ({bytes} bytes)");
        self.external_ptr = Some(base);
        self.max_working_set = bytes;
        self.initialized = true;
        Ok(())
    }

    /// Get the base pointer for probe operations.
    /// Returns the external buffer pointer if set, otherwise the owned buffer.
    fn get_base_ptr(&mut self) -> *mut u64 {
        if let Some(ext) = self.external_ptr {
            ext
        } else {
            self.memory_buffer.as_mut().expect("an initialized engine has its own buffer or an external one").as_mut_ptr() as *mut u64
        }
    }

    /// Setup random pointer-chasing pattern in memory
    /// Each u64 location stores the address of the next random location, in one cycle through all `len` of them
    /// This creates a random walk through memory that defeats prefetchers
    ///
    /// Laid in place, with no index array the working set's size beside it: a run's calibration lays it after the run's allocation,
    /// up to four times CPUID's L3 (1.9 GB on a 480 MB L3), and that much heap beside it paged, or at the commit limit aborted the
    /// process. Each slot takes its index, Sattolo's shuffle (a swap with a slot below, never itself) makes them one cycle, slot i
    /// holding the index of the one after it, and each index becomes its slot's address. A uniform random cycle, as the
    /// shuffle-then-link before it was.
    unsafe fn setup_pointer_chase(base: *mut u64, len: usize, thread_id: usize) {
        if len == 0 {
            return;
        }
        // SAFETY: the caller's `len` u64s at `base`, nothing else using them meanwhile
        let slots = unsafe { std::slice::from_raw_parts_mut(base, len) };
        for (i, slot) in slots.iter_mut().enumerate() {
            *slot = i as u64;
        }

        // Sattolo's shuffle using XorShift RNG
        let mut rng_state = 0x123456789ABCDEFu64.wrapping_add(thread_id as u64);
        for i in (1..len).rev() {
            rng_state ^= rng_state << 13;
            rng_state ^= rng_state >> 17;
            rng_state ^= rng_state << 5;
            slots.swap(i, (rng_state as usize) % i);
        }

        // Each slot's next, an index below `len`, as its address
        for slot in slots.iter_mut() {
            *slot = base.wrapping_add(*slot as usize) as usize as u64;
        }
    }

    /// Run a probe at the specified workload size
    /// Returns the latency samples in nanoseconds (empty if the probe could not run)
    #[cfg(target_arch = "x86_64")]
    pub fn probe(&mut self, workload_size: usize, config: &CalibrationConfig) -> Vec<f64> {
        if !self.initialized {
            return Vec::new();
        }

        let base = self.get_base_ptr();

        // Limit workload to what we have allocated
        let actual_size = workload_size.min(self.max_working_set);
        let working_set_u64 = actual_size / std::mem::size_of::<u64>();

        if working_set_u64 < 8 {
            return Vec::new();
        }

        // CRITICAL: Re-initialize pointer-chase pattern for THIS working set size
        // The chain must be constrained to the working set being tested, otherwise we'll be hitting random locations across the entire
        // buffer (always DRAM)
        // SAFETY: `working_set_u64` is at most `max_working_set` / 8, the engine buffer's u64s
        unsafe {
            Self::setup_pointer_chase(base, working_set_u64, 0);
        }

        let iterations_per_sample = 1000usize.min(working_set_u64);
        let mut stats = OnlineStats::with_capacity(1000);
        let start_time = Instant::now();
        let max_duration = std::time::Duration::from_millis(config.max_probe_time_ms);

        // Start pointer at beginning of chain
        let mut ptr = base;

        loop {
            // Take a measurement
            // SAFETY: RDTSCP, which every CPU past the startup gate has, and loads along the chain `setup_pointer_chase` just laid in the
            // working set, every link an address inside it
            unsafe {
                std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
                let mut aux = 0u32;
                let start_cycles = __rdtscp(&mut aux);

                // Pointer chase for iterations_per_sample steps
                for _ in 0..iterations_per_sample {
                    let addr = *ptr;
                    ptr = addr as *mut u64;
                }

                let end_cycles = __rdtscp(&mut aux);
                std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);

                // CRITICAL: Prevent compiler from optimizing away the pointer chase
                std::hint::black_box(ptr);

                let delta_cycles = end_cycles - start_cycles;
                let cycles_per_read = delta_cycles as f64 / iterations_per_sample as f64;
                let latency_ns = cycles_per_read / self.tsc_frequency_ghz;

                // Filter obvious outliers (should be 0.5-1000 ns range for cache/DRAM)
                if latency_ns > 0.1 && latency_ns < 10000.0 {
                    stats.add_sample(latency_ns);
                }
            }

            let elapsed = start_time.elapsed();

            // Check for convergence (enough samples and low relative error)
            if stats.count() >= config.min_samples as u64 {
                let rel_error = stats.relative_error();
                if rel_error < config.convergence_threshold {
                    return stats.samples().to_vec();
                }
            }

            // Check time limit
            if elapsed >= max_duration {
                return stats.samples().to_vec();
            }
        }
    }

    /// Non-x86_64 stub
    #[cfg(not(target_arch = "x86_64"))]
    pub fn probe(&mut self, _workload_size: usize, _config: &CalibrationConfig) -> Vec<f64> {
        Vec::new()
    }

    /// Get the maximum working set size
    pub fn max_working_set(&self) -> usize {
        self.max_working_set
    }
}

// ============================================================================
// Sweep Probe Result
// ============================================================================

/// Result of a single probe in the sweep
#[derive(Debug, Clone)]
pub struct SweepProbe {
    pub size: usize,
    pub median_ns: f64,
    pub spread: f64,
    pub window_stddev: f64, // Stability metric from sliding window
}

// ============================================================================
// Fine Probe (Phase 2 extended calibration)
// ============================================================================

/// Result from a fine-grain probe with multiple repeats at a single size
#[derive(Clone, Debug)]
pub struct FineProbe {
    pub size: usize,
    pub repeats: u32,
    pub stable_count: u32,
    pub overall_median_ns: f64,
    pub worst_spread: f64,
    pub latency_consistency: f64,
}

impl FineProbe {
    /// A probe is reliable if ALL repeats were stable
    pub fn is_reliable(&self) -> bool {
        self.stable_count == self.repeats
    }
}

// ============================================================================
// Calibration Test (Orchestrator)
// ============================================================================

/// The CPUs the calibration runs on (TODO 22): anchor A's, the first CPU of the top-ranked core (Windows' preferred), and its NUMA node;
/// B's, the SMT pair of the first core in that order on the node that has one; C's, every logical CPU of the node, and the most of them
/// one L3 group has. One node's, as a run's thread uses its own node's memory: the calibration measures on memory on `node`, and on an
/// Intel CPU with SNC each node's memory is cached in that node's part of the L3 alone. On the m8i.metal-48xl (SNC3, 2026-11-05) A read
/// the L3 at 37 ns on its own node's memory and 57 ns on another's, and C on every CPU put all 192 in one node's L3.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalibrationCpus {
    pub single: usize,
    pub node: u32,
    /// The machine's NUMA nodes
    pub nodes: usize,
    pub pair: Option<[usize; 2]>,
    pub all: Vec<usize>,
    pub per_l3: usize,
}

impl CalibrationCpus {
    /// From the cores the selection ranks (`cpu_selection::cores`)
    pub fn of(cores: &[crate::cpu_selection::Core]) -> Self {
        let order = crate::cpu_selection::rank(cores);
        let (single, node) = order.first().map_or((0, 0), |&i| (cores[i].cpus[0], cores[i].numa_node));
        // The node's cores, ranked
        let on_node: Vec<&crate::cpu_selection::Core> = order.iter().map(|&i| &cores[i]).filter(|core| core.numa_node == node).collect();
        let pair = on_node.iter().find(|core| core.cpus.len() >= 2).map(|core| [core.cpus[0], core.cpus[1]]);
        let mut all: Vec<usize> = on_node.iter().flat_map(|core| core.cpus.iter().copied()).collect();
        all.sort_unstable();
        let mut on_l3: HashMap<crate::cpu_topology::L3Group, usize> = HashMap::new();
        for core in &on_node {
            *on_l3.entry(core.l3).or_default() += core.cpus.len();
        }
        let mut nodes: Vec<u32> = cores.iter().map(|core| core.numa_node).collect();
        nodes.sort_unstable();
        nodes.dedup();
        Self { single, node, nodes: nodes.len(), pair, all, per_l3: on_l3.into_values().max().unwrap_or(1) }
    }

    /// C's CPUs as the output names them: every CPU, or with more than one node, every CPU of A's
    fn every(&self) -> String {
        if self.nodes > 1 { format!("Every CPU of NUMA node {}", self.node) } else { "Every CPU".to_string() }
    }

    /// This machine's
    pub fn detect() -> Self {
        Self::of(&crate::cpu_selection::cores(crate::cpu_topology::get_cpu_topology()))
    }
}

/// One probe of a recheck (`CalibrationTest::recheck_on`, TODO 116): what it probed, at what size, its median latency of three, and
/// whether it should still read inside the L3 (at the stored size: at most `limit_ns`) or past it (at one and a half times it: more)
#[derive(Debug, Clone, PartialEq)]
pub struct RecheckProbe {
    pub what: &'static str,
    pub size: usize,
    pub ns: f64,
    pub should_be_inside: bool,
    pub limit_ns: f64,
}

impl RecheckProbe {
    /// It reads as it should
    pub fn holds(&self) -> bool {
        (self.ns <= self.limit_ns) == self.should_be_inside
    }
}

/// What a recheck found: its probes, the stored L3 latency they're measured against, whether the stored calibration holds (every
/// probe as it should be; with none fitting the run's memory it is kept unchecked), and how long it took. The limits leave room for
/// noise: a stored size reads inside at 1.5x the stored L3 latency, a size half as big again reads past it at 1.25x, the latency anchor
/// C takes its size at. On one, a recheck read C's size at 43.6 ns against 43.6 (2026-11-05, the dev box), a coin toss.
#[derive(Debug, Clone, PartialEq)]
pub struct Recheck {
    pub probes: Vec<RecheckProbe>,
    pub l3_ns: f64,
    pub held: bool,
    pub time_ms: u64,
}

impl Recheck {
    /// The console's lines: the verdict, then each probe; with no probe (none fits a thread's memory), that it wasn't rechecked
    pub fn text(&self) -> String {
        if self.probes.is_empty() {
            return "🔁 Cache calibration not rechecked: none of its probes fits a thread's memory on its own node (a larger memory= gives \
                    them room, unless the node is out of large pages and the rest came from another node); the stored one is kept\n"
                .to_string();
        }
        let verdict = if self.held { "holds" } else { "has drifted: measured again" };
        let mut text = format!(
            "🔁 Cache calibration rechecked in {:.1} s: it {verdict} (its L3 latency {:.1} ns)\n",
            self.time_ms as f64 / 1000.0,
            self.l3_ns
        );
        for probe in &self.probes {
            let (should, sign) = if probe.should_be_inside { ("inside", "≤") } else { ("past it", ">") };
            let mark = if probe.holds() { "✓" } else { "⚠️ it isn't" };
            text.push_str(&format!(
                "   {}: {} at {:.1} ns, {should} ({sign} {:.1} ns) {mark}\n",
                probe.what,
                format_size(probe.size),
                probe.ns,
                probe.limit_ns
            ));
        }
        text
    }
}

/// The cores a group runs at each point of the curve check (`run_curve`): every count up to 8 (a desktop's group, an EPYC CCX), and
/// past that a sample, 1, 2, 3, 4, 6, 8, 12, 16 and so on by halves of a power of two, the group's whole last. A Xeon's group is its
/// socket, 48 cores on an m8i.metal-48xl: every count, both layouts, would be about 96 points, half an hour.
fn curve_counts(per_group: usize) -> Vec<usize> {
    if per_group <= 8 {
        return (1..=per_group).collect();
    }
    let mut counts: Vec<usize> =
        (0..usize::BITS - 2).flat_map(|shift| [1usize << shift, 3usize << shift]).filter(|&n| n < per_group).collect();
    counts.sort_unstable();
    counts.dedup();
    counts.push(per_group);
    counts
}

/// One point of TODO 22's curve check (`CalibrationTest::run_curve`): the L3 a thread reached with `k` threads in its L3 group running,
/// one a core or both of a core's, its latency, and what the curve through anchors A and C says for `k`
#[derive(Debug, Clone, PartialEq)]
pub struct CurvePoint {
    pub k: usize,
    pub both: bool,
    pub reach: Option<usize>,
    pub ns: f64,
    pub curve: f64,
}

/// The curve check's table: each point's reach against the curve's, and how far off
pub fn display_curve(results: &CalibrationResults, points: &[CurvePoint], out: &mut dyn std::io::Write) -> std::io::Result<()> {
    writeln!(out, "\n📈 The L3 a thread reaches with k threads in its L3 group running, against the curve through A and C (TODO 22)")?;
    if let (Some(one), Some(shared)) = (results.l3_reach(1), &results.shared) {
        writeln!(
            out,
            "A, one thread: {}; C, every CPU ({} a group): {}",
            format_size(one as usize),
            shared.l3_sharers,
            shared.l3_loaded.map_or("not found".to_string(), format_size)
        )?;
    }
    writeln!(out, "{:>4}  {:<24} {:>12} {:>10} {:>12} {:>8}", "k", "threads", "measured", "latency", "curve", "off")?;
    for point in points {
        let layout = if point.both { "both threads of a core" } else { "one thread a core" };
        let (measured, ns, off) = match point.reach {
            Some(size) => (format_size(size), format!("{:.1}ns", point.ns), format!("{:+.0}%", (point.curve / size as f64 - 1.0) * 100.0)),
            None => ("not found".to_string(), String::new(), String::new()),
        };
        writeln!(out, "{:>4}  {:<24} {:>12} {:>10} {:>12} {:>8}", point.k, layout, measured, ns, format_size(point.curve as usize), off)?;
    }
    writeln!(out, "(off: the curve's reach against the measured one; positive, the curve gives a test more than fits)")?;
    Ok(())
}

/// What a loaded probe runs on (`CalibrationTest::loaded_reach`, TODO 22): its CPUs, how many of them share an L3 group, a thread's L2
/// then, and what its log lines call it
struct Loaded<'a> {
    cpus: &'a [usize],
    sharers: usize,
    own: usize,
    label: String,
}

/// A thread's own memory in a shared probe, and its CPU (TODO 22)
#[derive(Debug, Clone, Copy)]
struct Slot {
    cpu: usize,
    base: *mut u64,
    bytes: usize,
}

// SAFETY: a slot's memory is its thread's alone while a probe runs (`probe_shared` gives each thread one slot, and the slots don't
// overlap: `slots`), and the memory outlives the probe
unsafe impl Send for Slot {}
// SAFETY: as Send's: a probe's threads only read a slot's fields, each its own slot's memory
unsafe impl Sync for Slot {}

/// `count` slots of `block` bytes each, one after another from `base`, on `cpus` in turn
fn slots(base: *mut u64, block: usize, cpus: &[usize]) -> Vec<Slot> {
    // SAFETY: the caller's memory holds `cpus.len()` blocks of `block` bytes from `base` (`run_shared` sizes them so)
    cpus.iter().enumerate().map(|(i, &cpu)| Slot { cpu, base: unsafe { base.add(i * block / 8) }, bytes: block }).collect()
}

/// A random cycle through the cache lines of the `bytes` at `base`, one node a line, each holding the next one's address: the shared
/// probes' chain. u32 indices, so its setup takes a sixteenth of the block in heap (a thread of 384 on the metal box: 6 MB, 375 KB).
///
/// # Safety
/// `base` is 64-byte aligned and `bytes` of it are writable and the caller's alone.
unsafe fn setup_line_chase(base: *mut u64, bytes: usize, seed: usize) {
    let lines = bytes / 64;
    if lines == 0 {
        return;
    }
    let mut order: Vec<u32> = (0..lines as u32).collect();
    let mut rng = 0x9E37_79B9_7F4A_7C15u64 ^ seed as u64;
    for i in (1..lines).rev() {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        order.swap(i, (rng % (i as u64 + 1)) as usize);
    }
    for i in 0..lines {
        let next = order[(i + 1) % lines] as usize;
        // SAFETY: both lines are inside the caller's `bytes` (indices below `lines`)
        unsafe { *base.add(order[i] as usize * 8) = base.add(next * 8) as u64 };
    }
}

/// Samples of a chase along the chain at `base` for `time`: ns a load, each the mean of 1000 loads.
///
/// # Safety
/// `base` starts a chain that `setup_line_chase` (or `ProbeEngine::setup_pointer_chase`) laid, in memory the caller's alone.
unsafe fn chase_for(base: *mut u64, tsc_ghz: f64, time: std::time::Duration) -> Vec<f64> {
    const LOADS: usize = 1000;
    let mut samples = Vec::with_capacity(4096);
    let mut ptr = base;
    let start = Instant::now();
    while start.elapsed() < time {
        let mut aux = 0u32;
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
        // SAFETY: RDTSCP, which every CPU past the startup gate has; the loads follow the caller's chain, every link an address in it
        let (begin, end) = unsafe {
            let begin = __rdtscp(&mut aux);
            for _ in 0..LOADS {
                ptr = *ptr as *mut u64;
            }
            (begin, __rdtscp(&mut aux))
        };
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
        std::hint::black_box(ptr);
        let ns = (end - begin) as f64 / LOADS as f64 / tsc_ghz;
        if ns > 0.1 && ns < 10000.0 {
            samples.push(ns);
        }
    }
    samples
}

/// Every slot's thread, pinned to its CPU, chases its own `size` bytes with the others for `time`, all starting together (TODO 22): the
/// median of the threads' median latencies, and of their spreads. None when a thread got no samples.
fn probe_shared(slots: &[Slot], size: usize, tsc_ghz: f64, time: std::time::Duration) -> Option<ProbeStatistics> {
    let barrier = std::sync::Barrier::new(slots.len());
    let per_thread: Vec<Vec<f64>> = std::thread::scope(|scope| {
        let threads: Vec<_> = slots
            .iter()
            .enumerate()
            .map(|(i, &slot)| {
                let barrier = &barrier;
                scope.spawn(move || {
                    // The whole slot, which is Send, not its fields one by one (a closure captures what it uses)
                    let slot: Slot = slot;
                    if let Err(e) = crate::runner::pin_thread_to_cpu_with_config(i, slot.cpu, true) {
                        log::debug!("Calibration thread {i} not pinned to CPU {}: {e}", slot.cpu);
                    }
                    // SAFETY: the slot's memory, 64-byte aligned (test memory, cut in whole lines), its thread's alone
                    unsafe { setup_line_chase(slot.base, size.min(slot.bytes), i) };
                    barrier.wait();
                    // SAFETY: the chain just laid
                    unsafe { chase_for(slot.base, tsc_ghz, time) }
                })
            })
            .collect();
        threads.into_iter().map(|t| t.join().unwrap_or_default()).collect()
    });
    if per_thread.iter().any(Vec::is_empty) {
        return None;
    }
    let stats: Vec<ProbeStatistics> = per_thread.iter().map(|samples| StatisticsAnalyzer::analyze(samples)).collect();
    Some(ProbeStatistics {
        median_ns: median(&stats.iter().map(|s| s.median_ns).collect::<Vec<_>>()),
        spread_ratio: median(&stats.iter().map(|s| s.spread_ratio).collect::<Vec<_>>()),
    })
}

/// How long each shared probe runs: anchor B's (L1 and L2, a sample a few microseconds) and C's (L3 to DRAM, up to a tenth of a ms)
const SMT_PROBE_TIME: std::time::Duration = std::time::Duration::from_millis(50);
const LOADED_PROBE_TIME: std::time::Duration = std::time::Duration::from_millis(150);

/// A stop asked for (a first Ctrl+C, the window's Stop: `runner::request_stop`), which a run's calibration, before its tests, would
/// otherwise sit through, about 2 minutes: each sweep checks it before each probe and ends there, and a stopped calibration is an
/// error, `STOPPED`, so nothing partial is saved
fn stopped() -> bool {
    crate::runner::SHUTDOWN_REQUESTED.load(std::sync::atomic::Ordering::Relaxed)
}
const STOPPED: &str = "stopped before it finished; nothing saved";

/// Main calibration test that orchestrates tier calibration
pub struct CalibrationTest {
    /// Cache information from system detection
    cache_info: CacheInfo,
    /// TSC frequency in GHz
    tsc_frequency_ghz: f64,
    /// Calibration configuration
    config: CalibrationConfig,
}

impl CalibrationTest {
    /// Create with custom configuration
    pub fn with_config(cache_info: CacheInfo, config: CalibrationConfig) -> Self {
        let tsc_frequency_ghz = cache_info.tsc_frequency_ghz;
        Self { cache_info, tsc_frequency_ghz, config }
    }

    /// Get tier size from cache info
    fn get_tier_size(&self, tier: CacheTier) -> usize {
        match tier {
            CacheTier::L1 => self.cache_info.per_core_l1d,
            CacheTier::L2 => self.cache_info.per_core_l2,
            CacheTier::L3 => self.cache_info.l3_cache,
            CacheTier::Dram => self.cache_info.l3_cache * 2,
            CacheTier::FullDram => self.cache_info.l3_cache * 4,
        }
    }

    /// The calibration, on one thread (TODO 22: the standard and the extended merged, 2026-11-04): a sweep from a quarter of L1 to four
    /// times L3, then a finer one around each tier's edge. On the dev box the extended one's L3 agreed with itself within 10% over three
    /// runs (64-71 MB), the standard one's varied by a third (34-45 MB), for 10-30 s more. Then anchors B and C (`run_shared`). Allocates
    /// its own memory.
    pub fn run(&self) -> Result<CalibrationResults, String> {
        let start = Instant::now();
        let cpus = CalibrationCpus::detect();
        let bytes = self.memory_needed(&cpus);
        let mut probe_engine = ProbeEngine::new(self.tsc_frequency_ghz);
        probe_engine.initialize_with_page_size(bytes, &self.config.page_size, cpus.node)?;
        let page = probe_engine.page_obtained();
        let mut results = self.measure(probe_engine, &cpus, bytes)?;
        // The page size the buffer got, else the one asked for
        results.page_size = page.map_or_else(|| self.config.page_size.clone(), |p| page_size_str(p).to_string());
        results.total_time_ms = start.elapsed().as_millis() as u64;
        Ok(results)
    }

    /// The calibration on a run's own memory, its pages `page`, before its tests (TODO 22): the `bytes` at `base`, as much as it needs,
    /// refused on less than twice CPUID's L3.
    ///
    /// # Safety
    /// As `ProbeEngine::initialize_from_region`'s: the memory is the caller's alone for the call, and no thread of the caller's is pinned
    /// to the calibration's CPUs meanwhile (it would be measured too).
    pub unsafe fn run_on(&self, base: *mut u64, bytes: usize, page: PageSizeLevel) -> Result<CalibrationResults, String> {
        // A's sweep must pass the L3's edge, a gradual one 1.52x past it (`find_gradual_edge`), and its DRAM tier is read from twice the
        // L3. On less, each size past the span measures the span (`ProbeEngine::probe` clamps to it), inside the L3, no edge shows, and
        // the L3 would be saved at about CPUID's size as measured: a 10 MB span under the 8600G's 16 MB L3, about 16 MB.
        let least = self.get_tier_size(CacheTier::Dram);
        if bytes < least {
            return Err(format!(
                "a thread's memory on its own node, {}, is less than the {} it needs, twice the L3: a larger memory= gives it room, \
                 unless the node is out of large pages and the rest came from another node (--calibrate-cache measures on a buffer of \
                 its own); nothing saved",
                format_size(bytes),
                format_size(least)
            ));
        }
        let start = Instant::now();
        let cpus = CalibrationCpus::detect();
        let bytes = bytes.min(self.memory_needed(&cpus)) / crate::test_memory::GRANULE * crate::test_memory::GRANULE;
        let mut probe_engine = ProbeEngine::new(self.tsc_frequency_ghz);
        // SAFETY: as this fn's contract says
        unsafe { probe_engine.initialize_from_region(base, bytes) }?;
        log::info!(target: SHOWN_TARGET, "Calibration on the run's memory: {} on {} pages", format_size(bytes), page_size_name(page_size_str(page)));
        let mut results = self.measure(probe_engine, &cpus, bytes)?;
        results.page_size = page_size_str(page).to_string();
        results.total_time_ms = start.elapsed().as_millis() as u64;
        Ok(results)
    }

    /// The recheck on a run's memory (TODO 116, `calibrate=recheck`): from the `stored` calibration, the L3 one thread reaches (anchor
    /// A) and the one a thread reaches with every CPU running (C), each probed three times at its size, where it should still read
    /// inside the L3, and at one and a half times it, where it shouldn't. It holds when every probe reads as it should; otherwise the L3
    /// has shrunk or grown since (a VM's neighbours, a BIOS setting), and the run measures it again. L1 and L2 are the hardware's, and a
    /// hardware change already makes the stored calibration another machine's. A probe the memory can't hold is left out; with none left
    /// the stored one is kept unchecked.
    ///
    /// # Safety
    /// As `run_on`'s.
    pub unsafe fn recheck_on(&self, stored: &CalibrationResults, base: *mut u64, bytes: usize) -> Result<Recheck, String> {
        let start = Instant::now();
        let cpus = CalibrationCpus::detect();
        let l3 = stored.tiers.get(&CacheTier::L3).ok_or("the stored calibration has no L3")?;
        let l3_ns = l3.median_latency_ns;
        let granule = crate::test_memory::GRANULE;
        let whole = |size: usize| size / granule * granule;
        let mut probes = Vec::new();
        let mut push = |what: &'static str, size: usize, ns: f64, should_be_inside: bool| {
            let limit_ns = if should_be_inside { 1.5 * l3_ns } else { 1.25 * l3_ns };
            probes.push(RecheckProbe { what, size, ns, should_be_inside, limit_ns });
        };
        log::info!(target: SHOWN_TARGET, "Rechecking the cache calibration of {}", stored.timestamp.format("%Y-%m-%d %H:%M UTC"));

        // A: one thread, pinned, as the calibration measured it
        let sizes: Vec<(usize, bool)> =
            [(l3.optimal_size, true), (l3.optimal_size * 3 / 2, false)].into_iter().filter(|&(size, _)| size <= bytes).collect();
        let mut probe_engine = ProbeEngine::new(self.tsc_frequency_ghz);
        // SAFETY: as this fn's contract says
        unsafe { probe_engine.initialize_from_region(base, bytes) }?;
        let (single, config) = (cpus.single, self.config.clone());
        let single_ns: Vec<(usize, bool, f64)> = std::thread::scope(|scope| {
            scope
                .spawn(move || {
                    if let Err(e) = crate::runner::pin_thread_to_cpu_with_config(0, single, true) {
                        log::debug!("The recheck not pinned to CPU {single}: {e}");
                    }
                    sizes
                        .iter()
                        .map(|&(size, inside)| {
                            let size = whole(size);
                            let medians: Vec<f64> = (0..3)
                                .filter_map(|_| {
                                    let samples = probe_engine.probe(size, &config);
                                    (!samples.is_empty()).then(|| StatisticsAnalyzer::analyze(&samples).median_ns)
                                })
                                .collect();
                            (size, inside, if medians.is_empty() { f64::INFINITY } else { median(&medians) })
                        })
                        .collect()
                })
                .join()
                .map_err(|_| "the recheck's thread panicked".to_string())
        })?;
        for (size, inside, ns) in single_ns {
            push("one thread", size, ns, inside);
        }

        // C: every CPU of A's node, when it was measured
        if let Some(SharedReach { l3_loaded: Some(loaded), .. }) = &stored.shared
            && cpus.all.len() > 1
        {
            let block = whole(bytes / cpus.all.len());
            let all_slots = slots(base, block, &cpus.all);
            for (size, inside) in [(*loaded, true), (*loaded * 3 / 2, false)] {
                if size > block {
                    continue;
                }
                let size = whole(size);
                let medians: Vec<f64> = (0..3)
                    .filter_map(|_| probe_shared(&all_slots, size, self.tsc_frequency_ghz, LOADED_PROBE_TIME).map(|stats| stats.median_ns))
                    .collect();
                push("every CPU, a thread", size, if medians.is_empty() { f64::INFINITY } else { median(&medians) }, inside);
            }
        }
        let held = probes.iter().all(RecheckProbe::holds);
        Ok(Recheck { probes, l3_ns, held, time_ms: start.elapsed().as_millis() as u64 })
    }

    /// Anchor A on its CPU, pinned, as a worker is, then B and C (`run_shared`), on `probe_engine`'s `bytes`
    fn measure(&self, probe_engine: ProbeEngine, cpus: &CalibrationCpus, bytes: usize) -> Result<CalibrationResults, String> {
        let single = cpus.single;
        let (results, mut probe_engine) = std::thread::scope(|scope| {
            let mut probe_engine = probe_engine;
            scope
                .spawn(move || {
                    if let Err(e) = crate::runner::pin_thread_to_cpu_with_config(0, single, true) {
                        log::debug!("Calibration not pinned to CPU {single}: {e}");
                    }
                    (self.run_calibration(&mut probe_engine), probe_engine)
                })
                .join()
                .map_err(|_| "the calibration's thread panicked".to_string())
        })?;
        let mut results = results?;
        let base = probe_engine.get_base_ptr();
        // The engine's memory, `bytes` long, which nothing else uses, and which it keeps until after
        results.shared = Some(self.run_shared(&results, cpus, base, bytes));
        // A stop during B or C (A's ends `run_calibration`)
        if stopped() {
            return Err(STOPPED.to_string());
        }
        Ok(results)
    }

    /// The memory the calibration needs: anchor A's sweep, to four times CPUID's L3, or C's blocks, one a CPU, each twice the most it can
    /// reach (an L3 group's L3 over its CPUs, and an L2), whichever is more
    fn memory_needed(&self, cpus: &CalibrationCpus) -> usize {
        let single = self.get_tier_size(CacheTier::FullDram);
        let loaded = cpus.all.len() * Self::loaded_block(self.cache_info.l3_cache, self.cache_info.per_core_l2, cpus.per_l3);
        single.max(loaded).next_multiple_of(crate::test_memory::GRANULE)
    }

    /// A C thread's block: twice the most it can reach, an L3 over its sharers plus an L2, in whole 4 KiB granules
    fn loaded_block(l3: usize, l2: usize, sharers: usize) -> usize {
        (2 * (l3 / sharers.max(1) + l2)).next_multiple_of(crate::test_memory::GRANULE)
    }

    /// Anchors B and C (TODO 22), after A's `results`: B, L1 and L2 a thread with its SMT sibling chasing on the same core (none without
    /// SMT); C, the L3 a thread reaches with every logical CPU chasing. Each thread chases its own block of the `bytes` at `base`. Every
    /// field none when neither finds anything (no SMT pair, one CPU, no L3 edge) or A found no L1, L2 or L3 to size them from: still a
    /// calibration measured with them, which calibrate=auto loads (a `shared` of none is one from before them).
    ///
    /// The ranges come from A's: B sweeps from an eighth of A's L1 to one and a half of its L2, C from just past the thread's L2 (B's
    /// with SMT, every CPU running) to twice what it could reach. Each takes the edges as A does: the sweep's tipping points, then a
    /// finer sweep below each.
    fn run_shared(&self, results: &CalibrationResults, cpus: &CalibrationCpus, base: *mut u64, bytes: usize) -> SharedReach {
        let tsc = self.tsc_frequency_ghz;
        let size_of = |tier: CacheTier| results.tiers.get(&tier).map(|t| t.optimal_size);
        let (Some(a_l1), Some(a_l2), Some(a_l3)) = (size_of(CacheTier::L1), size_of(CacheTier::L2), size_of(CacheTier::L3)) else {
            log::warn!("Anchors B and C not measured: one thread found no L1, L2 or L3 to size them from");
            return SharedReach { l1_smt: None, l2_smt: None, l3_loaded: None, l3_loaded_ns: 0.0, l3_sharers: cpus.per_l3 };
        };
        let (repeats, fine_step, stable_spread) = (3u32, 1.05, 1.5);

        // B: the SMT pair
        let (mut l1_smt, mut l2_smt) = (None, None);
        if let Some(pair) = cpus.pair {
            log::info!(target: SHOWN_TARGET, "═══ Two threads on one core (CPUs {} and {}) ═══", pair[0], pair[1]);
            let block = (bytes / 2) / crate::test_memory::GRANULE * crate::test_memory::GRANULE;
            let pair_slots = slots(base, block, &pair);
            let mut probe = |size: usize| probe_shared(&pair_slots, size, tsc, SMT_PROBE_TIME);
            let sweep = self.run_sweep(&mut probe, (a_l1 / 8).max(4096), (a_l2 * 3 / 2).min(block), "Sweep, two threads on one core");
            let tps: Vec<usize> = self.find_tipping_points(&sweep).iter().map(|(s, _, _)| *s).collect();
            let (tp1, tp2) = (tps.first().copied().unwrap_or(a_l1 / 2), tps.get(1).copied().unwrap_or(a_l2 / 2));
            let fine_l1 = self.run_fine_sweep(&mut probe, tp1.min(a_l1) / 2, tp1.min(a_l1), fine_step, repeats, "L1→L2, two threads");
            let fine_l2 = self.run_fine_sweep(&mut probe, tp2.min(a_l2) / 2, tp2.min(a_l2), fine_step, repeats, "L2→L3, two threads");
            l1_smt = Self::find_largest_reliable(&fine_l1, stable_spread).map(|fp| fp.size);
            l2_smt = Self::find_largest_reliable(&fine_l2, stable_spread).map(|fp| fp.size);
            log::info!(target: SHOWN_TARGET,
                "Two threads on one core: L1 {} and L2 {} a thread",
                l1_smt.map_or("none steady".to_string(), format_size),
                l2_smt.map_or("none steady".to_string(), format_size)
            );
        }

        // C: every logical CPU of A's node
        let mut l3_loaded = None;
        let mut l3_loaded_ns = 0.0;
        if cpus.all.len() > 1 {
            log::info!(target: SHOWN_TARGET, "═══ {} at once ({}, {} sharing each L3) ═══", cpus.every(), cpus.all.len(), cpus.per_l3);
            let own = if cpus.pair.is_some() { l2_smt.unwrap_or(a_l2 / 2) } else { a_l2 };
            let loaded = Loaded { cpus: &cpus.all, sharers: cpus.per_l3, own, label: "every CPU".to_string() };
            if let Some((size, ns)) = self.loaded_reach(results, &loaded, base, bytes) {
                (l3_loaded, l3_loaded_ns) = (Some(size), ns);
            }
            log::info!(target: SHOWN_TARGET,
                "{} at once: L3 {} a thread ({} alone)",
                cpus.every(),
                l3_loaded.map_or("not found".to_string(), format_size),
                format_size(a_l3)
            );
        }

        SharedReach { l1_smt, l2_smt, l3_loaded, l3_loaded_ns, l3_sharers: cpus.per_l3 }
    }

    /// The L3 a thread reaches with `cpus` all chasing at once, `sharers` of them in an L3 group (anchor C's measure, TODO 22), each on
    /// its own block of the `bytes` at `base`, a thread's L2 `own`: the size and its latency, None when the sweep found no edge. The
    /// sweep runs from just past `own` to twice the most a thread could reach (CPUID's L3 share where it is more than A's: on the dev
    /// box, 8 threads held more than 144 MB, past twice A's 61 MB). The edge is A's, a step of 1.4x, but past the L3's own latency: with
    /// many threads the step into L3 comes first (4 to 34 ns at 1.4 MB there). The size is the largest steady one still within 1.25x of
    /// A's L3 latency, as the latency climbs before the step (23.5 MB a thread there was steady at 82 ns, past half way to DRAM's 130).
    fn loaded_reach(&self, results: &CalibrationResults, loaded: &Loaded, base: *mut u64, bytes: usize) -> Option<(usize, f64)> {
        let Loaded { cpus, sharers, own, label } = loaded;
        let (sharers, own) = (*sharers, *own);
        let (repeats, fine_step, stable_spread) = (3u32, 1.05, 1.5);
        let a_l3 = results.tiers.get(&CacheTier::L3)?.optimal_size;
        let a_l2 = results.tiers.get(&CacheTier::L2).map_or(own, |t| t.optimal_size);
        let granule = crate::test_memory::GRANULE;
        let block = Self::loaded_block(a_l3.max(self.cache_info.l3_cache), a_l2, sharers).min((bytes / cpus.len()) / granule * granule);
        let loaded_slots = slots(base, block, cpus);
        let tsc = self.tsc_frequency_ghz;
        let mut probe = |size: usize| probe_shared(&loaded_slots, size, tsc, LOADED_PROBE_TIME);
        let low = (own + own / 10).max(4096);
        let sweep = self.run_sweep(&mut probe, low, block, &format!("Sweep, {label}"));
        // A stop ended it early: not that it found no edge
        if stopped() {
            return None;
        }
        let l3_ns = results.tiers.get(&CacheTier::L3).map_or(0.0, |t| t.median_latency_ns);
        let Some(edge) =
            self.find_tipping_points(&sweep).into_iter().find(|&(_, before, _)| before >= 0.8 * l3_ns).map(|(size, _, _)| size)
        else {
            log::warn!("{label}: no L3 edge between {} and {} a thread", format_size(low), format_size(block));
            return None;
        };
        let fine = self.run_fine_sweep(&mut probe, (edge / 2).max(low), edge, fine_step, repeats, &format!("L3→DRAM, {label}"));
        let cap = 1.25 * l3_ns;
        // The sizes that qualify, largest first: steady, and within the cap
        let mut candidates: Vec<(usize, f64)> = fine
            .iter()
            .filter(|fp| fp.is_reliable() && fp.worst_spread < stable_spread && fp.overall_median_ns <= cap)
            .map(|fp| (fp.size, fp.overall_median_ns))
            .collect();
        if candidates.is_empty() {
            candidates = sweep
                .iter()
                .filter(|p| p.size >= low && p.size < edge && p.spread < 2.0 && p.median_ns <= cap)
                .map(|p| (p.size, p.median_ns))
                .collect();
        }
        candidates.sort_by_key(|&(size, _)| std::cmp::Reverse(size));
        // Each confirmed before it is taken (TODO 116): three probes more at it, which must read inside the L3 as a recheck reads it
        // (within 1.5x A's latency), else the next smaller. On daddy-petz a run's C took 2.6 MB a thread with 16 sharing, 42 MB in a
        // 32 MB L3: something brief let all three of its finer probes read steady, and the next run's recheck read it at 31.6 ns.
        for &(size, ns) in candidates.iter().take(4) {
            if stopped() {
                break;
            }
            let medians: Vec<f64> = (0..3).filter_map(|_| probe(size).map(|stats| stats.median_ns)).collect();
            let confirmed = if medians.is_empty() { f64::INFINITY } else { median(&medians) };
            if confirmed <= 1.5 * l3_ns {
                return Some((size, ns.max(confirmed)));
            }
            log::warn!("{label}: {} a thread read {confirmed:.1} ns on a second look, past the L3: a smaller size", format_size(size));
        }
        None
    }

    /// TODO 22's check of the curve (`--calibrate-curve`, which saves nothing): the calibration, then the L3 a thread reaches with `k` in
    /// each L3 group running, one thread a core (k from 1 up to a group's cores, `curve_counts`: every count in a group of up to 8, past
    /// that a sample) and both of each core's (k twice those), each against the curve's reach(k). Allocates its own memory. Every group of
    /// A's NUMA node runs its `k` at once, as a run's threads do (one node's, as C's: `CalibrationCpus`).
    pub fn run_curve(&self) -> Result<(CalibrationResults, Vec<CurvePoint>), String> {
        let cores = crate::cpu_selection::cores(crate::cpu_topology::get_cpu_topology());
        let results = self.run()?;
        let cpus = CalibrationCpus::of(&cores);
        let cores: Vec<crate::cpu_selection::Core> = cores.into_iter().filter(|core| core.numa_node == cpus.node).collect();
        let bytes = self.memory_needed(&cpus);
        let mut probe_engine = ProbeEngine::new(self.tsc_frequency_ghz);
        probe_engine.initialize_with_page_size(bytes, &self.config.page_size, cpus.node)?;
        let base = probe_engine.get_base_ptr();
        let (a_l2, l2_smt) = (
            results.tiers.get(&CacheTier::L2).map_or(self.cache_info.per_core_l2, |t| t.optimal_size),
            results.shared.as_ref().and_then(|shared| shared.l2_smt),
        );
        // Each L3 group's cores, in the order the selection ranks them
        let order = crate::cpu_selection::rank(&cores);
        let mut groups: Vec<(crate::cpu_topology::L3Group, Vec<&crate::cpu_selection::Core>)> = Vec::new();
        for core in order.iter().map(|&i| &cores[i]) {
            match groups.iter_mut().find(|(l3, _)| *l3 == core.l3) {
                Some((_, members)) => members.push(core),
                None => groups.push((core.l3, vec![core])),
            }
        }
        let per_group = groups.iter().map(|(_, members)| members.len()).min().unwrap_or(1);
        let smt = groups.iter().all(|(_, members)| members.iter().all(|core| core.cpus.len() >= 2));
        let mut points = Vec::new();
        for both in [false, true] {
            if both && !smt {
                continue;
            }
            for used in curve_counts(per_group) {
                let k = if both { 2 * used } else { used };
                let chosen: Vec<usize> = groups
                    .iter()
                    .flat_map(|(_, members)| {
                        members[..used].iter().flat_map(|core| core.cpus.iter().take(if both { 2 } else { 1 }).copied())
                    })
                    .collect();
                let own = if both { l2_smt.unwrap_or(a_l2 / 2) } else { a_l2 };
                let layout = if both { "both threads of a core" } else { "one thread a core" };
                let label = format!("{k} a group, {layout}");
                log::info!(target: SHOWN_TARGET, "═══ {label} ({} CPUs) ═══", chosen.len());
                let reached = self.loaded_reach(&results, &Loaded { cpus: &chosen, sharers: k, own, label }, base, bytes);
                points.push(CurvePoint {
                    k,
                    both,
                    reach: reached.map(|(size, _)| size),
                    ns: reached.map_or(0.0, |(_, ns)| ns),
                    curve: results.l3_reach(k).unwrap_or(0.0),
                });
            }
        }
        Ok((results, points))
    }

    /// The calibration's sweeps on `probe_engine`'s memory, and the size each tier takes from them
    fn run_calibration(&self, probe_engine: &mut ProbeEngine) -> Result<CalibrationResults, String> {
        let start_time = Instant::now();

        log::info!(target: SHOWN_TARGET, "Starting cache calibration: a sweep, then a finer one around each edge");
        log::info!(target: SHOWN_TARGET,
            "  L1D: {} KB, L2: {} KB, L3: {} MB",
            self.cache_info.per_core_l1d / 1024,
            self.cache_info.per_core_l2 / 1024,
            self.cache_info.l3_cache / (1024 * 1024)
        );

        let l1_size = self.cache_info.per_core_l1d;
        let l2_size = self.cache_info.per_core_l2;
        let l3_size = self.cache_info.l3_cache;

        // The sweep, from a quarter of L1 to four times L3 (or the buffer's end)
        log::info!(target: SHOWN_TARGET, "═══ One thread ═══");
        let max_working_set = probe_engine.max_working_set();
        let config = self.config.clone();
        let mut probe = |size: usize| {
            let samples = probe_engine.probe(size, &config);
            (!samples.is_empty()).then(|| StatisticsAnalyzer::analyze(&samples))
        };
        let sweep_results = self.run_sweep(&mut probe, (l1_size / 4).max(4096), (l3_size * 4).min(max_working_set), "Sweep, one thread");
        let mut tipping_points = self.find_tipping_points(&sweep_results);
        // The L1's and the L2's edges but no L3's: a gradual one, where no step reaches 1.4x over the three before it
        if tipping_points.len() == 2
            && let Some(edge) = Self::find_gradual_edge(&sweep_results, tipping_points[1].0)
        {
            tipping_points.push(edge);
        }

        for (size, lat_before, lat_after) in &tipping_points {
            log::info!(target: SHOWN_TARGET,
                "  Tipping point at {}: {:.1}ns → {:.1}ns ({:.1}x jump)",
                format_size(*size),
                lat_before,
                lat_after,
                lat_after / lat_before
            );
        }

        let tp_sizes: Vec<usize> = tipping_points.iter().map(|(s, _, _)| *s).collect();
        let tp_l1_l2 = tp_sizes.first().copied().unwrap_or(l1_size);
        let tp_l2_l3 = tp_sizes.get(1).copied().unwrap_or(l2_size);
        let tp_l3_dram = tp_sizes.get(2).copied().unwrap_or(l3_size);

        // A finer sweep around each tipping point
        // Cap sweep ranges to CPU cache sizes — workload must fit in the tier
        log::info!(target: SHOWN_TARGET, "═══ Finer, around each edge ═══");

        let repeats = 3u32;
        let fine_step = 1.05; // 5% steps
        let stable_spread = 1.5;

        // Cap upper bounds: min of tipping point and CPU cache size
        let l1_cap = tp_l1_l2.min(l1_size);
        let l2_cap = tp_l2_l3.min(l2_size);
        let l3_cap = tp_l3_dram.min(l3_size);

        // Search window: from 50% below cap to the cap
        let fine_l1 = self.run_fine_sweep(&mut probe, l1_cap / 2, l1_cap, fine_step, repeats, "L1→L2");
        let fine_l2 = self.run_fine_sweep(&mut probe, l2_cap / 2, l2_cap, fine_step, repeats, "L2→L3");
        let fine_l3 = self.run_fine_sweep(&mut probe, l3_cap / 2, l3_cap, fine_step, repeats, "L3→DRAM");
        // A stop ended the sweeps early: no tier from them, nor their warnings, nor B and C after
        if stopped() {
            return Err(STOPPED.to_string());
        }

        // The tiers from the finer sweeps
        let cpu_signature = CpuSignature::from_cache_info(&self.cache_info);
        let mut results = CalibrationResults::new(cpu_signature);

        log::info!(target: SHOWN_TARGET, "═══ Final Selection ═══");

        // L1: Largest size where ALL repeats are stable, below TP1
        if let Some(fp) = Self::find_largest_reliable(&fine_l1, stable_spread) {
            log::info!(target: SHOWN_TARGET,
                "L1: {} @ {:.1}ns ({}/{} stable, worst_spread={:.2}x, consistency={:.2}ns)",
                format_size(fp.size),
                fp.overall_median_ns,
                fp.stable_count,
                fp.repeats,
                fp.worst_spread,
                fp.latency_consistency
            );
            results.add_tier_result(TierCalibrationResult {
                tier: CacheTier::L1,
                optimal_size: fp.size,
                median_latency_ns: fp.overall_median_ns,
                spread_ratio: fp.worst_spread,
                probes_attempted: sweep_results.len() as u32,
                converged: true,
                fallback_used: false,
            });
        } else if let Some(probe) = self.find_largest_stable_in_range(&sweep_results, 0, l1_cap, 1.5, 2.0) {
            log::warn!("L1: no steady size in the finer sweep, the sweep's: {} @ {:.1}ns", format_size(probe.size), probe.median_ns);
            results.add_tier_result(TierCalibrationResult {
                tier: CacheTier::L1,
                optimal_size: probe.size,
                median_latency_ns: probe.median_ns,
                spread_ratio: probe.spread,
                probes_attempted: sweep_results.len() as u32,
                converged: false,
                fallback_used: true,
            });
        }

        // L2: Largest reliable below L2 cap
        if let Some(fp) = Self::find_largest_reliable(&fine_l2, stable_spread) {
            log::info!(target: SHOWN_TARGET,
                "L2: {} @ {:.1}ns ({}/{} stable, worst_spread={:.2}x, consistency={:.2}ns)",
                format_size(fp.size),
                fp.overall_median_ns,
                fp.stable_count,
                fp.repeats,
                fp.worst_spread,
                fp.latency_consistency
            );
            results.add_tier_result(TierCalibrationResult {
                tier: CacheTier::L2,
                optimal_size: fp.size,
                median_latency_ns: fp.overall_median_ns,
                spread_ratio: fp.worst_spread,
                probes_attempted: sweep_results.len() as u32,
                converged: true,
                fallback_used: false,
            });
        } else if let Some(probe) = self.find_largest_stable_in_range(&sweep_results, tp_l1_l2, l2_cap, 1.5, 2.0) {
            log::warn!("L2: no steady size in the finer sweep, the sweep's: {} @ {:.1}ns", format_size(probe.size), probe.median_ns);
            results.add_tier_result(TierCalibrationResult {
                tier: CacheTier::L2,
                optimal_size: probe.size,
                median_latency_ns: probe.median_ns,
                spread_ratio: probe.spread,
                probes_attempted: sweep_results.len() as u32,
                converged: false,
                fallback_used: true,
            });
        }

        // L3: Largest reliable below L3 cap
        if let Some(fp) = Self::find_largest_reliable(&fine_l3, stable_spread) {
            log::info!(target: SHOWN_TARGET,
                "L3: {} @ {:.1}ns ({}/{} stable, worst_spread={:.2}x, consistency={:.2}ns)",
                format_size(fp.size),
                fp.overall_median_ns,
                fp.stable_count,
                fp.repeats,
                fp.worst_spread,
                fp.latency_consistency
            );
            results.add_tier_result(TierCalibrationResult {
                tier: CacheTier::L3,
                optimal_size: fp.size,
                median_latency_ns: fp.overall_median_ns,
                spread_ratio: fp.worst_spread,
                probes_attempted: sweep_results.len() as u32,
                converged: true,
                fallback_used: false,
            });
        } else if let Some(probe) = self.find_largest_stable_in_range(&sweep_results, tp_l2_l3, l3_cap, 2.0, 5.0) {
            log::warn!("L3: no steady size in the finer sweep, the sweep's: {} @ {:.1}ns", format_size(probe.size), probe.median_ns);
            results.add_tier_result(TierCalibrationResult {
                tier: CacheTier::L3,
                optimal_size: probe.size,
                median_latency_ns: probe.median_ns,
                spread_ratio: probe.spread,
                probes_attempted: sweep_results.len() as u32,
                converged: false,
                fallback_used: true,
            });
        }

        // DRAM + FullDRAM: from the sweep (no finer one needed for DRAM)
        // DRAM: Moderate size — 2x tipping point cap to differentiate from FullDRAM
        let total_cache = l3_size + l2_size + l1_size;
        let dram_min = tp_l3_dram.max(total_cache);
        let dram_max = tp_l3_dram * 2;
        let dram_max = dram_max.max(dram_min + 1);
        if let Some(probe) = self.find_largest_stable_in_range(&sweep_results, dram_min, dram_max, 2.0, 8.0) {
            log::info!(target: SHOWN_TARGET, "DRAM: {} @ {:.1}ns (from the sweep)", format_size(probe.size), probe.median_ns);
            results.add_tier_result(TierCalibrationResult {
                tier: CacheTier::Dram,
                optimal_size: probe.size,
                median_latency_ns: probe.median_ns,
                spread_ratio: probe.spread,
                probes_attempted: sweep_results.len() as u32,
                converged: true,
                fallback_used: false,
            });
        } else if let Some(probe) = self.find_largest_stable_in_range(&sweep_results, dram_min, usize::MAX, 2.0, 8.0) {
            log::info!(target: SHOWN_TARGET, "DRAM: {} @ {:.1}ns (from the sweep, past its range)", format_size(probe.size), probe.median_ns);
            results.add_tier_result(TierCalibrationResult {
                tier: CacheTier::Dram,
                optimal_size: probe.size,
                median_latency_ns: probe.median_ns,
                spread_ratio: probe.spread,
                probes_attempted: sweep_results.len() as u32,
                converged: true,
                fallback_used: false,
            });
        }
        // FullDRAM: Largest available (maximum memory pressure / TLB stress)
        if let Some(probe) = self.find_largest_stable(&sweep_results, l3_size * 2) {
            log::info!(target: SHOWN_TARGET, "FullDRAM: {} @ {:.1}ns (from the sweep)", format_size(probe.size), probe.median_ns);
            results.add_tier_result(TierCalibrationResult {
                tier: CacheTier::FullDram,
                optimal_size: probe.size,
                median_latency_ns: probe.median_ns,
                spread_ratio: probe.spread,
                probes_attempted: sweep_results.len() as u32,
                converged: true,
                fallback_used: false,
            });
        }

        log::info!(target: SHOWN_TARGET, "One thread's sweeps done in {} ms", start_time.elapsed().as_millis());
        Ok(results)
    }

    /// Run fine-grain sweep around a tipping point with multiple repeats per size, ended early by a stop (`stopped`)
    fn run_fine_sweep(
        &self, probe: &mut dyn FnMut(usize) -> Option<ProbeStatistics>, min_size: usize, max_size: usize, step_factor: f64, repeats: u32,
        label: &str,
    ) -> Vec<FineProbe> {
        let mut results = Vec::new();
        let mut current_size = min_size.max(4096);

        log::info!(target: SHOWN_TARGET,
            "  Fine sweep {}: {} to {} ({}x steps, {} repeats each)",
            label,
            format_size(min_size),
            format_size(max_size),
            step_factor,
            repeats
        );
        log::info!(target: SHOWN_TARGET, "  {:>10} {:>8} {:>6} {:>10} {:>10} {:>10}", "Size", "Median", "Stbl", "Worst Spr", "Consistency", "Latencies");

        while current_size <= max_size && !stopped() {
            let mut median_latencies = Vec::new();
            let mut stable_count = 0u32;
            let mut worst_spread = 0.0f64;

            for _rep in 0..repeats {
                if let Some(stats) = probe(current_size) {
                    median_latencies.push(stats.median_ns);
                    if stats.spread_ratio < 1.5 {
                        stable_count += 1;
                    }
                    if stats.spread_ratio > worst_spread {
                        worst_spread = stats.spread_ratio;
                    }
                }
            }

            if !median_latencies.is_empty() {
                median_latencies.sort_by(f64::total_cmp);
                let overall_median = median_latencies[median_latencies.len() / 2];

                // Consistency: std dev of median latencies across repeats
                let mean_lat = median_latencies.iter().sum::<f64>() / median_latencies.len() as f64;
                let consistency = if median_latencies.len() > 1 {
                    (median_latencies.iter().map(|x| (x - mean_lat).powi(2)).sum::<f64>() / median_latencies.len() as f64).sqrt()
                } else {
                    0.0
                };

                let lat_strs: Vec<String> = median_latencies.iter().map(|l| format!("{:.1}", l)).collect();
                let stable_marker = if stable_count == repeats {
                    "✓✓✓"
                } else if stable_count > 0 {
                    "✓~"
                } else {
                    "~~~"
                };

                log::info!(target: SHOWN_TARGET,
                    "  {:>10} {:>6.1}ns {:>6} {:>9.2}x {:>9.2}ns  [{}]",
                    format_size(current_size),
                    overall_median,
                    stable_marker,
                    worst_spread,
                    consistency,
                    lat_strs.join(", ")
                );

                results.push(FineProbe {
                    size: current_size,
                    repeats,
                    stable_count,
                    overall_median_ns: overall_median,
                    worst_spread,
                    latency_consistency: consistency,
                });
            }

            let next_size = ((current_size as f64) * step_factor) as usize;
            current_size = next_size.max(current_size + 4096);
        }

        results
    }

    /// Find the largest size where ALL repeats were stable
    fn find_largest_reliable(fine_probes: &[FineProbe], max_spread: f64) -> Option<FineProbe> {
        fine_probes.iter().filter(|fp| fp.is_reliable() && fp.worst_spread < max_spread).max_by_key(|fp| fp.size).cloned()
    }

    /// Run a continuous sweep from small to large sizes, ended early by a stop (`stopped`)
    /// Collects comprehensive stats for each probe to enable accurate tier detection
    fn run_sweep(
        &self, probe: &mut dyn FnMut(usize) -> Option<ProbeStatistics>, min_size: usize, max_size: usize, label: &str,
    ) -> Vec<SweepProbe> {
        let mut results = Vec::new();

        log::info!(target: SHOWN_TARGET, "{label}: {} to {}", format_size(min_size), format_size(max_size));
        log::info!(target: SHOWN_TARGET,
            "  {:>4} {:>10} {:>8} {:>7} {:>8} {:>6} {:>7} {:>6}",
            "#",
            "Size",
            "Median",
            "Spread",
            "Deriv",
            "Ratio",
            "WinSD",
            "Stbl"
        );

        // Use multiplicative steps - ~15% increase per step
        let step_factor = 1.15;

        let mut current_size = min_size;
        let mut probe_num = 0;
        let mut prev_latency: Option<f64> = None;
        let mut prev_size: Option<usize> = None;

        // For sliding window stability calculation
        let mut recent_latencies: Vec<f64> = Vec::new();
        const WINDOW_SIZE: usize = 5;

        while current_size <= max_size && !stopped() {
            probe_num += 1;

            if let Some(stats) = probe(current_size) {
                // Calculate derivative (latency change per size doubling)
                let derivative = if let (Some(prev_lat), Some(prev_sz)) = (prev_latency, prev_size) {
                    let lat_change = stats.median_ns - prev_lat;
                    let size_ratio = (current_size as f64) / (prev_sz as f64);
                    if size_ratio > 1.0 { lat_change / size_ratio.ln() } else { 0.0 }
                } else {
                    0.0
                };

                // Calculate ratio to previous
                let ratio = if let Some(prev) = prev_latency { stats.median_ns / prev } else { 1.0 };

                // Update sliding window for stability calculation
                recent_latencies.push(stats.median_ns);
                if recent_latencies.len() > WINDOW_SIZE {
                    recent_latencies.remove(0);
                }

                // Calculate stability (std dev of recent latencies in window)
                let window_stddev = if recent_latencies.len() >= 3 {
                    let mean = recent_latencies.iter().sum::<f64>() / recent_latencies.len() as f64;
                    let variance = recent_latencies.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / recent_latencies.len() as f64;
                    variance.sqrt()
                } else {
                    0.0
                };

                // Determine if this is a stable region (low CV in window)
                let is_stable = if recent_latencies.len() >= 3 {
                    let mean = recent_latencies.iter().sum::<f64>() / recent_latencies.len() as f64;
                    window_stddev / mean < 0.15 // CV < 15%
                } else {
                    true
                };

                let stable_marker = if is_stable { "✓" } else { "~" };

                log::info!(target: SHOWN_TARGET,
                    "  {:>4}: {:>10} {:>6.1}ns {:>6.2}x {:>+7.1} {:>5.2}x {:>6.2} {:>6}",
                    probe_num,
                    format_size(current_size),
                    stats.median_ns,
                    stats.spread_ratio,
                    derivative,
                    ratio,
                    window_stddev,
                    stable_marker
                );

                prev_latency = Some(stats.median_ns);
                prev_size = Some(current_size);

                results.push(SweepProbe { size: current_size, median_ns: stats.median_ns, spread: stats.spread_ratio, window_stddev });
            }

            // Next size - ensure we make progress
            let next_size = ((current_size as f64) * step_factor) as usize;
            current_size = next_size.max(current_size + 4096);
        }

        log::info!(target: SHOWN_TARGET, "Sweep complete: {} probes collected", results.len());
        results
    }

    /// A gradual edge past `after` (the L2's), for anchor A when `find_tipping_points` finds no L3 edge (TODO 22): the first probe reading
    /// at least 1.4x the one three before it (a size 1.52x as big, the sweep stepping 1.15x), the edge at that earlier probe, the last
    /// before the climb; three times `after` or more, as `find_tipping_points` keeps its edges apart. Without it A took CPUID's L3 and the
    /// finer sweep's largest steady size there, in DRAM. On the m8i.metal-48xl (SNC3, 2026-11-05) on 4 KiB pages the L3's latency climbed
    /// from 26 to 45 ns over 5-138 MB (TLB misses), and the step at its edge read 1.31x over the three before; 475 MB at 105 ns was saved,
    /// and this takes 119.9 MB. On another node's memory (99f4337) it takes 137.8 MB. Its 1 GiB pages' edge, sharp, is the usual test's.
    fn find_gradual_edge(sweep: &[SweepProbe], after: usize) -> Option<(usize, f64, f64)> {
        (3..sweep.len())
            .find(|&i| sweep[i - 3].size >= 3 * after && sweep[i].median_ns >= 1.4 * sweep[i - 3].median_ns)
            .map(|i| (sweep[i - 3].size, sweep[i - 3].median_ns, sweep[i].median_ns))
    }

    /// Find tipping points where latency jumps significantly
    /// A true tipping point is where we transition from one stable latency region to another
    /// We expect 3-4 real transitions: L1→L2, L2→L3, L3→DRAM, (optionally DRAM→HighTLB)
    fn find_tipping_points(&self, sweep: &[SweepProbe]) -> Vec<(usize, f64, f64)> {
        let mut tipping_points = Vec::new();

        if sweep.len() < 6 {
            return tipping_points;
        }

        // Calculate latency derivative (rate of change) for each point
        let mut derivatives: Vec<f64> = vec![0.0];
        for i in 1..sweep.len() {
            let lat_change = sweep[i].median_ns - sweep[i - 1].median_ns;
            let size_ratio = (sweep[i].size as f64) / (sweep[i - 1].size as f64);
            let derivative = lat_change / size_ratio.ln();
            derivatives.push(derivative);
        }

        // Find transitions with stricter criteria
        for i in 4..sweep.len() {
            let prev_lat_avg = (sweep[i - 3].median_ns + sweep[i - 2].median_ns + sweep[i - 1].median_ns) / 3.0;
            let curr_lat = sweep[i].median_ns;
            let latency_ratio = curr_lat / prev_lat_avg;

            // Only report significant transitions (>1.4x latency jump)
            if latency_ratio < 1.4 {
                continue;
            }

            let transition_size = sweep[i - 1].size;

            // Avoid duplicate tipping points (within 3x size of each other)
            let dominated = tipping_points.iter().any(|(s, _, _)| {
                let ratio = (*s as f64) / (transition_size as f64);
                ratio > 0.33 && ratio < 3.0
            });

            if !dominated {
                tipping_points.push((transition_size, prev_lat_avg, curr_lat));
            }
        }

        tipping_points
    }

    /// Find the largest stable probe within a size range
    /// Stable = spread < max_spread AND window_stddev < max_winsd
    fn find_largest_stable_in_range(
        &self, sweep: &[SweepProbe], min_size: usize, max_size: usize, max_spread: f64, max_winsd: f64,
    ) -> Option<SweepProbe> {
        sweep
            .iter()
            .filter(|p| p.size >= min_size && p.size < max_size && p.spread < max_spread && p.window_stddev < max_winsd)
            .max_by_key(|p| p.size)
            .cloned()
    }

    /// Find the largest stable size (for FullDRAM)
    /// Stable = spread < 2.0
    fn find_largest_stable(&self, sweep: &[SweepProbe], min_size: usize) -> Option<SweepProbe> {
        sweep.iter().filter(|p| p.size >= min_size && p.spread < 2.0).max_by_key(|p| p.size).cloned()
    }

    /// Writes calibration results to `out` in a formatted table
    pub fn display_results(results: &CalibrationResults, cache_info: &CacheInfo, out: &mut dyn std::io::Write) -> std::io::Result<()> {
        writeln!(out, "\n📊 Cache Calibration Results")?;
        writeln!(out, "═══════════════════════════════════════════════════════════════════════════════")?;
        writeln!(
            out,
            "{:<10} {:>12} {:>12} {:>8} {:>12} {:>10} {:>12}",
            "Tier", "CPU Size", "Calibrated", "%", "Latency", "Spread", "Status"
        )?;
        writeln!(out, "───────────────────────────────────────────────────────────────────────────────")?;

        for tier in CacheTier::all_tiers() {
            if let Some(result) = results.tiers.get(tier) {
                // Get the CPU-detected tier size
                let cpu_size = match tier {
                    CacheTier::L1 => cache_info.per_core_l1d,
                    CacheTier::L2 => cache_info.per_core_l2,
                    CacheTier::L3 => cache_info.l3_cache,
                    CacheTier::Dram => cache_info.l3_cache * 2,
                    CacheTier::FullDram => cache_info.l3_cache * 4,
                };

                let cpu_size_str = format_size(cpu_size);
                let calibrated_str = format_size(result.optimal_size);
                let percentage = if cpu_size > 0 { (result.optimal_size as f64 / cpu_size as f64) * 100.0 } else { 0.0 };

                let status = if result.fallback_used {
                    "⚠️ Fallback"
                } else if result.converged {
                    "✅ Stable"
                } else {
                    "⚡ OK"
                };

                writeln!(
                    out,
                    "{:<10} {:>12} {:>12} {:>7.0}% {:>10.1}ns {:>10.2}x {:>12}",
                    tier.name(),
                    cpu_size_str,
                    calibrated_str,
                    percentage,
                    result.median_latency_ns,
                    result.spread_ratio,
                    status
                )?;
            }
        }

        writeln!(out, "───────────────────────────────────────────────────────────────────────────────")?;
        // Its time isn't saved: a stored one's (`--show-calibration`) has when it was measured, and on what pages, in its place
        if results.total_time_ms > 0 {
            writeln!(out, "Total calibration time: {}ms", results.total_time_ms)?;
        } else {
            writeln!(out, "Measured {}", results.when_and_pages())?;
        }
        // TODO 22: anchors B and C, and the L3 a group splits, which a run's L3 tests take their share of
        if let Some(line) = results.two_threads_a_core(&format_size) {
            writeln!(out, "{line}")?;
        }
        if let Some(shared) = &results.shared {
            let size = |bytes: Option<usize>| bytes.map_or("none steady".to_string(), format_size);
            // C ran: A sized it (an L1, an L2 and an L3) and CPUs share an L3, which one CPU alone doesn't
            let c_ran =
                shared.l3_sharers > 1 && [CacheTier::L1, CacheTier::L2, CacheTier::L3].iter().all(|tier| results.tiers.contains_key(tier));
            match shared.l3_loaded {
                Some(_) => writeln!(
                    out,
                    "Every CPU at once ({} sharing each L3): L3 {} a thread at {:.1}ns",
                    shared.l3_sharers,
                    size(shared.l3_loaded),
                    shared.l3_loaded_ns
                )?,
                None if c_ran => writeln!(out, "Every CPU at once ({} sharing each L3): no L3 edge found", shared.l3_sharers)?,
                None => {}
            }
        }
        if let Some(line) = results.l3_a_thread(&format_size) {
            writeln!(out, "{line}")?;
        }

        // Check if DRAM and FullDRAM have similar latencies
        let dram_lat = results.tiers.get(&CacheTier::Dram).map(|r| r.median_latency_ns);
        let full_lat = results.tiers.get(&CacheTier::FullDram).map(|r| r.median_latency_ns);
        let is_large_pages = results.page_size != "regular" && results.page_size != "4kb";
        if let (Some(d), Some(f)) = (dram_lat, full_lat) {
            let ratio = if d > 0.0 { (f - d).abs() / d } else { 0.0 };
            if ratio < 0.10 {
                if is_large_pages {
                    writeln!(out, "ℹ️  DRAM and FullDRAM show similar latency ({:.1}ns vs {:.1}ns).", d, f)?;
                    writeln!(out, "    This is expected with large pages (2MB+) which minimize TLB pressure.")?;
                    writeln!(out, "    Use maxpage=regular to test with 4KB pages for TLB comparison.")?;
                } else {
                    writeln!(out, "ℹ️  DRAM and FullDRAM show similar latency ({:.1}ns vs {:.1}ns) even with 4KB pages.", d, f)?;
                    writeln!(out, "    This may indicate the working set sizes are too similar to differentiate.")?;
                }
            }
        }
        writeln!(out, "ℹ️  Calibration used {} pages.", page_size_name(&results.page_size))?;
        if results.page_size != "regular" && results.page_size != "4kb" {
            writeln!(out, "    Use maxpage=regular with --calibrate-cache to test with 4KB pages for TLB comparison.")?;
        }
        writeln!(out)?;
        Ok(())
    }
}

/// A calibration's `page_size` as its report names it.
fn page_size_name(page_size: &str) -> &'static str {
    match page_size {
        "regular" | "4kb" => "4KB regular",
        "huge" | "1gb" => "1GB huge",
        _ => "2MB large",
    }
}

// ============================================================================
// Seal Width Calibration (TODO 99)
// ============================================================================

/// The seal calibration's timed runs of each width, after one warm-up run of each.
const SEAL_RUNS: usize = 5;
/// Its buffer is this many times the L3, so each check reads DRAM, as the seal's checks in a run do (the cache calibration's FullDRAM tier,
/// `CacheTier::FullDram`)...
const SEAL_L3_MULTIPLE: usize = 4;
/// ...and at least this, so a run on a part with a small L3 lasts long enough to time (512 MiB: ~20 ms a fill at 25,000 MiB/s).
const SEAL_MIN_BYTES: usize = 512 * 1024 * 1024;

/// How much slower than the fastest width's median a narrower width's may be, at most, and still count as the same speed
/// (`choose_seal_width`).
const SEAL_SAME_SPEED: f64 = 0.01;

/// One width in a seal calibration: its fill and check speeds at their medians, and its fill + check time over the timed runs: median,
/// fastest, upper quartile and slowest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SealWidthTiming {
    /// 128, 256 or 512
    pub bits: u32,
    /// MiB/s, at the median fill and the median check
    #[serde(serialize_with = "serialize_round_2dp", deserialize_with = "deserialize_f64_lenient")]
    pub fill_mib_s: f64,
    #[serde(serialize_with = "serialize_round_2dp", deserialize_with = "deserialize_f64_lenient")]
    pub check_mib_s: f64,
    /// Fill + check in ms: the runs' median, fastest, upper quartile (the second slowest of 5; `same_speed_bound`) and slowest
    #[serde(serialize_with = "serialize_round_3dp", deserialize_with = "deserialize_f64_lenient")]
    pub ms: f64,
    #[serde(serialize_with = "serialize_round_3dp", deserialize_with = "deserialize_f64_lenient")]
    pub ms_min: f64,
    #[serde(default, serialize_with = "serialize_round_3dp", deserialize_with = "deserialize_f64_lenient")]
    pub ms_q3: f64,
    #[serde(serialize_with = "serialize_round_3dp", deserialize_with = "deserialize_f64_lenient")]
    pub ms_max: f64,
}

impl SealWidthTiming {
    /// The timing of the width of `bits` from its timed runs: each fill's and each check's seconds, over `mib` MiB.
    fn from_runs(bits: u32, mib: f64, fills: &[f64], checks: &[f64]) -> Self {
        let totals: Vec<f64> = fills.iter().zip(checks).map(|(f, c)| (f + c) * 1e3).collect();
        SealWidthTiming {
            bits,
            fill_mib_s: mib / median(fills),
            check_mib_s: mib / median(checks),
            ms: median(&totals),
            ms_min: quantile(&totals, 0.0),
            ms_q3: quantile(&totals, 0.75),
            ms_max: quantile(&totals, 1.0),
        }
    }

    /// The slowest median a narrower width may have and run at this one's speed (`choose_seal_width`): this one's upper quartile or its
    /// median and `SEAL_SAME_SPEED`, whichever is less.
    pub fn same_speed_bound(&self) -> f64 {
        self.ms_q3.min(self.ms * (1.0 + SEAL_SAME_SPEED))
    }
}

/// The seal's width, measured on this machine (`--calibrate-seal`, `--calibrate-all`; TODO 99).
/// `seal-width=auto` runs `width_bits` on the machine it was measured on
/// (`AppConfig::valid_seal_calibration`); a width set still wins.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SealCalibration {
    pub timestamp: DateTime<Utc>,
    /// The machine it was measured on, all of what the cache calibration's check compares (`MachineFingerprint`), set when it is saved
    /// (`AppConfig::set_seal_calibration`); it is used only while that is this machine's. Its own, not the config's identity: each kind of
    /// calibration's save would otherwise pass the other's, from other hardware, off as this machine's. One saved without it (empty) is
    /// never this machine's.
    #[serde(default)]
    pub fingerprint: crate::app_config::MachineFingerprint,
    /// The pattern measured: the default, `tmr`. A `tm5` run takes the width too.
    pub pattern: String,
    pub buffer_bytes: u64,
    /// The page size its buffer got (`allocate_calibration_buffer`): the one asked for (`maxpage=`), or a smaller one where that was
    /// refused. Named as the cache calibration's `page_size` is.
    pub page_size: String,
    /// Timed runs of each width, after a warm-up run
    pub runs: u32,
    /// Each width the CPU has, the narrowest first
    pub widths: Vec<SealWidthTiming>,
    /// The width `auto` runs (`choose_seal_width`), and the fastest; they differ when a narrower width runs at the fastest's speed
    pub width_bits: u32,
    pub fastest_bits: u32,
}

impl SealCalibration {
    /// The width `auto` runs.
    pub fn width(&self) -> Option<crate::seal::SealWidth> {
        crate::seal::SealWidth::from_bits(self.width_bits)
    }
}

/// A seal calibration that chose `width_bits`, for a unit test: measured nowhere yet (`AppConfig::set_seal_calibration` stamps the
/// machine).
#[cfg(test)]
pub(crate) fn test_seal_calibration(width_bits: u32) -> SealCalibration {
    let timing = |bits, ms: f64| SealWidthTiming {
        bits,
        fill_mib_s: 20000.0,
        check_mib_s: 30000.0,
        ms,
        ms_min: ms - 1.0,
        ms_q3: ms + 0.5,
        ms_max: ms + 1.0,
    };
    SealCalibration {
        timestamp: Utc::now(),
        fingerprint: Default::default(),
        pattern: "tmr".to_string(),
        buffer_bytes: 512 << 20,
        page_size: "large".to_string(),
        runs: 5,
        widths: vec![timing(128, 40.0), timing(256, 40.5)],
        width_bits,
        fastest_bits: 128,
    }
}

/// The seal's width from each width's timing: the fastest by median fill + check, unless a narrower width runs at its speed, when it is the
/// narrowest such: the narrower draws less power. Returns the chosen width's bits and the fastest's; `None` with none.
///
/// The same speed is a median no slower than the fastest's `same_speed_bound`: the tighter of its upper quartile and 1% over its median.
/// The quartile is the fastest's own run-to-run spread without its slowest run (the second slowest of 5), so one preempted run can't widen
/// it, as it would the slowest run (40 ms [39.5..55] would pass a width at 50 ms, 25% slower); a median past it is a difference the runs
/// show. The 1% caps what noise can cost: two disturbed runs of 5 still lift the quartile, and the choice is saved for every later run.
pub fn choose_seal_width(widths: &[SealWidthTiming]) -> Option<(u32, u32)> {
    let fastest = widths.iter().min_by(|a, b| a.ms.total_cmp(&b.ms).then(a.bits.cmp(&b.bits)))?;
    let bound = fastest.same_speed_bound();
    let chosen = widths.iter().filter(|w| w.bits <= fastest.bits && w.ms <= bound).min_by_key(|w| w.bits)?;
    Some((chosen.bits, fastest.bits))
}

/// The median of `values` (the mean of the middle two for an even count); 0 for none.
fn median(values: &[f64]) -> f64 {
    quantile(values, 0.5)
}

/// The `p` quantile of `values` (0 the least, 1 the greatest), interpolated between the two nearest when it falls between them (R's and
/// numpy's default): of 5, 0.75 is the second greatest. 0 for none.
fn quantile(values: &[f64], p: f64) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let Some(last) = sorted.len().checked_sub(1) else { return 0.0 };
    let at = p.clamp(0.0, 1.0) * last as f64;
    let (below, above) = (at.floor() as usize, at.ceil() as usize);
    sorted[below] + (sorted[above] - sorted[below]) * (at - below as f64)
}

/// Measures the seal's own kernels (`SealKernel::fill` and `check`, the default pattern `tmr`) at every width this CPU has, and chooses one
/// (`choose_seal_width`).
///
/// One thread, this one, as the latency calibration runs, so no other thread's traffic biases a width (the owner, 2026-10-20). The buffer
/// is `SEAL_L3_MULTIPLE` times the L3 (at least `SEAL_MIN_BYTES`) from the cache calibration's allocation, at the page size asked for
/// (`page_size`) or a smaller one where that is refused; it records the one it got.
/// Each round fills and then checks the whole buffer at every width in turn, in an order rotated each round so drift (heat, clocks, other
/// load) falls on every width alike; the first round warms up (faults in 4 KiB pages, settles clocks) and isn't timed.
pub fn calibrate_seal(cache_info: &CacheInfo, page_size: &str) -> Result<SealCalibration, String> {
    use crate::seal::{SealKernel, SealPattern, SealWidth};
    let page = calibration_page_size(page_size);
    let bytes = (cache_info.l3_cache * SEAL_L3_MULTIPLE).max(SEAL_MIN_BYTES).next_multiple_of(page.bytes());
    let kernels: Vec<SealKernel> = [SealWidth::W128, SealWidth::W256, SealWidth::W512]
        .into_iter()
        .filter(|w| w.resolve().is_ok())
        .map(|w| SealKernel::new(SealPattern::Tmr, w))
        .collect();
    log::info!(target: SHOWN_TARGET,
        "Seal width calibration: tmr at {:?} bits, {} (asking for {} pages), {} timed runs each",
        kernels.iter().map(|k| k.width.bits()).collect::<Vec<_>>(),
        format_size(bytes),
        page_size_name(page_size_str(page)),
        SEAL_RUNS
    );

    let (buffer, obtained) = allocate_calibration_buffer(bytes, page, None)?;
    let (ptr, words) = (buffer.as_mut_ptr() as *mut u64, bytes / 8);
    // Per kernel, the seconds of each timed fill and each timed check
    let mut times: Vec<(Vec<f64>, Vec<f64>)> = vec![(Vec::new(), Vec::new()); kernels.len()];
    let mut bad = 0;
    for round in 0..=SEAL_RUNS {
        for k in 0..kernels.len() {
            let i = (round + k) % kernels.len();
            // SAFETY: the buffer is `words` long and page aligned, and each kernel's width is one this CPU has (`resolve` above)
            let (fill, check) = unsafe {
                let start = Instant::now();
                kernels[i].fill(ptr, 0, words);
                let filled = Instant::now();
                bad += kernels[i].check(ptr, 0, words, "seal calibration's check");
                (filled - start, filled.elapsed())
            };
            if round > 0 {
                times[i].0.push(fill.as_secs_f64());
                times[i].1.push(check.as_secs_f64());
            }
        }
    }
    drop(buffer);
    if bad > 0 {
        return Err(format!(
            "the seal's check found {bad} bad words in memory it had just written: memory errors, so \
                            the timings aren't the seal's own; stabilize the memory, then calibrate"
        ));
    }

    let mib = bytes as f64 / crate::constants::MB_F64;
    let widths: Vec<SealWidthTiming> = kernels
        .iter()
        .zip(&times)
        .map(|(kernel, (fills, checks))| SealWidthTiming::from_runs(kernel.width.bits(), mib, fills, checks))
        .collect();
    let (width_bits, fastest_bits) = choose_seal_width(&widths).ok_or("no seal width to measure")?;
    Ok(SealCalibration {
        timestamp: Utc::now(),
        fingerprint: Default::default(),
        pattern: SealPattern::Tmr.name().to_string(),
        buffer_bytes: bytes as u64,
        page_size: page_size_str(obtained).to_string(),
        runs: SEAL_RUNS as u32,
        widths,
        width_bits,
        fastest_bits,
    })
}

/// Writes a seal calibration to `out`: each width's fill and check speed and fill + check time, and the width chosen, saying why when it
/// isn't the fastest.
pub fn display_seal_calibration(seal: &SealCalibration, cache_info: &CacheInfo, out: &mut dyn std::io::Write) -> std::io::Result<()> {
    let l3 = match cache_info.l3_cache {
        0 => String::new(),
        l3 => format!(", {:.0} × L3", seal.buffer_bytes as f64 / l3 as f64),
    };
    writeln!(
        out,
        "\n📊 Seal Width Calibration ({}, 1 thread, {}{l3}, {} pages; median of {} runs after a warm-up)",
        seal.pattern,
        format_size(seal.buffer_bytes as usize),
        page_size_name(&seal.page_size),
        seal.runs
    )?;
    writeln!(out, "═══════════════════════════════════════════════════════════════════════════════")?;
    writeln!(out, "{:<8} {:>13} {:>13} {:>28}", "Width", "Fill", "Check", "Fill + check")?;
    writeln!(out, "───────────────────────────────────────────────────────────────────────────────")?;
    for w in &seal.widths {
        writeln!(out, "{:<8} {:>7.0} MiB/s {:>7.0} MiB/s {:>28}", w.bits, w.fill_mib_s, w.check_mib_s, seal_time(w))?;
    }
    writeln!(out, "───────────────────────────────────────────────────────────────────────────────")?;
    match seal.widths.iter().find(|w| w.bits == seal.fastest_bits) {
        Some(fastest) if seal.fastest_bits != seal.width_bits => writeln!(
            out,
            "Seal width: {}: the fastest is {} ({}), but {}'s median is within {:.1} ms (the tighter of {}'s upper quartile \
             and its median + {:.0}%), so the narrower (less power, same speed)",
            seal.width_bits,
            fastest.bits,
            seal_time(fastest),
            seal.width_bits,
            fastest.same_speed_bound(),
            fastest.bits,
            SEAL_SAME_SPEED * 100.0
        )?,
        _ => writeln!(out, "Seal width: {}, the fastest", seal.width_bits)?,
    }
    writeln!(out)?;
    Ok(())
}

/// A width's fill + check time as the seal calibration's report gives it: median [min..max].
fn seal_time(w: &SealWidthTiming) -> String {
    format!("{:.1} ms [{:.1}..{:.1}]", w.ms, w.ms_min, w.ms_max)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cache_tier_all_tiers() {
        let tiers = CacheTier::all_tiers();
        assert_eq!(tiers.len(), 5);
        assert_eq!(tiers[0], CacheTier::L1);
        assert_eq!(tiers[4], CacheTier::FullDram);
    }

    #[test]
    fn test_online_stats_welford() {
        let mut stats = OnlineStats::new();

        // Add samples: 1, 2, 3, 4, 5
        for i in 1..=5 {
            stats.add_sample(i as f64);
        }

        assert_eq!(stats.count(), 5);
        assert!((stats.variance() - 2.5).abs() < 0.001); // Sample variance of 1,2,3,4,5
        assert!((stats.std_dev() - 1.5811).abs() < 0.01);
    }

    #[test]
    fn test_online_stats_single_sample() {
        let mut stats = OnlineStats::new();
        stats.add_sample(42.0);

        assert_eq!(stats.count(), 1);
        assert_eq!(stats.variance(), 0.0); // Variance undefined for n=1, returns 0
    }

    #[test]
    fn test_statistics_analyzer_basic() {
        let samples = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        let stats = StatisticsAnalyzer::analyze(&samples);

        assert!((stats.median_ns - 3.0).abs() < 0.001);
    }

    #[test]
    fn test_statistics_analyzer_percentiles() {
        // 100 samples from 1 to 100
        let samples: Vec<f64> = (1..=100).map(|x| x as f64).collect();
        let stats = StatisticsAnalyzer::analyze(&samples);

        // P50 should be around 50.5
        assert!((stats.median_ns - 50.5).abs() < 0.1);
        // P5 ≈ 5.95 and P95 ≈ 95.05 (linear interpolation), seen through their ratio
        assert!((stats.spread_ratio - 95.05 / 5.95).abs() < 0.1);
    }

    #[test]
    fn test_statistics_analyzer_spread_ratio() {
        // Uniform distribution: spread ratio should be close to P95/P5
        let samples: Vec<f64> = (1..=100).map(|x| x as f64).collect();
        let stats = StatisticsAnalyzer::analyze(&samples);

        // P95/P5 ≈ 95/5 ≈ 19 (but with interpolation it's different)
        assert!(stats.spread_ratio > 10.0);
        assert!(stats.spread_ratio < 25.0);
    }

    #[test]
    fn test_statistics_analyzer_bimodal_detection() {
        // Create bimodal distribution: half at 10, half at 100
        let mut samples = vec![10.0; 50];
        samples.extend(vec![100.0; 50]);

        let stats = StatisticsAnalyzer::analyze(&samples);

        // Bimodal should fail the fine sweep's stability cut (spread < 1.5)
        assert!(stats.spread_ratio > 1.5);
    }

    #[test]
    fn test_statistics_analyzer_stable_detection() {
        // Create stable distribution: all values close to 50
        let samples: Vec<f64> = (48..=52).map(|x| x as f64).collect();
        let stats = StatisticsAnalyzer::analyze(&samples);

        // Stable should pass the fine sweep's stability cut (spread < 1.5)
        assert!(stats.spread_ratio < 1.5);
    }

    #[test]
    fn test_statistics_analyzer_empty() {
        let samples: Vec<f64> = vec![];
        let stats = StatisticsAnalyzer::analyze(&samples);

        assert_eq!(stats.median_ns, 0.0);
        assert_eq!(stats.spread_ratio, 0.0);
    }

    /// A width's timing from its timed runs' fill + check, in ms (half each).
    fn runs(bits: u32, ms: &[f64]) -> SealWidthTiming {
        let halves: Vec<f64> = ms.iter().map(|ms| ms / 2e3).collect();
        SealWidthTiming::from_runs(bits, 1.0, &halves, &halves)
    }

    /// Five runs about `ms`, 0.5% apart.
    fn steady(bits: u32, ms: f64) -> SealWidthTiming {
        runs(bits, &[ms * 0.99, ms * 0.995, ms, ms * 1.005, ms * 1.01])
    }

    /// TODO 99: a width's timing is its runs' median, fastest, upper quartile (of 5, the second slowest) and slowest, and its fill and
    /// check speeds at their medians.
    #[test]
    fn a_seal_width_timing_is_its_runs_median_quartile_and_range() {
        let t = SealWidthTiming::from_runs(256, 1000.0, &[0.020, 0.030, 0.025, 0.021, 0.022], &[0.010, 0.010, 0.020, 0.011, 0.012]);
        let close = |a: f64, b: f64| (a - b).abs() < 1e-9;
        assert_eq!(t.bits, 256);
        assert!(close(t.fill_mib_s, 1000.0 / 0.022) && close(t.check_mib_s, 1000.0 / 0.011), "{t:?}");
        // Fill + check: 30, 40, 45, 32, 34 ms
        assert!(close(t.ms, 34.0) && close(t.ms_min, 30.0) && close(t.ms_q3, 40.0) && close(t.ms_max, 45.0), "{t:?}");
    }

    /// TODO 99: the seal's width is the fastest by median fill + check, unless a narrower width runs at its speed: a median within the
    /// tighter of the fastest's upper quartile and its median + 1%, when it is the narrowest such. One preempted run of the fastest's
    /// doesn't widen that (the reviewer's case); two disturbed runs leave the 1% to bind; a narrower width within 1% that the fastest's
    /// tight runs show slower isn't the same speed. A wider width at a narrower fastest's speed changes nothing.
    #[test]
    fn the_seal_width_is_the_fastest_or_the_narrowest_at_its_speed() {
        let choose = |widths: &[SealWidthTiming]| choose_seal_width(widths);
        // Apart: the fastest
        assert_eq!(choose(&[steady(128, 50.0), steady(256, 45.0), steady(512, 40.0)]), Some((512, 512)));
        assert_eq!(choose(&[steady(128, 50.0), steady(256, 40.0), steady(512, 45.0)]), Some((256, 256)));

        // The reviewer's: 512 at 40 ms [39.5..55], one run preempted, and 256 at 50 ms: 512
        let preempted = runs(512, &[39.5, 39.8, 40.0, 40.3, 55.0]);
        assert_eq!(seal_time(&preempted), "40.0 ms [39.5..55.0]");
        assert!(preempted.ms_max > 50.0, "the slowest run would pass 256");
        assert_eq!(choose(&[steady(256, 50.0), preempted.clone()]), Some((512, 512)));
        // ...where a width within its quartile, 40.3 ms, is the same speed
        assert_eq!(choose(&[steady(256, 40.25), preempted]), Some((256, 512)));

        // Within both: the narrowest of those within
        let spread = runs(512, &[39.6, 39.8, 40.0, 40.2, 40.4]);
        assert_eq!(choose(&[steady(128, 41.0), steady(256, 40.1), spread.clone()]), Some((256, 512)));
        assert_eq!(choose(&[steady(128, 40.15), steady(256, 40.1), spread.clone()]), Some((128, 512)));
        // Exactly the bound is within
        assert_eq!(spread.same_speed_bound(), spread.ms_q3);
        let at_bound = SealWidthTiming { bits: 256, ms: spread.ms_q3, ..steady(256, 40.0) };
        assert_eq!(choose(&[at_bound, spread]), Some((256, 512)));

        // Two runs disturbed: the quartile is 44 ms, so the 1% binds, at 40.4 ms
        let disturbed = runs(512, &[39.5, 39.8, 40.0, 44.0, 55.0]);
        assert_eq!(choose(&[steady(256, 41.0), disturbed.clone()]), Some((512, 512)));
        assert_eq!(choose(&[steady(256, 40.3), disturbed]), Some((256, 512)));

        // Tight runs: the quartile binds, at 40.1 ms, so 0.75% slower is slower
        let tight = runs(512, &[40.0, 40.0, 40.0, 40.1, 40.2]);
        assert_eq!(choose(&[steady(256, 40.3), tight.clone()]), Some((512, 512)));
        assert_eq!(choose(&[steady(256, 40.05), tight]), Some((256, 512)));

        // A wider width at a narrower fastest's speed: the narrower, the fastest
        assert_eq!(choose(&[steady(256, 40.0), steady(512, 40.1)]), Some((256, 256)));
        // Equal medians: the narrower is the fastest
        assert_eq!(choose(&[runs(256, &[40.0; 5]), runs(512, &[40.0; 5])]), Some((256, 256)));
        // One width, and none
        assert_eq!(choose(&[steady(256, 40.0)]), Some((256, 256)));
        assert_eq!(choose(&[]), None);
    }

    /// TODO 99: a calibration's buffer asks for its page size strictly, each large-page request with its page's alignment, and at each
    /// refusal asks for the next smaller size, down to 4 KiB, never a larger one than given; it returns the size it got, which the seal
    /// calibration records and its report names.
    #[test]
    fn a_calibration_buffer_steps_down_and_says_what_it_got() {
        use PageSizeLevel::{Huge, Large, Regular};

        use crate::memory::backend::AllocError;
        const SIZE: usize = 64 << 20;
        // The page size got, and the requests made: each its page size and alignment
        let allocate = |page, granted: &[PageSizeLevel]| {
            let mut asked = Vec::new();
            let got = allocate_stepping_down(SIZE, page, None, false, |config| {
                let PageSizePreference::Require(level) = config.page_size else { panic!("not strict: {:?}", config.page_size) };
                assert_eq!((config.size, config.numa_node, config.numa_strict), (SIZE, None, false));
                asked.push((level, config.alignment));
                if granted.contains(&level) { Ok(()) } else { Err(AllocError::other("refused")) }
            });
            (got.map(|((), level)| level), asked)
        };
        let (huge, large, regular) = ((Huge, Some(1 << 30)), (Large, Some(2 << 20)), (Regular, None));
        assert_eq!(allocate(Huge, &[Huge, Large, Regular]), (Ok(Huge), vec![huge]));
        assert_eq!(allocate(Huge, &[Large, Regular]), (Ok(Large), vec![huge, large]));
        assert_eq!(allocate(Huge, &[Regular]), (Ok(Regular), vec![huge, large, regular]));
        assert_eq!(allocate(Large, &[Huge, Large, Regular]), (Ok(Large), vec![large]));
        assert_eq!(allocate(Large, &[Huge, Regular]), (Ok(Regular), vec![large, regular]));
        assert_eq!(allocate(Regular, &[Huge, Large, Regular]), (Ok(Regular), vec![regular]));
        let (refused, asked) = allocate(Huge, &[]);
        assert_eq!(asked, vec![huge, large, regular]);
        assert!(refused.unwrap_err().contains("1GB huge pages (refused); 2MB large pages (refused); 4KB regular pages (refused)"));

        // Named as the report names them, and as maxpage= asks for them (2 MiB by default)
        assert_eq!([Regular, Large, Huge].map(|p| page_size_name(page_size_str(p))), ["4KB regular", "2MB large", "1GB huge"]);
        for (maxpage, page) in
            [("regular", Regular), ("4KB", Regular), ("large", Large), ("2mb", Large), ("huge", Huge), ("1gb", Huge), ("unknown", Large)]
        {
            assert_eq!(calibration_page_size(maxpage), page, "{maxpage}");
        }
        for page in [Regular, Large, Huge] {
            assert_eq!(calibration_page_size(page_size_str(page)), page);
        }
    }

    #[test]
    fn quantiles_interpolate_between_the_nearest_two() {
        let five = [55.0, 39.5, 40.3, 39.8, 40.0];
        assert_eq!([0.0, 0.5, 0.75, 1.0].map(|p| quantile(&five, p)), [39.5, 40.0, 40.3, 55.0]);
        assert_eq!(quantile(&[1.0, 2.0, 3.0, 4.0], 0.75), 3.25);
        assert_eq!(quantile(&[7.0], 0.75), 7.0);
        assert_eq!(quantile(&[], 0.75), 0.0);
    }

    /// TODO 116: a recheck holds when every probe reads as it should, inside the L3 at the stored size (within 1.5x its latency) and
    /// past it at one and a half times the size (over 1.25x); one that reads otherwise says so, and the verdict is to measure again
    #[test]
    fn a_recheck_says_each_probe_and_whether_it_holds() {
        let probe = |size, ns, should_be_inside| RecheckProbe {
            what: "one thread",
            size,
            ns,
            should_be_inside,
            limit_ns: if should_be_inside { 52.35 } else { 43.625 },
        };
        let mut recheck =
            Recheck { probes: vec![probe(64 << 20, 43.6, true), probe(96 << 20, 68.3, false)], l3_ns: 34.9, held: true, time_ms: 7200 };
        assert!(recheck.probes.iter().all(RecheckProbe::holds), "43.6 ns at the stored size is inside, with the margin");
        assert_eq!(
            recheck.text(),
            "🔁 Cache calibration rechecked in 7.2 s: it holds (its L3 latency 34.9 ns)\n   one thread: 64.0 MB at 43.6 ns, inside (≤ 52.4 ns) ✓\n   \
             one thread: 96.0 MB at 68.3 ns, past it (> 43.6 ns) ✓\n"
        );
        recheck.probes[1] = probe(96 << 20, 40.0, false);
        recheck.held = recheck.probes.iter().all(RecheckProbe::holds);
        assert!(
            !recheck.held
                && recheck.text().contains("it has drifted: measured again")
                && recheck.text().contains("(> 43.6 ns) ⚠️ it isn't")
        );
        assert_eq!(
            Recheck { probes: vec![], l3_ns: 34.9, held: true, time_ms: 0 }.text(),
            "🔁 Cache calibration not rechecked: none of its probes fits a thread's memory on its own node (a larger memory= gives them \
             room, unless the node is out of large pages and the rest came from another node); the stored one is kept\n",
            "no probe is no recheck, not one that holds"
        );
    }

    /// TODO 116: a recheck on a thread's memory too small for every probe, 4 MiB against a stored 8 MB L3 (A's at 8 and 12 MB) with
    /// no C, runs none, touches nothing, and keeps the stored one
    #[test]
    fn a_recheck_with_no_room_for_its_probes_runs_none() {
        let mut stored = CalibrationResults::new(CpuSignature::default());
        stored.add_tier_result(TierCalibrationResult {
            tier: CacheTier::L3,
            optimal_size: 8 << 20,
            median_latency_ns: 34.9,
            spread_ratio: 1.1,
            probes_attempted: 1,
            converged: true,
            fallback_used: false,
        });
        let test = CalibrationTest::with_config(CacheInfo::default(), CalibrationConfig::default());
        let mut memory = vec![std::simd::u64x8::splat(0); (4 << 20) / 64];
        // SAFETY: the test's own memory, 64-byte aligned, which no probe reaches
        let recheck = unsafe { test.recheck_on(&stored, memory.as_mut_ptr() as *mut u64, 4 << 20) }.unwrap();
        assert!(recheck.probes.is_empty() && recheck.held, "{recheck:?}");
        assert!(recheck.text().contains("not rechecked"));
    }

    /// TODO 22: a run's calibration refuses a thread's memory under twice CPUID's L3, before it probes: A's sweep, cut short inside
    /// the L3, would save about CPUID's size as measured. CacheInfo's default L3 is 8 MB.
    #[test]
    fn a_calibration_refuses_memory_under_twice_the_l3() {
        let test = CalibrationTest::with_config(CacheInfo::default(), CalibrationConfig::default());
        let mut memory = vec![std::simd::u64x8::splat(0); (4 << 20) / 64];
        // SAFETY: the test's own memory, 64-byte aligned, refused before anything probes it
        let e = unsafe { test.run_on(memory.as_mut_ptr() as *mut u64, 4 << 20, PageSizeLevel::Regular) }.unwrap_err();
        assert!(e.contains("4.0 MB") && e.contains("16.0 MB") && e.contains("nothing saved"), "{e}");
    }

    /// The stop (a first Ctrl+C, the window's Stop) ends a sweep and a finer one at the next probe, and a calibration it reaches before
    /// its first probe is an error, so nothing is saved. The flag is the process's: nextest's one process a test keeps it to this one.
    #[test]
    fn a_stop_ends_the_calibrations_sweeps() {
        let test = CalibrationTest::with_config(CacheInfo::default(), CalibrationConfig::default());
        let calls = std::cell::Cell::new(0);
        let mut probe = |_: usize| {
            calls.set(calls.get() + 1);
            if calls.get() == 3 {
                crate::runner::request_stop();
            }
            Some(ProbeStatistics { median_ns: 1.0, spread_ratio: 1.0 })
        };
        let sweep = test.run_sweep(&mut probe, 4096, 1 << 30, "stopped");
        crate::runner::clear_stop();
        assert_eq!(sweep.len(), 3);
        calls.set(0);
        let fine = test.run_fine_sweep(&mut probe, 4096, 1 << 30, 1.05, 3, "stopped");
        crate::runner::clear_stop();
        assert_eq!(fine.len(), 1, "the size whose third repeat stopped it");

        // A short probe time, should the flag be cleared under it (`cargo test` runs sessions beside it, which clear it)
        let short = CalibrationConfig { max_probe_time_ms: 1, ..CalibrationConfig::default() };
        let test = CalibrationTest::with_config(CacheInfo::default(), short);
        let mut memory = vec![std::simd::u64x8::splat(0); (16 << 20) / 64];
        crate::runner::request_stop();
        // SAFETY: the test's own memory, 64-byte aligned
        let stopped = unsafe { test.run_on(memory.as_mut_ptr() as *mut u64, 16 << 20, PageSizeLevel::Regular) };
        crate::runner::clear_stop();
        assert_eq!(stopped.unwrap_err(), STOPPED);
    }

    /// The pointer chase is one cycle through every slot, back at the first after `len` steps, laid in place
    #[test]
    fn a_pointer_chase_is_one_cycle_through_every_slot() {
        for len in [1, 2, 3, 4103] {
            let mut memory = vec![0u64; len];
            let base = memory.as_mut_ptr();
            // SAFETY: the test's own `len` u64s
            unsafe { ProbeEngine::setup_pointer_chase(base, len, 3) };
            let mut seen = vec![false; len];
            let mut at = base as usize;
            for _ in 0..len {
                let i = (at - base as usize) / 8;
                assert!(!seen[i], "slot {i} of {len} twice");
                seen[i] = true;
                at = memory[i] as usize;
            }
            assert_eq!(at, base as usize, "back at the first after {len}");
        }
    }

    /// TODO 22: a gradual L3 edge, from two sweeps of the m8i.metal-48xl (2026-11-05) past its L2 edge at 1.8 MB, each probe's size (MB)
    /// and median (ns). On 4 KiB pages the edge is at 119.9 MB, the last before the climb to 1.4x; on another node's memory (99f4337's
    /// recheck), 137.8 MB. A plateau with no climb has none, and nothing inside three times the L2's edge counts.
    #[test]
    fn a_gradual_l3_edge_is_where_the_climb_starts() {
        let sweep = |points: &[(f64, f64)]| -> Vec<SweepProbe> {
            points
                .iter()
                .map(|&(mb, ns)| SweepProbe { size: (mb * 1048576.0) as usize, median_ns: ns, spread: 1.05, window_stddev: 0.0 })
                .collect()
        };
        #[rustfmt::skip]
        let pages_4k = sweep(&[
            (1.8, 7.1), (2.1, 9.2), (2.4, 12.0), (2.8, 15.7), (3.2, 18.6), (3.6, 21.0), (4.2, 23.0), (4.8, 24.6), (5.5, 26.1), (6.4, 27.2),
            (7.3, 28.8), (8.4, 30.2), (9.7, 31.7), (11.1, 32.8), (12.8, 33.4), (14.7, 34.8), (16.9, 34.7), (19.5, 36.3), (22.4, 37.0),
            (25.8, 37.7), (29.6, 38.2), (34.1, 38.6), (39.2, 39.0), (45.1, 39.4), (51.8, 40.0), (59.6, 39.6), (68.5, 40.6), (78.8, 41.1),
            (90.6, 41.8), (104.2, 43.0), (119.9, 45.3), (137.8, 43.9), (158.5, 57.6), (182.3, 66.3), (209.6, 76.6), (241.1, 87.3),
            (277.2, 98.2), (318.8, 107.6), (366.7, 103.4), (421.6, 125.0), (484.9, 126.9),
        ]);
        #[rustfmt::skip]
        let remote = sweep(&[
            (1.8, 4.5), (2.1, 6.8), (2.4, 15.1), (2.8, 22.1), (3.2, 27.2), (3.6, 31.7), (4.2, 35.3), (4.8, 38.2), (5.5, 41.0), (6.4, 43.2),
            (7.3, 45.3), (8.4, 46.7), (9.7, 48.2), (11.1, 49.6), (12.8, 50.7), (14.7, 51.6), (16.9, 52.4), (19.5, 53.0), (22.4, 53.8),
            (25.8, 54.2), (29.6, 54.7), (34.1, 55.1), (39.2, 55.5), (45.1, 56.1), (51.8, 56.0), (59.6, 55.8), (68.5, 56.6), (78.8, 56.4),
            (90.6, 56.3), (104.2, 56.6), (119.9, 57.2), (137.8, 57.6), (158.5, 64.0), (182.3, 74.1), (209.6, 87.4), (241.1, 99.3),
        ]);
        let l2_edge = (1.8 * 1048576.0) as usize;
        let edge = |points: &[SweepProbe]| CalibrationTest::find_gradual_edge(points, l2_edge).map(|(size, ..)| size as f64 / 1048576.0);
        assert_eq!(edge(&pages_4k).map(|mb| (mb * 10.0).round() / 10.0), Some(119.9));
        assert_eq!(edge(&remote).map(|mb| (mb * 10.0).round() / 10.0), Some(137.8));
        assert_eq!(edge(&pages_4k[..30]), None, "the L3's own climb, 26 to 43 ns, is no edge");
        assert_eq!(edge(&remote[..8]), None, "the L2 to L3 ramp is inside three times the L2's edge");
    }

    /// TODO 22: the curve check measures every count up to 8 cores a group, and samples past that, the whole group last
    #[test]
    fn the_curve_check_samples_a_large_group() {
        assert_eq!(curve_counts(6), [1, 2, 3, 4, 5, 6]);
        assert_eq!(curve_counts(8), (1..=8).collect::<Vec<_>>());
        assert_eq!(curve_counts(48), [1, 2, 3, 4, 6, 8, 12, 16, 24, 32, 48]);
        assert_eq!(curve_counts(12), [1, 2, 3, 4, 6, 8, 12]);
    }

    /// TODO 22: the calibration's CPUs. On the 8600G (one L3, SMT) A takes the top-ranked core's first CPU and B that core's pair, C all
    /// 12 sharing one L3; on the m8a.4xlarge (two L3 groups of 8, no SMT) there is no pair and 8 share each L3.
    #[test]
    fn the_calibration_takes_the_top_core_its_pair_and_every_cpu() {
        use crate::cpu_selection::synthetic;
        let r8600g = CalibrationCpus::of(&synthetic(&[3, 2, 1, 4, 0, 4], 64, 64, true));
        assert_eq!((r8600g.single, r8600g.pair, r8600g.all.len(), r8600g.per_l3), (6, Some([6, 7]), 12, 12));
        let m8a = CalibrationCpus::of(&synthetic(&[0; 16], 8, 64, false));
        assert_eq!((m8a.single, m8a.pair, &m8a.all, m8a.per_l3), (0, None, &(0..16).collect::<Vec<_>>(), 8));
        assert_eq!((m8a.node, m8a.nodes, m8a.every()), (0, 1, "Every CPU".to_string()));
    }

    /// TODO 22: on more than one NUMA node, B and C take A's node's CPUs alone. The m8i.metal-48xl (SNC3): 96 cores, 32 a node and an L3
    /// group, SMT: A on CPU 0, C on node 0's 64 CPUs, 64 sharing its L3, not all 192 in it. The buffer is asked for on the node,
    /// strictly.
    #[test]
    fn the_calibration_takes_one_numa_nodes_cpus() {
        use crate::cpu_selection::synthetic;
        let snc3 = CalibrationCpus::of(&synthetic(&[0; 96], 32, 32, true));
        assert_eq!((snc3.single, snc3.node, snc3.nodes, snc3.pair), (0, 0, 3, Some([0, 1])));
        assert_eq!((&snc3.all, snc3.per_l3), (&(0..64).collect::<Vec<_>>(), 64));
        assert_eq!(snc3.every(), "Every CPU of NUMA node 0");
        // With node 0 left out (not the machine's), all of it moves to the node A is on
        let nodes_1_2: Vec<_> = synthetic(&[0; 96], 32, 32, true).into_iter().filter(|core| core.numa_node != 0).collect();
        let node1 = CalibrationCpus::of(&nodes_1_2);
        assert_eq!((node1.single, node1.node, node1.nodes, node1.pair), (64, 1, 2, Some([64, 65])));
        assert_eq!((node1.all, node1.per_l3), ((64..128).collect::<Vec<_>>(), 64));

        // Its buffer on the node, strictly; with no large page there, 4 KiB on it, never another node's large pages, whose latencies are
        // a remote node's, unless `elsewhere` (the node hasn't the size free: a Threadripper 2990WX's in NUMA mode, a vNUMA VM's, whose
        // 4 KiB pages are another node's anyway), when the same sizes from any node come first. The page size got, and each request's
        // (page, node, strict).
        use PageSizeLevel::{Huge, Large, Regular};
        let (on_node, anywhere, regular) =
            ([(Huge, Some(1), true), (Large, Some(1), true)], [(Huge, None, false), (Large, None, false)], [(Regular, Some(1), true)]);
        let allocate = |page, elsewhere, granted: fn(PageSizeLevel, Option<u32>) -> bool| {
            let mut asked = Vec::new();
            let got = allocate_stepping_down(64 << 20, page, Some(1), elsewhere, |config| {
                let PageSizePreference::Require(level) = config.page_size else { panic!("not strict: {:?}", config.page_size) };
                asked.push((level, config.numa_node, config.numa_strict));
                if granted(level, config.numa_node) { Ok(()) } else { Err(crate::memory::backend::AllocError::other("refused")) }
            });
            (got.map(|((), level)| level), asked)
        };
        for elsewhere in [false, true] {
            assert_eq!(allocate(Large, elsewhere, |_, _| true), (Ok(Large), vec![(Large, Some(1), true)]));
            assert_eq!(allocate(Huge, elsewhere, |level, _| level == Large), (Ok(Large), on_node.to_vec()));
            assert_eq!(allocate(Regular, elsewhere, |_, _| true), (Ok(Regular), regular.to_vec()));
        }
        // A node with the size free: 4 KiB on it, another node's large pages not asked for
        assert_eq!(
            allocate(Huge, false, |level, node| level == Regular || node.is_none()),
            (Ok(Regular), [&on_node[..], &regular[..]].concat())
        );
        assert_eq!(
            allocate(Large, false, |level, node| level == Regular || node.is_none()),
            (Ok(Regular), [on_node[1], regular[0]].to_vec())
        );
        let (refused, _) = allocate(Large, false, |_, _| false);
        assert!(refused.unwrap_err().contains("2MB large pages on NUMA node 1 (refused); 4KB regular pages (refused)"));
        // Elsewhere: any node's large pages before 4 KiB
        assert_eq!(allocate(Huge, true, |_, node| node.is_none()), (Ok(Huge), [&on_node[..], &anywhere[..1]].concat()));
        assert_eq!(allocate(Huge, true, |level, node| level == Large && node.is_none()), (Ok(Large), [on_node, anywhere].concat()));
        assert_eq!(allocate(Huge, true, |level, _| level == Regular), (Ok(Regular), [&on_node[..], &anywhere[..], &regular[..]].concat()));
        assert_eq!(allocate(Large, true, |level, _| level == Regular), (Ok(Regular), vec![on_node[1], anywhere[1], regular[0]]));
        let (refused, _) = allocate(Large, true, |_, _| false);
        assert!(refused.unwrap_err().contains(
            "2MB large pages on NUMA node 1 (refused); 2MB large pages from any NUMA node (refused); 4KB regular pages (refused)"
        ));
    }

    /// TODO 22: a thread's L3 reach with k sharing, from anchors A and C: one thread's 64 MiB and 9 MiB a thread with 8 sharing give the
    /// curve through both, 16.86 MiB at 4; with no C (or none found), 64 MiB over k; a C past A's, A's for any k
    #[test]
    fn the_l3_reach_is_the_curve_through_one_thread_and_every_cpu() {
        const MIB: f64 = (1 << 20) as f64;
        let tier = |tier, optimal_size| TierCalibrationResult {
            tier,
            optimal_size,
            median_latency_ns: 10.0,
            spread_ratio: 1.1,
            probes_attempted: 1,
            converged: true,
            fallback_used: false,
        };
        let mut cal = CalibrationResults::new(CpuSignature::default());
        assert_eq!(cal.l3_reach(8), None, "no L3");
        cal.add_tier_result(tier(CacheTier::L3, 64 << 20));
        assert_eq!(cal.l3_reach(8), Some(8.0 * MIB), "A alone");
        let loaded = |l3_loaded| SharedReach { l1_smt: None, l2_smt: None, l3_loaded, l3_loaded_ns: 40.0, l3_sharers: 8 };
        cal.shared = Some(loaded(Some(9 << 20)));
        let reach = |k| (cal.l3_reach(k).unwrap() / MIB * 100.0).round() / 100.0;
        assert_eq!([1, 4, 8].map(reach), [64.0, 16.86, 9.0]);
        cal.shared = Some(loaded(Some(80 << 20)));
        assert_eq!(cal.l3_reach(8), Some(64.0 * MIB), "C past A: no split");
        cal.shared = Some(loaded(None));
        assert_eq!(cal.l3_reach(8), Some(8.0 * MIB), "C found no edge: A alone");
    }

    /// TODO 22: anchors B and C that found nothing are still a calibration measured with them, which calibrate=auto loads (a `shared`
    /// of none is one from before them): A with no tier to size them from, and one CPU, with no SMT pair for B and none to share for C,
    /// each give an empty `SharedReach`; and the table doesn't say C found no edge where it never ran. Neither probes memory.
    #[test]
    fn the_shared_anchors_are_kept_when_they_find_nothing() {
        let tier = |tier, optimal_size| TierCalibrationResult {
            tier,
            optimal_size,
            median_latency_ns: 10.0,
            spread_ratio: 1.1,
            probes_attempted: 1,
            converged: true,
            fallback_used: false,
        };
        let cache_info = crate::tests::get_cache_info();
        let test = CalibrationTest::with_config(cache_info.clone(), CalibrationConfig::default());
        let one_cpu = CalibrationCpus { single: 0, node: 0, nodes: 1, pair: None, all: vec![0], per_l3: 1 };
        let nothing = SharedReach { l1_smt: None, l2_smt: None, l3_loaded: None, l3_loaded_ns: 0.0, l3_sharers: 1 };
        let mut cal = CalibrationResults::new(CpuSignature::default());
        assert_eq!(test.run_shared(&cal, &one_cpu, std::ptr::null_mut(), 0), nothing, "A found no tier");
        for (t, size) in [(CacheTier::L1, 48 << 10), (CacheTier::L2, 1 << 20), (CacheTier::L3, 32 << 20)] {
            cal.add_tier_result(tier(t, size));
        }
        assert_eq!(test.run_shared(&cal, &one_cpu, std::ptr::null_mut(), 0), nothing, "one CPU: neither B nor C");
        cal.shared = Some(nothing);
        let mut table = Vec::new();
        CalibrationTest::display_results(&cal, cache_info, &mut table).unwrap();
        let table = String::from_utf8(table).unwrap();
        assert!(!table.contains("Every CPU at once") && !table.contains("sharing;"), "C claimed on one CPU:\n{table}");
    }

    /// TODO 22: two threads, each chasing its own 64 KiB block of a heap buffer at once, give a latency and a spread
    #[test]
    fn a_shared_probe_times_every_threads_chase() {
        const BLOCK: usize = 64 << 10;
        let mut memory = vec![std::simd::u64x8::splat(0); 2 * BLOCK / 64];
        let pair = slots(memory.as_mut_ptr() as *mut u64, BLOCK, &[0, 1]);
        let tsc = crate::tests::get_cache_info().tsc_frequency_ghz.max(1.0);
        let stats = probe_shared(&pair, BLOCK, tsc, std::time::Duration::from_millis(20)).expect("both threads sampled");
        assert!(stats.median_ns > 0.1 && stats.median_ns < 1000.0 && stats.spread_ratio >= 1.0, "{stats:?}");
    }

    #[test]
    fn median_is_the_middle_or_the_mean_of_the_middle_two() {
        assert_eq!(median(&[3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(&[4.0, 1.0, 3.0, 2.0]), 2.5);
        assert_eq!(median(&[7.0]), 7.0);
        assert_eq!(median(&[]), 0.0);
    }
}
