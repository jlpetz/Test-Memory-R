//! Centralized parameter registry
//!
//! This module provides a single source of truth for all command-line parameters:
//! - Parameter names, types, and defaults
//! - Help text and examples
//! - Parsing and validation logic
//! - Config file override support
//!
//! Benefits of centralization:
//! - DRY: Define each parameter exactly once
//! - Consistency: Same parameter name/behavior everywhere
//! - Maintainability: Add new parameters in one place
//! - Validation: Centralized error handling

use std::collections::HashMap;

/// Parameter value types
#[derive(Debug, Clone)]
pub enum ParamValue {
    String(String),
    U32(u32),
    Usize(usize),
    Bool(bool),
}

/// Parameter definition with all metadata
pub struct ParamDef {
    /// Parameter key (e.g., "cycles", "memory", "skip-cores")
    pub key: &'static str,

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

impl Default for ParamRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ParamRegistry {
    /// Create the parameter registry with all defined parameters
    pub fn new() -> Self {
        let mut params = HashMap::new();
        let mut flags = HashMap::new();

        // Memory allocation parameters
        params.insert("memory", ParamDef {
            key: "memory",
            can_override_config: true,
            parser: |v| Ok(ParamValue::String(v.to_string())), // Return raw value, complex parsing happens later
        });

        params.insert("allocator", ParamDef {
            key: "allocator",
            can_override_config: true,
            parser: |v| Ok(ParamValue::String(v.to_string())), // Validated separately
        });

        // CPU configuration parameters
        params.insert("cpus", ParamDef {
            key: "cpus",
            can_override_config: true,
            parser: |v| Ok(ParamValue::String(v.to_string())),
        });

        params.insert("cputype", ParamDef {
            key: "cputype",
            can_override_config: true,
            parser: |v| {
                match v {
                    "threads" | "cores" => Ok(ParamValue::String(v.to_string())),
                    _ => Err(format!("Invalid cputype '{}'. Valid: threads, cores", v)),
                }
            },
        });

        // Stage 1 of CPU selection — FILTER which cores are eligible.
        // Accepts: `N` (skip first N), `N%` (skip first N% of cores), or `A-B`
        // (exclude the inclusive core-id range, e.g. skip-cores=0-7 tests only cores 8+).
        // Ranges are the diagnostic tool for multi-CCD/NUMA-domain machines.
        params.insert("skip-cores", ParamDef {
            key: "skip-cores",
            can_override_config: true,
            parser: |v| {
                // Keep as a string so the resolver can handle N / N% / A-B uniformly.
                // Validate the shape here so bad input fails at parse time.
                let s = v.trim();
                if let Some((a, b)) = s.split_once('-') {
                    let a = a.trim().parse::<usize>()
                        .map_err(|_| format!("Invalid skip-cores range start '{}' in '{}'", a, s))?;
                    let b = b.trim().parse::<usize>()
                        .map_err(|_| format!("Invalid skip-cores range end '{}' in '{}'", b, s))?;
                    if a > b {
                        return Err(format!("Invalid skip-cores range '{}': start {} > end {}", s, a, b));
                    }
                    Ok(ParamValue::String(s.to_string()))
                } else if let Some(pct) = s.strip_suffix('%') {
                    let p = pct.trim().parse::<u32>()
                        .map_err(|_| format!("Invalid skip-cores percentage '{}'", s))?;
                    if p > 100 {
                        return Err(format!("Invalid skip-cores '{}': percentage must be <= 100", s));
                    }
                    Ok(ParamValue::String(s.to_string()))
                } else {
                    s.parse::<usize>()
                        .map(|_| ParamValue::String(s.to_string()))
                        .map_err(|_| format!("Invalid skip-cores value '{}' (expected N, N%, or A-B)", s))
                }
            },
        });

        // Stage 2 of CPU selection — SPACING within the post-filter (available) pool.
        // `1` = densely packed (default, current behaviour). `N` = take every Nth core.
        // `even` = spread the requested count evenly across the whole available pool.
        // Errors (never silently adjusts) if count × stride overflows the pool — use the
        // `cpus=` percentage to right-size instead.
        params.insert("cpu-stride", ParamDef {
            key: "cpu-stride",
            can_override_config: true,
            parser: |v| {
                let s = v.trim();
                if s.eq_ignore_ascii_case("even") {
                    return Ok(ParamValue::String("even".to_string()));
                }
                match s.parse::<usize>() {
                    Ok(0) => Err("Invalid cpu-stride '0' (must be >= 1, or 'even')".to_string()),
                    Ok(_) => Ok(ParamValue::String(s.to_string())),
                    Err(_) => Err(format!("Invalid cpu-stride '{}' (expected a number >= 1, or 'even')", s)),
                }
            },
        });

        flags.insert("--disable-pinning", ParamDef {
            key: "--disable-pinning",
            can_override_config: true,
            parser: |_| Ok(ParamValue::Bool(true)),
        });

        params.insert("topology", ParamDef {
            key: "topology",
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
            can_override_config: true,
            parser: |v| {
                v.parse::<u32>()
                    .map(ParamValue::U32)
                    .map_err(|_| format!("Invalid cycles value '{}'", v))
            },
        });

        params.insert("duration", ParamDef {
            key: "duration",
            can_override_config: true,
            parser: |v| {
                v.parse::<u32>()
                    .map(ParamValue::U32)
                    .map_err(|_| format!("Invalid duration value '{}'", v))
            },
        });

        params.insert("errors", ParamDef {
            key: "errors",
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

        params.insert("channels", ParamDef {
            key: "channels",
            can_override_config: true,
            parser: |v| {
                let val = v.parse::<usize>()
                    .map_err(|_| format!("Invalid channels value '{}'", v))?;
                if val < 1 {
                    return Err("Channels must be at least 1".to_string());
                }
                Ok(ParamValue::Usize(val))
            },
        });

        params.insert("parameter", ParamDef {
            key: "parameter",
            can_override_config: true,
            parser: |v| {
                match v.to_lowercase().as_str() {
                    "none" | "off" | "-" => Ok(ParamValue::String("none".to_string())),
                    s if s.starts_with("subblocks:") || s.starts_with("sub:") => {
                        let num_str = s.split(':').nth(1).unwrap_or("0");
                        let n = num_str.parse::<u32>()
                            .map_err(|_| format!("Invalid subblock count '{}'. Use subblocks:2 or subblocks:4", num_str))?;
                        if !(2..=4).contains(&n) {
                            return Err(format!("Subblock count must be 2-4, got {}", n));
                        }
                        Ok(ParamValue::String(format!("subblocks:{}", n)))
                    }
                    s if s.starts_with("stride:") || s.starts_with("pagestride:") => {
                        let num_str = s.split(':').nth(1).unwrap_or("0");
                        let n = num_str.parse::<u32>()
                            .map_err(|_| format!("Invalid stride parameter '{}'. Use stride:510", num_str))?;
                        if n == 0 {
                            return Err("Stride parameter must be > 0".to_string());
                        }
                        Ok(ParamValue::String(format!("stride:{}", n)))
                    }
                    _ => Err(format!("Invalid parameter '{}'. Valid: none, subblocks:N (2-4), stride:N", v)),
                }
            },
        });

        // Pattern and repetition parameters
        params.insert("pattern-mode", ParamDef {
            key: "pattern-mode",
            can_override_config: true,
            parser: |v| {
                let val = v.parse::<u32>()
                    .map_err(|_| format!("Invalid pattern-mode '{}'. Must be 0-2 (TM5) or 10-12 (TMR-native)", v))?;
                match val {
                    0 | 1 | 2 | 10 | 11 | 12 => Ok(ParamValue::U32(val)),
                    _ => Err(format!("Invalid pattern-mode {}. Valid: 0-2 (TM5-faithful), 10-12 (TMR-native)", val)),
                }
            },
        });

        params.insert("verify-reps", ParamDef {
            key: "verify-reps",
            can_override_config: true,
            parser: |v| {
                let val = v.parse::<u32>()
                    .map_err(|_| format!("Invalid verify-reps '{}'. Must be 1-100", v))?;
                if val == 0 || val > 100 {
                    return Err(format!("verify-reps must be 1-100, got {}", val));
                }
                Ok(ParamValue::U32(val))
            },
        });

        params.insert("test-reps", ParamDef {
            key: "test-reps",
            can_override_config: true,
            parser: |v| {
                let val = v.parse::<u32>()
                    .map_err(|_| format!("Invalid test-reps '{}'. Must be 1-100", v))?;
                if val == 0 || val > 100 {
                    return Err(format!("test-reps must be 1-100, got {}", val));
                }
                Ok(ParamValue::U32(val))
            },
        });

        params.insert("write-read-cycles", ParamDef {
            key: "write-read-cycles",
            can_override_config: true,
            parser: |v| {
                let val = v.parse::<u32>()
                    .map_err(|_| format!("Invalid write-read-cycles '{}'. Must be 1-100", v))?;
                if val == 0 || val > 100 {
                    return Err(format!("write-read-cycles must be 1-100, got {}", val));
                }
                Ok(ParamValue::U32(val))
            },
        });

        // Page size parameters
        params.insert("minpage", ParamDef {
            key: "minpage",
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
            can_override_config: false,
            parser: |v| Ok(ParamValue::String(v.to_string())),
        });

        // Test filter parameter (supports glob patterns and comma-separated lists)
        params.insert("test", ParamDef {
            key: "test",
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
            || arg.starts_with("--ram-latency")
            || arg.starts_with("--cache-latency")
            || arg.starts_with("--no-calibration")
            || arg.starts_with("--startup-debug") {
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
    println!("  {} --ram-latency                    # Run DRAM latency tests (6 tests)", program_name);
    println!("  {} --cache-latency                  # Run full cache hierarchy latency tests (12 tests)", program_name);
    println!("  {} test=Spd-*                       # Run tests matching pattern (glob wildcards)", program_name);
    println!("  {} test=Mem-Mirror*,Lat-L3-*        # Run multiple patterns (comma-separated)", program_name);
    println!("  {} --version                         # Show version information", program_name);
    println!("  {} --show-topology                   # Show CPU Topology Mapping for debugging", program_name);
    println!("  {} --debug-topology                  # Runs multiple CPU Topology checks to debug if one works better", program_name);
    println!();
    println!("COMMAND LINE PARAMETERS:");
    println!("  memory=20%                           # Reserve 20% of system memory");
    println!("  memory=2GiB                         # Reserve 2 GiB");
    println!("  memory=2048MB                       # TM5-compatible allocation (2048MB from available)");
    println!("  memory=10%-from-available           # Reserve 10% from currently available memory");
    println!("  memory=8GiB-from-total              # Reserve 8 GiB from total system memory");
    println!("  memory=64GiB-target                 # Target 64 GiB allocation (may fail if unavailable)");
    println!("  cycles=5                            # Run 5 complete test cycles");
    println!("  duration=600                        # Maximum 10 minutes runtime");
    println!("  cpus=50%                            # Use 50% of available CPUs");
    println!("  cputype=cores                       # Use physical cores (vs threads/SMT)");
    println!("  skip-cores=1                        # Stage 1 FILTER: skip first N CPUs (default: 1 to preserve OS)");
    println!("  skip-cores=25%                      #   ...or skip the leading N% (portable across core counts)");
    println!("  skip-cores=0-7                      #   ...or exclude a core-id range (isolate a memory domain/CCD)");
    println!("  cpu-stride=2                        # Stage 2 SPACING: use every Nth available CPU");
    println!("  cpu-stride=even                     #   ...or spread the requested count evenly across the pool");
    println!("  --disable-pinning                   # Disable CPU thread pinning");
    println!("  allocator=plan-pagesize-pref        # Allocation strategy:");
    println!("    greedy                            #   Legacy: largest chunks first");
    println!("    plan-pagesize-pref                #   Plan-based: page type priority (default)");
    println!("    plan-blocksize-pref               #   Plan-based: block size priority");
    println!("  errors=halt                         # Error handling (log/halt/panic)");
    println!("  parameter=none                      # Clear test parameter (no subblocks/stride)");
    println!("  parameter=subblocks:4               # Override: 4 subblocks for MirrorMove");
    println!("  parameter=stride:510                # Override: page stride for MirrorMove");
    println!("  pattern-mode=0                      # Pattern mode: 0-2 (TM5-faithful), 10-12 (TMR-native)");
    println!("  verify-reps=5                       # Verify passes per write (TM5 retention stress: 5)");
    println!("  test-reps=3                         # Test op repetitions per write (MirrorMove round-trips)");
    println!("  write-read-cycles=4                 # Write+verify cycles per chunk (TM5 SimpleTest: 4)");
    println!("  topology=windowsv2                  # CPU detection method (auto/windows/windowsv2/cpuid)");
    println!("  --no-calibration                    # Skip loading calibration data (use CPUID heuristics)");
    println!();
    println!("CONFIG FILE OVERRIDES:");
    println!("  All parameters can override config file values:");
    println!("    {} config=test.json cycles=1 skip-cores=0 memory=4GiB", program_name);
    println!();
    println!("LOGGING:");
    println!("  RUST_LOG=info                       # Set log level (error/warn/info/debug/trace)");
    println!("                                        # Use debug for detailed per-thread logs");
    println!("  Logs saved to: .\\logs\\TMR_YYYY-MM-DD_HH-MM-SS.log      (local time)");
    println!("  Results saved to: .\\results\\TMR_YYYY-MM-DD_HH-MM-SS.json (same name as the log)");
    println!();
    println!("RESULT COMPARISON:");
    println!("  Test results are automatically saved as JSON files to .\\results\\");
    println!("  Use --compare-results to analyze performance differences");
    println!("  Useful for memory overclocking and timing optimization");
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
    println!("  {} cycles=5 duration=600        # 5 cycles OR 10 minutes max", program_name);
    println!("  {} cpus=50% cputype=cores       # Use 50% of CPU cores (cores=avoid SMT)", program_name);
    println!("  {} skip-cores=0                 # Don't skip any CPUs (default: skip first CPU)", program_name);
    println!("  {} skip-cores=0-7 cpus=100%     # Test ONLY cores 8+ (isolate a memory domain on multi-CCD parts)", program_name);
    println!("  {} cpus=50% cpu-stride=even     # Spread half the cores evenly (loads all memory domains equally)", program_name);
    println!("  {} --disable-pinning            # Disable CPU pinning (default: enabled)", program_name);
    println!("  {} errors=halt                  # Stop on first error", program_name);
    println!("  {} config=test.json             # Load comprehensive JSON config", program_name);
    println!("  {} config=legacy.cfg            # Auto-convert TM5 config + add stuck bit test", program_name);
    println!("  {} --create-demo-configs        # Create demo configurations", program_name);
    println!("  {} --compare-results old.json new.json # Compare results from .\\results\\", program_name);
    println!();
    println!("For full help: {} --help", program_name);
}
