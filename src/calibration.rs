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
}

fn default_page_size() -> String {
    "large".to_string()
}

impl CalibrationResults {
    /// Create new empty results
    pub fn new(cpu_signature: CpuSignature) -> Self {
        Self { timestamp: Utc::now(), cpu_signature, tiers: HashMap::new(), total_time_ms: 0, page_size: "large".to_string() }
    }

    /// Add a tier result
    pub fn add_tier_result(&mut self, result: TierCalibrationResult) {
        self.tiers.insert(result.tier, result);
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
/// page size it got. The cache calibration's probes and the seal's calibration both take theirs from here.
fn allocate_calibration_buffer(size: usize, page: PageSizeLevel) -> Result<(MemoryBuffer, PageSizeLevel), String> {
    let allocator = MemoryAllocator::new(BackendType::Auto)?;
    allocate_stepping_down(size, page, |config| allocator.allocate(config))
}

/// `allocate_calibration_buffer`'s requests, each made by `allocate`. Each is strict (`Require`), so the one granted proves its page size:
/// a `Prefer` request falls back inside the backend, which doesn't say to what, and a page size can't be read back after the fact. Each
/// large-page request carries its page's alignment, which is how it gets that page size (without it the kernel silently gives smaller
/// pages, TODO 87); 4 KiB needs none.
fn allocate_stepping_down<T>(
    size: usize, page: PageSizeLevel, mut allocate: impl FnMut(&AllocationConfig) -> Result<T, crate::memory::backend::AllocError>,
) -> Result<(T, PageSizeLevel), String> {
    let mut refused = Vec::new();
    for level in [PageSizeLevel::Huge, PageSizeLevel::Large, PageSizeLevel::Regular].into_iter().filter(|&l| l <= page) {
        let config = AllocationConfig {
            size,
            numa_node: None,
            page_size: PageSizePreference::Require(level),
            memory_type: MemoryType::WriteBack,
            zero_memory: false,
            alignment: (level != PageSizeLevel::Regular).then(|| level.bytes()),
            numa_strict: false,
        };
        match allocate(&config) {
            Ok(buffer) => {
                if refused.is_empty() {
                    log::info!("Calibration buffer: {} bytes on {} pages", size, page_size_name(page_size_str(level)));
                } else {
                    log::warn!(
                        "Calibration buffer: {} bytes on {} pages, after {} refused",
                        size,
                        page_size_name(page_size_str(level)),
                        refused.join("; ")
                    );
                }
                return Ok((buffer, level));
            }
            Err(e) => refused.push(format!("{} pages ({e})", page_size_name(page_size_str(level)))),
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

impl ProbeEngine {
    /// Create a new probe engine
    pub fn new(tsc_frequency_ghz: f64) -> Self {
        Self { tsc_frequency_ghz, memory_buffer: None, external_ptr: None, max_working_set: 0, initialized: false, page_obtained: None }
    }

    /// The page size its own buffer got, where it allocated one.
    pub fn page_obtained(&self) -> Option<PageSizeLevel> {
        self.page_obtained
    }

    /// Initialize with a specific page size preference
    /// page_size_str: "regular"/"4kb", "large"/"2mb", "huge"/"1gb"
    pub fn initialize_with_page_size(&mut self, max_size: usize, page_size_str: &str) -> Result<(), String> {
        if self.initialized {
            return Ok(());
        }

        match allocate_calibration_buffer(max_size, calibration_page_size(page_size_str)) {
            Ok((buffer, obtained)) => {
                self.page_obtained = Some(obtained);
                // Setup pointer-chase pattern
                let base = buffer.as_mut_ptr() as *mut u64;
                let len = max_size / std::mem::size_of::<u64>();

                // SAFETY: a page-aligned buffer of `max_size` bytes, just allocated: `len` u64s
                unsafe {
                    Self::setup_pointer_chase(base, len, 0);
                }

                self.memory_buffer = Some(buffer);
                self.max_working_set = max_size;
                self.initialized = true;

                log::info!("ProbeEngine: Initialized with {} byte buffer ({} u64 elements)", max_size, len);
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// Initialize from an existing MemoryBuffer (no allocation).
    /// Used when calibrating with test plan memory — same page types, no extra alloc/dealloc. The buffer must outlive the ProbeEngine and
    /// must not be concurrently modified.
    pub fn initialize_from_buffer(&mut self, buffer: &mut MemoryBuffer) -> Result<(), String> {
        if self.initialized {
            return Ok(());
        }

        let max_size = buffer.size();
        if max_size < 64 {
            return Err(format!("Buffer too small for calibration: {} bytes", max_size));
        }

        let base = buffer.as_mut_ptr() as *mut u64;
        let len = max_size / std::mem::size_of::<u64>();

        log::info!("ProbeEngine: Using external buffer ({} bytes, {} u64 elements)", max_size, len);

        // SAFETY: `len` u64s are the caller's buffer, test memory and so aligned, which outlives the engine and isn't touched meanwhile, as
        // this fn's doc requires
        unsafe {
            Self::setup_pointer_chase(base, len, 0);
        }

        self.external_ptr = Some(base);
        self.max_working_set = max_size;
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
    /// Each u64 location stores the address of the next random location
    /// This creates a random walk through memory that defeats prefetchers
    unsafe fn setup_pointer_chase(base: *mut u64, len: usize, thread_id: usize) {
        if len == 0 {
            return;
        }

        // Create array of indices
        let mut indices: Vec<usize> = (0..len).collect();

        // Fisher-Yates shuffle using XorShift RNG
        let mut rng_state = 0x123456789ABCDEFu64.wrapping_add(thread_id as u64);
        for i in (1..len).rev() {
            rng_state ^= rng_state << 13;
            rng_state ^= rng_state >> 17;
            rng_state ^= rng_state << 5;
            let j = (rng_state as usize) % (i + 1);
            indices.swap(i, j);
        }

        // Link: each location points to next in shuffled order
        for i in 0..len - 1 {
            let current_addr = base.add(indices[i]);
            let next_addr = base.add(indices[i + 1]) as usize as u64;
            *current_addr = next_addr;
        }

        // Loop back to start
        let last_addr = base.add(indices[len - 1]);
        let first_addr = base.add(indices[0]) as usize as u64;
        *last_addr = first_addr;
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

    /// Run the calibration test using a sweep-based approach (allocates its own memory)
    pub fn run(&self) -> Result<CalibrationResults, String> {
        let max_working_set = self.get_tier_size(CacheTier::FullDram);
        let mut probe_engine = ProbeEngine::new(self.tsc_frequency_ghz);
        probe_engine.initialize_with_page_size(max_working_set, &self.config.page_size)?;
        self.run_sweep_calibration(&mut probe_engine)
    }

    /// Run calibration using an existing memory buffer (no separate allocation).
    /// Used when calibrating with test plan memory — same page types, no extra alloc/dealloc.
    #[expect(dead_code, reason = "CLAUDE.md Open Design Question 1: kept as the path for calibrating on the test plan's own memory")]
    pub fn run_with_buffer(&self, buffer: &mut MemoryBuffer) -> Result<CalibrationResults, String> {
        let mut probe_engine = ProbeEngine::new(self.tsc_frequency_ghz);
        probe_engine.initialize_from_buffer(buffer)?;
        self.run_sweep_calibration(&mut probe_engine)
    }

    /// Core sweep calibration logic (shared by run() and run_with_buffer())
    fn run_sweep_calibration(&self, probe_engine: &mut ProbeEngine) -> Result<CalibrationResults, String> {
        let start_time = Instant::now();

        log::info!("Starting adaptive cache calibration (sweep mode)...");
        log::info!(
            "  L1D: {} KB, L2: {} KB, L3: {} MB",
            self.cache_info.per_core_l1d / 1024,
            self.cache_info.per_core_l2 / 1024,
            self.cache_info.l3_cache / (1024 * 1024)
        );

        let l1_size = self.cache_info.per_core_l1d;
        let l2_size = self.cache_info.per_core_l2;
        let l3_size = self.cache_info.l3_cache;

        let sweep_results = self.run_sweep(probe_engine, l1_size, l2_size, l3_size);
        let mut results = self.analyze_sweep(&sweep_results, l1_size, l2_size, l3_size);

        // The page size the buffer got, where the engine allocated it, else the one asked for
        results.page_size = probe_engine.page_obtained().map_or_else(|| self.config.page_size.clone(), |p| page_size_str(p).to_string());
        results.total_time_ms = start_time.elapsed().as_millis() as u64;

        log::info!("Calibration complete in {}ms", results.total_time_ms);

        Ok(results)
    }

    /// Run extended calibration (allocates its own memory)
    pub fn run_extended(&self) -> Result<CalibrationResults, String> {
        let max_working_set = self.get_tier_size(CacheTier::FullDram);
        let mut probe_engine = ProbeEngine::new(self.tsc_frequency_ghz);
        probe_engine.initialize_with_page_size(max_working_set, &self.config.page_size)?;
        self.run_extended_calibration(&mut probe_engine)
    }

    /// Run extended calibration using an existing memory buffer
    #[expect(dead_code, reason = "CLAUDE.md Open Design Question 1: kept as the path for calibrating on the test plan's own memory")]
    pub fn run_extended_with_buffer(&self, buffer: &mut MemoryBuffer) -> Result<CalibrationResults, String> {
        let mut probe_engine = ProbeEngine::new(self.tsc_frequency_ghz);
        probe_engine.initialize_from_buffer(buffer)?;
        self.run_extended_calibration(&mut probe_engine)
    }

    /// Core extended calibration logic (shared by run_extended() and run_extended_with_buffer())
    fn run_extended_calibration(&self, probe_engine: &mut ProbeEngine) -> Result<CalibrationResults, String> {
        let start_time = Instant::now();

        log::info!("Starting extended cache calibration (phase 1 + phase 2)...");
        log::info!(
            "  L1D: {} KB, L2: {} KB, L3: {} MB",
            self.cache_info.per_core_l1d / 1024,
            self.cache_info.per_core_l2 / 1024,
            self.cache_info.l3_cache / (1024 * 1024)
        );

        let l1_size = self.cache_info.per_core_l1d;
        let l2_size = self.cache_info.per_core_l2;
        let l3_size = self.cache_info.l3_cache;

        // Phase 1: Coarse sweep (same as --calibrate-cache)
        log::info!("═══ Phase 1: Coarse sweep ═══");
        let sweep_results = self.run_sweep(probe_engine, l1_size, l2_size, l3_size);
        let tipping_points = self.find_tipping_points(&sweep_results);

        for (size, lat_before, lat_after) in &tipping_points {
            log::info!(
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

        // Phase 2: Fine-grain sweep around each tipping point
        // Cap sweep ranges to CPU cache sizes — workload must fit in the tier
        log::info!("\n═══ Phase 2: Fine-grain verification ═══");

        let repeats = 3u32;
        let fine_step = 1.05; // 5% steps
        let stable_spread = 1.5;

        // Cap upper bounds: min of tipping point and CPU cache size
        let l1_cap = tp_l1_l2.min(l1_size);
        let l2_cap = tp_l2_l3.min(l2_size);
        let l3_cap = tp_l3_dram.min(l3_size);

        // Search window: from 50% below cap to the cap
        let fine_l1 = self.run_fine_sweep(probe_engine, l1_cap / 2, l1_cap, fine_step, repeats, "L1→L2");
        let fine_l2 = self.run_fine_sweep(probe_engine, l2_cap / 2, l2_cap, fine_step, repeats, "L2→L3");
        let fine_l3 = self.run_fine_sweep(probe_engine, l3_cap / 2, l3_cap, fine_step, repeats, "L3→DRAM");

        // Build results using phase 2 data
        let cpu_signature = CpuSignature::from_cache_info(&self.cache_info);
        let mut results = CalibrationResults::new(cpu_signature);

        log::info!("\n═══ Final Selection ═══");

        // L1: Largest size where ALL repeats are stable, below TP1
        if let Some(fp) = Self::find_largest_reliable(&fine_l1, stable_spread) {
            log::info!(
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
            log::warn!("L1: Phase 2 failed, using phase 1 fallback: {} @ {:.1}ns", format_size(probe.size), probe.median_ns);
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
            log::info!(
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
            log::warn!("L2: Phase 2 failed, using phase 1 fallback: {} @ {:.1}ns", format_size(probe.size), probe.median_ns);
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
            log::info!(
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
            log::warn!("L3: Phase 2 failed, using phase 1 fallback: {} @ {:.1}ns", format_size(probe.size), probe.median_ns);
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

        // DRAM + FullDRAM: Use phase 1 data (fine-grain not needed for DRAM)
        // DRAM: Moderate size — 2x tipping point cap to differentiate from FullDRAM
        let total_cache = l3_size + l2_size + l1_size;
        let dram_min = tp_l3_dram.max(total_cache);
        let dram_max = tp_l3_dram * 2;
        let dram_max = dram_max.max(dram_min + 1);
        if let Some(probe) = self.find_largest_stable_in_range(&sweep_results, dram_min, dram_max, 2.0, 8.0) {
            log::info!("DRAM: {} @ {:.1}ns (from phase 1)", format_size(probe.size), probe.median_ns);
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
            log::info!("DRAM: {} @ {:.1}ns (from phase 1, extended range)", format_size(probe.size), probe.median_ns);
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
            log::info!("FullDRAM: {} @ {:.1}ns (from phase 1)", format_size(probe.size), probe.median_ns);
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

        // The page size the buffer got, where the engine allocated it, else the one asked for
        results.page_size = probe_engine.page_obtained().map_or_else(|| self.config.page_size.clone(), |p| page_size_str(p).to_string());
        results.total_time_ms = start_time.elapsed().as_millis() as u64;
        log::info!("Extended calibration complete in {}ms", results.total_time_ms);
        Ok(results)
    }

    /// Run fine-grain sweep around a tipping point with multiple repeats per size
    fn run_fine_sweep(
        &self, probe_engine: &mut ProbeEngine, min_size: usize, max_size: usize, step_factor: f64, repeats: u32, label: &str,
    ) -> Vec<FineProbe> {
        let mut results = Vec::new();
        let mut current_size = min_size.max(4096);

        log::info!(
            "  Fine sweep {}: {} to {} ({}x steps, {} repeats each)",
            label,
            format_size(min_size),
            format_size(max_size),
            step_factor,
            repeats
        );
        log::info!("  {:>10} {:>8} {:>6} {:>10} {:>10} {:>10}", "Size", "Median", "Stbl", "Worst Spr", "Consistency", "Latencies");

        while current_size <= max_size {
            let mut median_latencies = Vec::new();
            let mut stable_count = 0u32;
            let mut worst_spread = 0.0f64;

            for _rep in 0..repeats {
                let samples = probe_engine.probe(current_size, &self.config);
                if !samples.is_empty() {
                    let stats = StatisticsAnalyzer::analyze(&samples);
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

                log::info!(
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

    /// Run a continuous sweep from small to large sizes
    /// Collects comprehensive stats for each probe to enable accurate tier detection
    fn run_sweep(&self, probe_engine: &mut ProbeEngine, l1_size: usize, _l2_size: usize, l3_size: usize) -> Vec<SweepProbe> {
        let mut results = Vec::new();

        // Start at 1/4 of L1 (well within L1)
        let min_size = (l1_size / 4).max(4096);
        // End at 4x L3 or buffer max
        let max_size = (l3_size * 4).min(probe_engine.max_working_set());

        log::info!("Running calibration sweep: {} to {}", format_size(min_size), format_size(max_size));
        log::info!(
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

        while current_size <= max_size {
            probe_num += 1;

            let samples = probe_engine.probe(current_size, &self.config);

            if !samples.is_empty() {
                let stats = StatisticsAnalyzer::analyze(&samples);

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

                log::info!(
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

        log::info!("Sweep complete: {} probes collected", results.len());
        results
    }

    /// Analyze sweep results to find optimal sizes for each tier
    /// Selection criteria: within tier bounds (CPUID), most stable measurements
    fn analyze_sweep(&self, sweep: &[SweepProbe], l1_size: usize, l2_size: usize, l3_size: usize) -> CalibrationResults {
        let cpu_signature = CpuSignature::from_cache_info(&self.cache_info);
        let mut results = CalibrationResults::new(cpu_signature);

        if sweep.is_empty() {
            log::error!("No sweep data to analyze!");
            return results;
        }

        log::info!("Analyzing sweep data for tier boundaries...");

        // Find tipping points by looking for latency jumps
        let tipping_points = self.find_tipping_points(sweep);

        for (size, lat_before, lat_after) in &tipping_points {
            log::info!(
                "  Tipping point at {}: {:.1}ns → {:.1}ns ({:.1}x jump)",
                format_size(*size),
                lat_before,
                lat_after,
                lat_after / lat_before
            );
        }

        // Selection strategy: Use tipping points to define tier boundaries, then find the LARGEST STABLE probe within each tier.
        // This maximizes working set utilization while avoiding spill to next tier.

        // Extract tipping point sizes (sorted by size)
        let tp_sizes: Vec<usize> = tipping_points.iter().map(|(s, _, _)| *s).collect();

        // Assign tipping points to tier transitions
        // TP[0] = L1→L2, TP[1] = L2→L3, TP[2] = L3→DRAM
        let tp_l1_l2 = tp_sizes.first().copied().unwrap_or(l1_size);
        let tp_l2_l3 = tp_sizes.get(1).copied().unwrap_or(l2_size);
        let tp_l3_dram = tp_sizes.get(2).copied().unwrap_or(l3_size);

        log::info!("Tier boundaries: L1<{}, L2<{}, L3<{}", format_size(tp_l1_l2), format_size(tp_l2_l3), format_size(tp_l3_dram));

        // Stability thresholds for "stable enough"
        let stable_spread = 1.5;
        let stable_winsd = 2.0;

        // L1: Largest stable probe below TP1
        if let Some(probe) = self.find_largest_stable_in_range(sweep, 0, tp_l1_l2, stable_spread, stable_winsd) {
            log::info!(
                "L1: {} @ {:.1}ns (spread={:.2}x, WinSD={:.2})",
                format_size(probe.size),
                probe.median_ns,
                probe.spread,
                probe.window_stddev
            );
            results.add_tier_result(TierCalibrationResult {
                tier: CacheTier::L1,
                optimal_size: probe.size,
                median_latency_ns: probe.median_ns,
                spread_ratio: probe.spread,
                probes_attempted: sweep.len() as u32,
                converged: true,
                fallback_used: false,
            });
        }

        // L2: Largest stable probe between TP1 and TP2
        if let Some(probe) = self.find_largest_stable_in_range(sweep, tp_l1_l2, tp_l2_l3, stable_spread, stable_winsd) {
            log::info!(
                "L2: {} @ {:.1}ns (spread={:.2}x, WinSD={:.2})",
                format_size(probe.size),
                probe.median_ns,
                probe.spread,
                probe.window_stddev
            );
            results.add_tier_result(TierCalibrationResult {
                tier: CacheTier::L2,
                optimal_size: probe.size,
                median_latency_ns: probe.median_ns,
                spread_ratio: probe.spread,
                probes_attempted: sweep.len() as u32,
                converged: true,
                fallback_used: false,
            });
        }

        // L3: Largest stable probe between TP2 and TP3
        if let Some(probe) = self.find_largest_stable_in_range(sweep, tp_l2_l3, tp_l3_dram, stable_spread, stable_winsd) {
            log::info!(
                "L3: {} @ {:.1}ns (spread={:.2}x, WinSD={:.2})",
                format_size(probe.size),
                probe.median_ns,
                probe.spread,
                probe.window_stddev
            );
            results.add_tier_result(TierCalibrationResult {
                tier: CacheTier::L3,
                optimal_size: probe.size,
                median_latency_ns: probe.median_ns,
                spread_ratio: probe.spread,
                probes_attempted: sweep.len() as u32,
                converged: true,
                fallback_used: false,
            });
        } else {
            // Fallback: relax thresholds for shared VM/cache
            log::warn!("L3: No stable probes found, relaxing thresholds");
            if let Some(probe) = self.find_largest_stable_in_range(sweep, tp_l2_l3, tp_l3_dram, 2.0, 5.0) {
                results.add_tier_result(TierCalibrationResult {
                    tier: CacheTier::L3,
                    optimal_size: probe.size,
                    median_latency_ns: probe.median_ns,
                    spread_ratio: probe.spread,
                    probes_attempted: sweep.len() as u32,
                    converged: false,
                    fallback_used: true,
                });
            }
        }

        // DRAM: Moderate size above the L3→DRAM tipping point.
        // Use 2x the tipping point as cap — large enough to be firmly in DRAM, but not so large it overlaps with FullDRAM (which uses the
        // largest available).
        let total_cache = l3_size + l2_size + l1_size;
        let dram_min = tp_l3_dram.max(total_cache);
        let dram_max = tp_l3_dram * 2; // 2x tipping point — moderate DRAM working set
        let dram_max = dram_max.max(dram_min + 1); // Ensure max > min
        log::info!(
            "DRAM range: {} to {} (total cache={}, TP3={})",
            format_size(dram_min),
            format_size(dram_max),
            format_size(total_cache),
            format_size(tp_l3_dram)
        );
        if let Some(probe) = self.find_largest_stable_in_range(sweep, dram_min, dram_max, 2.0, 8.0) {
            log::info!(
                "DRAM: {} @ {:.1}ns (spread={:.2}x, WinSD={:.2})",
                format_size(probe.size),
                probe.median_ns,
                probe.spread,
                probe.window_stddev
            );
            results.add_tier_result(TierCalibrationResult {
                tier: CacheTier::Dram,
                optimal_size: probe.size,
                median_latency_ns: probe.median_ns,
                spread_ratio: probe.spread,
                probes_attempted: sweep.len() as u32,
                converged: true,
                fallback_used: false,
            });
        } else if let Some(probe) = self.find_largest_stable_in_range(sweep, dram_min, usize::MAX, 2.0, 8.0) {
            // Fallback: no probes in capped range, use any stable DRAM probe
            log::info!(
                "DRAM: {} @ {:.1}ns (spread={:.2}x, WinSD={:.2}) [extended range]",
                format_size(probe.size),
                probe.median_ns,
                probe.spread,
                probe.window_stddev
            );
            results.add_tier_result(TierCalibrationResult {
                tier: CacheTier::Dram,
                optimal_size: probe.size,
                median_latency_ns: probe.median_ns,
                spread_ratio: probe.spread,
                probes_attempted: sweep.len() as u32,
                converged: true,
                fallback_used: false,
            });
        }

        // FullDRAM: Largest stable size above 2x L3
        if let Some(probe) = self.find_largest_stable(sweep, l3_size * 2) {
            log::info!(
                "FullDRAM: {} @ {:.1}ns (spread={:.2}x, WinSD={:.2})",
                format_size(probe.size),
                probe.median_ns,
                probe.spread,
                probe.window_stddev
            );
            results.add_tier_result(TierCalibrationResult {
                tier: CacheTier::FullDram,
                optimal_size: probe.size,
                median_latency_ns: probe.median_ns,
                spread_ratio: probe.spread,
                probes_attempted: sweep.len() as u32,
                converged: true,
                fallback_used: false,
            });
        }

        results
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
        writeln!(out, "Total calibration time: {}ms", results.total_time_ms)?;

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
    log::info!(
        "Seal width calibration: tmr at {:?} bits, {} (asking for {} pages), {} timed runs each",
        kernels.iter().map(|k| k.width.bits()).collect::<Vec<_>>(),
        format_size(bytes),
        page_size_name(page_size_str(page)),
        SEAL_RUNS
    );

    let (buffer, obtained) = allocate_calibration_buffer(bytes, page)?;
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
            let got = allocate_stepping_down(SIZE, page, |config| {
                let PageSizePreference::Require(level) = config.page_size else { panic!("not strict: {:?}", config.page_size) };
                assert_eq!((config.size, config.numa_node), (SIZE, None));
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

    #[test]
    fn median_is_the_middle_or_the_mean_of_the_middle_two() {
        assert_eq!(median(&[3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(&[4.0, 1.0, 3.0, 2.0]), 2.5);
        assert_eq!(median(&[7.0]), 7.0);
        assert_eq!(median(&[]), 0.0);
    }
}
