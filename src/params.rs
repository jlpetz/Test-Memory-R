/// Centralized parameter registry
///
/// This module provides a single source of truth for all command-line parameters:
/// - Parameter names, types, and defaults
/// - Help text and examples
/// - Parsing and validation logic
/// - Config file override support
///
/// Benefits of centralization:
/// - DRY: Define each parameter exactly once
/// - Consistency: Same parameter name/behavior everywhere
/// - Maintainability: Add new parameters in one place
/// - Validation: Centralized error handling

use std::collections::HashMap;

/// Parameter value types
#[derive(Debug, Clone)]
pub enum ParamValue {
    String(String),
    U32(u32),
    Usize(usize),
    Bool(bool),
    None,
}

/// Parameter definition with all metadata
pub struct ParamDef {
    /// Parameter key (e.g., "cycles", "memory", "skip-cores")
    pub key: &'static str,

    /// Whether this is a flag (--flag) or key=value parameter
    pub is_flag: bool,

    /// Default value
    pub default: ParamValue,

    /// Short description for help text
    pub help: &'static str,

    /// Example usage
    pub example: &'static str,

    /// Can this parameter override config file values?
    pub can_override_config: bool,

    /// Parser function: takes string value, returns parsed value or error
    pub parser: fn(&str) -> Result<ParamValue, String>,
}

/// Global parameter registry
pub struct ParamRegistry {
    params: HashMap<&'static str, ParamDef>,
    flags: HashMap<&'static str, ParamDef>,
}

impl ParamRegistry {
    /// Create the parameter registry with all defined parameters
    pub fn new() -> Self {
        let mut params = HashMap::new();
        let mut flags = HashMap::new();

        // Memory allocation parameters
        params.insert("memory", ParamDef {
            key: "memory",
            is_flag: false,
            default: ParamValue::String("10%-from-available:start=split:auto".to_string()),
            help: "Amount of memory to allocate for testing",
            example: "memory=20% or memory=4GiB or memory=2048MB",
            can_override_config: true,
            parser: |v| Ok(ParamValue::String(v.to_string())), // Return raw value, complex parsing happens later
        });

        params.insert("allocator", ParamDef {
            key: "allocator",
            is_flag: false,
            default: ParamValue::String("plan-pagesize-pref".to_string()),
            help: "Allocation strategy (greedy, plan-pagesize-pref, plan-blocksize-pref)",
            example: "allocator=plan-pagesize-pref",
            can_override_config: true,
            parser: |v| Ok(ParamValue::String(v.to_string())), // Validated separately
        });

        // CPU configuration parameters
        params.insert("cpus", ParamDef {
            key: "cpus",
            is_flag: false,
            default: ParamValue::String("100%".to_string()),
            help: "Percentage of CPUs to use for testing",
            example: "cpus=50%",
            can_override_config: true,
            parser: |v| Ok(ParamValue::String(v.to_string())),
        });

        params.insert("cputype", ParamDef {
            key: "cputype",
            is_flag: false,
            default: ParamValue::String("threads".to_string()),
            help: "CPU type: 'threads' (use SMT) or 'cores' (physical cores only)",
            example: "cputype=cores",
            can_override_config: true,
            parser: |v| {
                match v {
                    "threads" | "cores" => Ok(ParamValue::String(v.to_string())),
                    _ => Err(format!("Invalid cputype '{}'. Valid: threads, cores", v)),
                }
            },
        });

        params.insert("skip-cores", ParamDef {
            key: "skip-cores",
            is_flag: false,
            default: ParamValue::Usize(1),
            help: "Number of CPUs to skip from the beginning (default: 1 to preserve OS responsiveness)",
            example: "skip-cores=0",
            can_override_config: true,
            parser: |v| {
                v.parse::<usize>()
                    .map(ParamValue::Usize)
                    .map_err(|_| format!("Invalid skip-cores value '{}'", v))
            },
        });

        flags.insert("--disable-pinning", ParamDef {
            key: "--disable-pinning",
            is_flag: true,
            default: ParamValue::Bool(false),
            help: "Disable CPU thread pinning (default: enabled)",
            example: "--disable-pinning",
            can_override_config: true,
            parser: |_| Ok(ParamValue::Bool(true)),
        });

        params.insert("topology", ParamDef {
            key: "topology",
            is_flag: false,
            default: ParamValue::String("auto".to_string()),
            help: "CPU topology detection method (auto, windows, windowsv2, cpuid)",
            example: "topology=windowsv2",
            can_override_config: true,
            parser: |v| {
                match v.to_lowercase().as_str() {
                    "windows" | "windowsapi" | "windowsv2" | "v2" | "cpuid" | "auto" => {
                        Ok(ParamValue::String(v.to_string()))
                    }
                    _ => Err(format!("Invalid topology method '{}'. Valid: auto, windows, windowsv2, cpuid", v)),
                }
            },
        });

        // Test execution parameters
        params.insert("cycles", ParamDef {
            key: "cycles",
            is_flag: false,
            default: ParamValue::U32(3),
            help: "Number of test cycles to run",
            example: "cycles=5",
            can_override_config: true,
            parser: |v| {
                v.parse::<u32>()
                    .map(ParamValue::U32)
                    .map_err(|_| format!("Invalid cycles value '{}'", v))
            },
        });

        params.insert("duration", ParamDef {
            key: "duration",
            is_flag: false,
            default: ParamValue::None,
            help: "Maximum test duration in seconds",
            example: "duration=600",
            can_override_config: true,
            parser: |v| {
                v.parse::<u32>()
                    .map(ParamValue::U32)
                    .map_err(|_| format!("Invalid duration value '{}'", v))
            },
        });

        params.insert("errors", ParamDef {
            key: "errors",
            is_flag: false,
            default: ParamValue::String("log".to_string()),
            help: "Error handling mode (log, halt, panic)",
            example: "errors=halt",
            can_override_config: true,
            parser: |v| {
                match v.to_lowercase().as_str() {
                    "log" | "halt" | "stop" | "panic" | "debug" => {
                        Ok(ParamValue::String(v.to_string()))
                    }
                    _ => Err(format!("Invalid error mode '{}'. Valid: log, halt, panic", v)),
                }
            },
        });

        params.insert("streams", ParamDef {
            key: "streams",
            is_flag: false,
            default: ParamValue::Usize(1),
            help: "Number of concurrent memory streams (must be power of 2: 1, 2, 4, 8, etc.)",
            example: "streams=4",
            can_override_config: true,
            parser: |v| {
                let val = v.parse::<usize>()
                    .map_err(|_| format!("Invalid streams value '{}'", v))?;

                // Validate power of 2
                if val == 0 || (val & (val - 1)) != 0 {
                    return Err(format!("Streams must be a power of 2 (1, 2, 4, 8, etc.), got {}", val));
                }

                Ok(ParamValue::Usize(val))
            },
        });

        // Driver-related parameters
        flags.insert("--driver-chunking", ParamDef {
            key: "--driver-chunking",
            is_flag: true,
            default: ParamValue::Bool(false),
            help: "Enable driver-side memory chunking",
            example: "--driver-chunking",
            can_override_config: true,
            parser: |_| Ok(ParamValue::Bool(true)),
        });

        flags.insert("--batch-remap", ParamDef {
            key: "--batch-remap",
            is_flag: true,
            default: ParamValue::Bool(false),
            help: "Enable batch remapping mode",
            example: "--batch-remap",
            can_override_config: true,
            parser: |_| Ok(ParamValue::Bool(true)),
        });

        // Page size parameters
        params.insert("minpage", ParamDef {
            key: "minpage",
            is_flag: false,
            default: ParamValue::String("large".to_string()),
            help: "Minimum page size (regular/4kb, large/2mb, huge/1gb)",
            example: "minpage=regular",
            can_override_config: true,
            parser: |v| {
                match v.to_lowercase().as_str() {
                    "regular" | "4kb" | "large" | "2mb" | "huge" | "1gb" => {
                        Ok(ParamValue::String(v.to_lowercase()))
                    }
                    _ => Err(format!("Invalid page size '{}'. Valid: regular/4kb, large/2mb, huge/1gb", v)),
                }
            },
        });

        params.insert("maxpage", ParamDef {
            key: "maxpage",
            is_flag: false,
            default: ParamValue::String("huge".to_string()),
            help: "Maximum page size (regular/4kb, large/2mb, huge/1gb)",
            example: "maxpage=regular",
            can_override_config: true,
            parser: |v| {
                match v.to_lowercase().as_str() {
                    "regular" | "4kb" | "large" | "2mb" | "huge" | "1gb" => {
                        Ok(ParamValue::String(v.to_lowercase()))
                    }
                    _ => Err(format!("Invalid page size '{}'. Valid: regular/4kb, large/2mb, huge/1gb", v)),
                }
            },
        });

        // Config file parameter
        params.insert("config", ParamDef {
            key: "config",
            is_flag: false,
            default: ParamValue::None,
            help: "Load configuration from JSON file",
            example: "config=test.json",
            can_override_config: false,
            parser: |v| Ok(ParamValue::String(v.to_string())),
        });

        Self { params, flags }
    }

    /// Parse a command-line argument
    pub fn parse_arg(&self, arg: &str) -> Result<(String, ParamValue), String> {
        // Check if it's a flag
        if arg.starts_with("--") {
            if let Some(def) = self.flags.get(arg) {
                return (def.parser)(arg).map(|v| (def.key.to_string(), v));
            }
            return Err(format!("Unknown flag: '{}'", arg));
        }

        // Check if it's a key=value parameter
        if let Some(equals_pos) = arg.find('=') {
            let key = &arg[..equals_pos];
            let value = &arg[equals_pos + 1..];

            if let Some(def) = self.params.get(key) {
                return (def.parser)(value).map(|v| (def.key.to_string(), v));
            }
            return Err(format!("Unknown parameter: '{}'", key));
        }

        Err(format!("Invalid argument format: '{}' (expected 'key=value' or '--flag')", arg))
    }

    /// Check if a parameter can override config file values
    pub fn can_override_config(&self, key: &str) -> bool {
        self.params.get(key)
            .or_else(|| self.flags.get(key))
            .map(|def| def.can_override_config)
            .unwrap_or(false)
    }

    /// Get all parameters that can override config files
    pub fn get_config_overridable_params(&self) -> Vec<&str> {
        self.params.values()
            .filter(|def| def.can_override_config)
            .map(|def| def.key)
            .collect()
    }

    /// Generate help text for all parameters
    pub fn generate_help(&self) -> String {
        let mut help = String::new();
        help.push_str("COMMAND LINE PARAMETERS:\n");

        // Sort parameters by key for consistent output
        let mut param_keys: Vec<_> = self.params.keys().collect();
        param_keys.sort();

        for key in param_keys {
            let def = &self.params[key];
            help.push_str(&format!("  {:30} # {}\n", def.example, def.help));
        }

        help.push_str("\nFLAGS:\n");
        let mut flag_keys: Vec<_> = self.flags.keys().collect();
        flag_keys.sort();

        for key in flag_keys {
            let def = &self.flags[key];
            help.push_str(&format!("  {:30} # {}\n", def.example, def.help));
        }

        help
    }

    /// Get default value for a parameter
    pub fn get_default(&self, key: &str) -> Option<&ParamValue> {
        self.params.get(key)
            .or_else(|| self.flags.get(key))
            .map(|def| &def.default)
    }

    /// List all recognized parameter keys (for validation)
    pub fn all_keys(&self) -> Vec<&str> {
        self.params.keys()
            .chain(self.flags.keys())
            .copied()
            .collect()
    }
}

/// Global registry instance
pub fn get_registry() -> ParamRegistry {
    ParamRegistry::new()
}

/// Parse and validate all command-line arguments in one pass
/// Returns a HashMap of validated parameter values or error on first invalid param
pub fn parse_and_validate_args(args: &[String]) -> Result<HashMap<String, ParamValue>, String> {
    let registry = get_registry();
    let mut validated = HashMap::new();

    for arg in args {
        // Skip special commands (handled separately in main)
        if arg.starts_with("--help")
            || arg.starts_with("--version")
            || arg.starts_with("--create-demo-configs")
            || arg.starts_with("--compare-results")
            || arg.starts_with("--debug-topology")
            || arg.starts_with("--show-topology")
            || arg.starts_with("--setup-large-pages")
            || arg.starts_with("--quick-test")
            || arg.starts_with("--latency-test")
            || arg.starts_with("--single-test=") {
            continue;
        }

        match registry.parse_arg(arg) {
            Ok((key, value)) => {
                validated.insert(key, value);
            }
            Err(e) => {
                return Err(format!("❌ {}\n💡 Run with --help to see all valid parameters", e));
            }
        }
    }

    Ok(validated)
}

/// Print complete help text
pub fn print_help(program_name: &str) {
    println!("🚀 Test Memory R (TMR) v1.0.0 - High-Performance Memory Testing Tool");
    println!("===================================================================================");
    println!();
    println!("USAGE:");
    println!("  {}                                    # Run with defaults", program_name);
    println!("  {} config=test.json                  # Load modern JSON config (v2.0)", program_name);
    println!("  {} config=legacy.cfg                 # Load legacy TestMem5 config (v1.0)", program_name);
    println!("  {} --create-demo-configs              # Create demo configuration files", program_name);
    println!("  {} --compare-results baseline.json current.json [output.json]", program_name);
    println!("                                          # Compare two test results from .\\results\\");
    println!("  {} --setup-large-pages              # Configure large pages for optimal performance", program_name);
    println!("  {} --quick-test                     # Run 30-second validation test", program_name);
    println!("  {} --latency-test                   # Run latency measurement tests (Read, Write, Copy)", program_name);
    println!("  {} --single-test=SimpleTest         # Run only SimpleTest for 30 seconds", program_name);
    println!("  {} --version                         # Show version information", program_name);
    println!("  {} --show-topology                   # Show CPU Topology Mapping for debugging", program_name);
    println!("  {} --debug-topology                  # Runs multiple CPU Topology checks to debug if one works better", program_name);
    println!();
    println!("COMMAND LINE PARAMETERS:");
    println!("  memory=20%                           # Reserve 20% of system memory");
    println!("  memory=2GiB                         # Reserve 2 GiB");
    println!("  memory=2048MB                       # TM5-compatible allocation (2048MB from available)");
    println!("  cycles=5                            # Run 5 complete test cycles");
    println!("  duration=600                        # Maximum 10 minutes runtime");
    println!("  cpus=50%                            # Use 50% of available CPUs");
    println!("  cputype=cores                       # Use physical cores (vs threads/SMT)");
    println!("  skip-cores=1                        # Skip first N CPUs (default: 1 to preserve OS)");
    println!("  --disable-pinning                   # Disable CPU thread pinning");
    println!("  allocator=plan-pagesize-pref        # Allocation strategy:");
    println!("    greedy                            #   Legacy: largest chunks first");
    println!("    plan-pagesize-pref                #   Plan-based: page type priority (default)");
    println!("    plan-blocksize-pref               #   Plan-based: block size priority");
    println!("  errors=halt                         # Error handling (log/halt/panic)");
    println!("  topology=windowsv2                  # CPU detection method (auto/windows/windowsv2/cpuid)");
    println!("  --driver-chunking                   # Enable driver-side memory chunking");
    println!("  --batch-remap                       # Enable batch remapping mode");
    println!();
    println!("CONFIG FILE OVERRIDES:");
    println!("  All parameters can override config file values:");
    println!("    {} config=test.json cycles=1 skip-cores=0 memory=4GiB", program_name);
    println!();
    println!("LOGGING:");
    println!("  RUST_LOG=info                       # Set log level (error/warn/info/debug/trace)");
    println!("                                        # Use debug for detailed per-thread logs");
    println!("  Logs saved to: .\\logs\\TMR_YYYY-MM-DD_HH-MM-SS.log");
    println!("  Results saved to: .\\results\\TMR_YYYY-MM-DD_HH-MM-SS.json");
    println!();
    println!("RESULT COMPARISON:");
    println!("  Test results are automatically saved as JSON files to .\\results\\");
    println!("  Use --compare-results to analyze performance differences");
    println!("  Useful for memory overclocking and timing optimization");
    println!();
    println!("ADVANCED FEATURES:");
    println!("  DMA Memory: Install kernel driver for physical memory testing");
    println!("              - Provides true physical address access");
    println!("              - Guaranteed physically contiguous memory");
    println!("              - Better detection of memory controller issues");
    println!("  Installation: Run Install-TmrDriver.ps1 as Administrator");
    println!();
}

/// Extract a string value from validated params, or use default
pub fn get_string(validated: &HashMap<String, ParamValue>, key: &str, default: &str) -> String {
    match validated.get(key) {
        Some(ParamValue::String(s)) => s.clone(),
        _ => default.to_string(),
    }
}

/// Extract a u32 value from validated params, or use default
pub fn get_u32(validated: &HashMap<String, ParamValue>, key: &str, default: u32) -> u32 {
    match validated.get(key) {
        Some(ParamValue::U32(n)) => *n,
        _ => default,
    }
}

/// Extract a usize value from validated params, or use default
pub fn get_usize(validated: &HashMap<String, ParamValue>, key: &str, default: usize) -> usize {
    match validated.get(key) {
        Some(ParamValue::Usize(n)) => *n,
        _ => default,
    }
}

/// Extract a bool value from validated params, or use default
pub fn get_bool(validated: &HashMap<String, ParamValue>, key: &str, default: bool) -> bool {
    match validated.get(key) {
        Some(ParamValue::Bool(b)) => *b,
        _ => default,
    }
}

/// Check if a parameter was explicitly provided
pub fn has_param(validated: &HashMap<String, ParamValue>, key: &str) -> bool {
    validated.contains_key(key)
}

/// Print quick usage examples
pub fn print_usage(program_name: &str) {
    println!("Quick Usage Examples:");
    println!("  {} memory=10%-from-available     # Standard: Reserve 10% from currently available memory", program_name);
    println!("  {} memory=2048MB                 # TM5 compatible: Reserve 2048MB from available memory", program_name);
    println!("  {} memory=8GiB-from-total        # Failure test: Reserve 8 GiB from total (unrealistic)", program_name);
    println!("  {} memory=64GiB-target           # Failure test: Target 64 GiB allocation (may fail)", program_name);
    println!("  {} memory=10%-from-available:start=+2GiB     # Reserve 10%, start 2GiB above used memory", program_name);
    println!("  {} memory=10%-from-available:start=split:5%:95% # Reserve 10%, split: 5% pre-buffer, 95% post", program_name);
    println!("  {} memory=20%-from-available:start=split:auto # Reserve 20%, auto-split for post-boot testing", program_name);
    println!("  {} cycles=5 duration=600        # 5 cycles OR 10 minutes max", program_name);
    println!("  {} cpus=50% cputype=cores       # Use 50% of CPU cores (cores=avoid SMT)", program_name);
    println!("  {} skip-cores=0                 # Don't skip any CPUs (default: skip first CPU)", program_name);
    println!("  {} --disable-pinning            # Disable CPU pinning (default: enabled)", program_name);
    println!("  {} errors=halt                  # Stop on first error", program_name);
    println!("  {} config=test.json             # Load comprehensive JSON config", program_name);
    println!("  {} config=legacy.cfg            # Auto-convert TM5 config + add stuck bit test", program_name);
    println!("  {} --create-demo-configs        # Create demo configurations", program_name);
    println!("  {} --compare-results old.json new.json # Compare results from .\\results\\", program_name);
    println!();
    println!("For full help: {} --help", program_name);
}
