use crate::{ErrorMode};
use crate::layout::{MemoryStrategy, AllocationMode, WindowMode, BlockMode};
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
    pub default_block_mode: String,    // "auto_optimal", "fixed_size", "window_fraction"
    pub default_block_size_mb: Option<u32>, // For fixed_size mode
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
    
    pub block_mode: Option<String>,         // Override default block mode  
    pub block_size_mb: Option<u32>,         // For fixed block mode
    pub block_window_fraction: Option<f64>, // For window fraction mode
    
    pub allow_misaligned: Option<bool>,     // Allow unaligned accesses
    pub requires_locality: Option<bool>,    // Test needs temporal locality
    
    // Legacy TM5 compatibility (preserved but not used in new logic)
    pub pattern_mode: Option<u32>,
    pub pattern_param0: Option<u64>,
    pub pattern_param1: Option<u64>,
    pub parameter: Option<u32>,
}

// Legacy config parser (v1.0 - TestMem5 format) - unchanged
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
    pub test_block_size_mb: u32,
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
        report.push_str(&format!(", Window: {}, Block: {}\n", 
            self.system.memory_strategy.default_window_mode,
            self.system.memory_strategy.default_block_mode
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
            
            // Window override
            if let Some(mode) = &test.window_mode {
                report.push_str(&format!(", Window:{}", mode));
                if mode == "fixed_size" {
                    if let Some(mb) = test.window_size_mb {
                        report.push_str(&format!(" {}MB", mb));
                    }
                } else if mode == "cache_relative" {
                    if let Some(mult) = test.window_cache_multiplier {
                        report.push_str(&format!(" {}x", mult));
                    }
                }
            }
            
            // Block override
            if let Some(mode) = &test.block_mode {
                report.push_str(&format!(", Block:{}", mode));
                if mode == "fixed_size" {
                    if let Some(mb) = test.block_size_mb {
                        report.push_str(&format!(" {}MB", mb));
                    }
                } else if mode == "window_fraction" {
                    if let Some(frac) = test.block_window_fraction {
                        report.push_str(&format!(" {:.1}%", frac * 100.0));
                    }
                }
            }
            
            if test.allow_misaligned == Some(true) {
                report.push_str(", Misaligned");
            }
            if test.requires_locality == Some(true) {
                report.push_str(", Locality");
            }
            
            report.push_str("\n");
        }
        
        report
    }

    pub fn save_to_file(&self, path: &str) -> Result<(), String> {
        let json = serde_json::to_string_pretty(self).map_err(|e| format!("Failed to serialize config: {}", e))?;

        fs::write(path, json).map_err(|e| format!("Failed to write config file: {}", e))
    }

    // Convert to runtime configuration
    pub fn to_memory_strategy(&self) -> MemoryStrategy {
        let allocation_mode = match self.system.memory_strategy.allocation_mode.as_str() {
            "max_available" => AllocationMode::MaxAvailable { 
                reserve_mb: self.system.memory_strategy.reserve_mb.unwrap_or(128) 
            },
            "percentage_reserve" => AllocationMode::PercentageReserve { 
                reserve_percent: self.system.memory_strategy.reserve_percent.unwrap_or(10.0) 
            },
            "fixed_reserve" => AllocationMode::FixedReserve { 
                reserve_gib: self.system.memory_strategy.reserve_gib.unwrap_or(2.0) 
            },
            _ => AllocationMode::PercentageReserve { reserve_percent: 10.0 },
        };
        
        let default_window_mode = match self.system.memory_strategy.default_window_mode.as_str() {
            "full_allocation" => WindowMode::FullAllocation,
            "fixed_size" => WindowMode::FixedSize { 
                size_mb: self.system.memory_strategy.default_window_size_mb.unwrap_or(880) 
            },
            "cache_relative" => WindowMode::CacheRelative { 
                multiplier: self.system.memory_strategy.window_cache_multiplier.unwrap_or(2.0) 
            },
            _ => WindowMode::FullAllocation,
        };
        
        let default_block_mode = match self.system.memory_strategy.default_block_mode.as_str() {
            "auto_optimal" => BlockMode::AutoOptimal,
            "fixed_size" => BlockMode::FixedSize { 
                size_mb: self.system.memory_strategy.default_block_size_mb.unwrap_or(16) 
            },
            "window_fraction" => BlockMode::WindowFraction { 
                fraction: self.system.memory_strategy.block_window_fraction.unwrap_or(0.125) 
            },
            _ => BlockMode::AutoOptimal,
        };

        MemoryStrategy {
            allocation_mode,
            default_window_mode,
            default_block_mode,
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
        }
    }
    
    pub fn get_test_configs(&self) -> Vec<(&str, TestMemoryConfig)> {
        self.test_sequence.iter().filter(|t| t.enabled).map(|test| {
            let window_mode = self.parse_test_window_mode(test);
            let block_mode = self.parse_test_block_mode(test);
            let allow_misaligned = test.allow_misaligned.unwrap_or(false);
            let requires_locality = test.requires_locality.unwrap_or_else(|| {
                // Auto-detect based on function name
                matches!(test.function.as_str(), "CacheBusting" | "RefreshStable")
            });
            
            let timing = TestTiming {
                cycles: test.cycles.or(self.system.timing.default_test_cycles),
                duration_secs: test.duration_secs.or(self.system.timing.default_test_duration_secs),
                min_duration_secs: test.min_duration_secs,
            };
            
            let config = TestMemoryConfig::new(window_mode, block_mode, allow_misaligned, requires_locality)
                .with_timing(timing);
            
            (test.function.as_str(), config)
        }).collect()
    }
    
    fn parse_test_window_mode(&self, test: &TestConfig) -> WindowMode {
        if let Some(ref mode) = test.window_mode {
            match mode.as_str() {
                "full_allocation" => WindowMode::FullAllocation,
                "fixed_size" => WindowMode::FixedSize { 
                    size_mb: test.window_size_mb.unwrap_or(64) 
                },
                "cache_relative" => WindowMode::CacheRelative { 
                    multiplier: test.window_cache_multiplier.unwrap_or(2.0) 
                },
                _ => self.to_memory_strategy().default_window_mode,
            }
        } else {
            self.to_memory_strategy().default_window_mode
        }
    }
    
    fn parse_test_block_mode(&self, test: &TestConfig) -> BlockMode {
        if let Some(ref mode) = test.block_mode {
            match mode.as_str() {
                "auto_optimal" => BlockMode::AutoOptimal,
                "fixed_size" => BlockMode::FixedSize { 
                    size_mb: test.block_size_mb.unwrap_or(16) 
                },
                "window_fraction" => BlockMode::WindowFraction { 
                    fraction: test.block_window_fraction.unwrap_or(0.125) 
                },
                _ => self.to_memory_strategy().default_block_mode,
            }
        } else {
            self.to_memory_strategy().default_block_mode
        }
    }

    pub fn create_demo_config() -> Self {
        ModernConfig {
            config_format_version: CONFIG_VERSION.to_string(),
            application_name: format!("{} ({})", APP_NAME, APP_SHORT_NAME),
            metadata: ConfigMetadata {
                name: "TMR Comprehensive Memory Test".to_string(),
                author: "tmr_user".to_string(),
                version: "1.0".to_string(),
                description: Some("Comprehensive three-stage memory testing with timing controls and full memory coverage".to_string()),
                created: Some("2025-06-29".to_string()),
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
                    default_block_mode: "auto_optimal".to_string(),
                    default_block_size_mb: None,
                    block_window_fraction: None,
                },
                cpu_config: CpuConfig {
                    cpu_type: "cores".to_string(),
                    usage_percent: 100,
                },
                error_mode: "log".to_string(),
                timing: TimingConfig {
                    global_cycles: Some(3),                // 3 complete test suite cycles
                    global_duration_secs: None,            // No time limit
                    default_test_cycles: None,             // Per-test timing below
                    default_test_duration_secs: Some(10),  // Default 10s per test
                },
                large_pages: true,
            },
            test_sequence: vec![
                // Critical: Full memory stuck bit test
                TestConfig {
                    enabled: true,
                    function: "StuckBitTest".to_string(),
                    cycles: Some(1),                       // Run once per cycle (it's thorough)
                    duration_secs: None,
                    min_duration_secs: None,
                    window_mode: Some("full_allocation".to_string()), // Test ALL memory
                    window_size_mb: None,
                    window_cache_multiplier: None,
                    block_mode: Some("window_fraction".to_string()),
                    block_size_mb: None,
                    block_window_fraction: Some(0.0625),   // 1/16th for efficiency
                    allow_misaligned: Some(false),
                    requires_locality: Some(false),
                    pattern_mode: None,
                    pattern_param0: None,
                    pattern_param1: None,
                    parameter: None,
                },
                
                // SIMD tests with locality
                TestConfig {
                    enabled: true,
                    function: "MirrorMove128NonTemporal".to_string(),
                    cycles: None,
                    duration_secs: Some(15),               // 15 seconds of SIMD testing
                    min_duration_secs: None,
                    window_mode: Some("fixed_size".to_string()),
                    window_size_mb: Some(64),              // 64MB window for locality
                    window_cache_multiplier: None,
                    block_mode: Some("fixed_size".to_string()),
                    block_size_mb: Some(16),               // 16MB blocks for alignment
                    block_window_fraction: None,
                    allow_misaligned: Some(false),
                    requires_locality: Some(true),
                    pattern_mode: None,
                    pattern_param0: None,
                    pattern_param1: None,
                    parameter: None,
                },
                
                TestConfig {
                    enabled: true,
                    function: "MirrorMove256NonTemporal".to_string(),
                    cycles: None,
                    duration_secs: Some(15),
                    min_duration_secs: None,
                    window_mode: Some("fixed_size".to_string()),
                    window_size_mb: Some(128),
                    window_cache_multiplier: None,
                    block_mode: Some("fixed_size".to_string()),
                    block_size_mb: Some(32),
                    block_window_fraction: None,
                    allow_misaligned: Some(false),
                    requires_locality: Some(true),
                    pattern_mode: None,
                    pattern_param0: None,
                    pattern_param1: None,
                    parameter: None,
                },
                
                // Memory pattern tests on full allocation
                TestConfig {
                    enabled: true,
                    function: "SimpleTest".to_string(),
                    cycles: Some(100),                     // 100 cycles
                    duration_secs: Some(30),               // Or 30 seconds max
                    min_duration_secs: None,
                    window_mode: Some("full_allocation".to_string()), // Test large portions
                    window_size_mb: None,
                    window_cache_multiplier: None,
                    block_mode: Some("fixed_size".to_string()),
                    block_size_mb: Some(4),
                    block_window_fraction: None,
                    allow_misaligned: Some(false),
                    requires_locality: Some(false),
                    pattern_mode: Some(1),
                    pattern_param0: Some(0x1E5F),
                    pattern_param1: Some(0x45357354),
                    parameter: Some(0),
                },
                
                // Cache tests with locality
                TestConfig {
                    enabled: true,
                    function: "CacheBusting".to_string(),
                    cycles: None,
                    duration_secs: Some(20),               // 20 seconds of cache busting
                    min_duration_secs: None,
                    window_mode: Some("cache_relative".to_string()),
                    window_size_mb: None,
                    window_cache_multiplier: Some(0.5),    // Half cache size
                    block_mode: Some("fixed_size".to_string()),
                    block_size_mb: Some(1),
                    block_window_fraction: None,
                    allow_misaligned: Some(false),
                    requires_locality: Some(true),
                    pattern_mode: None,
                    pattern_param0: None,
                    pattern_param1: None,
                    parameter: None,
                },
                
                // Stress tests on full allocation
                TestConfig {
                    enabled: true,
                    function: "RandomTorture".to_string(),
                    cycles: None,
                    duration_secs: Some(25),               // 25 seconds of torture
                    min_duration_secs: None,
                    window_mode: Some("full_allocation".to_string()),
                    window_size_mb: None,
                    window_cache_multiplier: None,
                    block_mode: Some("fixed_size".to_string()),
                    block_size_mb: Some(8),
                    block_window_fraction: None,
                    allow_misaligned: Some(true),          // Maximum stress
                    requires_locality: Some(false),
                    pattern_mode: None,
                    pattern_param0: None,
                    pattern_param1: None,
                    parameter: None,
                },
                
                // Bandwidth test on full allocation
                TestConfig {
                    enabled: true,
                    function: "BandwidthSat".to_string(),
                    cycles: None,
                    duration_secs: Some(15),               // 15 seconds of bandwidth
                    min_duration_secs: None,
                    window_mode: Some("full_allocation".to_string()),
                    window_size_mb: None,
                    window_cache_multiplier: None,
                    block_mode: Some("fixed_size".to_string()),
                    block_size_mb: Some(32),               // Large blocks for bandwidth
                    block_window_fraction: None,
                    allow_misaligned: Some(false),
                    requires_locality: Some(false),
                    pattern_mode: None,
                    pattern_param0: None,
                    pattern_param1: None,
                    parameter: None,
                },
            ],
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
                    default_block_mode: "auto_optimal".to_string(),
                    default_block_size_mb: None,
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
                    block_mode: None,                      // Use default auto-optimal
                    block_size_mb: None,
                    block_window_fraction: None,
                    allow_misaligned: Some(false),
                    requires_locality: Some(false),
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
                    block_mode: Some("fixed_size".to_string()),
                    block_size_mb: Some(16),               // TM5-style block size
                    block_window_fraction: None,
                    allow_misaligned: Some(false),
                    requires_locality: Some(false),
                    pattern_mode: Some(1),
                    pattern_param0: Some(0x1E5F),
                    pattern_param1: Some(0x45357354),
                    parameter: Some(0),
                },
            ],
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
                    test_block_size_mb: test.get("Test Block Size (Mb)").and_then(|s| s.parse().ok()).unwrap_or(0),
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
    pub fn to_modern_config(&self) -> ModernConfig {
        let global_time_multiplier = self.main_section.time_percent as f64 / 100.0;
        
        // Add critical stuck bit test first
        let mut test_sequence = vec![
            TestConfig {
                enabled: true,
                function: "StuckBitTest".to_string(),
                cycles: Some(1),
                duration_secs: None,
                min_duration_secs: None,
                window_mode: Some("full_allocation".to_string()),
                window_size_mb: None,
                window_cache_multiplier: None,
                block_mode: Some("window_fraction".to_string()),
                block_size_mb: None,
                block_window_fraction: Some(0.0625),
                allow_misaligned: Some(false),
                requires_locality: Some(false),
                pattern_mode: None,
                pattern_param0: None,
                pattern_param1: None,
                parameter: None,
            }
        ];
        
        // Add legacy tests
        for test in &self.tests {
            if test.enabled {
                // Calculate effective time based on both global and test-specific multipliers
                let base_duration = (test.time_percent as f64 * global_time_multiplier / 10.0) as u32;
                let effective_duration = base_duration.max(1).min(300); // 1-300 seconds
                
                test_sequence.push(TestConfig {
                    enabled: true,
                    function: Self::map_legacy_function(&test.function),
                    cycles: None,
                    duration_secs: Some(effective_duration),
                    min_duration_secs: None,
                    
                    // Map legacy window to modern equivalent
                    window_mode: if test.test_block_size_mb > 0 {
                        Some("fixed_size".to_string()) // Use TM5 window
                    } else {
                        Some("full_allocation".to_string()) // Test more memory
                    },
                    window_size_mb: None, // Use global setting
                    window_cache_multiplier: None,
                    
                    // Map legacy block size to modern equivalent  
                    block_mode: if test.test_block_size_mb > 0 {
                        Some("fixed_size".to_string())
                    } else {
                        None // Use default auto-optimal
                    },
                    block_size_mb: if test.test_block_size_mb > 0 {
                        Some(test.test_block_size_mb)
                    } else {
                        None
                    },
                    block_window_fraction: None,
                    allow_misaligned: Some(false), // Legacy configs assume aligned access
                    requires_locality: Some(matches!(test.function.as_str(), "RefreshStable")),
                    
                    // Preserve legacy test parameters
                    pattern_mode: Some(test.pattern_mode),
                    pattern_param0: Some(test.pattern_param0),
                    pattern_param1: Some(test.pattern_param1),
                    parameter: Some(test.parameter),
                });
            }
        }

        ModernConfig {
            config_format_version: CONFIG_VERSION.to_string(),
            application_name: format!("{} ({})", APP_NAME, APP_SHORT_NAME),
            metadata: ConfigMetadata {
                name: format!("{} (Legacy Converted)", self.main_section.config_name),
                author: self.main_section.config_author.clone(),
                version: "1.0".to_string(),
                description: Some("Converted from legacy TestMem5 config with comprehensive memory testing including stuck bit test".to_string()),
                created: None,
                tested_with_version: APP_VERSION.to_string(),
            },
            system: SystemConfig {
                memory_strategy: MemoryStrategyConfig {
                    // TM5-compatible Stage 1 allocation
                    allocation_mode: "max_available".to_string(),
                    reserve_mb: Some(self.memory_setup.reserved_memory_mb),
                    reserve_percent: None,
                    reserve_gib: None,
                    
                    // TM5-compatible Stage 2 window
                    default_window_mode: "fixed_size".to_string(),
                    default_window_size_mb: Some(self.memory_setup.testing_window_size_mb),
                    window_cache_multiplier: None,
                    
                    // Modern Stage 3 block sizing
                    default_block_mode: "auto_optimal".to_string(),
                    default_block_size_mb: None,
                    block_window_fraction: None,
                },
                cpu_config: CpuConfig {
                    cpu_type: if self.main_section.cores > 0 { "cores" } else { "threads" }.to_string(),
                    usage_percent: 100,
                },
                error_mode: "log".to_string(),
                timing: TimingConfig {
                    global_cycles: Some(self.main_section.cycles),
                    global_duration_secs: None,
                    default_test_cycles: None,
                    default_test_duration_secs: Some(10), // Default 10s per test
                },
                large_pages: true,
            },
            test_sequence,
        }
    }

    // Map legacy function names to modern equivalents
    fn map_legacy_function(legacy_name: &str) -> String {
        match legacy_name {
            "RefreshStable" => "RefreshStable".to_string(),
            "SimpleTest" => "SimpleTest".to_string(),
            "MirrorMove" => "MirrorMove128NonTemporal".to_string(),
            "MirrorMove128" => "MirrorMove128NonTemporal".to_string(),
            "MirrorMove256" => "MirrorMove256NonTemporal".to_string(),
            "MirrorMove512" => "MirrorMove512NonTemporal".to_string(),
            "BlockMove" => "BandwidthSat".to_string(), // Map to bandwidth test
            _ => {
                log::warn!("Unknown legacy function '{}', mapping to SimpleTest", legacy_name);
                "SimpleTest".to_string()
            }
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
        Ok(legacy.to_modern_config())
    } else {
        // Try JSON first, then legacy
        ModernConfig::load_from_file(path).or_else(|_| {
            let legacy = LegacyConfig::load_from_file(path)?;
            Ok(legacy.to_modern_config())
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
    println!("  Critical: StuckBitTest ensures full memory coverage for bit errors");

    Ok(())
}
