use crate::{ErrorMode};
use crate::constants::{gib_to_bytes, BYTES_PER_MIB};
use crate::tests::{WindowMode, ChunkMode, CacheTarget, parse_size_string};
use crate::memory::allocation_strategy::{EnhancedMemoryStrategy, AllocationMode, ReserveAmount, StartAddressMode};
use crate::runner::TestSuiteTiming;
use crate::tests::{TestTiming, TestMemoryConfig};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

// Application constants
pub const APP_NAME: &str = "Test Memory R";
pub const APP_SHORT_NAME: &str = "TMR";
pub const APP_VERSION: &str = "1.0.0";
pub const CONFIG_VERSION: &str = "2.0";

// Modern JSON configuration format v2.0 (simplified)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModernConfig {
    pub config_format_version: String,
    pub application_name: String,
    pub metadata: ConfigMetadata,
    pub system: SystemConfig,
    pub test_sequence: Vec<TestConfig>,
    pub legacy_metadata: Option<LegacyMetadata>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LegacyMetadata {
    pub tm5_test_sequence: Vec<u32>,
    pub tm5_cycles: u32,
    pub tm5_time_percent: u32,
    /// Memory channel count from TM5 .cfg (default 2). Used in stride formula.
    #[serde(default = "default_channels")]
    pub tm5_channels: u32,
}

fn default_channels() -> u32 { 2 }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigMetadata {
    pub name: String,
    pub author: String,
    pub version: String,
    pub description: Option<String>,
    pub created: Option<String>,
    pub tested_with_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemConfig {
    pub memory_strategy: MemoryStrategyConfig,
    pub cpu_config: CpuConfig,
    pub error_mode: String,
    pub timing: TimingConfig,
    pub large_pages: bool,
	#[serde(default)]
	pub cpu_pinning: CpuPinningConfig,
	#[serde(default)]
    pub memory_allocation: MemoryAllocationConfig,
    /// Memory channel count for stride calculation (default 2 for consumer DDR5 dual-channel).
    /// Higher values produce larger strides to match DDR interleave boundaries.
    /// Consumer: 2, Server: 4-12+.
    #[serde(default = "default_channels")]
    pub channels: u32,
}

// Define CpuPinningConfig in config.rs
//
// CPU selection is a two-stage pipeline:
//   Stage 1 FILTER  — `skip_spec` decides which cores are *eligible* (Skipped vs Available)
//   Stage 2 SPACING — `cpus=` count + `stride_spec` decide which eligible cores get *Assigned*
// `cpus=` percentages are relative to the post-filter Available pool, so any value <= 100%
// is always valid — that keeps configs portable across 4/6/8/16-core machines.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CpuPinningConfig {
    pub enable_pinning: bool,
    /// Resolved count of leading cores to skip. Kept for back-compat with existing JSON
    /// configs and the topology display; derived from `skip_spec` when that is set.
    #[serde(default = "default_cpus_to_skip")]
    pub cpus_to_skip: usize,
    #[serde(default = "default_avoid_smt_doubling")]
    pub avoid_smt_doubling: bool,
    /// Stage 1 filter spec: "N" | "N%" | "A-B" (inclusive core-id range to exclude).
    #[serde(default = "default_skip_spec")]
    pub skip_spec: String,
    /// Stage 2 spacing spec: "1" (packed) | "N" (every Nth) | "even" (spread across pool).
    #[serde(default = "default_stride_spec")]
    pub stride_spec: String,
}

fn default_cpus_to_skip() -> usize { 1 }
fn default_avoid_smt_doubling() -> bool { false }
fn default_skip_spec() -> String { "1".to_string() }
fn default_stride_spec() -> String { "1".to_string() }


// Define MemoryAllocationConfig
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryAllocationConfig {
    // Driver control
    #[serde(default = "default_use_driver")]
    pub use_driver: Option<bool>,              // None = auto-detect, Some(true/false) = force
    
    #[serde(default = "default_driver_chunking")]
    pub driver_chunking: bool,                 // Let driver control page allocation strategy
    
    #[serde(default = "default_remap_mode")]
    pub remap_mode: String,                    // "remap_all" or "batch"
    
    // Page size constraints (using your existing system)
    #[serde(default = "default_min_page_size")]
    pub min_page_size: String,                 // "regular", "large", "huge"
    
    #[serde(default = "default_max_page_size")]
    pub max_page_size: String,                 // "regular", "large", "huge"
    
    // Allocation behavior
    #[serde(default = "default_zero_memory")]
    pub zero_memory: bool,                     // Zero memory on allocation
    
    #[serde(default = "default_require_contiguous")]
    pub require_contiguous: bool,              // Require contiguous physical memory
    
    #[serde(default = "default_allocation_strategy")]
    pub allocation_strategy: String,           // "greedy", "plan-pagesize-pref", "plan-blocksize-pref"
    
    #[serde(default = "default_memory_type")]
    pub default_memory_type: String,           // "write_back", "write_through", "uncached", "write_combining"
    
    // Timing/retry parameters
    #[serde(default = "default_allocation_timeout_ms")]
    pub allocation_timeout_ms: u32,            // Default: 10000
    
    #[serde(default = "default_retry_interval_ms")]
    pub retry_interval_ms: u32,                // Default: 10
    
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,                      // Default: 100
    
    // NUMA behavior
    #[serde(default = "default_strict_numa")]
    pub strict_numa: bool,                     // Fail if can't allocate on requested NUMA node
}

fn default_use_driver() -> Option<bool> { None }
fn default_driver_chunking() -> bool { false }
fn default_remap_mode() -> String { "remap_all".to_string() }
fn default_min_page_size() -> String { "large".to_string() }
fn default_max_page_size() -> String { "huge".to_string() }
fn default_zero_memory() -> bool { false }
fn default_require_contiguous() -> bool { true }
fn default_allocation_strategy() -> String { "plan-pagesize-pref".to_string() }
fn default_memory_type() -> String { "write_back".to_string() }
fn default_allocation_timeout_ms() -> u32 { 10000 }
fn default_retry_interval_ms() -> u32 { 100 }
fn default_max_retries() -> u32 { 3 }
fn default_strict_numa() -> bool { false }

impl Default for MemoryAllocationConfig {
    fn default() -> Self {
        Self {
            use_driver: default_use_driver(),
            driver_chunking: default_driver_chunking(),
            remap_mode: default_remap_mode(),
            min_page_size: default_min_page_size(),
            max_page_size: default_max_page_size(),
            zero_memory: default_zero_memory(),
            require_contiguous: default_require_contiguous(),
            allocation_strategy: default_allocation_strategy(),
            default_memory_type: default_memory_type(),
            allocation_timeout_ms: default_allocation_timeout_ms(),
            retry_interval_ms: default_retry_interval_ms(),
            max_retries: default_max_retries(),
            strict_numa: default_strict_numa(),
        }
    }
}

impl MemoryAllocationConfig {
    // Convert string page size to PageSize enum
    pub fn parse_page_size(size_str: &str) -> Result<crate::driver::PageSize, String> {
        match size_str.to_lowercase().as_str() {
            "regular" | "4kb" => Ok(crate::driver::PageSize::Regular),
            "large" | "2mb" => Ok(crate::driver::PageSize::Large),
            "huge" | "1gb" => Ok(crate::driver::PageSize::Huge),
            _ => Err(format!("Invalid page size: {}", size_str)),
        }
    }
    
    // Convert string memory type to MemoryType enum
    pub fn parse_memory_type(type_str: &str) -> Result<crate::driver::MemoryType, String> {
        match type_str.to_lowercase().replace('_', "").as_str() {
            "writeback" => Ok(crate::driver::MemoryType::WriteBack),
            "writethrough" => Ok(crate::driver::MemoryType::WriteThrough),
            "uncached" => Ok(crate::driver::MemoryType::Uncached),
            "writecombining" => Ok(crate::driver::MemoryType::WriteCombining),
            _ => Err(format!("Invalid memory type: {}", type_str)),
        }
    }
    
    // Create a DmaConfig from this allocation config
    pub fn to_dma_config(&self, numa_node: Option<u32>) -> Result<crate::memory::DmaConfig, String> {
        Ok(crate::memory::DmaConfig {
            minimum_page_size: Self::parse_page_size(&self.min_page_size)?,
            maximum_page_size: Self::parse_page_size(&self.max_page_size)?,
            prefer_numa_node: numa_node,
            zero_memory: self.zero_memory,
            memory_type: Self::parse_memory_type(&self.default_memory_type)?,
            contiguous: self.require_contiguous,
            timeout_ms: self.allocation_timeout_ms,
            retry_interval_ms: self.retry_interval_ms,
            max_retries: self.max_retries,
            strict_numa: self.strict_numa,
        })
    }
}

impl Default for CpuPinningConfig {
    fn default() -> Self {
        Self {
            enable_pinning: true,
			cpus_to_skip: 1,
			avoid_smt_doubling: false,
			skip_spec: default_skip_spec(),
			stride_spec: default_stride_spec(),
        }
    }
}

impl WindowSpec {
    pub fn full_allocation() -> Self {
        WindowSpec { mode: "full_allocation".to_string(), ..Default::default() }
    }
    pub fn cache(target: &str) -> Self {
        WindowSpec { mode: "cache".to_string(), target: Some(target.to_string()), ..Default::default() }
    }
    pub fn cache_total(fraction: f64) -> Self {
        WindowSpec { mode: "cache_total".to_string(), fraction: Some(fraction), ..Default::default() }
    }
    pub fn absolute(size: &str) -> Self {
        WindowSpec { mode: "absolute".to_string(), size: Some(size.to_string()), ..Default::default() }
    }
}

impl ChunkSpec {
    pub fn auto() -> Self {
        ChunkSpec { mode: "auto".to_string(), ..Default::default() }
    }
    pub fn cache(target: &str) -> Self {
        ChunkSpec { mode: "cache".to_string(), target: Some(target.to_string()), ..Default::default() }
    }
    pub fn cache_total(fraction: f64) -> Self {
        ChunkSpec { mode: "cache_total".to_string(), fraction: Some(fraction), ..Default::default() }
    }
    pub fn absolute(size: &str) -> Self {
        ChunkSpec { mode: "absolute".to_string(), size: Some(size.to_string()), ..Default::default() }
    }
    pub fn fraction(fraction: f64) -> Self {
        ChunkSpec { mode: "fraction".to_string(), fraction: Some(fraction), ..Default::default() }
    }
}

/// Convert a WindowSpec into a runtime WindowMode. Returns Err with a human-readable
/// reason on malformed input. Mode names are case-insensitive.
pub fn spec_to_window_mode(spec: &WindowSpec) -> Result<WindowMode, String> {
    match spec.mode.to_ascii_lowercase().as_str() {
        "full_allocation" | "full-allocation" | "full" => Ok(WindowMode::FullAllocation),
        "cache" => {
            let target_str = spec.target.as_deref()
                .ok_or_else(|| "window mode 'cache' requires 'target' field (e.g. \"L3/2\", \"DRAM*4\")".to_string())?;
            let target = CacheTarget::parse(target_str)
                .ok_or_else(|| format!("invalid cache target '{}'", target_str))?;
            Ok(WindowMode::Cache { target })
        }
        "cache_total" | "cache-total" => {
            let fraction = spec.fraction
                .ok_or_else(|| "window mode 'cache_total' requires 'fraction' field".to_string())?;
            if fraction <= 0.0 {
                return Err(format!("cache_total fraction must be > 0 (got {})", fraction));
            }
            Ok(WindowMode::CacheTotal { fraction })
        }
        "absolute" => {
            let size_str = spec.size.as_deref()
                .ok_or_else(|| "window mode 'absolute' requires 'size' field (e.g. \"880MB\", \"4GiB\")".to_string())?;
            let size_bytes = parse_size_string(size_str)?;
            Ok(WindowMode::Absolute { size_bytes })
        }
        other => Err(format!("unknown window mode '{}'; valid: full_allocation, cache, cache_total, absolute", other)),
    }
}

/// Convert a ChunkSpec into a runtime ChunkMode. Same conventions as spec_to_window_mode
/// plus the `fraction` mode (fraction of resolved window).
pub fn spec_to_chunk_mode(spec: &ChunkSpec) -> Result<ChunkMode, String> {
    match spec.mode.to_ascii_lowercase().as_str() {
        "auto" => Ok(ChunkMode::Auto),
        "cache" => {
            let target_str = spec.target.as_deref()
                .ok_or_else(|| "chunk mode 'cache' requires 'target' field (e.g. \"L3/2\", \"DRAM*4\")".to_string())?;
            let target = CacheTarget::parse(target_str)
                .ok_or_else(|| format!("invalid cache target '{}'", target_str))?;
            Ok(ChunkMode::Cache { target })
        }
        "cache_total" | "cache-total" => {
            let fraction = spec.fraction
                .ok_or_else(|| "chunk mode 'cache_total' requires 'fraction' field".to_string())?;
            if fraction <= 0.0 {
                return Err(format!("cache_total fraction must be > 0 (got {})", fraction));
            }
            Ok(ChunkMode::CacheTotal { fraction })
        }
        "absolute" => {
            let size_str = spec.size.as_deref()
                .ok_or_else(|| "chunk mode 'absolute' requires 'size' field (e.g. \"16MB\", \"64KiB\")".to_string())?;
            let size_bytes = parse_size_string(size_str)?;
            Ok(ChunkMode::Absolute { size_bytes })
        }
        "fraction" => {
            let fraction = spec.fraction
                .ok_or_else(|| "chunk mode 'fraction' requires 'fraction' field (0.0-1.0 of resolved window)".to_string())?;
            if !(0.0..=1.0).contains(&fraction) {
                return Err(format!("chunk fraction must be in [0, 1] (got {})", fraction));
            }
            Ok(ChunkMode::Fraction { fraction })
        }
        other => Err(format!("unknown chunk mode '{}'; valid: auto, cache, cache_total, absolute, fraction", other)),
    }
}

/// Render a WindowSpec for display in summary reports.
pub fn describe_window_spec(spec: &WindowSpec) -> String {
    match spec.mode.to_ascii_lowercase().as_str() {
        "full_allocation" | "full-allocation" | "full" => "FullAllocation".to_string(),
        "cache" => match spec.target.as_deref() {
            Some(t) => format!("Cache({})", t),
            None => "Cache(?)".to_string(),
        },
        "cache_total" | "cache-total" => match spec.fraction {
            Some(f) => format!("CacheTotal({:.2}x)", f),
            None => "CacheTotal(?)".to_string(),
        },
        "absolute" => match spec.size.as_deref() {
            Some(s) => format!("Absolute({})", s),
            None => "Absolute(?)".to_string(),
        },
        other => format!("Unknown({})", other),
    }
}

/// Render a ChunkSpec for display in summary reports.
pub fn describe_chunk_spec(spec: &ChunkSpec) -> String {
    match spec.mode.to_ascii_lowercase().as_str() {
        "auto" => "Auto".to_string(),
        "cache" => match spec.target.as_deref() {
            Some(t) => format!("Cache({})", t),
            None => "Cache(?)".to_string(),
        },
        "cache_total" | "cache-total" => match spec.fraction {
            Some(f) => format!("CacheTotal({:.2}x)", f),
            None => "CacheTotal(?)".to_string(),
        },
        "absolute" => match spec.size.as_deref() {
            Some(s) => format!("Absolute({})", s),
            None => "Absolute(?)".to_string(),
        },
        "fraction" => match spec.fraction {
            Some(f) => format!("Fraction({:.1}%)", f * 100.0),
            None => "Fraction(?)".to_string(),
        },
        other => format!("Unknown({})", other),
    }
}

/// Window specification — nested JSON shape: `{ "mode": "...", ... }`.
/// See `doc/window_chunk_modes.md` for full syntax.
///
/// Modes:
/// - `full_allocation` — use entire per-thread allocation (no other fields needed)
/// - `cache` — tier-aware sizing; requires `target` (e.g. `"L3/2"`, `"DRAM*4"`)
/// - `cache_total` — coarse `(L1+L2+L3) × fraction`; requires `fraction`
/// - `absolute` — hard byte size; requires `size` (string like `"880MB"`, `"4GiB"`)
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WindowSpec {
    pub mode: String,
    /// CacheTarget string for `cache` mode (e.g. `"L3/2"`, `"DRAM*4"`).
    #[serde(default)]
    pub target: Option<String>,
    /// Fraction for `cache_total` mode.
    #[serde(default)]
    pub fraction: Option<f64>,
    /// Size string for `absolute` mode (`"880MB"`, `"4GiB"`, etc.).
    #[serde(default)]
    pub size: Option<String>,
}

/// Chunk specification — same nested shape as WindowSpec but with extra `fraction` mode.
///
/// Modes:
/// - `auto` — per-test heuristic
/// - `cache` — tier-aware; requires `target`
/// - `cache_total` — coarse cache fraction; requires `fraction`
/// - `absolute` — hard byte size; requires `size`
/// - `fraction` — fraction of resolved window; requires `fraction`
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ChunkSpec {
    pub mode: String,
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub fraction: Option<f64>,
    #[serde(default)]
    pub size: Option<String>,
}

// Simplified memory strategy configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryStrategyConfig {
    // Stage 1: Allocation strategy
    pub allocation_mode: String, // "max_available", "percentage_reserve", "fixed_reserve"
    pub reserve_mb: Option<u32>,       // For max_available mode
    pub reserve_percent: Option<f64>,  // For percentage_reserve mode
    pub reserve_gib: Option<f64>,      // For fixed_reserve mode

    /// Default window spec — applied when a test does not override it.
    pub default_window: WindowSpec,
    /// Default chunk spec — applied when a test does not override it.
    pub default_chunk: ChunkSpec,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimingConfig {
    // Global test suite timing
    pub global_cycles: Option<u32>,
    pub global_duration_secs: Option<u32>,
    
    // Default per-test timing (can be overridden per test)
    pub default_test_cycles: Option<u32>,
    pub default_test_duration_secs: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CpuConfig {
    #[serde(rename = "type")]
    pub cpu_type: String, // "threads", "cores"
    pub usage_percent: u32, // 1-100
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestConfig {
    pub enabled: bool,
    pub function: String,
    
    // Per-test timing overrides
    pub cycles: Option<u32>,
    pub duration_secs: Option<u32>,
    pub min_duration_secs: Option<u32>,

    /// Per-test window spec override. Same shape as `system.memory_strategy.default_window`.
    #[serde(default)]
    pub window: Option<WindowSpec>,
    /// Per-test chunk spec override. Same shape as `system.memory_strategy.default_chunk`.
    #[serde(default)]
    pub chunk: Option<ChunkSpec>,

    pub allow_misaligned: Option<bool>,     // Allow unaligned accesses
    pub requires_locality: Option<bool>,    // Test needs temporal locality

    /// Flush each chunk out of cache (CLFLUSHOPT + MFENCE) between the write and verify phases,
    /// so the verify round-trips through DRAM instead of reading the just-written cached copy
    /// (#59). Defaults to false. Only meaningful where the window/chunk would otherwise stay
    /// cache-resident — at large chunks natural eviction already forces DRAM reads, so enabling
    /// it there costs bandwidth without changing what is tested.
    #[serde(default)]
    pub flush_before_verify: Option<bool>,


    // TMR-native test parameters (each used by specific tests, see doc/test_parameters.md)
    pub stride_patterns: Option<u32>,       // CacheBust: number of interleaved stride pattern variants
    pub rng_sequences: Option<u32>,         // RandomTorture: number of independent RNG sequences
    pub subdivisions: Option<u32>,          // StrideAccess: number of chunk subdivisions
    pub copy_directions: Option<u32>,       // BlockMove: number of copy direction patterns

    // TM5 pattern configuration (preserved for TM5-faithful pattern generation)
    pub pattern_mode: Option<u32>,
    pub pattern_param0: Option<u64>,
    pub pattern_param1: Option<u64>,
    pub parameter: Option<u32>,             // Raw TM5 parameter — interpreted via TestParameterContext

    // v2: Enable v2 test variants when true (uses corrected parameter interpretation)
    #[serde(default)]
    pub use_v2_tests: Option<bool>,
}

/// Correctly interpreted TM5 parameter context for v2 tests.
///
/// TM5's `Parameter` field means different things per test type:
/// - **SimpleTest**: stride in cache lines. TM5 formula: `Channels * Parameter - 1` cache lines.
///   Converted to u64 elements: `stride_cachelines * (cache_line_bytes / 8)`.
/// - **MirrorMove**: subblock count (1-4, how many mirror regions)
/// - **MirrorMove128**: page stride in bytes ((Parameter + 1) * 128)
///
/// The v1 code incorrectly funneled all of these into a single field.
#[derive(Debug, Clone, Default)]
pub struct TestParameterContext {
    /// Raw TM5 parameter value, preserved for debugging.
    pub raw_parameter: u32,
    /// SimpleTest: stride in cache lines (64-byte units).
    /// TM5: `Channels * Parameter - 1`. TMR JSON: direct value.
    /// 0 or None means sequential (no striding).
    pub stride_cachelines: Option<usize>,
    /// SimpleTest: stride in u64 elements (derived from stride_cachelines at runtime).
    /// `stride_cachelines * (cache_line_bytes / 8)`.
    pub stride_elements: Option<usize>,
    /// MirrorMove: number of subblocks (1-4). Each subblock is mirrored independently.
    pub subblock_count: Option<u32>,
    /// MirrorMove128: page stride in bytes. `(Parameter + 1) * 128`.
    pub page_stride_bytes: Option<usize>,
    /// CacheBust: number of interleaved stride pattern variants (default 4).
    pub stride_patterns: Option<u32>,
    /// RandomTorture: number of independent RNG sequences (default 8).
    pub rng_sequences: Option<u32>,
    /// StrideAccess: number of chunk subdivisions (default 4).
    pub subdivisions: Option<u32>,
    /// BlockMove: number of copy direction patterns (default 1).
    pub copy_directions: Option<u32>,
}

/// Interpret TM5 Parameter correctly based on test function name.
///
/// This replaces the old parameter mapping which incorrectly treated all
/// parameters as stream counts.
///
/// `channels`: memory channel count (default 2 for DDR5 dual-channel).
/// TM5 SimpleTest stride formula: `JumpStep = BlkSize * (Channels * Parameter - 1)`
/// where BlkSize=64 bytes (cache line). In cache line units: `Channels * Parameter - 1`.
pub fn interpret_tm5_parameter(function: &str, parameter: u32) -> TestParameterContext {
    interpret_tm5_parameter_with_channels(function, parameter, 2)
}

/// Interpret TM5 Parameter with explicit channel count.
pub fn interpret_tm5_parameter_with_channels(function: &str, parameter: u32, channels: u32) -> TestParameterContext {
    match function {
        "SimpleTest" | "Mem-Simple" | "Mem-SimpleV2" => {
            if parameter == 0 {
                TestParameterContext {
                    raw_parameter: parameter,
                    ..Default::default()
                }
            } else {
                // TM5: JumpStep = BlkSize * (Channels * Parameter - 1)
                let stride_cl = (channels as usize * parameter as usize).saturating_sub(1);
                TestParameterContext {
                    raw_parameter: parameter,
                    stride_cachelines: Some(stride_cl),
                    stride_elements: Some(stride_cl * 8), // 64-byte cache line / 8 bytes per u64
                    ..Default::default()
                }
            }
        }
        "MirrorMove" | "Mem-Mirror" | "Mem-MirrorV2" | "Mem-MirrorV2-Auto" => {
            // TM5 MirrorMove only branches on exactly 2, 3, or 4 subblocks.
            // Any other value (0, 1, 16384, etc.) falls through to single-block mirror (1).
            let subblocks = match parameter {
                2..=4 => parameter,
                _ => 1,
            };
            TestParameterContext {
                raw_parameter: parameter,
                subblock_count: Some(subblocks),
                ..Default::default()
            }
        }
        "MirrorMove128" | "MirrorMove256" | "MirrorMove512"
        | "Mem-MirrorV2-128" | "Mem-MirrorV2-256" | "Mem-MirrorV2-512" => {
            TestParameterContext {
                raw_parameter: parameter,
                page_stride_bytes: if parameter == 0 {
                    None
                } else {
                    Some((parameter as usize + 1) * 128)
                },
                ..Default::default()
            }
        }
        _ => {
            // Unknown test type — preserve raw parameter, no interpretation
            TestParameterContext {
                raw_parameter: parameter,
                ..Default::default()
            }
        }
    }
}

// Legacy config parser (v1.0 - TestMem5 format) - unchanged structure
#[derive(Debug, Clone)]
pub struct LegacyConfig {
    pub main_section: LegacyMainSection,
    pub memory_setup: LegacyMemorySetup,
    pub tests: Vec<LegacyTest>,
}

#[derive(Debug, Clone)]
pub struct LegacyMainSection {
    pub config_name: String,
    pub config_author: String,
    pub cores: u32,
    pub tests: u32,
    pub time_percent: u32,
    pub cycles: u32,
    pub test_sequence: Vec<u32>,
}

#[derive(Debug, Clone)]
pub struct LegacyMemorySetup {
    pub testing_window_size_mb: u32,
    pub reserved_memory_mb: u32,
    /// Memory channel count from .cfg (TM5: 1-3, default 2).
    /// Used in SimpleTest stride formula: `Channels * Parameter - 1` cache lines.
    pub channels: u32,
}

#[derive(Debug, Clone)]
pub struct LegacyTest {
    pub id: u32,
    pub enabled: bool,
    pub time_percent: u32,
    pub function: String,
    pub pattern_mode: u32,
    pub pattern_param0: u64,
    pub pattern_param1: u64,
    pub parameter: u32,
    pub test_chunk_size_mb: u32,
}

impl ModernConfig {
    pub fn load_from_file(path: &str) -> Result<Self, String> {
        let content = fs::read_to_string(path).map_err(|e| format!("Failed to read config file: {}", e))?;

        let config: ModernConfig = serde_json::from_str(&content).map_err(|e| format!("Failed to parse JSON config: {}", e))?;

        // Validate config version compatibility
        match config.config_format_version.as_str() {
            "2.0" => Ok(config),
            "1.0" => Err("Config format version 1.0 detected - please use legacy .cfg format or upgrade to v2.0".to_string()),
            version => Err(format!(
                "Unsupported config format version '{}' - TMR {} supports v2.0",
                version, APP_VERSION
            )),
        }
    }
	
    pub fn to_report(&self) -> String {
        let mut report = String::new();
        
        // Main configuration
        report.push_str(&format!("Configuration: {}\n", self.metadata.name));
        report.push_str(&format!("  Version: {} | Author: {}\n", self.metadata.version, self.metadata.author));
        if let Some(desc) = &self.metadata.description {
            report.push_str(&format!("  Description: {}\n", desc));
        }
        
        // System settings
        report.push_str(&format!("  CPU: {}% of {} ({})\n", 
            self.system.cpu_config.usage_percent,
            self.system.cpu_config.cpu_type,
            if self.system.large_pages { "Large Pages Enabled" } else { "Standard Pages" }
        ));
        
        // Memory strategy
        report.push_str("  Memory Strategy: ");
        match self.system.memory_strategy.allocation_mode.as_str() {
            "max_available" => report.push_str(&format!("Max Available (reserve {} MB)", 
                self.system.memory_strategy.reserve_mb.unwrap_or(0))),
            "percentage_reserve" => report.push_str(&format!("{}% Reserve", 
                self.system.memory_strategy.reserve_percent.unwrap_or(0.0))),
            "fixed_reserve" => report.push_str(&format!("{:.1} GiB Reserve", 
                self.system.memory_strategy.reserve_gib.unwrap_or(0.0))),
            _ => report.push_str("Unknown"),
        }
        report.push_str(&format!(", Window: {}, Chunk: {}\n",
            describe_window_spec(&self.system.memory_strategy.default_window),
            describe_chunk_spec(&self.system.memory_strategy.default_chunk)
        ));
        
        // Timing
        report.push_str("  Timing: ");
        match (self.system.timing.global_cycles, self.system.timing.global_duration_secs) {
            (Some(c), Some(d)) => report.push_str(&format!("{} cycles or {}s max", c, d)),
            (Some(c), None) => report.push_str(&format!("{} cycles", c)),
            (None, Some(d)) => report.push_str(&format!("{}s duration", d)),
            (None, None) => report.push_str("Unlimited"),
        }
        report.push_str(&format!(", Error Mode: {}\n", self.system.error_mode));
        
        // Test sequence summary
        let enabled_tests: Vec<_> = self.test_sequence.iter().filter(|t| t.enabled).collect();
        report.push_str(&format!("  Test Sequence: {} tests enabled\n", enabled_tests.len()));
        
        // Individual test details
        for (i, test) in enabled_tests.iter().enumerate() {
            report.push_str(&format!("    {}. {} - ", i + 1, test.function));
            
            // Timing
            match (&test.cycles, &test.duration_secs) {
                (Some(c), Some(d)) => report.push_str(&format!("{}cycles/{}s", c, d)),
                (Some(c), None) => report.push_str(&format!("{}cycles", c)),
                (None, Some(d)) => report.push_str(&format!("{}s", d)),
                _ => report.push_str("default timing"),
            }
            
            // TMR-native test parameters
            if let Some(sp) = test.stride_patterns {
                report.push_str(&format!(", stride_patterns={}", sp));
            }
            
            // Window override
            if let Some(spec) = &test.window {
                report.push_str(&format!(", Window:{}", describe_window_spec(spec)));
            }

            // Block override
            if let Some(spec) = &test.chunk {
                report.push_str(&format!(", Block:{}", describe_chunk_spec(spec)));
            }
            
            if test.allow_misaligned == Some(true) {
                report.push_str(", Misaligned");
            }
            if test.requires_locality == Some(true) {
                report.push_str(", Locality");
            }
            
            report.push('\n');
        }
        
        report
    }

    pub fn save_to_file(&self, path: &str) -> Result<(), String> {
        let json = serde_json::to_string_pretty(self).map_err(|e| format!("Failed to serialize config: {}", e))?;

        fs::write(path, json).map_err(|e| format!("Failed to write config file: {}", e))
    }

    // Convert to runtime allocation strategy (window/chunk modes are now test-specific)
    pub fn to_memory_strategy(&self) -> EnhancedMemoryStrategy {
        let allocation_mode = match self.system.memory_strategy.allocation_mode.as_str() {
            "max_available" => AllocationMode::ReserveFromAvailable { 
                reserve: ReserveAmount::Bytes((self.system.memory_strategy.reserve_mb.unwrap_or(128) as u64) * BYTES_PER_MIB)
            },
            "percentage_reserve" => AllocationMode::ReserveFromAvailable { 
                reserve: ReserveAmount::Percentage(self.system.memory_strategy.reserve_percent.unwrap_or(10.0))
            },
            "fixed_reserve" => AllocationMode::ReserveFromAvailable { 
                reserve: ReserveAmount::Bytes(gib_to_bytes(self.system.memory_strategy.reserve_gib.unwrap_or(2.0)))
            },
            _ => AllocationMode::ReserveFromAvailable { 
                reserve: ReserveAmount::Percentage(10.0) 
            },
        };

        EnhancedMemoryStrategy {
            allocation_mode,
            start_address_mode: StartAddressMode::default(),
        }
    }
    
    // Parse default window spec into runtime WindowMode
    pub fn get_default_window_mode(&self) -> WindowMode {
        spec_to_window_mode(&self.system.memory_strategy.default_window)
            .unwrap_or_else(|e| {
                log::warn!("Invalid default window spec ({}); using full_allocation", e);
                WindowMode::FullAllocation
            })
    }

    // Parse default chunk spec into runtime ChunkMode
    pub fn get_default_chunk_mode(&self) -> ChunkMode {
        spec_to_chunk_mode(&self.system.memory_strategy.default_chunk)
            .unwrap_or_else(|e| {
                log::warn!("Invalid default chunk spec ({}); using auto", e);
                ChunkMode::Auto
            })
    }

    pub fn to_error_mode(&self) -> ErrorMode {
        match self.system.error_mode.as_str() {
            "halt" | "stop" => ErrorMode::Halt,
            "panic" | "debug" => ErrorMode::Panic,
            _ => ErrorMode::Log, // default
        }
    }
    
    pub fn to_test_suite_timing(&self) -> TestSuiteTiming {
        TestSuiteTiming {
            global_cycles: self.system.timing.global_cycles,
            global_duration_secs: self.system.timing.global_duration_secs,
            per_test_cycle_multiplier: self.system.timing.default_test_cycles.unwrap_or(1) as f64,
        }
    }
    
    pub fn get_test_configs(&self) -> Vec<(&str, TestMemoryConfig)> {
        // Channels: prefer JSON system.channels, fall back to legacy TM5 metadata, default 2
        let channels = if self.system.channels > 0 {
            self.system.channels
        } else {
            self.legacy_metadata.as_ref().map_or(2, |m| m.tm5_channels)
        };
        self.test_sequence.iter().filter(|t| t.enabled).map(|test| {
            let window_mode = self.parse_test_window_mode(test);
            let chunk_mode = self.parse_test_chunk_mode(test);
            let allow_misaligned = test.allow_misaligned.unwrap_or(false);
            let requires_locality = test.requires_locality.unwrap_or({
                // Auto-detect based on function name
                matches!(test.function.as_str(), "Mem-CacheBust" | "Mem-Refresh")
            });

            let timing = TestTiming {
                cycles: test.cycles.or(self.system.timing.default_test_cycles),
                duration_secs: test.duration_secs.or(self.system.timing.default_test_duration_secs),
                min_duration_secs: test.min_duration_secs,
            };

            let mut config = TestMemoryConfig::new(window_mode, chunk_mode, allow_misaligned, requires_locality)
                .with_timing(timing)
                .with_pattern_config(test.pattern_mode, test.pattern_param0, test.pattern_param1)
                // #59: CLFLUSHOPT-verify, opt-in per test from the config
                .with_flush_before_verify(test.flush_before_verify.unwrap_or(false));

            // v2: Attach correctly interpreted parameter context
            if let Some(param) = test.parameter {
                config = config.with_parameter_context(
                    interpret_tm5_parameter_with_channels(&test.function, param, channels)
                );
            }

            // Fold TMR-native test parameters into parameter_context
            if test.stride_patterns.is_some() || test.rng_sequences.is_some()
                || test.subdivisions.is_some() || test.copy_directions.is_some()
            {
                let ctx = config.parameter_context.get_or_insert(TestParameterContext::default());
                if let Some(v) = test.stride_patterns { ctx.stride_patterns = Some(v); }
                if let Some(v) = test.rng_sequences { ctx.rng_sequences = Some(v); }
                if let Some(v) = test.subdivisions { ctx.subdivisions = Some(v); }
                if let Some(v) = test.copy_directions { ctx.copy_directions = Some(v); }
            }

            // TM5 SimpleTest: dLoopCounter=5 (write once, verify 5 times),
            // ST_WriteReadCycles=4 (repeat the write+verify sequence 4 times per chunk)
            if test.function.starts_with("Mem-Simple") || test.function == "SimpleTest" {
                config.verify_reps = 5;
                config.write_read_cycles = 4;
            }

            (test.function.as_str(), config)
        }).collect()
    }

    /// Get test configs in TM5 test sequence order (if available) with repetition support
    pub fn get_test_configs_with_sequence(&self) -> Vec<(&str, TestMemoryConfig)> {
        // Check if we have TM5 test sequence data
        if let Some(ref metadata) = self.legacy_metadata
            && !metadata.tm5_test_sequence.is_empty() {
                return self.get_tm5_sequence_configs(&metadata.tm5_test_sequence);
            }
        
        // Fallback to standard sequential execution
        self.get_test_configs()
    }
    
    /// Get test configs following TM5 test sequence order and repetition
    fn get_tm5_sequence_configs(&self, sequence: &[u32]) -> Vec<(&str, TestMemoryConfig)> {
        // Channels: prefer JSON system.channels, fall back to legacy TM5 metadata, default 2
        let channels = if self.system.channels > 0 {
            self.system.channels
        } else {
            self.legacy_metadata.as_ref().map_or(2, |m| m.tm5_channels)
        };
        let mut result = Vec::new();

        for &test_index in sequence {
            // Find the test by index (TM5 uses 0-based indexing)
            if let Some(test) = self.test_sequence.get(test_index as usize) {
                if test.enabled {
                    let window_mode = self.parse_test_window_mode(test);
                    let chunk_mode = self.parse_test_chunk_mode(test);
                    let allow_misaligned = test.allow_misaligned.unwrap_or(false);
                    let requires_locality = test.requires_locality.unwrap_or({
                        // Auto-detect based on function name
                        matches!(test.function.as_str(), "Mem-CacheBust" | "Mem-Refresh")
                    });

                    let timing = TestTiming {
                        cycles: test.cycles.or(self.system.timing.default_test_cycles),
                        duration_secs: test.duration_secs.or(self.system.timing.default_test_duration_secs),
                        min_duration_secs: test.min_duration_secs,
                    };

                    let mut config = TestMemoryConfig::new(window_mode, chunk_mode, allow_misaligned, requires_locality)
                        .with_timing(timing)
                        .with_pattern_config(test.pattern_mode, test.pattern_param0, test.pattern_param1);

                    // v2: Attach correctly interpreted parameter context
                    if let Some(param) = test.parameter {
                        config = config.with_parameter_context(
                            interpret_tm5_parameter_with_channels(&test.function, param, channels)
                        );
                    }

                    // Fold TMR-native test parameters into parameter_context
                    if test.stride_patterns.is_some() || test.rng_sequences.is_some()
                        || test.subdivisions.is_some() || test.copy_directions.is_some()
                    {
                        let ctx = config.parameter_context.get_or_insert(TestParameterContext::default());
                        if let Some(v) = test.stride_patterns { ctx.stride_patterns = Some(v); }
                        if let Some(v) = test.rng_sequences { ctx.rng_sequences = Some(v); }
                        if let Some(v) = test.subdivisions { ctx.subdivisions = Some(v); }
                        if let Some(v) = test.copy_directions { ctx.copy_directions = Some(v); }
                    }

                    // TM5 SimpleTest: dLoopCounter=5 (write once, verify 5 times),
                    // ST_WriteReadCycles=4 (repeat the write+verify sequence 4 times per chunk)
                    if test.function.starts_with("Mem-Simple") || test.function == "SimpleTest" {
                        config.verify_reps = 5;
                        config.write_read_cycles = 4;
                    }

                    result.push((test.function.as_str(), config));
                }
            } else {
                log::warn!("TM5 test sequence references invalid test index: {}", test_index);
            }
        }
        
        if result.is_empty() {
            log::warn!("TM5 test sequence produced no valid tests, falling back to sequential order");
            return self.get_test_configs();
        }
        
        result
    }
    
fn parse_test_window_mode(&self, test: &TestConfig) -> WindowMode {
    if let Some(ref spec) = test.window {
        match spec_to_window_mode(spec) {
            Ok(mode) => mode,
            Err(e) => {
                log::warn!("Invalid per-test window spec for '{}' ({}); falling back to global default",
                    test.function, e);
                self.get_default_window_mode()
            }
        }
    } else {
        self.get_default_window_mode()
    }
}

fn parse_test_chunk_mode(&self, test: &TestConfig) -> ChunkMode {
    if let Some(ref spec) = test.chunk {
        match spec_to_chunk_mode(spec) {
            Ok(mode) => mode,
            Err(e) => {
                log::warn!("Invalid per-test chunk spec for '{}' ({}); falling back to global default",
                    test.function, e);
                self.get_default_chunk_mode()
            }
        }
    } else {
        self.get_default_chunk_mode()
    }
}

pub fn create_demo_config() -> Self {
    ModernConfig {
        config_format_version: CONFIG_VERSION.to_string(),
        application_name: format!("{} ({})", APP_NAME, APP_SHORT_NAME),
        metadata: ConfigMetadata {
            name: "TMR Quick Demo Test".to_string(),
            author: "tmr_user".to_string(),
            version: "1.0".to_string(),
            description: Some("Quick 1-cycle demo showcasing each test with optimal configurations".to_string()),
            created: Some("2025-01-03".to_string()),
            tested_with_version: APP_VERSION.to_string(),
        },
        system: SystemConfig {
            memory_strategy: MemoryStrategyConfig {
                allocation_mode: "percentage_reserve".to_string(),
                reserve_mb: None,
                reserve_percent: Some(10.0),           // Reserve 10% for OS
                reserve_gib: None,
                default_window: WindowSpec::full_allocation(),
                default_chunk: ChunkSpec::auto(),
            },
            cpu_config: CpuConfig {
                cpu_type: "cores".to_string(),
                usage_percent: 100,
            },
            error_mode: "log".to_string(),
            timing: TimingConfig {
                global_cycles: Some(1),                // Just 1 global cycle for demo
                global_duration_secs: None,
                default_test_cycles: Some(1),          // Default 1 cycle per test
                default_test_duration_secs: None,
            },
            large_pages: true,
			cpu_pinning: CpuPinningConfig::default(),
			memory_allocation: MemoryAllocationConfig::default(),
            channels: 2,
        },
        test_sequence: vec![
            // Critical: Full memory stuck bit test
            TestConfig {
                enabled: true,
                function: "Mem-StuckBit".to_string(),
                cycles: Some(1),                       // 1 cycle is thorough enough
                duration_secs: None,
                min_duration_secs: None,
                window: Some(WindowSpec::full_allocation()), // Must test ALL memory
                chunk: Some(ChunkSpec::fraction(0.0625)),    // 1/16th for efficiency
                allow_misaligned: Some(false),
                requires_locality: Some(false),
                flush_before_verify: None,
                stride_patterns: None,
                rng_sequences: None,
                subdivisions: None,
                copy_directions: None,
                pattern_mode: None,
                pattern_param0: None,
                pattern_param1: None,
                parameter: None,
                use_v2_tests: None,
            },

            // Mem-Refresh - needs small window for refresh timing
            TestConfig {
                enabled: true,
                function: "Mem-Refresh".to_string(),
                cycles: Some(1),
                duration_secs: None,
                min_duration_secs: None,
                window: Some(WindowSpec::cache_total(2.0)), // 2x cache for refresh testing
                chunk: Some(ChunkSpec::absolute("1MB")),    // Small 1MB blocks
                allow_misaligned: Some(false),
                requires_locality: Some(true),
                flush_before_verify: None,
                stride_patterns: None,
                rng_sequences: None,
                subdivisions: None,
                copy_directions: None,
                pattern_mode: None,
                pattern_param0: None,
                pattern_param1: None,
                parameter: None,
                use_v2_tests: None,
            },
            
            // Mem-Simple - general pattern test with TM5 compatibility
            TestConfig {
                enabled: true,
                function: "Mem-SimpleV2".to_string(),
                cycles: Some(1),
                duration_secs: None,
                min_duration_secs: None,
                window: Some(WindowSpec::absolute("880MB")), // TM5 default window
                chunk: Some(ChunkSpec::absolute("16MB")),    // TM5 typical block size
                allow_misaligned: Some(false),
                requires_locality: Some(false),
                flush_before_verify: None,
                stride_patterns: None,
                rng_sequences: None,
                subdivisions: None,
                copy_directions: None,
                pattern_mode: Some(1),
                pattern_param0: Some(0x1E5F),
                pattern_param1: Some(0x45357354),
                parameter: None,
                use_v2_tests: None,
            },

            // Mem-MirrorV2-128 - SIMD test with optimal locality
            TestConfig {
                enabled: true,
                function: "Mem-MirrorV2-128".to_string(),
                cycles: Some(1),
                duration_secs: None,
                min_duration_secs: None,
                window: Some(WindowSpec::absolute("64MB")),  // Good SIMD locality
                chunk: Some(ChunkSpec::absolute("16MB")),    // 16MB for 128-bit alignment
                allow_misaligned: Some(false),
                requires_locality: Some(true),
                flush_before_verify: None,
                stride_patterns: None,
                rng_sequences: None,
                subdivisions: None,
                copy_directions: None,
                pattern_mode: None,
                pattern_param0: None,
                pattern_param1: None,
                parameter: None,
                use_v2_tests: None,
            },
            
            // Mem-MirrorV2-256 - AVX2 with dual subblocks
            TestConfig {
                enabled: true,
                function: "Mem-MirrorV2-256".to_string(),
                cycles: Some(1),
                duration_secs: None,
                min_duration_secs: None,
                window: Some(WindowSpec::absolute("128MB")), // Larger for AVX2
                chunk: Some(ChunkSpec::absolute("32MB")),    // 32MB for 256-bit alignment
                allow_misaligned: Some(false),
                requires_locality: Some(true),
                flush_before_verify: None,
                stride_patterns: None,
                rng_sequences: None,
                subdivisions: None,
                copy_directions: None,
                pattern_mode: None,
                pattern_param0: None,
                pattern_param1: None,
                parameter: None,
                use_v2_tests: None,
            },
            
            // Mem-CacheBust - specifically sized for cache stress
            TestConfig {
                enabled: true,
                function: "Mem-CacheBust".to_string(),
                cycles: Some(1),
                duration_secs: None,
                min_duration_secs: None,
                window: Some(WindowSpec::cache_total(0.5)), // Half total cache to ensure busting
                chunk: Some(ChunkSpec::absolute("1MB")),    // 1MB blocks for cache lines
                allow_misaligned: Some(false),
                requires_locality: Some(true),
                flush_before_verify: None,
                stride_patterns: Some(4),              // 4 interleaved stride patterns
                rng_sequences: None,
                subdivisions: None,
                copy_directions: None,
                pattern_mode: None,
                pattern_param0: None,
                pattern_param1: None,
                parameter: None,
                use_v2_tests: None,
            },
            
            // Mem-Random - full memory random access
            TestConfig {
                enabled: true,
                function: "Mem-Random".to_string(),
                cycles: Some(1),
                duration_secs: None,
                min_duration_secs: None,
                window: Some(WindowSpec::full_allocation()), // Need full memory
                chunk: Some(ChunkSpec::absolute("8MB")),    // 8MB blocks
                allow_misaligned: Some(true),          // Maximum stress
                requires_locality: Some(false),
                flush_before_verify: None,
                stride_patterns: None,
                rng_sequences: Some(8),                // 8 independent RNG sequences
                subdivisions: None,
                copy_directions: None,
                pattern_mode: None,
                pattern_param0: None,
                pattern_param1: None,
                parameter: None,
                use_v2_tests: None,
            },
            
            // Mem-Stride - test various stride patterns
            TestConfig {
                enabled: true,
                function: "Mem-Stride".to_string(),
                cycles: Some(1),
                duration_secs: None,
                min_duration_secs: None,
                window: Some(WindowSpec::full_allocation()),
                chunk: Some(ChunkSpec::auto()),             // Let TMR optimize
                allow_misaligned: Some(false),
                requires_locality: Some(false),
                flush_before_verify: None,
                stride_patterns: None,
                rng_sequences: None,
                subdivisions: Some(4),                 // 4 chunk subdivisions
                copy_directions: None,
                pattern_mode: None,
                pattern_param0: None,
                pattern_param1: None,
                parameter: None,
                use_v2_tests: None,
            },
            
            // Mem-BlockMove - memory copy test
            TestConfig {
                enabled: true,
                function: "Mem-BlockMove".to_string(),
                cycles: Some(1),
                duration_secs: None,
                min_duration_secs: None,
                window: Some(WindowSpec::full_allocation()), // Need src+dst space
                chunk: Some(ChunkSpec::absolute("16MB")),    // 16MB blocks
                allow_misaligned: Some(false),
                requires_locality: Some(false),
                flush_before_verify: None,
                stride_patterns: None,
                rng_sequences: None,
                subdivisions: None,
                copy_directions: Some(2),              // Forward + backward copy
                pattern_mode: None,
                pattern_param0: None,
                pattern_param1: None,
                parameter: None,
                use_v2_tests: None,
            },
            
            // Legacy TM5-style test showing "window-size" block mode
            TestConfig {
                enabled: true,
                function: "Mem-SimpleV2".to_string(),
                cycles: Some(1),
                duration_secs: None,
                min_duration_secs: None,
                window: None,                                // Use global default
                chunk: Some(ChunkSpec::fraction(1.0)),       // Block = window (TM5 0)
                allow_misaligned: Some(false),
                requires_locality: Some(false),
                flush_before_verify: None,
                stride_patterns: None,
                rng_sequences: None,
                subdivisions: None,
                copy_directions: None,
                pattern_mode: Some(0),
                pattern_param0: Some(0),
                pattern_param1: Some(0),
                parameter: None,
                use_v2_tests: None,
            },
        ],
        legacy_metadata: None,
    }
}
    
    pub fn create_tm5_compatible_config() -> Self {
        ModernConfig {
            config_format_version: CONFIG_VERSION.to_string(),
            application_name: format!("{} ({})", APP_NAME, APP_SHORT_NAME),
            metadata: ConfigMetadata {
                name: "TM5-Compatible Memory Test".to_string(),
                author: "tmr_user".to_string(),
                version: "1.0".to_string(),
                description: Some("TM5-compatible configuration with maximum memory allocation and fixed testing window".to_string()),
                created: Some("2025-06-29".to_string()),
                tested_with_version: APP_VERSION.to_string(),
            },
            system: SystemConfig {
                memory_strategy: MemoryStrategyConfig {
                    allocation_mode: "max_available".to_string(),
                    reserve_mb: Some(128),                 // TM5-style fixed reserve
                    reserve_percent: None,
                    reserve_gib: None,
                    default_window: WindowSpec::absolute("880MB"), // TM5 default window
                    default_chunk: ChunkSpec::auto(),
                },
                cpu_config: CpuConfig {
                    cpu_type: "cores".to_string(),
                    usage_percent: 100,
                },
                error_mode: "log".to_string(),
                timing: TimingConfig {
                    global_cycles: Some(3),
                    global_duration_secs: None,
                    default_test_cycles: Some(1),          // TM5-style single runs
                    default_test_duration_secs: None,
                },
                large_pages: true,
				cpu_pinning: CpuPinningConfig::default(),
				memory_allocation: MemoryAllocationConfig::default(),
                channels: 2,
	            },
            test_sequence: vec![
                TestConfig {
                    enabled: true,
                    function: "Mem-StuckBit".to_string(),
                    cycles: Some(1),
                    duration_secs: None,
                    min_duration_secs: None,
                    window: Some(WindowSpec::full_allocation()), // Override to test all memory
                    chunk: None,                                 // Use default auto
                    allow_misaligned: Some(false),
                    requires_locality: Some(false),
                    flush_before_verify: None,
                    stride_patterns: None,
                    rng_sequences: None,
                    subdivisions: None,
                    copy_directions: None,
                    pattern_mode: None,
                    pattern_param0: None,
                    pattern_param1: None,
                    parameter: None,
                    use_v2_tests: None,
                },
                TestConfig {
                    enabled: true,
                    function: "Mem-SimpleV2".to_string(),
                    cycles: Some(1),
                    duration_secs: None,
                    min_duration_secs: None,
                    window: None,                                // Use default 880MB
                    chunk: Some(ChunkSpec::absolute("16MB")),    // TM5-style block size
                    allow_misaligned: Some(false),
                    requires_locality: Some(false),
                    flush_before_verify: None,
                    stride_patterns: None,
                    rng_sequences: None,
                    subdivisions: None,
                    copy_directions: None,
                    pattern_mode: Some(1),
                    pattern_param0: Some(0x1E5F),
                    pattern_param1: Some(0x45357354),
                    parameter: None,
                    use_v2_tests: None,
                },
            ],
            legacy_metadata: None,
        }
    }
}

impl LegacyConfig {
    pub fn load_from_file(path: &str) -> Result<Self, String> {
        let content = fs::read_to_string(path).map_err(|e| format!("Failed to read legacy config file: {}", e))?;

        Self::parse_legacy_format(&content)
    }

    fn parse_legacy_format(content: &str) -> Result<Self, String> {
        let mut sections: HashMap<String, HashMap<String, String>> = HashMap::new();
        let mut current_section = String::new();

        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            if line.starts_with('[') && line.ends_with(']') {
                current_section = line[1..line.len() - 1].to_string();
                sections.insert(current_section.clone(), HashMap::new());
            } else if let Some(eq_pos) = line.find('=') {
                let key = line[..eq_pos].trim().to_string();
                let value = line[eq_pos + 1..].trim().to_string();
                if let Some(section) = sections.get_mut(&current_section) {
                    section.insert(key, value);
                }
            }
        }

        // Parse main section
        let main = sections.get("Main Section").ok_or("Missing [Main Section]")?;

        let test_sequence = main
            .get("Test Sequence")
            .map(|s| s.split(',').filter_map(|n| n.trim().parse::<u32>().ok()).collect())
            .unwrap_or_default();

        let main_section = LegacyMainSection {
            config_name: main.get("Config Name").unwrap_or(&"Unknown".to_string()).clone(),
            config_author: main.get("Config Author").unwrap_or(&"Unknown".to_string()).clone(),
            cores: main.get("Cores").and_then(|s| s.parse().ok()).unwrap_or(0),
            tests: main.get("Tests").and_then(|s| s.parse().ok()).unwrap_or(0),
            time_percent: main.get("Time (%)").and_then(|s| s.parse().ok()).unwrap_or(100),
            cycles: main.get("Cycles").and_then(|s| s.parse().ok()).unwrap_or(1),
            test_sequence,
        };

        // Parse memory setup
        let memory = sections.get("Global Memory Setup").ok_or("Missing [Global Memory Setup]")?;

        let memory_setup = LegacyMemorySetup {
            testing_window_size_mb: memory.get("Testing Window Size (Mb)").and_then(|s| s.parse().ok()).unwrap_or(880),
            reserved_memory_mb: memory
                .get("Reserved Memory for Windows (Mb)")
                .and_then(|s| s.parse().ok())
                .unwrap_or(128),
            channels: memory.get("Channels").and_then(|s| s.parse().ok()).unwrap_or(2).clamp(1, 8),
        };

        // Parse tests
        let mut tests = Vec::new();
        for i in 0..=15 {
            let test_section = format!("Test{}", i);
            if let Some(test) = sections.get(&test_section) {
                let legacy_test = LegacyTest {
                    id: i,
                    enabled: test.get("Enable").and_then(|s| s.parse::<u32>().ok()).unwrap_or(0) == 1,
                    time_percent: test.get("Time (%)").and_then(|s| s.parse().ok()).unwrap_or(100),
                    function: test.get("Function").unwrap_or(&"Unknown".to_string()).clone(),
                    pattern_mode: test.get("Pattern Mode").and_then(|s| s.parse().ok()).unwrap_or(0),
                    pattern_param0: test
                        .get("Pattern Param0")
                        .and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok())
                        .unwrap_or(0),
                    pattern_param1: test
                        .get("Pattern Param1")
                        .and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok())
                        .unwrap_or(0),
                    parameter: test.get("Parameter").and_then(|s| s.parse().ok()).unwrap_or(0),
                    test_chunk_size_mb: test.get("Test Block Size (Mb)").and_then(|s| s.parse().ok()).unwrap_or(0),
                };
                tests.push(legacy_test);
            }
        }

        Ok(LegacyConfig {
            main_section,
            memory_setup,
            tests,
        })
    }

    // Convert legacy config to modern config v2.0
 pub fn to_modern_config(&self) -> Result<ModernConfig, String> {
    let global_time_multiplier = self.main_section.time_percent as f64 / 100.0;
    
    // Start with empty test sequence - only add what's in the config
    let mut test_sequence = Vec::new();
    
    // Add legacy tests WITHOUT auto-inserting StuckBitTest
    for test in &self.tests {
        if test.enabled {
            // Calculate effective cycles based on Time(%)
            // TM5: Time(%)=100 = 1 cycle, Time(%)=200 = 2 cycles, etc.
            let base_cycles = test.time_percent as f64 / 100.0;
            let effective_cycles = ((base_cycles * global_time_multiplier).ceil() as u32).max(1);
            
            test_sequence.push(TestConfig {
                enabled: true,
                function: Self::map_legacy_function(&test.function)?,
                
                // Use cycles for TM5 Time(%) compatibility
                cycles: Some(effective_cycles),
                duration_secs: None,  // Don't use duration-based timing
                min_duration_secs: None,
                
                // Handle TM5 window behavior - no overrides for legacy
                window: None,  // Use global default

                // Handle TM5 block size: 0 = use window size, else absolute MB
                chunk: Some(if test.test_chunk_size_mb == 0 {
                    ChunkSpec::fraction(1.0)  // 0 = use entire window
                } else {
                    ChunkSpec::absolute(&format!("{}MB", test.test_chunk_size_mb))
                }),
                
                allow_misaligned: Some(false), // Legacy configs assume aligned access
                requires_locality: Some(matches!(test.function.as_str(), "RefreshStable")),
                flush_before_verify: None,
                
                stride_patterns: None,
                rng_sequences: None,
                subdivisions: None,
                copy_directions: None,
                
                // Preserve legacy test parameters
                pattern_mode: Some(test.pattern_mode),
                pattern_param0: Some(test.pattern_param0),
                pattern_param1: Some(test.pattern_param1),
                parameter: Some(test.parameter),
                use_v2_tests: None,
            });
        }
    }

    Ok(ModernConfig {
        config_format_version: CONFIG_VERSION.to_string(),
        application_name: format!("{} ({})", APP_NAME, APP_SHORT_NAME),
        metadata: ConfigMetadata {
            name: format!("{} (Legacy Converted)", self.main_section.config_name),
            author: self.main_section.config_author.clone(),
            version: "1.0".to_string(),
            description: Some("Converted from legacy TestMem5 config".to_string()),
            created: None,
            tested_with_version: APP_VERSION.to_string(),
        },
        system: SystemConfig {
            memory_strategy: MemoryStrategyConfig {
                allocation_mode: "max_available".to_string(),
                reserve_mb: Some(self.memory_setup.reserved_memory_mb),
                reserve_percent: None,
                reserve_gib: None,
                default_window: WindowSpec::absolute(&format!("{}MB", self.memory_setup.testing_window_size_mb)),
                default_chunk: ChunkSpec::auto(),
            },
            cpu_config: CpuConfig {
                cpu_type: if self.main_section.cores > 0 { "cores" } else { "threads" }.to_string(),
                usage_percent: 100,
            },
            error_mode: "log".to_string(),
            timing: TimingConfig {
                global_cycles: Some(self.main_section.cycles),
                global_duration_secs: None,  // TM5 doesn't use duration
                default_test_cycles: None,   // Each test has its own from Time(%)
                default_test_duration_secs: None,
            },
            large_pages: true,
			cpu_pinning: CpuPinningConfig::default(),
			memory_allocation: MemoryAllocationConfig::default(),
            channels: self.memory_setup.channels,
        },
        test_sequence,
        legacy_metadata: Some(LegacyMetadata {
            tm5_test_sequence: self.main_section.test_sequence.clone(),
            tm5_cycles: self.main_section.cycles,
            tm5_time_percent: self.main_section.time_percent,
            tm5_channels: self.memory_setup.channels,
        }),
    })
}

    // Map legacy function names to modern equivalents

fn map_legacy_function(legacy_name: &str) -> Result<String, String> {
    match legacy_name {
        // TM5 legacy names -> new prefixed names
        "RefreshStable" => Ok("Mem-Refresh".to_string()),
        "SimpleTest" => Ok("Mem-SimpleV2".to_string()),
        "MirrorMove" => Ok("Mem-MirrorV2-128".to_string()),  // TM5 base MirrorMove -> v2 128-bit SIMD
        "MirrorMove128" => Ok("Mem-MirrorV2-128".to_string()),
        "MirrorMove256" => Ok("Mem-MirrorV2-256".to_string()),
        "MirrorMove512" => Ok("Mem-MirrorV2-512".to_string()),
        "BlockMove" => Ok("Mem-BlockMove".to_string()),
        // Also accept new names directly
        "Mem-Refresh" => Ok("Mem-Refresh".to_string()),
        "Mem-Simple" => Ok("Mem-SimpleV2".to_string()),
        "Mem-Mirror" => Ok("Mem-MirrorV2".to_string()),
        "Mem-Mirror128" => Ok("Mem-MirrorV2-128".to_string()),
        "Mem-Mirror256" => Ok("Mem-MirrorV2-256".to_string()),
        "Mem-Mirror512" => Ok("Mem-MirrorV2-512".to_string()),
        "Mem-BlockMove" => Ok("Mem-BlockMove".to_string()),
        "Mem-StuckBit" => Ok("Mem-StuckBit".to_string()),
        "Mem-CacheBust" => Ok("Mem-CacheBust".to_string()),
        "Mem-Random" => Ok("Mem-Random".to_string()),
        "Mem-Stride" => Ok("Mem-Stride".to_string()),
        // v2 test names (accepted directly)
        "Mem-SimpleV2" => Ok("Mem-SimpleV2".to_string()),
        "Mem-MirrorV2" => Ok("Mem-MirrorV2".to_string()),
        "Mem-MirrorV2-128" => Ok("Mem-MirrorV2-128".to_string()),
        "Mem-MirrorV2-256" => Ok("Mem-MirrorV2-256".to_string()),
        "Mem-MirrorV2-512" => Ok("Mem-MirrorV2-512".to_string()),
        "Mem-MirrorV2-Auto" => Ok("Mem-MirrorV2-Auto".to_string()),
        _ => Err(format!("Unknown legacy test function: '{}'", legacy_name))
    }
}

}

// Configuration loader that handles both formats
pub fn load_config(path: &str) -> Result<ModernConfig, String> {
    if !Path::new(path).exists() {
        return Err(format!("Config file does not exist: {}", path));
    }

    // Try to detect format by file extension or content
    if path.ends_with(".json") {
        ModernConfig::load_from_file(path)
    } else if path.ends_with(".cfg") {
        // Legacy format (v1.0)
        let legacy = LegacyConfig::load_from_file(path)?;
        legacy.to_modern_config()
    } else {
        // Try JSON first, then legacy
        ModernConfig::load_from_file(path).or_else(|_| {
            let legacy = LegacyConfig::load_from_file(path)?;
            legacy.to_modern_config()
        })
    }
}

// Generate demo configs
pub fn create_demo_configs() -> Result<(), String> {
    // Create modern comprehensive config
    let modern_config = ModernConfig::create_demo_config();
    modern_config.save_to_file("demo_comprehensive_test.json")?;

    // Create TM5-compatible config
    let tm5_config = ModernConfig::create_tm5_compatible_config();
    tm5_config.save_to_file("demo_tm5_compatible.json")?;

    println!("✅ Created demo_comprehensive_test.json - Modern comprehensive memory testing");
    println!("   Features: Full memory stuck bit test + timed stress tests");
    println!("   Timing: 3 cycles, ~2-3 minutes per cycle with comprehensive coverage");
    println!("   Memory: Uses full allocation for critical tests, optimized windows for others");
    println!();
    println!("✅ Created demo_tm5_compatible.json - TM5-compatible configuration");
    println!("   Features: TM5-style allocation with modern stuck bit test added");
    println!("   Timing: 3 cycles, faster execution for compatibility");
    println!("   Memory: Maximum allocation minus 128MB reserve, 880MB testing window");
    println!();
    println!("Configuration Architecture Summary:");
    println!("  Stage 1: Memory Allocation - Maximum available memory per thread");
    println!("  Stage 2: Testing Window - Configurable window within allocation");
    println!("  Stage 3: Block/Chunk Size - Auto-optimized per test with alignment");
    println!("  Timing: Per-test cycles/duration limits + global suite limits");
    println!("  Critical: Mem-StuckBit ensures full memory coverage for bit errors");

    Ok(())
}