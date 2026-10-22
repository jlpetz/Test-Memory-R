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
            parser: |v| match crate::cpu_topology::TopologyDetectionMethod::parse(v) {
                Some(_) => Ok(ParamValue::String(v.to_string())),
                None => Err(format!("Invalid topology method '{}'. Valid: auto, windows, windowsv2, cpuid, amd", v)),
            },
        });

        // The native patterns' run settings (pattern spec v2, `crate::pattern`): read early in
        // `session::run`, before a config loads; registered so they validate
        params.insert("seed", ParamDef {
            key: "seed",
            can_override_config: true,
            parser: |v| crate::pattern::parse_seed(v).map(|_| ParamValue::String(v.to_string())),
        });
        params.insert("rescan", ParamDef {
            key: "rescan",
            can_override_config: true,
            parser: |v| match v.to_ascii_lowercase().as_str() {
                "on" | "off" => Ok(ParamValue::String(v.to_ascii_lowercase())),
                _ => Err(format!("rescan '{v}': use on or off")),
            },
        });
        params.insert("beat", ParamDef {
            key: "beat",
            can_override_config: true,
            parser: |v| crate::pattern::parse_beat(v).map(|_| ParamValue::String(v.to_string())),
        });
        params.insert("patterns", ParamDef {
            key: "patterns",
            can_override_config: true,
            parser: |v| match v.to_ascii_lowercase().as_str() {
                "native" | "legacy" => Ok(ParamValue::String(v.to_ascii_lowercase())),
                _ => Err(format!("patterns '{v}': use native or legacy")),
            },
        });

        // Read early in `session::run`, before any topology is detected; registered so it validates
        params.insert("core-priority", ParamDef {
            key: "core-priority",
            can_override_config: true,
            parser: |v| crate::cpu_topology::CorePriority::parse(v).map(|_| ParamValue::String(v.to_string())),
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

        // The seal around every chunk of the correctness tests (TODO 74, seal.rs)
        params.insert("seal", ParamDef {
            key: "seal",
            can_override_config: true,
            parser: |v| {
                crate::seal::SealSettings::parse(v, "auto").map(|_| ParamValue::String(v.trim().to_lowercase()))
            },
        });
        params.insert("seal-width", ParamDef {
            key: "seal-width",
            can_override_config: true,
            parser: |v| {
                crate::seal::SealWidth::parse(v).map(|w| ParamValue::String(w.name().to_string()))
            },
        });
        // The tests' SIMD width (TODO 99, `crate::simd::Width`): read early in `session::run`,
        // before a config loads; registered so it validates
        params.insert("width", ParamDef {
            key: "width",
            can_override_config: true,
            parser: |v| {
                crate::simd::Width::parse(v).map(|w| ParamValue::String(w.map_or("auto", crate::simd::Width::name).to_string()))
            },
        });

        params.insert("mirror", ParamDef {
            key: "mirror",
            can_override_config: true,
            parser: |v| {
                v.to_lowercase().parse::<crate::config::MirrorMode>()
                    .map(|m| ParamValue::String(m.to_string()))
            },
        });

        // Pattern and repetition parameters
        params.insert("pattern-mode", ParamDef {
            key: "pattern-mode",
            can_override_config: true,
            parser: |v| {
                let val = v.parse::<u32>()
                    .map_err(|_| format!("Invalid pattern-mode '{}'. Valid: {}", v, crate::pattern::pattern_modes()))?;
                if crate::pattern::is_pattern_mode(val) {
                    Ok(ParamValue::U32(val))
                } else {
                    Err(format!("Invalid pattern-mode {}{}. Valid: {}", val, crate::pattern::renumbered(val), crate::pattern::pattern_modes()))
                }
            },
        });

        params.insert("sequence", ParamDef {
            key: "sequence",
            can_override_config: true,
            parser: |v| crate::sequence::Sequence::parse(v, crate::sequence::Order::Forward)
                .map(|_| ParamValue::String(v.to_string())),
        });

        params.insert("order", ParamDef {
            key: "order",
            can_override_config: true,
            parser: |v| crate::sequence::Order::parse(v).map(|_| ParamValue::String(v.to_string()))
                .ok_or_else(|| format!("Invalid order '{v}'. Valid: forward, reverse, random, stride")),
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

        // Block sizes and share rounding. Each parses here; how they fit together is checked by
        // MemoryAllocationConfig::share_rounding.
        for key in ["hugechunk", "largechunk", "largefloor", "blkroundtarget"] {
            params.insert(key, ParamDef {
                key,
                can_override_config: true,
                parser: |v| crate::tests::parse_size_string(v).map(|_| ParamValue::String(v.to_string())),
            });
        }

        params.insert("blkround", ParamDef {
            key: "blkround",
            can_override_config: true,
            parser: |v| {
                v.parse::<crate::memory::allocation_strategy::RoundDirection>()?;
                Ok(ParamValue::String(v.to_lowercase()))
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

        params.insert("priority", ParamDef {
            key: "priority",
            can_override_config: true,
            parser: |v| v.parse::<crate::runner::ThreadPriority>().map(|p| ParamValue::String(p.to_string())),
        });

        params.insert("plan", ParamDef {
            key: "plan",
            can_override_config: false,
            parser: |v| crate::plans::find(v).map(|p| ParamValue::String(p.name.to_string())),
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

#[cfg(test)]
mod tests {
    /// `pattern-mode=` takes the numbered modes and the native families' numbers; 12 and 14, the
    /// old numbers of 20 and 30, are refused, saying so (TODO 97).
    #[test]
    fn pattern_mode_takes_the_numbers_there_are() {
        let parse = |v: &str| super::parse_and_validate_args(&[format!("pattern-mode={v}")]);
        for ok in ["0", "2", "13", "20", "30", "15", "31", "33"] {
            assert!(parse(ok).is_ok(), "pattern-mode={ok}");
        }
        for bad in ["12", "14", "3", "99"] {
            assert!(parse(bad).is_err(), "pattern-mode={bad}");
        }
        assert!(parse("14").unwrap_err().contains("is 30 since"));
    }
}
