use crate::{ErrorMode, MemoryReserve};
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
    pub tested_with_version: String, // TMR version this config was developed with
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemConfig {
    pub memory_reserve: MemoryReserveConfig,
    pub cpu_config: CpuConfig,
    pub error_mode: String, // "log", "halt", "panic"
    pub cycles: u32,
    pub large_pages: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryReserveConfig {
    #[serde(rename = "type")]
    pub reserve_type: String, // "percent", "gib", "mib"
    pub value: f64,
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
    pub time_percent: u32,
    pub block_size_mb: Option<u32>,
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

    pub fn save_to_file(&self, path: &str) -> Result<(), String> {
        let json = serde_json::to_string_pretty(self).map_err(|e| format!("Failed to serialize config: {}", e))?;

        fs::write(path, json).map_err(|e| format!("Failed to write config file: {}", e))
    }

    // Convert to runtime configuration
    pub fn to_memory_reserve(&self) -> MemoryReserve {
        match self.system.memory_reserve.reserve_type.as_str() {
            "percent" => MemoryReserve::PercentFree(self.system.memory_reserve.value),
            "gib" => MemoryReserve::GiB(self.system.memory_reserve.value),
            "mib" => MemoryReserve::MiB(self.system.memory_reserve.value),
            _ => MemoryReserve::PercentFree(10.0), // default
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
                name: "High Performance DDR5 Test".to_string(),
                author: "tmr_user".to_string(),
                version: "1.0".to_string(),
                description: Some("Comprehensive DDR5 memory testing with SIMD optimizations".to_string()),
                created: Some("2025-06-27".to_string()),
                tested_with_version: APP_VERSION.to_string(),
            },
            system: SystemConfig {
                memory_reserve: MemoryReserveConfig {
                    reserve_type: "percent".to_string(),
                    value: 15.0,
                },
                cpu_config: CpuConfig {
                    cpu_type: "cores".to_string(),
                    usage_percent: 75,
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
                    block_size_mb: None,
                    pattern_mode: None,
                    pattern_param0: None,
                    pattern_param1: None,
                    parameter: None,
                },
                TestConfig {
                    enabled: true,
                    function: "MirrorMove256NonTemporal".to_string(),
                    time_percent: 100,
                    block_size_mb: None,
                    pattern_mode: None,
                    pattern_param0: None,
                    pattern_param1: None,
                    parameter: None,
                },
                TestConfig {
                    enabled: true,
                    function: "MirrorMove512NonTemporal".to_string(),
                    time_percent: 100,
                    block_size_mb: None,
                    pattern_mode: None,
                    pattern_param0: None,
                    pattern_param1: None,
                    parameter: None,
                },
                TestConfig {
                    enabled: true,
                    function: "SimpleTest".to_string(),
                    time_percent: 200,
                    block_size_mb: Some(16),
                    pattern_mode: Some(1),
                    pattern_param0: Some(0x1E5F),
                    pattern_param1: Some(0x45357354),
                    parameter: Some(0),
                },
                TestConfig {
                    enabled: true,
                    function: "CacheBusting".to_string(),
                    time_percent: 150,
                    block_size_mb: None,
                    pattern_mode: None,
                    pattern_param0: None,
                    pattern_param1: None,
                    parameter: None,
                },
                TestConfig {
                    enabled: true,
                    function: "RandomTorture".to_string(),
                    time_percent: 300,
                    block_size_mb: None,
                    pattern_mode: None,
                    pattern_param0: None,
                    pattern_param1: None,
                    parameter: None,
                },
                TestConfig {
                    enabled: true,
                    function: "StrideAccess".to_string(),
                    time_percent: 100,
                    block_size_mb: None,
                    pattern_mode: None,
                    pattern_param0: None,
                    pattern_param1: None,
                    parameter: None,
                },
                TestConfig {
                    enabled: true,
                    function: "BandwidthSat".to_string(),
                    time_percent: 100,
                    block_size_mb: None,
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
            // Legacy configs typically have Test0-Test15
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
        let memory_reserve = if self.memory_setup.testing_window_size_mb > 0 {
            // Convert testing window size to percentage (rough approximation)
            let estimated_percent = (self.memory_setup.reserved_memory_mb as f64
                / (self.memory_setup.testing_window_size_mb + self.memory_setup.reserved_memory_mb) as f64)
                * 100.0;
            MemoryReserveConfig {
                reserve_type: "percent".to_string(),
                value: estimated_percent.max(5.0).min(50.0), // Clamp to reasonable range
            }
        } else {
            MemoryReserveConfig {
                reserve_type: "percent".to_string(),
                value: 10.0,
            }
        };

        let test_sequence = self
            .tests
            .iter()
            .filter(|t| t.enabled)
            .map(|test| TestConfig {
                enabled: true,
                function: Self::map_legacy_function(&test.function),
                time_percent: test.time_percent,
                block_size_mb: if test.test_block_size_mb > 0 {
                    Some(test.test_block_size_mb)
                } else {
                    None
                },
                pattern_mode: Some(test.pattern_mode),
                pattern_param0: Some(test.pattern_param0),
                pattern_param1: Some(test.pattern_param1),
                parameter: Some(test.parameter),
            })
            .collect();

        ModernConfig {
            config_format_version: CONFIG_VERSION.to_string(),
            application_name: format!("{} ({})", APP_NAME, APP_SHORT_NAME),
            metadata: ConfigMetadata {
                name: format!("{} (Legacy)", self.main_section.config_name),
                author: self.main_section.config_author.clone(),
                version: "1.0".to_string(),
                description: Some("Converted from legacy TestMem5 config".to_string()),
                created: None,
                tested_with_version: APP_VERSION.to_string(),
            },
            system: SystemConfig {
                memory_reserve,
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
            "MirrorMove" => "MirrorMove128NonTemporal".to_string(), // Map basic to 128-bit
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
    // Create modern demo config
    let modern_config = ModernConfig::create_demo_config();
    modern_config.save_to_file("demo_modern_v2.json")?;

    println!("✅ Created demo_modern_v2.json - Modern configuration format v2.0");
    println!("   Features: JSON format, version tracking, full parameter control, SIMD tests");
    println!("   Compatible with: {} v{}", APP_NAME, APP_VERSION);

    Ok(())
}
