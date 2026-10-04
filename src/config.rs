use crate::{ErrorMode};
use crate::constants::{gib_to_bytes, BYTES_PER_MIB};
use crate::tests::{ExtentMode, ChunkMode, CacheTarget, parse_size_string};
use crate::memory::allocation_strategy::{EnhancedMemoryStrategy, AllocationMode, ReserveAmount, RoundDirection, ShareRounding};
use crate::memory::allocator::{BlockSizing, DEFAULT_LARGE_CHUNK};
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
#[serde(deny_unknown_fields)]
pub struct ModernConfig {
    pub config_format_version: String,
    pub application_name: String,
    pub metadata: ConfigMetadata,
    pub system: SystemConfig,
    pub test_sequence: Vec<TestConfig>,
    pub legacy_metadata: Option<LegacyMetadata>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegacyMetadata {
    pub tm5_test_sequence: Vec<u32>,
    pub tm5_cycles: u32,
    pub tm5_time_percent: u32,
    /// Memory channel count from TM5 .cfg (default 2). Used in stride formula.
    #[serde(default = "default_channels")]
    pub tm5_channels: u32,
    /// The `.cfg`'s Testing Window Size and Lock Memory Granularity (MiB): they size block codes
    /// 0-3 and cap larger blocks, nothing else (TODO 76). Shown in the plan.
    #[serde(default)]
    pub tm5_window_mb: u32,
    #[serde(default)]
    pub tm5_lock_mb: u32,
}

fn default_channels() -> u32 { 2 }

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigMetadata {
    pub name: String,
    pub author: String,
    pub version: String,
    pub description: Option<String>,
    pub created: Option<String>,
    pub tested_with_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct MemoryAllocationConfig {
    // Page size constraints (using your existing system)
    #[serde(default = "default_min_page_size")]
    pub min_page_size: String,                 // "regular", "large", "huge"
    
    #[serde(default = "default_max_page_size")]
    pub max_page_size: String,                 // "regular", "large", "huge"

    // Block sizes, checked by `block_sizing` (see `BlockSizing`). Sizes as `parse_size_string`.
    #[serde(default = "default_huge_chunk")]
    pub huge_chunk: String,                    // `hugechunk`: first size of each 1 GiB-page request
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub large_chunk: Option<String>,           // `largechunk`: first size of each 2 MiB-page request; unset = 128 MiB
    #[serde(default = "default_large_floor")]
    pub large_floor: String,                   // `largefloor`: smallest 2 MiB-page request

    // Per-thread share rounding before any allocator runs, checked by `share_rounding`
    #[serde(default = "default_share_round_step")]
    pub share_round_step: String,              // `blkroundtarget`: round each thread's share to a multiple of this
    #[serde(default = "default_share_round")]
    pub share_round: String,                   // `blkround`: "up", "down", "nearest"
}

fn default_min_page_size() -> String { "large".to_string() }
fn default_max_page_size() -> String { "huge".to_string() }
fn default_huge_chunk() -> String { "1GiB".to_string() }
fn default_large_floor() -> String { "16MiB".to_string() }
fn default_share_round_step() -> String { "1GiB".to_string() }
fn default_share_round() -> String { "up".to_string() }

impl Default for MemoryAllocationConfig {
    fn default() -> Self {
        Self {
            min_page_size: default_min_page_size(),
            max_page_size: default_max_page_size(),
            huge_chunk: default_huge_chunk(),
            large_chunk: None,
            large_floor: default_large_floor(),
            share_round_step: default_share_round_step(),
            share_round: default_share_round(),
        }
    }
}

impl MemoryAllocationConfig {
    /// The command-line keys `set_block_param` takes.
    pub const BLOCK_PARAMS: [&'static str; 5] = ["hugechunk", "largechunk", "largefloor", "blkroundtarget", "blkround"];

    /// Sets the field a `BLOCK_PARAMS` key names. Returns false for any other key.
    pub fn set_block_param(&mut self, key: &str, value: &str) -> bool {
        if key == "largechunk" {
            self.large_chunk = Some(value.to_string());
            return true;
        }
        let field = match key {
            "hugechunk" => &mut self.huge_chunk,
            "largefloor" => &mut self.large_floor,
            "blkroundtarget" => &mut self.share_round_step,
            "blkround" => &mut self.share_round,
            _ => return false,
        };
        *field = value.to_string();
        true
    }

    /// `hugechunk`, `largechunk` and `largefloor`, parsed and checked. An unset `largechunk` is
    /// `DEFAULT_LARGE_CHUNK`, never below `largefloor`.
    pub fn block_sizing(&self) -> Result<BlockSizing, String> {
        let size = |key: &str, value: &str| parse_size_string(value).map_err(|e| format!("{key}={value}: {e}"));
        let large_floor = size("largefloor", &self.large_floor)?;
        let large_chunk = match &self.large_chunk {
            Some(value) => size("largechunk", value)?,
            None => DEFAULT_LARGE_CHUNK.max(large_floor),
        };
        BlockSizing::new(size("hugechunk", &self.huge_chunk)?, large_chunk, large_floor)
    }

    /// `blkroundtarget` and `blkround`, parsed and checked. The step must be a multiple of
    /// `largefloor`, so the smallest request can fill a share exactly, and no bigger than the
    /// largest request (`hugechunk` or `largechunk`). `largefloor` is also the finest step a
    /// share falls back to when rounding would pass the memory there is.
    pub fn share_rounding(&self) -> Result<ShareRounding, String> {
        let sizing = self.block_sizing()?;
        let step = parse_size_string(&self.share_round_step)
            .map_err(|e| format!("blkroundtarget={}: {e}", self.share_round_step))?;
        let mib = |b: usize| b / BYTES_PER_MIB as usize;
        let top = sizing.largest_request();
        if step < sizing.large_floor || !step.is_multiple_of(sizing.large_floor) || step > top {
            return Err(format!(
                "blkroundtarget={} MiB must be a multiple of largefloor ({} MiB) and at most the largest request, hugechunk or largechunk ({} MiB)",
                mib(step), mib(sizing.large_floor), mib(top)));
        }
        let direction: RoundDirection = self.share_round.parse()?;
        Ok(ShareRounding { step_bytes: step as u64, direction, floor_bytes: sizing.large_floor as u64 })
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

impl ExtentSpec {
    pub fn full_allocation() -> Self {
        ExtentSpec { mode: "full_allocation".to_string(), ..Default::default() }
    }
    pub fn cache_total(fraction: f64) -> Self {
        ExtentSpec { mode: "cache_total".to_string(), fraction: Some(fraction), ..Default::default() }
    }
    pub fn absolute(size: &str) -> Self {
        ExtentSpec { mode: "absolute".to_string(), size: Some(size.to_string()), ..Default::default() }
    }
}

impl ChunkSpec {
    pub fn auto() -> Self {
        ChunkSpec { mode: "auto".to_string(), ..Default::default() }
    }
    pub fn absolute(size: &str) -> Self {
        ChunkSpec { mode: "absolute".to_string(), size: Some(size.to_string()), ..Default::default() }
    }
    /// A TM5 block code 0-3 over a `.cfg` window of `window_mb`: see `ChunkMode::Tm5Block`.
    pub fn tm5_block(window_mb: u64, divisor: u32, granularity_mb: u64) -> Self {
        ChunkSpec {
            mode: "tm5_block".to_string(),
            size: Some(format!("{window_mb}MiB")),
            divisor: Some(divisor),
            granularity: Some(format!("{granularity_mb}MiB")),
            ..Default::default()
        }
    }
}

/// Per-chunk repetition for one test: its own `verify_reps` / `write_read_cycles` / `test_reps`
/// when set, else the test's default. SimpleTest defaults to TM5's loop at 100 % / 100 %:
/// 4 x (1 fill + 5 verifies) per chunk (`mtests0.asm` ST_Check :298-308).
fn apply_repetition(test: &TestConfig, config: &mut TestMemoryConfig) {
    let simple = test.function.starts_with("Mem-Simple") || test.function == "SimpleTest";
    if simple {
        config.verify_reps = 5;
        config.write_read_cycles = 4;
    }
    if let Some(reps) = test.verify_reps {
        config.verify_reps = reps;
    }
    if let Some(cycles) = test.write_read_cycles {
        config.write_read_cycles = cycles;
    }
    if let Some(reps) = test.test_reps {
        config.test_reps = reps;
    }
}

/// Convert a ExtentSpec into a runtime ExtentMode. Returns Err with a human-readable
/// reason on malformed input. Mode names are case-insensitive.
pub fn spec_to_extent_mode(spec: &ExtentSpec) -> Result<ExtentMode, String> {
    match spec.mode.to_ascii_lowercase().as_str() {
        "full_allocation" | "full-allocation" | "full" => Ok(ExtentMode::FullAllocation),
        "cache" => {
            let target_str = spec.target.as_deref()
                .ok_or_else(|| "extent mode 'cache' requires 'target' field (e.g. \"L3/2\", \"DRAM*4\")".to_string())?;
            let target = CacheTarget::parse(target_str)
                .ok_or_else(|| format!("invalid cache target '{}' (L1, L2, L3, DRAM or DRAM-FULL, optionally /N or *N with a scale of 0.01-100)", target_str))?;
            Ok(ExtentMode::Cache { target })
        }
        "cache_total" | "cache-total" => {
            let fraction = spec.fraction
                .ok_or_else(|| "extent mode 'cache_total' requires 'fraction' field".to_string())?;
            if fraction <= 0.0 {
                return Err(format!("cache_total fraction must be > 0 (got {})", fraction));
            }
            Ok(ExtentMode::CacheTotal { fraction })
        }
        "absolute" => {
            let size_str = spec.size.as_deref()
                .ok_or_else(|| "extent mode 'absolute' requires 'size' field (e.g. \"880MB\", \"4GiB\")".to_string())?;
            let size_bytes = parse_size_string(size_str)?;
            if size_bytes == 0 {
                return Err("extent size must be above 0".to_string());
            }
            Ok(ExtentMode::Absolute { size_bytes })
        }
        other => Err(format!("unknown extent mode '{}'; valid: full_allocation, cache, cache_total, absolute", other)),
    }
}

/// Convert a ChunkSpec into a runtime ChunkMode. Same conventions as spec_to_extent_mode.
pub fn spec_to_chunk_mode(spec: &ChunkSpec) -> Result<ChunkMode, String> {
    match spec.mode.to_ascii_lowercase().as_str() {
        "auto" => Ok(ChunkMode::Auto),
        "whole" => Ok(ChunkMode::Whole),
        "cache" => {
            let target_str = spec.target.as_deref()
                .ok_or_else(|| "chunk mode 'cache' requires 'target' field (e.g. \"L3/2\", \"DRAM*4\")".to_string())?;
            let target = CacheTarget::parse(target_str)
                .ok_or_else(|| format!("invalid cache target '{}' (L1, L2, L3, DRAM or DRAM-FULL, optionally /N or *N with a scale of 0.01-100)", target_str))?;
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
            if size_bytes == 0 {
                return Err("chunk size must be above 0".to_string());
            }
            Ok(ChunkMode::Absolute { size_bytes })
        }
        "tm5_block" => {
            let size = |field: &Option<String>, name: &str| -> Result<usize, String> {
                let text = field.as_deref().ok_or_else(|| format!("chunk mode 'tm5_block' requires '{name}'"))?;
                match parse_size_string(text)? {
                    0 => Err(format!("tm5_block {name} must be above 0")),
                    bytes => Ok(bytes),
                }
            };
            let divisor = spec.divisor.ok_or_else(|| "chunk mode 'tm5_block' requires 'divisor' (a TM5 code + 1, 1-4)".to_string())?;
            if !(1..=4).contains(&divisor) {
                return Err(format!("tm5_block divisor must be 1-4 (got {divisor})"));
            }
            Ok(ChunkMode::Tm5Block { window: size(&spec.size, "size")?, divisor, granularity: size(&spec.granularity, "granularity")? })
        }
        other => Err(format!("unknown chunk mode '{}'; valid: auto, whole, cache, cache_total, absolute, tm5_block", other)),
    }
}

/// Extent specification — nested JSON shape: `{ "mode": "...", ... }`.
/// See `doc/extent_chunk_modes.md` for full syntax.
///
/// Modes:
/// - `full_allocation` — use entire per-thread allocation (no other fields needed)
/// - `cache` — tier-aware sizing; requires `target` (e.g. `"L3/2"`, `"DRAM*4"`)
/// - `cache_total` — coarse `(L1+L2+L3) × fraction`; requires `fraction`
/// - `absolute` — hard byte size; requires `size` (string like `"880MB"`, `"4GiB"`)
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ExtentSpec {
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

/// Chunk specification — same nested shape as ExtentSpec.
///
/// Modes:
/// - `auto` — per-test heuristic
/// - `whole` — one chunk, the whole extent
/// - `cache` — tier-aware; requires `target`
/// - `cache_total` — coarse cache fraction; requires `fraction`
/// - `absolute` — hard byte size; requires `size`
/// - `tm5_block` — a TM5 block code 0-3 (what a `.cfg` import makes): the smaller of `size` (the
///   `.cfg`'s Testing Window Size) and the extent, / `divisor`, floored to `granularity`
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ChunkSpec {
    pub mode: String,
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub fraction: Option<f64>,
    #[serde(default)]
    pub size: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub divisor: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub granularity: Option<String>,
}

// Simplified memory strategy configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryStrategyConfig {
    // Stage 1: Allocation strategy
    pub allocation_mode: String, // "max_available", "percentage_reserve", "fixed_reserve"
    pub reserve_mb: Option<u32>,       // For max_available mode
    pub reserve_percent: Option<f64>,  // For percentage_reserve mode
    pub reserve_gib: Option<f64>,      // For fixed_reserve mode

    /// Default extent spec — applied when a test does not override it.
    pub default_extent: ExtentSpec,
    /// Default chunk spec — applied when a test does not override it.
    pub default_chunk: ChunkSpec,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimingConfig {
    // Global test suite timing
    pub global_cycles: Option<u32>,
    pub global_duration_secs: Option<u32>,
    
    // Default per-test timing (can be overridden per test)
    pub default_test_cycles: Option<u32>,
    pub default_test_duration_secs: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CpuConfig {
    #[serde(rename = "type")]
    pub cpu_type: String, // "threads", "cores"
    pub usage_percent: u32, // 1-100
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestConfig {
    pub enabled: bool,
    pub function: String,
    
    // Per-test timing overrides
    pub cycles: Option<u32>,
    pub duration_secs: Option<u32>,
    pub min_duration_secs: Option<u32>,

    /// Per-test extent spec override. Same shape as `system.memory_strategy.default_extent`.
    #[serde(default)]
    pub extent: Option<ExtentSpec>,
    /// Per-test chunk spec override. Same shape as `system.memory_strategy.default_chunk`.
    #[serde(default)]
    pub chunk: Option<ChunkSpec>,

    pub requires_locality: Option<bool>,    // Test needs temporal locality

    /// Flush each chunk out of cache (CLFLUSHOPT + MFENCE) between the write and verify phases,
    /// so the verify round-trips through DRAM instead of reading the just-written cached copy
    /// (#59). Defaults to false. Only meaningful where the extent/chunk would otherwise stay
    /// cache-resident — at large chunks natural eviction already forces DRAM reads, so enabling
    /// it there costs bandwidth without changing what is tested.
    #[serde(default)]
    pub flush_before_verify: Option<bool>,


    // TMR-native test parameters (each used by specific tests, see doc/test_parameters.md)
    pub stride_patterns: Option<u32>,       // CacheBust: number of interleaved stride pattern variants
    pub rng_sequences: Option<u32>,         // RandomTorture: number of independent RNG sequences
    pub subdivisions: Option<u32>,          // StrideAccess: number of chunk subdivisions
    pub copy_directions: Option<u32>,       // BlockMove: number of copy direction patterns

    // Per-chunk repetition: how long each chunk is worked (TODO 79 B2). Unset is the test's own
    // default; loading a TM5 `.cfg` fills them from `Time (%)`.
    #[serde(default)]
    pub verify_reps: Option<u32>,           // Verify passes after each fill
    #[serde(default)]
    pub write_read_cycles: Option<u32>,     // Fill + verify rounds per chunk
    #[serde(default)]
    pub test_reps: Option<u32>,             // Test-op repetitions before verifying (MirrorMove: round trips)

    // TM5 pattern configuration (preserved for TM5-faithful pattern generation)
    pub pattern_mode: Option<u32>,
    pub pattern_param0: Option<u64>,
    pub pattern_param1: Option<u64>,
    pub parameter: Option<u32>,             // Raw TM5 parameter — interpreted via TestParameterContext
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
    /// `Lock Memory Granularity (Mb)` (TM5: 1-512, default 16). TM5 floors a fractional block
    /// size to it (`MainThread.asm` ~627-650).
    pub lock_granularity_mb: u32,
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
    /// Check the system fields with the parsers the CLI uses for the same settings, so a typo in
    /// a JSON config stops the run instead of quietly becoming a default (an unknown `error_mode`
    /// used to mean `log`, an unknown page size `regular`).
    pub fn validate_system(&self) -> Result<(), String> {
        let registry = crate::params::get_registry();
        let sys = &self.system;
        let checks = [
            ("errors", "error_mode", sys.error_mode.as_str()),
            ("cputype", "cpu_config.cpu_type", sys.cpu_config.cpu_type.as_str()),
            ("minpage", "memory_allocation.min_page_size", sys.memory_allocation.min_page_size.as_str()),
            ("maxpage", "memory_allocation.max_page_size", sys.memory_allocation.max_page_size.as_str()),
            ("skip-cores", "cpu_pinning.skip_spec", sys.cpu_pinning.skip_spec.as_str()),
            ("cpu-stride", "cpu_pinning.stride_spec", sys.cpu_pinning.stride_spec.as_str()),
        ];
        for (cli_key, json_key, value) in checks {
            registry.parse_arg(&format!("{cli_key}={value}"))
                .map_err(|e| format!("system.{json_key}: {e}"))?;
        }
        match sys.memory_strategy.allocation_mode.as_str() {
            "max_available" | "percentage_reserve" | "fixed_reserve" => Ok(()),
            other => Err(format!("system.memory_strategy.allocation_mode: unknown mode '{other}'; \
                valid: max_available, percentage_reserve, fixed_reserve")),
        }
    }

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
	
    pub fn save_to_file(&self, path: &str) -> Result<(), String> {
        let json = serde_json::to_string_pretty(self).map_err(|e| format!("Failed to serialize config: {}", e))?;

        fs::write(path, json).map_err(|e| format!("Failed to write config file: {}", e))
    }

    // Convert to runtime allocation strategy (extent/chunk modes are now test-specific)
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

        EnhancedMemoryStrategy { allocation_mode }
    }
    
    // Parse default extent spec into runtime ExtentMode. An invalid spec is an error, not a
    // fallback: the run stops rather than test something the config didn't ask for.
    pub fn get_default_extent_mode(&self) -> Result<ExtentMode, String> {
        spec_to_extent_mode(&self.system.memory_strategy.default_extent)
            .map_err(|e| format!("invalid default extent spec: {e}"))
    }

    // Parse default chunk spec into runtime ChunkMode. Invalid is an error, as above.
    pub fn get_default_chunk_mode(&self) -> Result<ChunkMode, String> {
        spec_to_chunk_mode(&self.system.memory_strategy.default_chunk)
            .map_err(|e| format!("invalid default chunk spec: {e}"))
    }

    pub fn to_error_mode(&self) -> ErrorMode {
        match self.system.error_mode.to_lowercase().as_str() {
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
    
    pub fn get_test_configs(&self) -> Result<Vec<(&str, TestMemoryConfig)>, String> {
        // Channels: prefer JSON system.channels, fall back to legacy TM5 metadata, default 2
        let channels = if self.system.channels > 0 {
            self.system.channels
        } else {
            self.legacy_metadata.as_ref().map_or(2, |m| m.tm5_channels)
        };
        self.test_sequence.iter().enumerate().filter(|(_, t)| t.enabled).map(|(i, test)| {
            let extent_mode = self.parse_test_extent_mode(test).map_err(|e| Self::test_error(i, test, e))?;
            let chunk_mode = self.parse_test_chunk_mode(test).map_err(|e| Self::test_error(i, test, e))?;
            if let Some(mode) = test.pattern_mode
                && !matches!(mode, 0..=2 | 10..=13)
            {
                return Err(Self::test_error(i, test, format!("pattern_mode {mode} is not a mode; valid: 0-2 (TM5-faithful), 10-13 (TMR-native)")));
            }
            if let ChunkMode::Absolute { size_bytes } = chunk_mode
                && !size_bytes.is_multiple_of(crate::test_memory::GRANULE)
            {
                log::info!("test_sequence[{i}] ('{}'): chunk {} bytes -> {} KiB (chunks are multiples of 4 KiB, rounded up; then at least the test's minimum and at most the extent)",
                    test.function, size_bytes, size_bytes.next_multiple_of(crate::test_memory::GRANULE) / 1024);
            }
            let requires_locality = test.requires_locality.unwrap_or({
                // Auto-detect based on function name
                matches!(test.function.as_str(), "Mem-CacheBust" | "Mem-Refresh")
            });

            let timing = TestTiming {
                cycles: test.cycles.or(self.system.timing.default_test_cycles),
                duration_secs: test.duration_secs.or(self.system.timing.default_test_duration_secs),
                min_duration_secs: test.min_duration_secs,
            };

            let mut config = TestMemoryConfig::new(extent_mode, chunk_mode, requires_locality)
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

            apply_repetition(test, &mut config);

            Ok((test.function.as_str(), config))
        }).collect()
    }

    /// Get test configs in TM5 test sequence order (if available) with repetition support
    #[expect(dead_code, reason = "TODO #74: TM5 `Test Sequence` is parsed but never wired; TMR runs enabled tests in index order")]
    pub fn get_test_configs_with_sequence(&self) -> Result<Vec<(&str, TestMemoryConfig)>, String> {
        // Check if we have TM5 test sequence data
        if let Some(ref metadata) = self.legacy_metadata
            && !metadata.tm5_test_sequence.is_empty() {
                return self.get_tm5_sequence_configs(&metadata.tm5_test_sequence);
            }
        
        // Fallback to standard sequential execution
        self.get_test_configs()
    }
    
    /// Get test configs following TM5 test sequence order and repetition
    fn get_tm5_sequence_configs(&self, sequence: &[u32]) -> Result<Vec<(&str, TestMemoryConfig)>, String> {
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
                    let extent_mode = self.parse_test_extent_mode(test)
                        .map_err(|e| Self::test_error(test_index as usize, test, e))?;
                    let chunk_mode = self.parse_test_chunk_mode(test)
                        .map_err(|e| Self::test_error(test_index as usize, test, e))?;
                    let requires_locality = test.requires_locality.unwrap_or({
                        // Auto-detect based on function name
                        matches!(test.function.as_str(), "Mem-CacheBust" | "Mem-Refresh")
                    });

                    let timing = TestTiming {
                        cycles: test.cycles.or(self.system.timing.default_test_cycles),
                        duration_secs: test.duration_secs.or(self.system.timing.default_test_duration_secs),
                        min_duration_secs: test.min_duration_secs,
                    };

                    let mut config = TestMemoryConfig::new(extent_mode, chunk_mode, requires_locality)
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

                    apply_repetition(test, &mut config);

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
        
        Ok(result)
    }
    
/// Which entry an error belongs to: its `test_sequence` index and function, since a config can
/// repeat a function (a TM5 import has eleven `Mem-SimpleV2`).
fn test_error(index: usize, test: &TestConfig, e: String) -> String {
    format!("test_sequence[{index}] ('{}'): {e}", test.function)
}

fn parse_test_extent_mode(&self, test: &TestConfig) -> Result<ExtentMode, String> {
    match test.extent {
        Some(ref spec) => spec_to_extent_mode(spec).map_err(|e| format!("invalid extent spec: {e}")),
        None => self.get_default_extent_mode(),
    }
}

fn parse_test_chunk_mode(&self, test: &TestConfig) -> Result<ChunkMode, String> {
    match test.chunk {
        Some(ref spec) => spec_to_chunk_mode(spec).map_err(|e| format!("invalid chunk spec: {e}")),
        None => self.get_default_chunk_mode(),
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
                default_extent: ExtentSpec::full_allocation(),
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
                extent: Some(ExtentSpec::full_allocation()), // Must test ALL memory
                chunk: Some(ChunkSpec::absolute("512MiB")),  // Above a desktop L3; capped per block piece
                requires_locality: Some(false),
                flush_before_verify: None,
                stride_patterns: None,
                rng_sequences: None,
                subdivisions: None,
                copy_directions: None,
                verify_reps: None,
                write_read_cycles: None,
                test_reps: None,
                pattern_mode: None,
                pattern_param0: None,
                pattern_param1: None,
                parameter: None,
            },

            // Mem-Refresh - needs small extent for refresh timing
            TestConfig {
                enabled: true,
                function: "Mem-Refresh".to_string(),
                cycles: Some(1),
                duration_secs: None,
                min_duration_secs: None,
                extent: Some(ExtentSpec::cache_total(2.0)), // 2x cache for refresh testing
                chunk: Some(ChunkSpec::absolute("1MB")),    // Small 1MB blocks
                requires_locality: Some(true),
                flush_before_verify: None,
                stride_patterns: None,
                rng_sequences: None,
                subdivisions: None,
                copy_directions: None,
                verify_reps: None,
                write_read_cycles: None,
                test_reps: None,
                pattern_mode: None,
                pattern_param0: None,
                pattern_param1: None,
                parameter: None,
            },
            
            // Mem-Simple - general pattern test with TM5 compatibility
            TestConfig {
                enabled: true,
                function: "Mem-SimpleV2".to_string(),
                cycles: Some(1),
                duration_secs: None,
                min_duration_secs: None,
                extent: Some(ExtentSpec::absolute("880MB")), // a fixed extent
                chunk: Some(ChunkSpec::absolute("16MB")),    // TM5 typical block size
                requires_locality: Some(false),
                flush_before_verify: None,
                stride_patterns: None,
                rng_sequences: None,
                subdivisions: None,
                copy_directions: None,
                verify_reps: None,
                write_read_cycles: None,
                test_reps: None,
                pattern_mode: Some(1),
                pattern_param0: Some(0x1E5F),
                pattern_param1: Some(0x45357354),
                parameter: None,
            },

            // Mem-MirrorV2-128 - SIMD test with optimal locality
            TestConfig {
                enabled: true,
                function: "Mem-MirrorV2-128".to_string(),
                cycles: Some(1),
                duration_secs: None,
                min_duration_secs: None,
                extent: Some(ExtentSpec::absolute("64MB")),  // Good SIMD locality
                chunk: Some(ChunkSpec::absolute("16MB")),    // 16MB for 128-bit alignment
                requires_locality: Some(true),
                flush_before_verify: None,
                stride_patterns: None,
                rng_sequences: None,
                subdivisions: None,
                copy_directions: None,
                verify_reps: None,
                write_read_cycles: None,
                test_reps: None,
                pattern_mode: None,
                pattern_param0: None,
                pattern_param1: None,
                parameter: None,
            },
            
            // Mem-MirrorV2-256 - AVX2 with dual subblocks
            TestConfig {
                enabled: true,
                function: "Mem-MirrorV2-256".to_string(),
                cycles: Some(1),
                duration_secs: None,
                min_duration_secs: None,
                extent: Some(ExtentSpec::absolute("128MB")), // Larger for AVX2
                chunk: Some(ChunkSpec::absolute("32MB")),    // 32MB for 256-bit alignment
                requires_locality: Some(true),
                flush_before_verify: None,
                stride_patterns: None,
                rng_sequences: None,
                subdivisions: None,
                copy_directions: None,
                verify_reps: None,
                write_read_cycles: None,
                test_reps: None,
                pattern_mode: None,
                pattern_param0: None,
                pattern_param1: None,
                parameter: None,
            },
            
            // Mem-CacheBust - specifically sized for cache stress
            TestConfig {
                enabled: true,
                function: "Mem-CacheBust".to_string(),
                cycles: Some(1),
                duration_secs: None,
                min_duration_secs: None,
                extent: Some(ExtentSpec::cache_total(0.5)), // Half total cache to ensure busting
                chunk: Some(ChunkSpec::absolute("1MB")),    // 1MB blocks for cache lines
                requires_locality: Some(true),
                flush_before_verify: None,
                stride_patterns: Some(4),              // 4 interleaved stride patterns
                rng_sequences: None,
                subdivisions: None,
                copy_directions: None,
                verify_reps: None,
                write_read_cycles: None,
                test_reps: None,
                pattern_mode: None,
                pattern_param0: None,
                pattern_param1: None,
                parameter: None,
            },
            
            // Mem-Random - full memory random access
            TestConfig {
                enabled: true,
                function: "Mem-Random".to_string(),
                cycles: Some(1),
                duration_secs: None,
                min_duration_secs: None,
                extent: Some(ExtentSpec::full_allocation()), // Need full memory
                chunk: Some(ChunkSpec::absolute("8MB")),    // 8MB blocks
                requires_locality: Some(false),
                flush_before_verify: None,
                stride_patterns: None,
                rng_sequences: Some(8),                // 8 independent RNG sequences
                subdivisions: None,
                copy_directions: None,
                verify_reps: None,
                write_read_cycles: None,
                test_reps: None,
                pattern_mode: None,
                pattern_param0: None,
                pattern_param1: None,
                parameter: None,
            },
            
            // Mem-Stride - test various stride patterns
            TestConfig {
                enabled: true,
                function: "Mem-Stride".to_string(),
                cycles: Some(1),
                duration_secs: None,
                min_duration_secs: None,
                extent: Some(ExtentSpec::full_allocation()),
                chunk: Some(ChunkSpec::auto()),             // Let TMR optimize
                requires_locality: Some(false),
                flush_before_verify: None,
                stride_patterns: None,
                rng_sequences: None,
                subdivisions: Some(4),                 // 4 chunk subdivisions
                copy_directions: None,
                verify_reps: None,
                write_read_cycles: None,
                test_reps: None,
                pattern_mode: None,
                pattern_param0: None,
                pattern_param1: None,
                parameter: None,
            },
            
            // Mem-BlockMove - memory copy test
            TestConfig {
                enabled: true,
                function: "Mem-BlockMove".to_string(),
                cycles: Some(1),
                duration_secs: None,
                min_duration_secs: None,
                extent: Some(ExtentSpec::full_allocation()), // Need src+dst space
                chunk: Some(ChunkSpec::absolute("16MB")),    // 16MB blocks
                requires_locality: Some(false),
                flush_before_verify: None,
                stride_patterns: None,
                rng_sequences: None,
                subdivisions: None,
                copy_directions: Some(2),              // Forward + backward copy
                verify_reps: None,
                write_read_cycles: None,
                test_reps: None,
                pattern_mode: None,
                pattern_param0: None,
                pattern_param1: None,
                parameter: None,
            },
            
            // Legacy TM5-style test with one large block
            TestConfig {
                enabled: true,
                function: "Mem-SimpleV2".to_string(),
                cycles: Some(1),
                duration_secs: None,
                min_duration_secs: None,
                extent: None,                                // Use global default
                chunk: Some(ChunkSpec::absolute("512MiB")),
                requires_locality: Some(false),
                flush_before_verify: None,
                stride_patterns: None,
                rng_sequences: None,
                subdivisions: None,
                copy_directions: None,
                verify_reps: None,
                write_read_cycles: None,
                test_reps: None,
                pattern_mode: Some(0),
                pattern_param0: Some(0),
                pattern_param1: Some(0),
                parameter: None,
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
                description: Some("TM5-compatible configuration: maximum memory allocation, every test over all of it".to_string()),
                created: Some("2025-06-29".to_string()),
                tested_with_version: APP_VERSION.to_string(),
            },
            system: SystemConfig {
                memory_strategy: MemoryStrategyConfig {
                    allocation_mode: "max_available".to_string(),
                    reserve_mb: Some(128),                 // TM5-style fixed reserve
                    reserve_percent: None,
                    reserve_gib: None,
                    default_extent: ExtentSpec::full_allocation(), // as a TM5 .cfg import
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
                    extent: Some(ExtentSpec::full_allocation()), // Override to test all memory
                    chunk: None,                                 // Use default auto
                    requires_locality: Some(false),
                    flush_before_verify: None,
                    stride_patterns: None,
                    rng_sequences: None,
                    subdivisions: None,
                    copy_directions: None,
                    verify_reps: None,
                    write_read_cycles: None,
                    test_reps: None,
                    pattern_mode: None,
                    pattern_param0: None,
                    pattern_param1: None,
                    parameter: None,
                },
                TestConfig {
                    enabled: true,
                    function: "Mem-SimpleV2".to_string(),
                    cycles: Some(1),
                    duration_secs: None,
                    min_duration_secs: None,
                    extent: None,                                // the full allocation
                    chunk: Some(ChunkSpec::absolute("16MB")),    // TM5-style block size
                    requires_locality: Some(false),
                    flush_before_verify: None,
                    stride_patterns: None,
                    rng_sequences: None,
                    subdivisions: None,
                    copy_directions: None,
                    verify_reps: None,
                    write_read_cycles: None,
                    test_reps: None,
                    pattern_mode: Some(1),
                    pattern_param0: Some(0x1E5F),
                    pattern_param1: Some(0x45357354),
                    parameter: None,
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
            lock_granularity_mb: memory.get("Lock Memory Granularity (Mb)").and_then(|s| s.parse().ok()).unwrap_or(16).clamp(1, 512),
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
    // Start with empty test sequence - only add what's in the config
    let mut test_sequence = Vec::new();
    
    // Add legacy tests WITHOUT auto-inserting StuckBitTest
    for test in &self.tests {
        if test.enabled {
            let (verify_reps, write_read_cycles, test_reps) = self.repetition(test);

            test_sequence.push(TestConfig {
                enabled: true,
                function: Self::map_legacy_function(&test.function)?,
                
                // One pass per plan cycle; TM5 `Time (%)` is per-chunk dwell, set below (TODO 79 B2)
                cycles: Some(1),
                duration_secs: None,  // Don't use duration-based timing
                min_duration_secs: None,
                
                // The default extent, the full allocation: every TM5 test visits every locked
                // page, walking its AWE window over them (TODO 76)
                extent: None,

                chunk: Some(self.chunk_spec(test)),

                // Not `None`: the auto-detect would set it for Mem-Refresh, which under a full
                // allocation caps the extent at L3 x 2
                requires_locality: Some(false),
                flush_before_verify: None,
                
                stride_patterns: None,
                rng_sequences: None,
                subdivisions: None,
                copy_directions: None,
                verify_reps,
                write_read_cycles,
                test_reps,
                
                // Preserve legacy test parameters
                pattern_mode: Some(test.pattern_mode),
                pattern_param0: Some(test.pattern_param0),
                pattern_param1: Some(test.pattern_param1),
                parameter: Some(test.parameter),
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
                default_extent: ExtentSpec::full_allocation(),
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
            tm5_window_mb: self.memory_setup.testing_window_size_mb,
            tm5_lock_mb: self.memory_setup.lock_granularity_mb.max(1),
        }),
    })
}

    /// TM5 `Test Block Size (Mb)` as a TMR chunk (TODO 79 B1), sized exactly as TM5 sizes it
    /// (`mt_ini.asm:287-303`, `MainThread.asm:627-661`). 0-3 are fraction codes, resolved at run
    /// time against the thread's memory (`ChunkMode::Tm5Block`): the smaller of tm5_window and the
    /// extent, / (V + 1), floored to the `.cfg`'s Lock Memory Granularity. 4 and up are binary
    /// megabytes, clamped to tm5_window and floored to 4 KiB. tm5_window is the `.cfg`'s own
    /// `Testing Window Size`, not TMR's extent, which for an import is the full allocation. A size
    /// that differs from the plain fraction or value is logged.
    fn chunk_spec(&self, test: &LegacyTest) -> ChunkSpec {
        let tm5_window = self.memory_setup.testing_window_size_mb as u64 * BYTES_PER_MIB;
        let granule = crate::test_memory::GRANULE as u64;
        let lock = self.memory_setup.lock_granularity_mb.max(1) as u64 * BYTES_PER_MIB;
        let (size, what) = match test.test_chunk_size_mb {
            code @ 0..=3 => {
                let divisor = code + 1;
                let part = tm5_window / divisor as u64;
                let floored = crate::tests::tm5_block_size(tm5_window as usize, divisor, lock as usize) as u64;
                if floored != part {
                    log::info!("TM5 Test{} ({}): Test Block Size {} = .cfg window/{} = {:.2} MiB -> {} MiB (TM5 floors it to the {} MiB Lock Memory Granularity; a thread with less memory than the window takes the fraction of its memory)",
                              test.id, test.function, code, divisor, part as f64 / BYTES_PER_MIB as f64,
                              floored / BYTES_PER_MIB, lock / BYTES_PER_MIB);
                }
                return ChunkSpec::tm5_block(tm5_window / BYTES_PER_MIB, divisor, lock / BYTES_PER_MIB);
            }
            mb => {
                let requested = mb as u64 * BYTES_PER_MIB;
                if requested > tm5_window {
                    log::info!("TM5 Test{} ({}): Test Block Size {} MiB is more than the {} MiB .cfg window; the chunk is the .cfg window",
                              test.id, test.function, mb, tm5_window / BYTES_PER_MIB);
                }
                (requested.min(tm5_window), format!("{mb} MiB"))
            }
        };
        // TM5 floors to 4 KiB, at least 4 KiB
        let chunk = (size / granule).max(1) * granule;
        if chunk != size {
            log::info!("TM5 Test{} ({}): Test Block Size {} = {what} = {:.2} MiB -> {} KiB chunk (a multiple of 4 KiB)",
                      test.id, test.function, test.test_chunk_size_mb,
                      size as f64 / BYTES_PER_MIB as f64, chunk / 1024);
        }
        ChunkSpec::absolute(&format!("{}KiB", chunk / 1024))
    }

    /// TM5 `Time (%)` as per-chunk repetition (TODO 79 B2), returned as (`verify_reps`,
    /// `write_read_cycles`, `test_reps`). N = test % x global % / 2000, at least 1 (`mtests0.asm`
    /// ST_Check :298-308, MirrorMove_Check :1135-1145, MirrorMove128_Check :1566-1576): how long
    /// each chunk is worked, not passes over the extent. SimpleTest does 4 x (1 fill + N
    /// verifies). MirrorMove does N mirror passes; TMR's test op is a round trip (mirror and
    /// back), so N/2 rounded up, never less dwell than TM5. RefreshStable ignores `Time (%)`.
    /// BlockMove reads it too, but no shipped config uses it and TMR's BlockMove has its own loop,
    /// so it is left at its default.
    fn repetition(&self, test: &LegacyTest) -> (Option<u32>, Option<u32>, Option<u32>) {
        let n = (test.time_percent as u64 * self.main_section.time_percent as u64 / 2000).max(1) as u32;
        let reps = match test.function.as_str() {
            "SimpleTest" => (Some(n), Some(4), None),
            "MirrorMove" | "MirrorMove128" => (None, None, Some(n.div_ceil(2))),
            _ => (None, None, None),
        };
        if n != 5 && reps != (None, None, None) {
            log::info!("TM5 Test{} ({}): Time (%) {} x global {} = {} passes per chunk (TM5's default is 5)",
                      test.id, test.function, test.time_percent, self.main_section.time_percent, n);
        }
        reps
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
    let config = if path.ends_with(".json") {
        ModernConfig::load_from_file(path)?
    } else if path.ends_with(".cfg") {
        // Legacy format (v1.0)
        let legacy = LegacyConfig::load_from_file(path)?;
        legacy.to_modern_config()?
    } else {
        // Try JSON first, then legacy. If neither parses, say why for both: one of the two
        // messages is the real problem, and it isn't always the second.
        match ModernConfig::load_from_file(path) {
            Ok(config) => config,
            Err(json_err) => match LegacyConfig::load_from_file(path).and_then(|l| l.to_modern_config()) {
                Ok(config) => config,
                Err(cfg_err) => return Err(format!(
                    "{path} is neither a TMR JSON config ({json_err}) nor a TM5 .cfg ({cfg_err})")),
            },
        }
    };
    config.validate_system()?;
    Ok(config)
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
    println!("   Memory: Maximum allocation minus 128MB reserve, every test over all of it");
    println!();
    println!("Configuration Architecture Summary:");
    println!("  Stage 1: Memory Allocation - Maximum available memory per thread");
    println!("  Stage 2: Extent - how much of the allocation each test covers");
    println!("  Stage 3: Block/Chunk Size - Auto-optimized per test with alignment");
    println!("  Timing: Per-test cycles/duration limits + global suite limits");
    println!("  Critical: Mem-StuckBit ensures full memory coverage for bit errors");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: usize = 1 << 20;

    fn test(id: u32, function: &str, time_percent: u32, block: u32) -> LegacyTest {
        LegacyTest {
            id,
            enabled: true,
            time_percent,
            function: function.to_string(),
            pattern_mode: 0,
            pattern_param0: 0,
            pattern_param1: 0,
            parameter: 0,
            test_chunk_size_mb: block,
        }
    }

    /// A `1usmus_v3.cfg`-like setup: 880 MiB window, global `Time (%)` as given.
    fn legacy(global_time: u32, tests: Vec<LegacyTest>) -> LegacyConfig {
        LegacyConfig {
            main_section: LegacyMainSection {
                config_name: "t".to_string(),
                config_author: "t".to_string(),
                cores: 0,
                time_percent: global_time,
                cycles: 3,
                test_sequence: Vec::new(),
            },
            memory_setup: LegacyMemorySetup { testing_window_size_mb: 880, reserved_memory_mb: 128, channels: 2, lock_granularity_mb: 16 },
            tests,
        }
    }

    /// TODO 79 B1, TODO 76: code 0 is the `.cfg`'s window, 1-3 are window fractions floored to
    /// the Lock Memory Granularity (16 MiB here) as TM5 does, 4 and up are binary megabytes
    /// clamped to the window. A thread with less memory than the window (500 MiB) takes them as
    /// fractions of its memory, as TM5 does, and the plan can tell they shrank. Every import
    /// covers the full allocation.
    #[test]
    fn tm5_block_size_codes_are_window_fractions() {
        let tests = [0, 1, 2, 3, 4, 1536].iter().enumerate()
            .map(|(i, &block)| test(i as u32, "SimpleTest", 100, block))
            .collect();
        let modern = legacy(100, tests).to_modern_config().unwrap();
        let configs = modern.get_test_configs().unwrap();
        let chunks = |extent: usize| -> Vec<usize> {
            configs.iter().map(|(name, c)| c.calculate_chunk_size(name, extent) / MIB).collect()
        };
        // The 880 MiB window; window/2 = 440 -> 432, window/3 = 293.33 -> 288, window/4 = 220
        // -> 208 (each floored to 16 MiB, as TM5 does); 4 MiB; 1536 MiB clamped to the window.
        assert_eq!(chunks(5 * 1024 * MIB), [880, 432, 288, 208, 4, 880]);
        // 500 MiB of memory: 500, 250 -> 240, 166.67 -> 160, 125 -> 112; 4; and 880 capped at 500
        assert_eq!(chunks(500 * MIB), [500, 240, 160, 112, 4, 500]);
        let shrunk: Vec<bool> = configs.iter().map(|(name, c)| c.resolve_chunk(name, 500 * MIB).shrunk_by_memory()).collect();
        assert_eq!(shrunk, [true, true, true, true, false, true]);
        assert!(!configs.iter().any(|(name, c)| c.resolve_chunk(name, 5 * 1024 * MIB).shrunk_by_memory()));
        assert_eq!(modern.system.memory_strategy.default_extent.mode, "full_allocation");
        let meta = modern.legacy_metadata.as_ref().unwrap();
        assert_eq!((meta.tm5_window_mb, meta.tm5_lock_mb), (880, 16));
    }

    /// TODO 76: an imported RefreshStable covers the full allocation too. Its locality flag would
    /// cap it at L3 x 2, and `None` would let the auto-detect set it.
    #[test]
    fn imported_refresh_covers_the_full_allocation() {
        let modern = legacy(100, vec![test(0, "RefreshStable", 100, 0)]).to_modern_config().unwrap();
        let configs = modern.get_test_configs().unwrap();
        let (name, config) = &configs[0];
        assert!(!config.requires_locality);
        assert_eq!(config.calculate_extent_size(name, 5 * 1024 * MIB), 5 * 1024 * MIB);
    }

    /// An invalid extent or chunk spec stops the run: no fallback to the default, which would
    /// test something the config didn't ask for. A removed mode such as `fraction` is one.
    #[test]
    fn invalid_specs_are_errors() {
        let mut modern = legacy(100, vec![test(0, "SimpleTest", 100, 16)]).to_modern_config().unwrap();
        assert!(modern.get_test_configs().is_ok());

        modern.test_sequence[0].chunk = Some(ChunkSpec { mode: "fraction".to_string(), fraction: Some(0.5), ..Default::default() });
        let err = modern.get_test_configs().unwrap_err();
        assert!(err.contains("Mem-SimpleV2") && err.contains("chunk") && err.contains("fraction"), "{err}");

        modern.test_sequence[0].chunk = None;
        modern.system.memory_strategy.default_chunk = ChunkSpec { mode: "bogus".to_string(), ..Default::default() };
        let err = modern.get_test_configs().unwrap_err();
        assert!(err.contains("default chunk"), "{err}");

        modern.system.memory_strategy.default_chunk = ChunkSpec::auto();
        modern.test_sequence[0].extent = Some(ExtentSpec { mode: "bogus".to_string(), ..Default::default() });
        let err = modern.get_test_configs().unwrap_err();
        assert!(err.contains("test_sequence[0] ('Mem-SimpleV2')") && err.contains("extent"), "{err}");

        modern.test_sequence[0].extent = None;
        modern.system.memory_strategy.default_extent = ExtentSpec { mode: "bogus".to_string(), ..Default::default() };
        let err = modern.get_test_configs().unwrap_err();
        assert!(err.contains("default extent"), "{err}");

        // A bad cache-target scale is an error, not the tier default; no scale is the default.
        modern.system.memory_strategy.default_extent = ExtentSpec::full_allocation();
        for bad in ["L3x4", "L2/0", "DRAM*200", "DRAM-FULLX"] {
            modern.test_sequence[0].extent = Some(ExtentSpec { mode: "cache".to_string(), target: Some(bad.to_string()), ..Default::default() });
            assert!(modern.get_test_configs().is_err(), "{bad} should be rejected");
        }
        modern.test_sequence[0].extent = Some(ExtentSpec { mode: "cache".to_string(), target: Some("L3".to_string()), ..Default::default() });
        assert!(modern.get_test_configs().is_ok());
        assert!(matches!(CacheTarget::parse("L3"), Some(CacheTarget::L3 { scale }) if scale == 0.5));
        assert!(matches!(CacheTarget::parse("DRAM"), Some(CacheTarget::Dram { scale }) if scale == 4.0));

        // A zero size is an error, not a test of nothing
        modern.test_sequence[0].extent = Some(ExtentSpec::absolute("0"));
        assert!(modern.get_test_configs().is_err());
        modern.test_sequence[0].extent = None;

        // So is a pattern mode that doesn't exist; 13 does
        modern.test_sequence[0].pattern_mode = Some(7);
        let err = modern.get_test_configs().unwrap_err();
        assert!(err.contains("pattern_mode 7"), "{err}");
        modern.test_sequence[0].pattern_mode = Some(13);
        assert!(modern.get_test_configs().is_ok());
    }

    /// The configs TMR generates itself must load, unknown-key check and system fields included.
    #[test]
    fn generated_configs_have_valid_specs() {
        for config in [ModernConfig::create_demo_config(), ModernConfig::create_tm5_compatible_config(),
                       legacy(100, vec![test(0, "SimpleTest", 100, 16)]).to_modern_config().unwrap()] {
            assert!(config.get_test_configs().is_ok());
            config.validate_system().unwrap();
            let json = serde_json::to_string(&config).unwrap();
            serde_json::from_str::<ModernConfig>(&json).unwrap();
        }
    }

    /// The tracked JSON config under `test_configs/` still loads.
    #[test]
    fn tracked_json_configs_load() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/test_configs/flush_chunk_sweep.json");
        let config = load_config(path).unwrap();
        assert!(config.get_test_configs().is_ok());
    }

    /// Unknown JSON keys and bad system values are errors, not ignored or defaulted.
    #[test]
    fn unknown_keys_and_bad_system_values_are_errors() {
        let config = ModernConfig::create_demo_config();
        let mut json: serde_json::Value = serde_json::to_value(&config).unwrap();
        json["test_sequence"][0]["chunks"] = serde_json::json!({ "mode": "auto" });
        let err = serde_json::from_value::<ModernConfig>(json).unwrap_err().to_string();
        assert!(err.contains("chunks"), "{err}");

        let mut bad = config.clone();
        bad.system.error_mode = "halts".to_string();
        assert!(bad.validate_system().unwrap_err().contains("error_mode"));
        let mut bad = config.clone();
        bad.system.memory_allocation.min_page_size = "1GiB".to_string();
        assert!(bad.validate_system().unwrap_err().contains("min_page_size"));
        let mut bad = config.clone();
        bad.system.memory_strategy.allocation_mode = "max-available".to_string();
        assert!(bad.validate_system().unwrap_err().contains("allocation_mode"));

        let mut ok = config;
        ok.system.error_mode = "Halt".to_string();
        ok.validate_system().unwrap();
        assert!(matches!(ok.to_error_mode(), ErrorMode::Halt));
    }

    /// TODO 79 B2: `Time (%)` is per-chunk dwell, not whole-extent passes.
    #[test]
    fn tm5_time_percent_is_per_chunk_dwell() {
        let tests = vec![
            test(0, "SimpleTest", 100, 0),
            test(1, "SimpleTest", 300, 0),
            test(2, "MirrorMove", 100, 0),
            test(3, "MirrorMove128", 300, 0),
            test(4, "RefreshStable", 300, 0),
        ];
        let modern = legacy(100, tests).to_modern_config().unwrap();
        assert!(modern.test_sequence.iter().all(|t| t.cycles == Some(1)), "one pass per plan cycle");
        let reps: Vec<(u32, u32, u32)> = modern.get_test_configs().unwrap().iter()
            .map(|(_, c)| (c.verify_reps, c.write_read_cycles, c.test_reps))
            .collect();
        assert_eq!(reps, vec![
            (5, 4, 1),   // 100 x 100 / 2000 = 5 verifies, TM5's default
            (15, 4, 1),  // 300 x 100 / 2000 = 15
            (1, 1, 3),   // 5 mirror passes -> 3 round trips
            (1, 1, 8),   // 15 mirror passes -> 8 round trips
            (1, 1, 1),   // RefreshStable ignores Time (%)
        ]);
        // A global 50 % halves it: 100 x 50 / 2000 = 2.
        let half = legacy(50, vec![test(0, "SimpleTest", 100, 0)]).to_modern_config().unwrap();
        assert_eq!(half.get_test_configs().unwrap()[0].1.verify_reps, 2);
    }

}
