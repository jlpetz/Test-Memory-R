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

        // Memory allocation parameters. Kept as given: the session parses it again, with this `AllocationMode::parse`, into its strategy.
        params.insert("memory", ParamDef {
            key: "memory",
            can_override_config: true,
            parser: |v| match crate::memory::allocation_strategy::AllocationMode::parse(v) {
                Ok(_) => Ok(ParamValue::String(v.to_string())),
                Err(e) => Err(format!(
                    "Invalid memory '{v}': {e}. Valid: a share (20%) or a size (2GiB, 2048MB), alone or followed by -from-available, \
                     -from-total or -target"
                )),
            },
        });

        // CPU configuration parameters. `cpus=` is a share of the available CPUs or a count of them, as `session::run` reads it (clamped to
        // 1..=available); it would read anything else as every CPU.
        params.insert("cpus", ParamDef {
            key: "cpus",
            can_override_config: true,
            parser: |v| {
                let s = v.trim();
                let read = match s.strip_suffix('%') {
                    Some(share) => share.parse::<u32>().is_ok(),
                    None => s.parse::<usize>().is_ok(),
                };
                if read {
                    Ok(ParamValue::String(s.to_string()))
                } else {
                    Err(format!("Invalid cpus '{v}'. Valid: N% (a share of the available CPUs, e.g. 50%) or N (a count, e.g. 4)"))
                }
            },
        });

        params.insert("cputype", ParamDef {
            key: "cputype",
            can_override_config: true,
            parser: |v| match v {
                "threads" | "cores" => Ok(ParamValue::String(v.to_string())),
                _ => Err(format!("Invalid cputype '{}'. Valid: threads, cores", v)),
            },
        });

        // `skip-cores=`: `N` (the last N cores in the core order, the weakest), `N%` (a share of the cores, from the end too), or `A-B`
        // (the cores numbered A to B, e.g. skip-cores=0-7 tests only cores 8+, to isolate a memory domain). Kept as a string;
        // `cpu_selection::SkipSpec` reads it, here to check it and in the session to use it (TODO 108).
        params.insert("skip-cores", ParamDef {
            key: "skip-cores",
            can_override_config: true,
            parser: |v| crate::cpu_selection::SkipSpec::parse(v).map(|_| ParamValue::String(v.trim().to_string())),
        });

        // `cpu-stride=`, when given: the cores by number instead of their ranked order, the picks spaced by it. `1` = packed, `N` = every
        // Nth core, `even` = the requested count spread over all of them. Errors (never silently adjusts) if count × stride overflows
        // them: use the `cpus=` percentage to right-size instead.
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

        // The native patterns' run settings (pattern spec v2, `crate::pattern`): read early in `session::run`, before a config loads;
        // registered so they validate
        params.insert("seed", ParamDef {
            key: "seed",
            can_override_config: true,
            parser: |v| crate::pattern::parse_seed(v).map(|_| ParamValue::String(v.to_string())),
        });
        // The cache calibration (TODO 22): auto measures one before the tests when none of this machine's, with the shared anchors, is
        // stored; recheck also probes a stored one first (TODO 116); always measures each run; off sizes from CPUID and leaves the seal's
        // width to its rule
        params.insert("calibrate", ParamDef {
            key: "calibrate",
            can_override_config: true,
            parser: |v| match v.to_ascii_lowercase().as_str() {
                "auto" | "recheck" | "always" | "off" => Ok(ParamValue::String(v.to_ascii_lowercase())),
                _ => Err(format!("calibrate '{v}': use auto, recheck, always or off")),
            },
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
            parser: |v| v.parse::<u32>().map(ParamValue::U32).map_err(|_| format!("Invalid cycles value '{}'", v)),
        });

        params.insert("duration", ParamDef {
            key: "duration",
            can_override_config: true,
            parser: |v| v.parse::<u32>().map(ParamValue::U32).map_err(|_| format!("Invalid duration value '{}'", v)),
        });

        params.insert("errors", ParamDef {
            key: "errors",
            can_override_config: true,
            parser: |v| match v.to_lowercase().as_str() {
                "log" | "halt" | "stop" | "panic" | "debug" => Ok(ParamValue::String(v.to_string())),
                _ => Err(format!("Invalid error mode '{}'. Valid: log, halt, panic", v)),
            },
        });

        params.insert("channels", ParamDef {
            key: "channels",
            can_override_config: true,
            parser: |v| {
                let val = v.parse::<usize>().map_err(|_| format!("Invalid channels value '{}'", v))?;
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
            parser: |v| crate::seal::SealSettings::parse(v, "auto").map(|_| ParamValue::String(v.trim().to_lowercase())),
        });
        params.insert("seal-width", ParamDef {
            key: "seal-width",
            can_override_config: true,
            parser: |v| crate::seal::SealWidth::parse(v).map(|w| ParamValue::String(w.name().to_string())),
        });
        // The tests' SIMD width (TODO 99, `crate::simd::Width`): read early in `session::run`, before a config loads; registered so it
        // validates
        params.insert("width", ParamDef {
            key: "width",
            can_override_config: true,
            parser: |v| crate::simd::Width::parse(v).map(|w| ParamValue::String(w.map_or("auto", crate::simd::Width::name).to_string())),
        });

        params.insert("mirror", ParamDef {
            key: "mirror",
            can_override_config: true,
            parser: |v| v.to_lowercase().parse::<crate::config::MirrorMode>().map(|m| ParamValue::String(m.to_string())),
        });

        // The loaded latency tests' settings (TODO 117, `loaded::Params`), as their config fields: Lat-Turnaround-DRAM's levels, either's
        // pairs and window. Kept as the levels' text, which the session parses again.
        params.insert("load-levels", ParamDef {
            key: "load-levels",
            can_override_config: true,
            parser: |v| {
                crate::loaded::Level::parse_list(v)
                    .map(|levels| ParamValue::String(levels.iter().map(ToString::to_string).collect::<Vec<_>>().join(",")))
                    .map_err(|e| format!("Invalid load-levels '{v}': {e}"))
            },
        });
        params.insert("load-pairs", ParamDef {
            key: "load-pairs",
            can_override_config: true,
            parser: |v| match v.trim().parse::<u32>() {
                Ok(n) if (1..=crate::loaded::MAX_PAIRS).contains(&n) => Ok(ParamValue::U32(n)),
                _ => Err(format!("Invalid load-pairs '{v}'. Valid: 1 to {}", crate::loaded::MAX_PAIRS)),
            },
        });
        params.insert("load-window-ms", ParamDef {
            key: "load-window-ms",
            can_override_config: true,
            parser: |v| match v.trim().parse::<u32>() {
                Ok(n) if (crate::loaded::MIN_WINDOW_MS..=crate::loaded::MAX_WINDOW_MS).contains(&n) => Ok(ParamValue::U32(n)),
                _ => Err(format!(
                    "Invalid load-window-ms '{v}'. Valid: {} to {} (ms)",
                    crate::loaded::MIN_WINDOW_MS,
                    crate::loaded::MAX_WINDOW_MS
                )),
            },
        });

        // Pattern and repetition parameters
        params.insert("pattern-mode", ParamDef {
            key: "pattern-mode",
            can_override_config: true,
            parser: |v| {
                let val =
                    v.parse::<u32>().map_err(|_| format!("Invalid pattern-mode '{}'. Valid: {}", v, crate::pattern::pattern_modes()))?;
                if crate::pattern::is_pattern_mode(val) {
                    Ok(ParamValue::U32(val))
                } else {
                    Err(format!(
                        "Invalid pattern-mode {}{}. Valid: {}",
                        val,
                        crate::pattern::renumbered(val),
                        crate::pattern::pattern_modes()
                    ))
                }
            },
        });

        params.insert("sequence", ParamDef {
            key: "sequence",
            can_override_config: true,
            parser: |v| crate::sequence::Sequence::parse(v, crate::sequence::Order::Forward).map(|_| ParamValue::String(v.to_string())),
        });

        params.insert("order", ParamDef {
            key: "order",
            can_override_config: true,
            parser: |v| {
                crate::sequence::Order::parse(v)
                    .map(|_| ParamValue::String(v.to_string()))
                    .ok_or_else(|| format!("Invalid order '{v}'. Valid: forward, reverse, random, stride"))
            },
        });

        params.insert("verify-reps", ParamDef {
            key: "verify-reps",
            can_override_config: true,
            parser: |v| {
                let val = v.parse::<u32>().map_err(|_| format!("Invalid verify-reps '{}'. Must be 1-100", v))?;
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
                let val = v.parse::<u32>().map_err(|_| format!("Invalid test-reps '{}'. Must be 1-100", v))?;
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
                let val = v.parse::<u32>().map_err(|_| format!("Invalid write-read-cycles '{}'. Must be 1-100", v))?;
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
            parser: |v| match v.to_lowercase().as_str() {
                "regular" | "4kb" | "large" | "2mb" | "huge" | "1gb" => Ok(ParamValue::String(v.to_lowercase())),
                _ => Err(format!("Invalid page size '{}'. Valid: regular/4kb, large/2mb, huge/1gb", v)),
            },
        });

        params.insert("maxpage", ParamDef {
            key: "maxpage",
            can_override_config: true,
            parser: |v| match v.to_lowercase().as_str() {
                "regular" | "4kb" | "large" | "2mb" | "huge" | "1gb" => Ok(ParamValue::String(v.to_lowercase())),
                _ => Err(format!("Invalid page size '{}'. Valid: regular/4kb, large/2mb, huge/1gb", v)),
            },
        });

        // Block sizes and share rounding. Each parses here; how they fit together is checked by MemoryAllocationConfig::share_rounding.
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

        // Config file parameter: a path, which `config::load_config` looks for. An empty one the session takes as no config at all.
        params.insert("config", ParamDef {
            key: "config",
            can_override_config: false,
            parser: |v| {
                if v.trim().is_empty() {
                    Err(format!("Invalid config '{v}'. Valid: a config file's path (a TMR .json or a TM5 .cfg)"))
                } else {
                    Ok(ParamValue::String(v.to_string()))
                }
            },
        });

        // Test filter parameter: names or glob patterns, comma-separated. `runner::retain_matching` drops a pattern that isn't a glob, with
        // a warning in the log alone, and runs what the others match.
        params.insert("test", ParamDef {
            key: "test",
            can_override_config: false,
            parser: |v| match v.split(',').map(str::trim).find_map(|pattern| globset::Glob::new(pattern).err()) {
                None => Ok(ParamValue::String(v.to_string())),
                Some(e) => Err(format!("Invalid test '{v}': {e}. Valid: test names or glob patterns (Mem-*, Lat-L?-*), comma-separated")),
            },
        });

        params.insert("priority", ParamDef {
            key: "priority",
            can_override_config: true,
            parser: |v| v.parse::<crate::runner::ThreadPriority>().map(|p| ParamValue::String(p.to_string())),
        });
        // The window's thread during a run (the owner, 2026-11-02): the workers' values, its default theirs (`session::priorities`)
        params.insert("window-priority", ParamDef {
            key: "window-priority",
            can_override_config: true,
            parser: |v| {
                v.parse::<crate::runner::ThreadPriority>()
                    .map(|p| ParamValue::String(p.to_string()))
                    .map_err(|_| format!("Invalid window-priority '{v}': normal, high or realtime (default: the workers', priority=)"))
            },
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
        self.params.get(key).or_else(|| self.flags.get(key)).map(|def| def.can_override_config).unwrap_or(false)
    }

    /// List all recognized parameter keys (for validation)
    pub fn all_keys(&self) -> Vec<&str> {
        self.params.keys().chain(self.flags.keys()).copied().collect()
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
            || arg == "--no-serials"
            || arg.starts_with("--startup-debug")
        {
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

    /// `window-priority=` takes `priority=`'s values, in any case, as their lower-case names, and refuses others, naming itself
    #[test]
    fn window_priority_takes_the_workers_values() {
        let parse = |key: &str, v: &str| super::parse_and_validate_args(&[format!("{key}={v}")]);
        for (given, name) in [("normal", "normal"), ("High", "high"), ("REALTIME", "realtime")] {
            for key in ["priority", "window-priority"] {
                let parsed = parse(key, given).unwrap_or_else(|e| panic!("{key}={given}: {e}"));
                assert_eq!(super::get_string(&parsed, key, ""), name, "{key}={given}");
            }
        }
        for bad in ["", "turbo", "idle", "15"] {
            let e = parse("window-priority", bad).unwrap_err();
            assert!(e.contains(&format!("Invalid window-priority '{bad}'")), "{e}");
        }
    }

    /// `key=v`'s value as the parser keeps it, or its message
    fn kept(key: &str, v: &str) -> Result<String, String> {
        super::parse_and_validate_args(&[format!("{key}={v}")]).map(|parsed| super::get_string(&parsed, key, "(none)"))
    }

    /// `cpus=` takes a share of the available CPUs or a count of them, as the session reads them (clamped there: 0 is one CPU, more than
    /// the pool all of it), trimmed, and refuses anything else, naming the two, which the session would read as every CPU
    #[test]
    fn cpus_takes_a_share_or_a_count() {
        #[rustfmt::skip]
        let valid = [
            ("100%", "100%"), ("50%", "50%"), ("1%", "1%"), ("0%", "0%"), ("150%", "150%"),
            ("1", "1"), ("4", "4"), ("0", "0"), ("512", "512"), (" 8 ", "8"),
        ];
        for (given, kept_as) in valid {
            assert_eq!(kept("cpus", given).as_deref(), Ok(kept_as), "cpus={given}");
        }
        for bad in ["lots", "", "%", "50%%", "%50", "-1", "1.5", "50 %", "half", "4cores", "1-4", "99999999999%"] {
            let e = kept("cpus", bad).unwrap_err();
            assert!(e.contains(&format!("Invalid cpus '{bad}'. Valid: N% (a share of the available CPUs, e.g. 50%) or N (a count")), "{e}");
        }
    }

    /// `memory=` takes what `AllocationMode::parse` does, kept as given, and refuses the rest at parsing, with that parse's reason and
    /// the forms
    #[test]
    fn memory_takes_what_the_allocation_reads() {
        for ok in ["50%", "10%-from-available", "2GiB", "2048MB", "1024-legacy", "8GiB-from-total", "64GiB-target", "80%-target"] {
            assert_eq!(kept("memory", ok).as_deref(), Ok(ok), "memory={ok}");
        }
        for (bad, reason) in [
            ("lots", "Unrecognized memory format"),
            ("", "Unrecognized memory format"),
            ("150%", "Percentage must be between 0-100"),
            ("xGiB-target", "Invalid GiB/GB format"),
        ] {
            let e = kept("memory", bad).unwrap_err();
            assert!(e.contains(&format!("Invalid memory '{bad}': {reason}")) && e.contains("Valid: a share (20%) or a size"), "{e}");
        }
    }

    /// `test=` takes names and glob patterns, comma-separated, kept as given, and refuses a pattern that isn't a glob, which the runner
    /// would drop and run the rest
    #[test]
    fn test_takes_names_and_globs() {
        for ok in ["Mem-SimpleV2-512_A", "Spd-*", "Mem-Mirror*,Lat-L3-*", "Lat-L?-*, Mem-*", "Mem-{Refresh}*"] {
            assert_eq!(kept("test", ok).as_deref(), Ok(ok), "test={ok}");
        }
        // The last: the list splits at its commas first, so an alternation can't hold one
        for bad in ["Lat-[L3", "Mem-*,Lat-{L1", "Mem-{Refresh,Mirror}*"] {
            let e = kept("test", bad).unwrap_err();
            assert!(e.contains(&format!("Invalid test '{bad}': ")) && e.contains("Valid: test names or glob patterns"), "{e}");
        }
    }

    /// TODO 117: `load-levels=` takes shares and pinned MiB/s, comma-separated, kept in the results' form; `load-pairs=` 1 to 50 and
    /// `load-window-ms=` 10 to 60000, as the config's fields; each refuses the rest, naming itself
    #[test]
    fn the_loaded_latency_settings_take_their_ranges() {
        assert_eq!(kept("load-levels", "25%, 50% ,8058").as_deref(), Ok("25%,50%,8058"));
        for bad in ["", "0%", "150%", "fast", "1%,2%,3%,4%,5%,6%,7%,8%,9%"] {
            let e = kept("load-levels", bad).unwrap_err();
            assert!(e.contains(&format!("Invalid load-levels '{bad}': ")), "{e}");
        }
        let parsed = |key: &str, v: &str| super::parse_and_validate_args(&[format!("{key}={v}")]).map(|p| super::get_u32(&p, key, 0));
        assert_eq!((parsed("load-pairs", "1"), parsed("load-pairs", "50")), (Ok(1), Ok(50)));
        assert_eq!((parsed("load-window-ms", "10"), parsed("load-window-ms", "60000")), (Ok(10), Ok(60_000)));
        for (key, bad) in
            [("load-pairs", "0"), ("load-pairs", "51"), ("load-window-ms", "9"), ("load-window-ms", "60001"), ("load-pairs", "x")]
        {
            let e = parsed(key, bad).unwrap_err();
            assert!(e.contains(&format!("Invalid {key} '{bad}'. Valid: ")), "{e}");
        }
    }

    /// `config=` takes any path, kept as given (the loader looks for it), and refuses an empty one, which would be no config
    #[test]
    fn config_takes_a_path() {
        for ok in ["x.cfg", r"C:\My configs\x.cfg", "configs/anta777.cfg"] {
            assert_eq!(kept("config", ok).as_deref(), Ok(ok), "config={ok}");
        }
        for bad in ["", "  "] {
            let e = kept("config", bad).unwrap_err();
            assert!(e.contains(&format!("Invalid config '{bad}'. Valid: a config file's path")), "{e}");
        }
    }
}
