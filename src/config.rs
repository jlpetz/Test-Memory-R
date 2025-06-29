use crate::{ErrorMode, MemoryStrategy};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

// Application constants
pub const APP_NAME: &str = "Test Memory R";
pub const APP_SHORT_NAME: &str = "TMR";
pub const APP_VERSION: &str = "1.0.0";
pub const CONFIG_VERSION: &str = "2.0";

// Modern JSON configuration format v2.0
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
    pub cycles: u32,
    pub large_pages: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryStrategyConfig {
    #[serde(rename = "type")]
    pub strategy_type: String, // "tm5_compatible", "modern_optimal", "custom"
    
    // TM5-compatible settings (Stage 1 allocation)
    pub testing_window_size_mb: Option<u32>,  // Stage 2: Total window across all threads
    pub reserved_memory_mb: Option<u32>,      // Stage 1: OS memory reserve
    pub test_block_size_mb: Option<u32>,      // Stage 3: Default block size (can be overridden per test)
    
    // Modern settings
    pub memory_reserve_percent: Option<f64>,
    pub memory_reserve_gib: Option<f64>,
    pub memory_reserve_mib: Option<f64>,
    
    // Custom settings
    pub blocks_per_thread: Option<u32>,
    pub min_block_size_mb: Option<u32>,
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
    pub time_percent: u32,  // Effective test duration: combines TM5's global and per-test percentages
    
    // Stage 2 & 3 configuration per test
    pub window_size_mb: Option<u32>,    // Override default window size for this test
    pub block_size_mb: Option<u32>,     // Override default block size for this test
    pub allow_misaligned: Option<bool>, // Allow unaligned accesses for stress testing
    
    // Legacy TM5 compatibility
    pub pattern_mode: Option<u32>,
    pub pattern_param0: Option<u64>,
    pub pattern_param1: Option<u64>,
    pub parameter: Option<u32>,
}

// Legacy config parser (v1.0 - TestMem5 format)
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
    pub time_percent: u32,  // Global time multiplier (100 = normal, 1250 = 12.5x longer)
    pub cycles: u32,
    pub test_sequence: Vec<u32>,
}

#[derive(Debug, Clone)]
pub struct LegacyMemorySetup {
    pub testing_window_size_mb: u32,    // Stage 2: Total testing window
    pub reserved_memory_mb: u32,        // Stage 1: Memory reserved for OS
}

#[derive(Debug, Clone)]
pub struct LegacyTest {
    pub id: u32,
    pub enabled: bool,
    pub time_percent: u32,  // Relative time weighting for this test vs others
    pub function: String,
    pub pattern_mode: u32,
    pub pattern_param0: u64,
    pub pattern_param1: u64,
    pub parameter: u32,
    pub test_block_size_mb: u32,  // Stage 3: Block size for this specific test
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

    pub fn save_to_file(&self, path: &str) -> Result<(), String> {
        let json = serde_json::to_string_pretty(self).map_err(|e| format!("Failed to serialize config: {}", e))?;

        fs::write(path, json).map_err(|e| format!("Failed to write config file: {}", e))
    }

    // Convert to runtime configuration
    pub fn to_memory_strategy(&self) -> MemoryStrategy {
        match self.system.memory_strategy.strategy_type.as_str() {
            "tm5_compatible" => MemoryStrategy::TM5Compatible {
                testing_window_size_mb: self.system.memory_strategy.testing_window_size_mb.unwrap_or(880),
                reserved_memory_mb: self.system.memory_strategy.reserved_memory_mb.unwrap_or(128),
                test_block_size_mb: self.system.memory_strategy.test_block_size_mb.unwrap_or(0),
            },
            "modern_optimal" => {
                if let Some(gib) = self.system.memory_strategy.memory_reserve_gib {
                    MemoryStrategy::ModernOptimal { reserve_gib: Some(gib) }
                } else if let Some(mib) = self.system.memory_strategy.memory_reserve_mib {
                    MemoryStrategy::ModernOptimal { reserve_gib: Some(mib / 1024.0) }
                } else {
                    MemoryStrategy::ModernOptimal { reserve_gib: None }
                }
            },
            "custom" => MemoryStrategy::Custom {
                blocks_per_thread: self.system.memory_strategy.blocks_per_thread.unwrap_or(1),
                min_block_size_mb: self.system.memory_strategy.min_block_size_mb.unwrap_or(256),
            },
            _ => MemoryStrategy::TM5Compatible {
                testing_window_size_mb: 880,
                reserved_memory_mb: 128,
                test_block_size_mb: 0,
            },
        }
    }

    pub fn to_error_mode(&self) -> ErrorMode {
        match self.system.error_mode.as_str() {
            "halt" | "stop" => ErrorMode::Halt,
            "panic" | "debug" => ErrorMode::Panic,
            _ => ErrorMode::Log, // default
        }
    }

    pub fn create_demo_config() -> Self {
        ModernConfig {
            config_format_version: CONFIG_VERSION.to_string(),
            application_name: format!("{} ({})", APP_NAME, APP_SHORT_NAME),
            metadata: ConfigMetadata {
                name: "TM5-Compatible Three-Stage Memory Test".to_string(),
                author: "tmr_user".to_string(),
                version: "1.0".to_string(),
                description: Some("Three-stage memory testing: allocation, window, and block sizing with per-test configuration".to_string()),
                created: Some("2025-06-29".to_string()),
                tested_with_version: APP_VERSION.to_string(),
            },
            system: SystemConfig {
                memory_strategy: MemoryStrategyConfig {
                    strategy_type: "tm5_compatible".to_string(),
                    testing_window_size_mb: Some(880),     // Stage 2: 880MB total window
                    reserved_memory_mb: Some(128),         // Stage 1: 128MB for OS
                    test_block_size_mb: Some(16),          // Stage 3: Default 16MB blocks
                    memory_reserve_percent: None,
                    memory_reserve_gib: None,
                    memory_reserve_mib: None,
                    blocks_per_thread: None,
                    min_block_size_mb: None,
                },
                cpu_config: CpuConfig {
                    cpu_type: "cores".to_string(),
                    usage_percent: 100,
                },
                error_mode: "log".to_string(),
                cycles: 3,
                large_pages: true,
            },
            test_sequence: vec![
                TestConfig {
                    enabled: true,
                    function: "MirrorMove128NonTemporal".to_string(),
                    time_percent: 100,
                    window_size_mb: Some(64),          // Stage 2: 64MB window for this test
                    block_size_mb: Some(16),           // Stage 3: 16MB blocks for 128-bit alignment
                    allow_misaligned: Some(false),     // Require aligned accesses
                    pattern_mode: None,
                    pattern_param0: None,
                    pattern_param1: None,
                    parameter: None,
                },
                TestConfig {
                    enabled: true,
                    function: "MirrorMove256NonTemporal".to_string(),
                    time_percent: 100,
                    window_size_mb: Some(128),         // Stage 2: 128MB window for this test
                    block_size_mb: Some(32),           // Stage 3: 32MB blocks for 256-bit alignment
                    allow_misaligned: Some(false),
                    pattern_mode: None,
                    pattern_param0: None,
                    pattern_param1: None,
                    parameter: None,
                },
                TestConfig {
                    enabled: true,
                    function: "SimpleTest".to_string(),
                    time_percent: 200,
                    window_size_mb: None,              // Stage 2: Auto-calculate optimal window
                    block_size_mb: Some(4),            // Stage 3: 4MB blocks
                    allow_misaligned: Some(false),
                    pattern_mode: Some(1),
                    pattern_param0: Some(0x1E5F),
                    pattern_param1: Some(0x45357354),
                    parameter: Some(0),
                },
                TestConfig {
                    enabled: true,
                    function: "CacheBusting".to_string(),
                    time_percent: 150,
                    window_size_mb: None,              // Stage 2: Auto-size to cache dimensions
                    block_size_mb: Some(1),            // Stage 3: 1MB blocks for cache busting
                    allow_misaligned: Some(false),
                    pattern_mode: None,
                    pattern_param0: None,
                    pattern_param1: None,
                    parameter: None,
                },
                TestConfig {
                    enabled: true,
                    function: "RandomTorture".to_string(),
                    time_percent: 300,
                    window_size_mb: None,              // Stage 2: Auto-calculate
                    block_size_mb: Some(8),            // Stage 3: 8MB blocks
                    allow_misaligned: Some(true),      // Allow misaligned for stress testing
                    pattern_mode: None,
                    pattern_param0: None,
                    pattern_param1: None,
                    parameter: None,
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
        
        let test_sequence = self
            .tests
            .iter()
            .filter(|t| t.enabled)
            .map(|test| {
                // Calculate effective time percent considering both global and test-specific multipliers
                let effective_time_percent = ((test.time_percent as f64 * global_time_multiplier) as u32).max(1);
                
                TestConfig {
                    enabled: true,
                    function: Self::map_legacy_function(&test.function),
                    time_percent: effective_time_percent,
                    
                    // Map legacy block size to per-test configuration
                    window_size_mb: None, // Use global window size from memory setup
                    block_size_mb: if test.test_block_size_mb > 0 {
                        Some(test.test_block_size_mb)
                    } else {
                        None // Use default from strategy
                    },
                    allow_misaligned: Some(false), // Legacy configs assume aligned access
                    
                    // Preserve legacy test parameters
                    pattern_mode: Some(test.pattern_mode),
                    pattern_param0: Some(test.pattern_param0),
                    pattern_param1: Some(test.pattern_param1),
                    parameter: Some(test.parameter),
                }
            })
            .collect();

        ModernConfig {
            config_format_version: CONFIG_VERSION.to_string(),
            application_name: format!("{} ({})", APP_NAME, APP_SHORT_NAME),
            metadata: ConfigMetadata {
                name: format!("{} (Legacy Converted)", self.main_section.config_name),
                author: self.main_section.config_author.clone(),
                version: "1.0".to_string(),
                description: Some("Converted from legacy TestMem5 config with three-stage memory architecture".to_string()),
                created: None,
                tested_with_version: APP_VERSION.to_string(),
            },
            system: SystemConfig {
                memory_strategy: MemoryStrategyConfig {
                    strategy_type: "tm5_compatible".to_string(),
                    // Stage 1 & 2 settings from legacy config
                    testing_window_size_mb: Some(self.memory_setup.testing_window_size_mb),
                    reserved_memory_mb: Some(self.memory_setup.reserved_memory_mb),
                    test_block_size_mb: Some(0), // Legacy configs use per-test block sizes
                    memory_reserve_percent: None,
                    memory_reserve_gib: None,
                    memory_reserve_mib: None,
                    blocks_per_thread: None,
                    min_block_size_mb: None,
                },
                cpu_config: CpuConfig {
                    cpu_type: if self.main_section.cores > 0 { "cores" } else { "threads" }.to_string(),
                    usage_percent: 100,
                },
                error_mode: "log".to_string(),
                cycles: self.main_section.cycles,
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
    // Create modern demo config - TM5 compatible with three-stage architecture
    let modern_config = ModernConfig::create_demo_config();
    modern_config.save_to_file("demo_tm5_three_stage.json")?;

    // Create modern optimal config
    let mut modern_optimal = modern_config.clone();
    modern_optimal.metadata.name = "Modern Optimal Three-Stage Test".to_string();
    modern_optimal.metadata.description = Some("Modern optimized three-stage memory testing strategy".to_string());
    modern_optimal.system.memory_strategy = MemoryStrategyConfig {
        strategy_type: "modern_optimal".to_string(),
        testing_window_size_mb: None,
        reserved_memory_mb: None,
        test_block_size_mb: None,
        memory_reserve_percent: Some(15.0),
        memory_reserve_gib: None,
        memory_reserve_mib: None,
        blocks_per_thread: None,
        min_block_size_mb: None,
    };
    
    // Update test configs for modern optimal strategy
    for test in &mut modern_optimal.test_sequence {
        test.window_size_mb = None; // Let auto-calculation determine optimal sizes
        test.allow_misaligned = Some(false); // Modern systems prefer aligned access
    }
    
    modern_optimal.save_to_file("demo_modern_three_stage.json")?;

    println!("✅ Created demo_tm5_three_stage.json - TM5-compatible three-stage memory allocation");
    println!("   Features: Stage 1 (max allocation), Stage 2 (configurable windows), Stage 3 (per-test blocks)");
    println!("   Compatible with: Legacy TM5 configs with automatic conversion");
    println!();
    println!("✅ Created demo_modern_three_stage.json - Modern optimized three-stage allocation");
    println!("   Features: Auto-sizing windows and blocks based on system cache hierarchy");
    println!("   Compatible with: {} v{} with intelligent memory management", APP_NAME, APP_VERSION);
    println!();
    println!("Three-Stage Architecture Summary:");
    println!("  Stage 1: Memory Allocation - Allocate maximum available memory per thread");
    println!("  Stage 2: Testing Window - Focus testing on subset of allocation for temporal locality");
    println!("  Stage 3: Block/Chunk Size - Control access patterns and alignment within window");

    Ok(())
}
