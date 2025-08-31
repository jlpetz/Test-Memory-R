use crate::{ErrorMode};
use crate::constants::{gib_to_bytes, BYTES_PER_MIB};
use crate::tests::{WindowMode, ChunkMode};
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
}

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
	#[serde(default)]  // Add this for backward compatibility
	pub cpu_pinning: CpuPinningConfig,  // Add this
	#[serde(default)]  // Add this for backward compatibility
    pub memory_allocation: MemoryAllocationConfig,  // Add this
}

// Define CpuPinningConfig in config.rs
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CpuPinningConfig {
    pub enable_pinning: bool,
    #[serde(default = "default_cpus_to_skip")]
    pub cpus_to_skip: usize,
    #[serde(default = "default_avoid_smt_doubling")]
    pub avoid_smt_doubling: bool,
}

fn default_cpus_to_skip() -> usize { 1 }
fn default_avoid_smt_doubling() -> bool { false }


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
        }
    }
}

// Simplified memory strategy configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryStrategyConfig {
    // Stage 1: Allocation strategy
    pub allocation_mode: String, // "max_available", "percentage_reserve", "fixed_reserve"
    pub reserve_mb: Option<u32>,       // For max_available mode
    pub reserve_percent: Option<f64>,  // For percentage_reserve mode  
    pub reserve_gib: Option<f64>,      // For fixed_reserve mode
    
    // Stage 2: Default window sizing
    pub default_window_mode: String,   // "full_allocation", "fixed_size", "cache_relative"
    pub default_window_size_mb: Option<u32>, // For fixed_size mode
    pub window_cache_multiplier: Option<f64>, // For cache_relative mode
    
    // Stage 3: Default block sizing  
    pub default_chunk_mode: String,    // "auto_optimal", "fixed_size", "window_fraction"
    pub default_chunk_size_mb: Option<u32>, // For fixed_size mode
    pub block_window_fraction: Option<f64>,  // For window_fraction mode
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
    
    // Stage 2 & 3 per-test overrides
    pub window_mode: Option<String>,        // Override default window mode
    pub window_size_mb: Option<u32>,        // For fixed window mode
    pub window_cache_multiplier: Option<f64>, // For cache relative mode
    
    pub chunk_mode: Option<String>,         // Override default block mode  
    pub block_size_mb: Option<u32>,         // For fixed block mode
    pub block_window_fraction: Option<f64>, // For window fraction mode
    
    pub allow_misaligned: Option<bool>,     // Allow unaligned accesses
    pub requires_locality: Option<bool>,    // Test needs temporal locality
    
    // Access pattern configuration
    pub streams: Option<u32>,               // Number of access streams (equivalent to TM5 jump/parameter)
    
    // Legacy TM5 compatibility (preserved but not used in new logic)
    pub pattern_mode: Option<u32>,
    pub pattern_param0: Option<u64>,
    pub pattern_param1: Option<u64>,
    pub parameter: Option<u32>,             // Legacy parameter field (mapped to streams on load)
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

        let mut config: ModernConfig = serde_json::from_str(&content).map_err(|e| format!("Failed to parse JSON config: {}", e))?;

        // Handle backward compatibility: if parameter exists but streams doesn't, copy it
        for test in &mut config.test_sequence {
            if test.streams.is_none() && test.parameter.is_some() {
                test.streams = test.parameter;
            }
        }

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
        report.push_str(&format!(", Window: {}, Block: {}\n", 
            self.system.memory_strategy.default_window_mode,
            self.system.memory_strategy.default_chunk_mode
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
            
            // Streams
            if let Some(streams) = test.streams {
                report.push_str(&format!(", {}streams", streams));
            }
            
            // Window override
            if let Some(mode) = &test.window_mode {
                report.push_str(&format!(", Window:{}", mode));
                if mode == "fixed_size" {
                    if let Some(mb) = test.window_size_mb {
                        report.push_str(&format!(" {}MB", mb));
                    }
                } else if mode == "cache_relative"
                    && let Some(mult) = test.window_cache_multiplier {
                        report.push_str(&format!(" {}x", mult));
                    }
            }
            
            // Block override
            if let Some(mode) = &test.chunk_mode {
                report.push_str(&format!(", Block:{}", mode));
                if mode == "fixed_size" {
                    if let Some(mb) = test.block_size_mb {
                        report.push_str(&format!(" {}MB", mb));
                    }
                } else if mode == "window_fraction"
                    && let Some(frac) = test.block_window_fraction {
                        report.push_str(&format!(" {:.1}%", frac * 100.0));
                    }
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
    
    // Parse default window mode for tests (moved out of allocation strategy)
    pub fn get_default_window_mode(&self) -> WindowMode {
        match self.system.memory_strategy.default_window_mode.as_str() {
            "full_allocation" => WindowMode::FullAllocation,
            "fixed_size" => WindowMode::FixedSize { 
                size_mb: self.system.memory_strategy.default_window_size_mb.unwrap_or(880) 
            },
            "cache_relative" => WindowMode::CacheRelative { 
                multiplier: self.system.memory_strategy.window_cache_multiplier.unwrap_or(2.0) 
            },
            _ => WindowMode::FullAllocation,
        }
    }
    
    // Parse default chunk mode for tests (moved out of allocation strategy)
    pub fn get_default_chunk_mode(&self) -> ChunkMode {
        match self.system.memory_strategy.default_chunk_mode.as_str() {
            "auto_optimal" => ChunkMode::AutoOptimal,
            "fixed_size" => ChunkMode::FixedSize { 
                size_mb: self.system.memory_strategy.default_chunk_size_mb.unwrap_or(16) 
            },
            "window_fraction" => ChunkMode::WindowFraction { 
                fraction: self.system.memory_strategy.block_window_fraction.unwrap_or(0.125) 
            },
            _ => ChunkMode::AutoOptimal,
        }
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
        self.test_sequence.iter().filter(|t| t.enabled).map(|test| {
            let window_mode = self.parse_test_window_mode(test);
            let chunk_mode = self.parse_test_chunk_mode(test);
            let allow_misaligned = test.allow_misaligned.unwrap_or(false);
            let requires_locality = test.requires_locality.unwrap_or({
                // Auto-detect based on function name
                matches!(test.function.as_str(), "CacheBusting" | "RefreshStable")
            });
            
            let timing = TestTiming {
                cycles: test.cycles.or(self.system.timing.default_test_cycles),
                duration_secs: test.duration_secs.or(self.system.timing.default_test_duration_secs),
                min_duration_secs: test.min_duration_secs,
            };
            
            let config = TestMemoryConfig::new(window_mode, chunk_mode, allow_misaligned, requires_locality)
                .with_timing(timing)
                .with_streams(test.streams.unwrap_or(1)) // Default to 1 stream (equivalent to TM5 jump=1)
                .with_pattern_config(test.pattern_mode, test.pattern_param0, test.pattern_param1);
            
            (test.function.as_str(), config)
        }).collect()
    }

    /// Get test configs in TM5 test sequence order (if available) with repetition support
    pub fn get_test_configs_with_sequence(&self) -> Vec<(&str, TestMemoryConfig)> {
        // Check if we have TM5 test sequence data
        if let Some(ref metadata) = self.legacy_metadata {
            if !metadata.tm5_test_sequence.is_empty() {
                return self.get_tm5_sequence_configs(&metadata.tm5_test_sequence);
            }
        }
        
        // Fallback to standard sequential execution
        self.get_test_configs()
    }
    
    /// Get test configs following TM5 test sequence order and repetition
    fn get_tm5_sequence_configs(&self, sequence: &[u32]) -> Vec<(&str, TestMemoryConfig)> {
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
                        matches!(test.function.as_str(), "CacheBusting" | "RefreshStable")
                    });
                    
                    let timing = TestTiming {
                        cycles: test.cycles.or(self.system.timing.default_test_cycles),
                        duration_secs: test.duration_secs.or(self.system.timing.default_test_duration_secs),
                        min_duration_secs: test.min_duration_secs,
                    };
                    
                    let config = TestMemoryConfig::new(window_mode, chunk_mode, allow_misaligned, requires_locality)
                        .with_timing(timing)
                        .with_streams(test.streams.unwrap_or(1))
                        .with_pattern_config(test.pattern_mode, test.pattern_param0, test.pattern_param1);
                    
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
    if let Some(ref mode) = test.window_mode {
        match mode.as_str() {
            "full_allocation" | "full-allocation" => WindowMode::FullAllocation,
            "fixed_size" | "fixed-size" => WindowMode::FixedSize { 
                size_mb: test.window_size_mb.unwrap_or(64) 
            },
            "cache_relative" | "cache-relative" => WindowMode::CacheRelative { 
                multiplier: test.window_cache_multiplier.unwrap_or(2.0) 
            },
            "global_window" | "global-window" | "0" => {
                // Use the global default window
                self.get_default_window_mode()
            }
            _ => {
                log::warn!("Unknown window mode '{}', using default", mode);
                self.get_default_window_mode()
            }
        }
    } else if let Some(size_mb) = test.window_size_mb {
        // Legacy behavior: if size is specified without mode
        if size_mb == 0 {
            // 0 means use global window
            self.get_default_window_mode()
        } else {
            WindowMode::FixedSize { size_mb }
        }
    } else {
        self.get_default_window_mode()
    }
}
    
fn parse_test_chunk_mode(&self, test: &TestConfig) -> ChunkMode {
    if let Some(ref mode) = test.chunk_mode {
        match mode.as_str() {
            "auto_optimal" | "auto-optimal" => ChunkMode::AutoOptimal,
            "fixed_size" | "fixed-size" => ChunkMode::FixedSize { 
                size_mb: test.block_size_mb.unwrap_or(16) 
            },
            "window_fraction" | "window-fraction" => ChunkMode::WindowFraction { 
                fraction: test.block_window_fraction.unwrap_or(0.125) 
            },
            "window_size" | "window-size" | "0" => {
                // Use window size as block size (TM5 behavior for 0)
                ChunkMode::WindowFraction { fraction: 1.0 }
            }
            _ => {
                log::warn!("Unknown chunk mode '{}', using default", mode);
                self.get_default_chunk_mode()
            }
        }
    } else if let Some(size_mb) = test.block_size_mb {
        // Legacy behavior: if size is specified without mode
        if size_mb == 0 {
            // 0 means use window size (TM5 behavior)
            ChunkMode::WindowFraction { fraction: 1.0 }
        } else {
            ChunkMode::FixedSize { size_mb }
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
                default_window_mode: "full_allocation".to_string(),
                default_window_size_mb: None,
                window_cache_multiplier: None,
                default_chunk_mode: "auto_optimal".to_string(),
                default_chunk_size_mb: None,
                block_window_fraction: None,
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
			cpu_pinning: CpuPinningConfig::default(),  // Add this
			memory_allocation: MemoryAllocationConfig::default(),  // Add this
        },
        test_sequence: vec![
            // Critical: Full memory stuck bit test
            TestConfig {
                enabled: true,
                function: "StuckBitTest".to_string(),
                cycles: Some(1),                       // 1 cycle is thorough enough
                duration_secs: None,
                min_duration_secs: None,
                window_mode: Some("full-allocation".to_string()), // Must test ALL memory
                window_size_mb: None,
                window_cache_multiplier: None,
                chunk_mode: Some("window-fraction".to_string()),
                block_size_mb: None,
                block_window_fraction: Some(0.0625),   // 1/16th for efficiency
                allow_misaligned: Some(false),
                requires_locality: Some(false),
                streams: Some(1),
                pattern_mode: None,
                pattern_param0: None,
                pattern_param1: None,
                parameter: None,
            },
            
            // RefreshStable - needs small window for refresh timing
            TestConfig {
                enabled: true,
                function: "RefreshStable".to_string(),
                cycles: Some(1),
                duration_secs: None,
                min_duration_secs: None,
                window_mode: Some("cache-relative".to_string()),
                window_size_mb: None,
                window_cache_multiplier: Some(2.0),    // 2x cache for refresh testing
                chunk_mode: Some("fixed-size".to_string()),
                block_size_mb: Some(1),                // Small 1MB blocks
                block_window_fraction: None,
                allow_misaligned: Some(false),
                requires_locality: Some(true),
                streams: Some(1),
                pattern_mode: None,
                pattern_param0: None,
                pattern_param1: None,
                parameter: None,
            },
            
            // SimpleTest - general pattern test with TM5 compatibility
            TestConfig {
                enabled: true,
                function: "SimpleTest".to_string(),
                cycles: Some(1),
                duration_secs: None,
                min_duration_secs: None,
                window_mode: Some("fixed-size".to_string()),
                window_size_mb: Some(880),             // TM5 default window
                window_cache_multiplier: None,
                chunk_mode: Some("fixed-size".to_string()),
                block_size_mb: Some(16),               // TM5 typical block size
                block_window_fraction: None,
                allow_misaligned: Some(false),
                requires_locality: Some(false),
                streams: Some(1),
                pattern_mode: Some(1),
                pattern_param0: Some(0x1E5F),
                pattern_param1: Some(0x45357354),
                parameter: None,
            },
            
            // MirrorMove128 - SIMD test with optimal locality
            TestConfig {
                enabled: true,
                function: "MirrorMove128".to_string(),
                cycles: Some(1),
                duration_secs: None,
                min_duration_secs: None,
                window_mode: Some("fixed-size".to_string()),
                window_size_mb: Some(64),              // Good SIMD locality
                window_cache_multiplier: None,
                chunk_mode: Some("fixed-size".to_string()),
                block_size_mb: Some(16),               // 16MB for 128-bit alignment
                block_window_fraction: None,
                allow_misaligned: Some(false),
                requires_locality: Some(true),
                streams: Some(1),
                pattern_mode: None,
                pattern_param0: None,
                pattern_param1: None,
                parameter: None,
            },
            
            // MirrorMove256 - AVX2 with dual streams
            TestConfig {
                enabled: true,
                function: "MirrorMove256".to_string(),
                cycles: Some(1),
                duration_secs: None,
                min_duration_secs: None,
                window_mode: Some("fixed-size".to_string()),
                window_size_mb: Some(128),             // Larger for AVX2
                window_cache_multiplier: None,
                chunk_mode: Some("fixed-size".to_string()),
                block_size_mb: Some(32),               // 32MB for 256-bit alignment
                block_window_fraction: None,
                allow_misaligned: Some(false),
                requires_locality: Some(true),
                streams: Some(2),                      // Dual stream for AVX2
                pattern_mode: None,
                pattern_param0: None,
                pattern_param1: None,
                parameter: None,
            },
            
            // CacheBusting - specifically sized for cache stress
            TestConfig {
                enabled: true,
                function: "CacheBusting".to_string(),
                cycles: Some(1),
                duration_secs: None,
                min_duration_secs: None,
                window_mode: Some("cache-relative".to_string()),
                window_size_mb: None,
                window_cache_multiplier: Some(0.5),    // Half cache to ensure busting
                chunk_mode: Some("fixed-size".to_string()),
                block_size_mb: Some(1),                // 1MB blocks for cache lines
                block_window_fraction: None,
                allow_misaligned: Some(false),
                requires_locality: Some(true),
                streams: Some(4),                      // 4 streams for cache stress
                pattern_mode: None,
                pattern_param0: None,
                pattern_param1: None,
                parameter: None,
            },
            
            // RandomTorture - full memory random access
            TestConfig {
                enabled: true,
                function: "RandomTorture".to_string(),
                cycles: Some(1),
                duration_secs: None,
                min_duration_secs: None,
                window_mode: Some("full-allocation".to_string()), // Need full memory
                window_size_mb: None,
                window_cache_multiplier: None,
                chunk_mode: Some("fixed-size".to_string()),
                block_size_mb: Some(8),                // 8MB blocks
                block_window_fraction: None,
                allow_misaligned: Some(true),          // Maximum stress
                requires_locality: Some(false),
                streams: Some(8),                      // 8 streams for chaos
                pattern_mode: None,
                pattern_param0: None,
                pattern_param1: None,
                parameter: None,
            },
            
            // StrideAccess - test various stride patterns
            TestConfig {
                enabled: true,
                function: "StrideAccess".to_string(),
                cycles: Some(1),
                duration_secs: None,
                min_duration_secs: None,
                window_mode: Some("full-allocation".to_string()),
                window_size_mb: None,
                window_cache_multiplier: None,
                chunk_mode: Some("auto-optimal".to_string()), // Let TMR optimize
                block_size_mb: None,
                block_window_fraction: None,
                allow_misaligned: Some(false),
                requires_locality: Some(false),
                streams: Some(4),                      // 4 streams for patterns
                pattern_mode: None,
                pattern_param0: None,
                pattern_param1: None,
                parameter: None,
            },
            
            // BandwidthSat - maximum bandwidth test
            TestConfig {
                enabled: true,
                function: "BandwidthSat".to_string(),
                cycles: Some(1),
                duration_secs: None,
                min_duration_secs: None,
                window_mode: Some("full-allocation".to_string()), // Max bandwidth
                window_size_mb: None,
                window_cache_multiplier: None,
                chunk_mode: Some("fixed-size".to_string()),
                block_size_mb: Some(32),               // Large blocks for bandwidth
                block_window_fraction: None,
                allow_misaligned: Some(false),
                requires_locality: Some(false),
                streams: Some(1),                      // Single stream for max bandwidth
                pattern_mode: None,
                pattern_param0: None,
                pattern_param1: None,
                parameter: None,
            },
            
            // BlockMove - memory copy test
            TestConfig {
                enabled: true,
                function: "BlockMove".to_string(),
                cycles: Some(1),
                duration_secs: None,
                min_duration_secs: None,
                window_mode: Some("full-allocation".to_string()), // Need src+dst space
                window_size_mb: None,
                window_cache_multiplier: None,
                chunk_mode: Some("fixed-size".to_string()),
                block_size_mb: Some(16),               // 16MB blocks
                block_window_fraction: None,
                allow_misaligned: Some(false),
                requires_locality: Some(false),
                streams: Some(2),                      // Dual stream copy pattern
                pattern_mode: None,
                pattern_param0: None,
                pattern_param1: None,
                parameter: None,
            },
            
            // Legacy TM5-style test showing "window-size" block mode
            TestConfig {
                enabled: true,
                function: "SimpleTest".to_string(),
                cycles: Some(1),
                duration_secs: None,
                min_duration_secs: None,
                window_mode: Some("global-window".to_string()), // Use global default
                window_size_mb: None,
                window_cache_multiplier: None,
                chunk_mode: Some("window-size".to_string()),    // Block = window (TM5 0)
                block_size_mb: None,
                block_window_fraction: None,
                allow_misaligned: Some(false),
                requires_locality: Some(false),
                streams: Some(1),
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
                    default_window_mode: "fixed_size".to_string(),
                    default_window_size_mb: Some(880),     // TM5 default window
                    window_cache_multiplier: None,
                    default_chunk_mode: "auto_optimal".to_string(),
                    default_chunk_size_mb: None,
                    block_window_fraction: None,
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
				cpu_pinning: CpuPinningConfig::default(),  // Add this
				memory_allocation: MemoryAllocationConfig::default(),  // Add this
	            },
            test_sequence: vec![
                TestConfig {
                    enabled: true,
                    function: "StuckBitTest".to_string(),
                    cycles: Some(1),
                    duration_secs: None,
                    min_duration_secs: None,
                    window_mode: Some("full_allocation".to_string()), // Override to test all memory
                    window_size_mb: None,
                    window_cache_multiplier: None,
                    chunk_mode: None,                      // Use default auto-optimal
                    block_size_mb: None,
                    block_window_fraction: None,
                    allow_misaligned: Some(false),
                    requires_locality: Some(false),
                    streams: Some(1),                      // Single stream
                    pattern_mode: None,
                    pattern_param0: None,
                    pattern_param1: None,
                    parameter: None,
                },
                TestConfig {
                    enabled: true,
                    function: "SimpleTest".to_string(),
                    cycles: Some(1),
                    duration_secs: None,
                    min_duration_secs: None,
                    window_mode: None,                     // Use default fixed 880MB
                    window_size_mb: None,
                    window_cache_multiplier: None,
                    chunk_mode: Some("fixed_size".to_string()),
                    block_size_mb: Some(16),               // TM5-style block size
                    block_window_fraction: None,
                    allow_misaligned: Some(false),
                    requires_locality: Some(false),
                    streams: Some(1),                      // Single stream
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
                window_mode: None,  // Use global default
                window_size_mb: None,
                window_cache_multiplier: None,
                
                // Handle TM5 block size with new special values
                chunk_mode: if test.test_chunk_size_mb == 0 {
                    Some("window-size".to_string())  // 0 = use window size
                } else {
                    Some("fixed_size".to_string())
                },
                block_size_mb: if test.test_chunk_size_mb == 0 {
                    None  // "window-size" mode doesn't need a value
                } else {
                    Some(test.test_chunk_size_mb)
                },
                block_window_fraction: None,
                
                allow_misaligned: Some(false), // Legacy configs assume aligned access
                requires_locality: Some(matches!(test.function.as_str(), "RefreshStable")),
                
                // Map parameter to streams
                streams: Some(Self::map_parameter_to_streams(&test.function, test.parameter)),
                
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
                
                default_window_mode: "fixed_size".to_string(),
                default_window_size_mb: Some(self.memory_setup.testing_window_size_mb),
                window_cache_multiplier: None,
                
                default_chunk_mode: "auto_optimal".to_string(),
                default_chunk_size_mb: None,
                block_window_fraction: None,
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
			cpu_pinning: CpuPinningConfig::default(),  // Add this
			memory_allocation: MemoryAllocationConfig::default(),  // Add this
        },
        test_sequence,
        legacy_metadata: Some(LegacyMetadata {
            tm5_test_sequence: self.main_section.test_sequence.clone(),
            tm5_cycles: self.main_section.cycles,
            tm5_time_percent: self.main_section.time_percent,
        }),
    })
}

    // Map legacy function names to modern equivalents

fn map_legacy_function(legacy_name: &str) -> Result<String, String> {
    match legacy_name {
        "RefreshStable" => Ok("RefreshStable".to_string()),
        "SimpleTest" => Ok("SimpleTest".to_string()),
        "MirrorMove" => Ok("MirrorMove128".to_string()),  // TM5 base MirrorMove -> TMR 128-bit SIMD
        "MirrorMove128" => Ok("MirrorMove128".to_string()),
        "MirrorMove256" => Ok("MirrorMove256".to_string()),
        "MirrorMove512" => Ok("MirrorMove512".to_string()),
        "BlockMove" => Ok("BlockMove".to_string()),
        _ => Err(format!("Unknown legacy test function: '{}'", legacy_name))
    }
}

    // Map TM5 parameter values to modern streams concept
    fn map_parameter_to_streams(function: &str, parameter: u32) -> u32 {
        match function {
            "MirrorMove" | "MirrorMove128" | "MirrorMove256" | "MirrorMove512" => {
                // For MirrorMove tests, parameter directly maps to thread simulation count
                match parameter {
                    0 | 1 => 1,      // Single stream
                    2 => 2,          // Dual stream
                    3 => 3,          // Triple stream
                    4 => 4,          // Quad stream
                    254 => 2,        // Special dual stream pattern
                    510 => 2,        // Special dual stream pattern
                    16384 => 16,     // 16 streams
                    _ => {
                        // For other values, try to map sensibly
                        if parameter > 100 {
                            4 // Default to quad stream for large values
                        } else {
                            parameter.min(16) // Cap at 16 streams
                        }
                    }
                }
            }
            "SimpleTest" => {
                // SimpleTest doesn't use parameter for streams, default to 1
                1
            }
            _ => 1, // Default single stream
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
    println!("   Features: Full memory stuck bit test + timed stress tests with streams");
    println!("   Timing: 3 cycles, ~2-3 minutes per cycle with comprehensive coverage");
    println!("   Memory: Uses full allocation for critical tests, optimized windows for others");
    println!("   Streams: Configurable access patterns (1-16 streams) for different test scenarios");
    println!();
    println!("✅ Created demo_tm5_compatible.json - TM5-compatible configuration");
    println!("   Features: TM5-style allocation with modern stuck bit test added");
    println!("   Timing: 3 cycles, faster execution for compatibility");
    println!("   Memory: Maximum allocation minus 128MB reserve, 880MB testing window");
    println!("   Streams: Single stream mode for compatibility");
    println!();
    println!("Configuration Architecture Summary:");
    println!("  Stage 1: Memory Allocation - Maximum available memory per thread");
    println!("  Stage 2: Testing Window - Configurable window within allocation");
    println!("  Stage 3: Block/Chunk Size - Auto-optimized per test with alignment");
    println!("  Access Patterns: 1-16 configurable streams (TM5 jump/parameter equivalent)");
    println!("  Timing: Per-test cycles/duration limits + global suite limits");
    println!("  Critical: StuckBitTest ensures full memory coverage for bit errors");

    Ok(())
}