//! A session (TODO 29): a command line's setup, then its run, shown through a frontend. This is what the CLI did before the
//! engine/presentation split, with an event (`frontend::Setup`) where it printed and an end (`SessionEnd`) where it exited, so the CLI and
//! the window take the same parameters and the console shows what it did. The question it asks, the large-page restart, is the frontend's
//! to answer (`Frontend::large_page_restart`).
//!
//! One session a process: the run settings it sets (the seed, the width, the topology's typing) are first-set-wins.

use std::collections::HashMap;
use std::env;
use std::sync::Arc;
use std::time::Duration;

use crate::commands::Command;
use crate::config::{CpuPinningConfig, MemoryAllocationConfig};
use crate::cpu_topology::{CoreType, get_cpu_topology, is_hybrid_cpu};
use crate::frontend::{CacheCalibrationState, Frontend, ParamNote, RestartChoice, SealCalibrationState, Selection, Setup};
use crate::memory::allocation_strategy::EnhancedMemoryStrategy;
use crate::runner::{RunStatus, TestSuiteTiming, detect_runtime_capabilities, run_tests_with_layout_and_timing_filtered};
use crate::{ErrorMode, load_config, params};

/// `--ram-latency`: the 6 DRAM latency tests, DRAM-* and DRAMFull-* (TODO 92)
pub(crate) const RAM_LATENCY_FILTER: &str = "Lat-DRAM-*,Lat-DRAMFull-*";
/// `--cache-latency`: Read/Write/Copy at L1, L2, L3 and DRAM, 12 tests; DRAMFull is `--ram-latency`'s (TODO 92)
pub(crate) const CACHE_LATENCY_FILTER: &str = "Lat-L1-*,Lat-L2-*,Lat-L3-*,Lat-DRAM-*";
/// The flags that pick a mode's defaults, which `run` takes out before it looks for a one-shot command (`logfile::wanted` too)
pub(crate) const MODE_FLAGS: [&str; 3] = ["--ram-latency", "--cache-latency", "--quick-test"];

/// How a session ended. An error that stops it is `run`'s `Err`.
pub enum SessionEnd {
    /// The run ran, or with `plan_only` (`--startup-debug`) showed its plan: its status, and the time from the end of setup
    Ran { status: RunStatus, elapsed: Duration, plan_only: bool },
    /// The large-page restart was chosen (`RestartChoice::Exit`): nothing ran
    RestartChosen,
    /// Setup stopped it before testing
    Stopped(Stopped),
    /// The command line is a one-shot command, for the caller to run (`commands::run`): nothing ran
    Command(Command),
}

/// Why setup stopped a session.
pub enum Stopped {
    /// `-` flags it doesn't know
    UnknownFlags(Vec<String>),
    /// A parameter it can't take, and why (`params::parse_and_validate_args`)
    Invalid(String),
    /// A `memory=` that can't override the config's, and why
    MemoryOverride { spec: String, error: String },
    /// The CPUs asked for can't be selected (`cpu_selection::calculate_thread_allocation`)
    CpuSelection(String),
}

/// A session: `command_line` (the exe's name first) set up, then run, its events to `fe`. Returns how it ended.
pub fn run(command_line: Vec<String>, fe: Arc<dyn Frontend>) -> crate::Result<SessionEnd> {
    // A frontend checks first (tmr.exe does), and this is for one that didn't: the kernels would fault on a CPU without them
    if let Some(refusal) = crate::cpu_check::refusal() {
        return Err(refusal.into());
    }
    // The command line as typed, for the results file (`RunConfigSnapshot::command_line`): `args` gains the defaults the modes below add
    let mut args = command_line.clone();

    // core-priority= first: it decides how the topology is typed, which `--show-topology` and
    // `--debug-topology` print before any other parameter is read (TODO 96)
    if let Some(value) = args.iter().rev().find_map(|a| a.strip_prefix("core-priority=")) {
        crate::cpu_topology::set_core_priority(crate::cpu_topology::CorePriority::parse(value)?);
    }
    // The native patterns' run settings, before a config loads: patterns= decides how a .cfg's SimpleTests import. The last of each wins,
    // as for every parameter. width= too, before a config's can (TODO 99): a width this CPU lacks stops the run here.
    let mut seen_keys = std::collections::HashSet::new();
    for (key, value) in args.iter().rev().filter_map(|a| a.split_once('=')) {
        if !seen_keys.insert(key) {
            continue;
        }
        match key {
            "seed" => crate::pattern::set_run_seed(crate::pattern::parse_seed(value)?),
            "rescan" => crate::pattern::set_rescan(!value.eq_ignore_ascii_case("off")),
            "beat" => {
                if let Some(beat) = crate::pattern::parse_beat(value)? {
                    crate::pattern::set_beat_override(beat);
                }
            }
            "patterns" => crate::pattern::set_legacy_import(value.eq_ignore_ascii_case("legacy")),
            "width" => crate::simd::set_width(crate::simd::Width::parse(value)?, "width=")?,
            _ => {}
        }
    }

    // Handle test= parameter for filtering tests by pattern
    let mut test_filter_param: Option<String> = None;
    for arg in &args {
        if let Some(test_pattern) = arg.strip_prefix("test=") {
            test_filter_param = Some(test_pattern.to_string());
            fe.event(Setup::TestFilter { pattern: test_pattern.to_string() }.into());
        }
    }

    // Inject cycles=1 default when test= filter is used (for quick targeted runs)
    // This only applies if cycles= was not explicitly specified
    if test_filter_param.is_some() {
        let has_explicit_cycles = args.iter().any(|a| a.starts_with("cycles="));
        if !has_explicit_cycles {
            args.push("cycles=1".to_string());
        }
    }

    // plan= (TODO 25): its cycles when none are given. --quick-test names it below; a run that
    // names no plan, config or filter takes the default when the tests are chosen.
    let mut plan_param: Option<&'static crate::plans::Plan> = None;
    for arg in &args {
        if let Some(name) = arg.strip_prefix("plan=") {
            plan_param = Some(crate::plans::find(name)?);
        }
    }
    if let Some(cycles) = plan_param.and_then(|p| p.cycles)
        && !args.iter().any(|a| a.starts_with("cycles="))
    {
        args.push(format!("cycles={cycles}"));
    }

    // Validate that all -- flags are recognized before processing
    // This catches typos and deprecated flags early with helpful error messages
    let known_flags = [
        "--ram-latency", "--cache-latency", "--quick-test", "--calibrate-cache", "--calibrate-cache-ext", "--calibrate-seal",
        "--calibrate-all", "--calibration-file", "--output", "--no-calibration", "--yes", "--no-save", "--create-demo-configs",
        "--compare-results", "--debug-topology", "--show-topology", "--setup-large-pages", "--startup-debug", "--disable-pinning",
        "--help", "-h", "-?", "--version", "-v",
    ];

    let unknown_flags: Vec<String> = args
        .iter()
        .filter(|arg| arg.starts_with("--") || arg.starts_with("-"))
        .filter(|arg| {
            // Check if it matches any known flag (handle --flag=value style)
            !known_flags.iter().any(|known| arg.starts_with(known) || arg == known)
        })
        .cloned()
        .collect();

    if !unknown_flags.is_empty() {
        return Ok(SessionEnd::Stopped(Stopped::UnknownFlags(unknown_flags)));
    }

    // Track if a test filter should be applied (for --ram-latency and --cache-latency)
    let mut test_filter: Option<String> = None;

    // Handle --ram-latency: DRAM latency characterization with TLB analysis
    // Tests: Read/Write/Copy × (DRAM, DRAMFull) = 6 tests
    // Uses 50% CPUs for realistic multi-threaded DRAM behavior
    let ram_latency_idx = args.iter().position(|a| a == "--ram-latency");
    if ram_latency_idx.is_some() {
        args.retain(|a| a != "--ram-latency");

        // Inject ram-latency defaults
        let mut ram_latency_defaults = vec![
            ("memory", "10%-from-available"),
            ("cpus", "50%"), // Multi-threaded for realistic DRAM behavior
            ("cycles", "1"), // Single cycle for quick latency measurement
        ];

        for (key, default_value) in ram_latency_defaults.drain(..) {
            let param_prefix = format!("{}=", key);
            if !args.iter().any(|a| a.starts_with(&param_prefix)) {
                args.push(format!("{}={}", key, default_value));
            }
        }

        test_filter = Some(RAM_LATENCY_FILTER.to_string());
    }

    // Handle --cache-latency: Full cache hierarchy diagnostic (single-thread)
    // Tests: Read/Write/Copy × (L1, L2, L3, DRAM) = 12 tests
    // Uses 1 CPU for clean single-threaded measurements
    let cache_latency_idx = args.iter().position(|a| a == "--cache-latency");
    if cache_latency_idx.is_some() {
        args.retain(|a| a != "--cache-latency");

        // Inject cache-latency defaults: single thread for clean measurements
        let mut cache_latency_defaults = vec![
            ("memory", "10%-from-available"),
            ("cpus", "1"),   // Single physical core for diagnostic-quality measurements
            ("cycles", "1"), // Single cycle for quick latency measurement
        ];

        for (key, default_value) in cache_latency_defaults.drain(..) {
            let param_prefix = format!("{}=", key);
            if !args.iter().any(|a| a.starts_with(&param_prefix)) {
                args.push(format!("{}={}", key, default_value));
            }
        }

        test_filter = Some(CACHE_LATENCY_FILTER.to_string());
    }

    // Handle --quick-test by injecting defaults BEFORE parameter parsing
    // This allows CLI overrides to work: --quick-test cycles=2 skip-cores=0
    let has_quick_test = args.iter().any(|a| a == "--quick-test");
    if has_quick_test {
        fe.event(Setup::QuickTest.into());

        // Inject quick-test defaults (only if not already specified)
        let mut quick_defaults =
            vec![("memory", "60%-from-available"), ("cycles", "1"), ("skip-cores", "1"), ("cpus", "100%"), ("plan", "quick")];
        if !args.iter().any(|a| a.starts_with("plan=")) {
            plan_param = crate::plans::find("quick").ok();
        }

        // Only add defaults that aren't already specified on command line
        for (key, default_value) in quick_defaults.drain(..) {
            let param_prefix = format!("{}=", key);
            if !args.iter().any(|a| a.starts_with(&param_prefix)) {
                args.push(format!("{}={}", key, default_value));
            }
        }

        // Remove --quick-test flag (it's not a real parameter)
        args.retain(|a| a != "--quick-test");
    }

    // Check for special commands
    if let Some(command) = Command::parse(&args) {
        return Ok(SessionEnd::Command(command));
    }
    // TMR's home and its folders, which a run writes its results and log to (TODO 104): one it can't write to stops it here, before it
    // allocates, rather than at the end with no results file
    crate::home::prepare()?;
    if let Some(e) = calibration_flag_without_calibration(&args) {
        return Ok(SessionEnd::Stopped(Stopped::Invalid(e)));
    }

    // Parse and validate ALL command-line arguments using centralized registry
    let validated_params = match params::parse_and_validate_args(&args[1..]) {
        Ok(params) => params,
        Err(e) => return Ok(SessionEnd::Stopped(Stopped::Invalid(e))),
    };

    // Check for config file parameter
    let config_file = params::get_string(&validated_params, "config", "");
    let config_file = if config_file.is_empty() { None } else { Some(config_file) };

    let (
        mut enhanced_memory_strategy,
        mut error_mode,
        mut suite_timing,
        mut cputype,
        mut cpus,
        mut pinning_config,
        mut alloc_config,
        config_opt,
    ) = if let Some(ref config_path) = config_file {
        let config = load_config(config_path)?;
        (
            config.to_memory_strategy(),
            config.to_error_mode(),
            config.to_test_suite_timing(),
            config.system.cpu_config.cpu_type.clone(),
            format!("{}%", config.system.cpu_config.usage_percent),
            config.system.cpu_pinning.clone(),
            config.system.memory_allocation.clone(), // Use the config's allocation settings
            Some(config),                            // Keep the full config for test sequence
        )
    } else {
        // Build config from validated command-line parameters
        let (enhanced_memory_strategy, error_mode, suite_timing, cputype, cpus, pinning_config, alloc_config) =
            build_config_from_validated_params(&validated_params, fe.as_ref())?;
        (enhanced_memory_strategy, error_mode, suite_timing, cputype, cpus, pinning_config, alloc_config, None)
    };

    // A config's SIMD width (TODO 99), unless width= gave one: a width this CPU lacks stops the run
    if let Some(width) = config_opt.as_ref().and_then(|c| c.system.width.as_deref()) {
        crate::simd::set_width(crate::simd::Width::parse(width)?, "the config's width")?;
    }

    // Apply CLI parameter overrides even when using a config file
    // This allows: tmr.exe config=test.json cycles=1 skip-cores=0 memory=4GiB cpus=50%
    if config_opt.is_some() {
        let registry = params::get_registry();

        // Apply overrides for each parameter (already validated)
        for (key, value) in &validated_params {
            // Only apply if this parameter can override config files
            if !registry.can_override_config(key) {
                continue;
            }

            // Apply the override based on parameter type
            match key.as_str() {
                "errors" => {
                    error_mode = match params::get_string(&validated_params, "errors", "log").to_lowercase().as_str() {
                        "halt" | "stop" => ErrorMode::Halt,
                        "panic" | "debug" => ErrorMode::Panic,
                        _ => ErrorMode::Log,
                    };
                    log::debug!("CLI override: error_mode = {:?}", error_mode);
                }
                "cycles" => {
                    let cycles = params::get_u32(&validated_params, "cycles", 3);
                    suite_timing.global_cycles = Some(cycles);
                    log::debug!("CLI override: global_cycles = {}", cycles);
                }
                "duration" => {
                    let duration = params::get_u32(&validated_params, "duration", 0);
                    suite_timing.global_duration_secs = Some(duration);
                    log::debug!("CLI override: global_duration_secs = {}", duration);
                }
                "memory" => {
                    if let params::ParamValue::String(memory_str) = value {
                        match parse_enhanced_memory_parameter(memory_str) {
                            Ok(strategy) => {
                                enhanced_memory_strategy = strategy;
                                log::debug!("CLI override: enhanced_memory_strategy");
                            }
                            Err(e) => {
                                return Ok(SessionEnd::Stopped(Stopped::MemoryOverride { spec: memory_str.clone(), error: e }));
                            }
                        }
                    }
                }
                "cpus" => {
                    cpus = params::get_string(&validated_params, "cpus", "100%");
                    log::debug!("CLI override: cpus = {}", cpus);
                }
                "cputype" => {
                    cputype = params::get_string(&validated_params, "cputype", "threads");
                    log::debug!("CLI override: cputype = {}", cputype);
                }
                "skip-cores" => {
                    // Stage 1 filter spec; resolved later against the real core count.
                    pinning_config.skip_spec = params::get_string(&validated_params, "skip-cores", "1");
                    log::debug!("CLI override: skip_spec = {}", pinning_config.skip_spec);
                }
                "cpu-stride" => {
                    // Stage 2 spacing spec.
                    pinning_config.stride_spec = params::get_string(&validated_params, "cpu-stride", "1");
                    log::debug!("CLI override: stride_spec = {}", pinning_config.stride_spec);
                }
                "--disable-pinning" => {
                    pinning_config.enable_pinning = false;
                    log::debug!("CLI override: enable_pinning = false");
                }
                "topology" => {
                    if let params::ParamValue::String(topology_str) = value {
                        use crate::cpu_topology::{TopologyDetectionMethod, set_topology_detection_method};
                        // Validated by the parameter's parser
                        let method = TopologyDetectionMethod::parse(topology_str).unwrap_or(TopologyDetectionMethod::Auto);
                        set_topology_detection_method(method);
                        log::debug!("CLI override: topology_detection_method = {:?}", method);
                    }
                }
                "minpage" => {
                    if let params::ParamValue::String(page_str) = value {
                        alloc_config.min_page_size = page_str.to_string();
                        log::debug!("CLI override: min_page_size = {}", page_str);
                    }
                }
                "maxpage" => {
                    if let params::ParamValue::String(page_str) = value {
                        alloc_config.max_page_size = page_str.to_string();
                        log::debug!("CLI override: max_page_size = {}", page_str);
                    }
                }
                "hugechunk" | "largechunk" | "largefloor" | "blkroundtarget" | "blkround" => {
                    if let params::ParamValue::String(v) = value {
                        alloc_config.set_block_param(key, v);
                        log::debug!("CLI override: {} = {}", key, v);
                    }
                }
                _ => {} // Ignore unknown overrides
            }
        }
    }

    // Display startup mode and parameters
    // Display build type (helps identify performance issues in logs)
    let build_type = if cfg!(debug_assertions) { "Debug (unoptimized)" } else { "Release (optimized)" };
    fe.event(Setup::StartupHeader { command_line: args.clone(), release: !cfg!(debug_assertions) }.into());
    log::info!("TMR v{} starting - {} build on {}", crate::BUILD.version, build_type, std::env::consts::ARCH);

    // Check environment variables
    let rust_log = env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string());

    let rust_backtrace = env::var("RUST_BACKTRACE").unwrap_or_else(|_| "not set".to_string());
    let rust_backtrace = (rust_backtrace != "not set").then_some(rust_backtrace);

    // Show which parameters were explicitly set vs defaults
    let mut explicit_params = Vec::new();
    let mut default_params = Vec::new();

    // Use centralized registry to check for recognized parameters
    let registry = params::get_registry();

    for arg in &args[1..] {
        // Check if it's a recognized parameter or flag
        let is_recognized = if arg.starts_with("--") {
            registry.all_keys().contains(&arg.as_str())
        } else if let Some(key) = arg.split('=').next() {
            registry.all_keys().contains(&key)
        } else {
            false
        };

        if is_recognized {
            explicit_params.push(arg.clone());
        }
    }

    // Add defaults that weren't explicitly set (only if no config file)
    if config_file.is_none() {
        if !args.iter().any(|a| a.starts_with("cputype=")) {
            default_params.push("cputype=threads (default)".to_string());
        }
        if !args.iter().any(|a| a.starts_with("cpus=")) {
            default_params.push("cpus=100% (default)".to_string());
        }
        if !args.iter().any(|a| a.starts_with("memory=")) {
            default_params.push("memory=10%-from-available (default)".to_string());
        }
        if !args.iter().any(|a| a.starts_with("errors=")) {
            default_params.push("errors=log (default)".to_string());
        }
        if !args.iter().any(|a| a.starts_with("cycles=")) {
            default_params.push("cycles=3 (default)".to_string());
        }
    }

    fe.event(Setup::StartupParams { rust_log, rust_backtrace, explicit: explicit_params, defaults: default_params }.into());

    // Calculate CPU/thread count
    // From TMR's topology, which covers every processor group (num_cpus sees only one: 64 CPUs)
    let total_cpus = match cputype.as_str() {
        "cores" => crate::cpu_topology::physical_core_count(),
        _ => crate::cpu_topology::logical_cpu_ids().len(),
    };

    // When using cores mode, we should automatically avoid SMT doubling
    let avoid_smt = cputype == "cores";
    fe.event(Setup::CpuType { cputype: cputype.clone(), avoid_smt }.into());

    // Stage 1 FILTER: resolve the skip-cores spec ("N" | "N%" | "A-B") into a leading-skip count and/or an excluded core-id range. The
    // range form is how you isolate one memory domain on a multi-CCD part (e.g. skip-cores=0-7 tests only cores 8+).
    let (resolved_skip, skip_excluded_range) = crate::cpu_selection::resolve_skip_spec(&pinning_config.skip_spec, total_cpus);
    pinning_config.cpus_to_skip = resolved_skip;
    if let Some((lo, hi)) = skip_excluded_range {
        fe.event(Setup::CpuFilter { lo, hi }.into());
    }
    // Cores removed by an excluded range shrink the available pool too.
    let range_excluded_count = skip_excluded_range
        .map(|(lo, hi)| {
            let topo = get_cpu_topology();
            if avoid_smt {
                // cores mode: count distinct physical cores in the excluded range
                topo.iter()
                    .filter(|c| c.physical_core_id >= lo && c.physical_core_id <= hi)
                    .map(|c| c.physical_core_id)
                    .collect::<std::collections::HashSet<_>>()
                    .len()
            } else {
                // threads mode: count logical CPUs on those cores
                topo.iter().filter(|c| c.physical_core_id >= lo && c.physical_core_id <= hi).count()
            }
        })
        .unwrap_or(0);

    // Calculate available CPUs after accounting for skipped cores
    let available_cpus_before_range = if pinning_config.cpus_to_skip > 0 {
        // Get topology to count logical CPUs on skipped physical cores
        let topology = get_cpu_topology();
        let mut cores_by_id: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
        for cpu in topology {
            *cores_by_id.entry(cpu.physical_core_id).or_insert(0) += 1;
        }
        // Sort core IDs to determine which cores get skipped (E-cores first for hybrid)
        let is_hybrid = is_hybrid_cpu(get_cpu_topology());
        let mut sorted_cores: Vec<_> = cores_by_id.iter().collect();
        if is_hybrid {
            // For hybrid, E-cores are skipped first - sort by core type then ID
            let topo = get_cpu_topology();
            sorted_cores.sort_by(|a, b| {
                let a_type = topo.iter().find(|c| c.physical_core_id == *a.0).map(|c| c.core_type);
                let b_type = topo.iter().find(|c| c.physical_core_id == *b.0).map(|c| c.core_type);
                // E-cores (Efficiency) should come first for skipping
                match (a_type, b_type) {
                    (Some(CoreType::Efficiency(_)), Some(CoreType::Performance(_))) => std::cmp::Ordering::Less,
                    (Some(CoreType::Performance(_)), Some(CoreType::Efficiency(_))) => std::cmp::Ordering::Greater,
                    _ => a.0.cmp(b.0),
                }
            });
        } else {
            sorted_cores.sort_by_key(|(id, _)| *id);
        }
        // Count logical CPUs on skipped physical cores
        let skipped_logical: usize = sorted_cores.iter().take(pinning_config.cpus_to_skip).map(|(_, count)| *count).sum();
        let available = if avoid_smt {
            // For cores mode, available = physical cores - skipped
            total_cpus.saturating_sub(pinning_config.cpus_to_skip)
        } else {
            // For threads mode, available = logical threads - skipped logical
            total_cpus.saturating_sub(skipped_logical)
        };
        available.max(1)
    } else {
        total_cpus
    };

    // Subtract cores removed by an excluded id-range to get the true Available pool.
    let available_cpus = available_cpus_before_range.saturating_sub(range_excluded_count).max(1);

    // `cpus=` is RELATIVE to the post-filter Available pool: any value <= 100% is therefore
    // always valid, which keeps configs portable across 4/6/8/16-core machines.
    let threads = if cpus.ends_with('%') {
        // Percentage of available CPUs
        let percent = cpus.trim_end_matches('%').parse::<u32>().unwrap_or(100);
        // Round to nearest instead of truncating: +50 before dividing by 100
        let n = ((available_cpus as u32 * percent + 50) / 100).max(1).min(available_cpus as u32) as usize;
        // Make the relative math explicit so `cpus=50%` after a skip isn't a surprise.
        fe.event(Setup::CpuCount { percent, available: available_cpus, threads: n }.into());
        n
    } else {
        // Absolute count
        cpus.parse::<usize>().unwrap_or(available_cpus).min(available_cpus).max(1)
    };

    // Selected even when pinning is off: the list also decides which NUMA node each thread's memory comes from, so a pinned/unpinned A/B
    // differs in scheduling alone. The pool skips only the affinity call (`RuntimeConfig.pin_threads`).
    // Override avoid_smt_doubling if cputype=cores
    let effective_avoid_smt = pinning_config.avoid_smt_doubling || avoid_smt;
    let (actual_threads, cpu_list) = match crate::cpu_selection::calculate_thread_allocation(
        threads, pinning_config.cpus_to_skip, effective_avoid_smt, skip_excluded_range, &pinning_config.stride_spec,
    ) {
        Ok(result) => result,
        Err(e) => {
            // Never silently right-size the request — tell the user and stop.
            return Ok(SessionEnd::Stopped(Stopped::CpuSelection(e)));
        }
    };

    // Detect SIMD capabilities
    let simd_caps = crate::detect_simd_capabilities();
    fe.event(
        Setup::Selection(Box::new(Selection {
            error_mode,
            threads: actual_threads,
            available: available_cpus,
            cputype: cputype.clone(),
            cpu_list: cpu_list.clone(),
            pinned: pinning_config.enable_pinning,
            cpus_to_skip: pinning_config.cpus_to_skip,
            avoid_smt_doubling: pinning_config.avoid_smt_doubling,
            cycles: suite_timing.global_cycles,
            duration_secs: suite_timing.global_duration_secs,
            simd: simd_caps.clone(),
        }))
        .into(),
    );

    fe.event(Setup::LargePageCheck.into());

    // Check for restart needed scenario first
    if crate::check_restart_needed() {
        match fe.large_page_restart() {
            RestartChoice::Exit => return Ok(SessionEnd::RestartChosen),
            // The default minpage needs large pages, so say 4 KB for this run (TODO 88)
            RestartChoice::Continue4K => alloc_config.min_page_size = "regular".to_string(),
        }
    } else {
        // Normal large page setup flow
        let large_page_diagnostic = crate::memory::diagnose_and_setup_large_pages();
        fe.event(Setup::LargePageDiagnostic { report: large_page_diagnostic }.into());
    }

    let usable = crate::check_large_page_privilege().is_ok();
    // Without the privilege, a large minpage (the default) refuses the run at allocation
    let min_needs_large = crate::memory::allocator::PageSizeLevel::from_config_str(&alloc_config.min_page_size)
        > crate::memory::allocator::PageSizeLevel::Regular;
    fe.event(Setup::LargePageStatus { usable, min_page: alloc_config.min_page_size.clone(), min_needs_large }.into());

    // Get system info for later use (formatted output comes via reporter below)
    let system_info = crate::tests::get_system_info();
    let cache_info = system_info.get_cache_info();

    // Set SMT sharing factor: 2 if hyperthreading AND using threads mode, 1 otherwise
    let smt_threads = if system_info.has_hyperthreading && !avoid_smt { 2 } else { 1 };
    crate::set_active_threads_per_core(smt_threads);

    // Detect SMBIOS data for identity validation
    let smbios = crate::smbios::get_smbios();

    // Load calibration from tmr-cfg.json (unless --no-calibration)
    let no_calibration = args.iter().any(|a| a == "--no-calibration");
    if !no_calibration {
        let cfg_path = crate::app_config::AppConfig::default_path();
        let app_config = crate::app_config::AppConfig::load(&cfg_path);
        if app_config.is_calibration_valid(system_info, smbios) {
            if let Some(cal) = app_config.get_best_calibration() {
                let extended = app_config.calibration_extended.is_some();
                fe.event(Setup::CacheCalibration(CacheCalibrationState::Loaded { extended, at: cal.timestamp }).into());
                crate::set_calibration_data(Some(cal.clone()));
            } else {
                fe.event(Setup::CacheCalibration(CacheCalibrationState::None).into());
                crate::set_calibration_data(None);
            }
        } else if app_config.machine_id.is_some() {
            fe.event(Setup::CacheCalibration(CacheCalibrationState::Stale).into());
            crate::set_calibration_data(None);
        } else {
            fe.event(Setup::CacheCalibration(CacheCalibrationState::None).into());
            crate::set_calibration_data(None);
        }
        let seal = load_seal_calibration(&app_config, system_info, smbios, crate::seal::set_calibrated_width);
        fe.event(Setup::SealCalibration(seal).into());
    } else {
        fe.event(Setup::CacheCalibration(CacheCalibrationState::Disabled).into());
        crate::set_calibration_data(None);
    }

    // Display CPU topology right after system information
    let (report, smt_p_cores, e_cores_skipped_first) =
        crate::cpu_topology::startup_topology(&cpu_list, pinning_config.cpus_to_skip, avoid_smt, pinning_config.enable_pinning);
    fe.event(Setup::CpuTopology { report: Box::new(report), smt_p_cores, e_cores_skipped_first }.into());

    // Show memory stats
    fe.event(Setup::MemoryStatus(crate::runner::memory_status()).into());

    {
        use crate::reporting::system_info_builder::build_system_info_report;

        let report = build_system_info_report(system_info, &simd_caps);
        fe.event(Setup::SystemInfo { report: Box::new(report), memory_modules: smbios.memory_modules.clone() }.into());
    }

    // Calculate memory layout using enhanced system
    // Checks the block sizes too, so a bad one stops the run before anything is allocated
    let share_rounding = alloc_config.share_rounding()?;
    let enhanced_layout = enhanced_memory_strategy.create_layout(actual_threads, share_rounding)?;

    fe.event(Setup::Starting.into());

    let mut runtime_config = detect_runtime_capabilities(&alloc_config);
    runtime_config.cpu_list = Some(cpu_list);
    runtime_config.pin_threads = pinning_config.enable_pinning;

    // The workers' priority (TODO 77): priority= over the config's, else High
    let priority = if params::has_param(&validated_params, "priority") {
        Some(params::get_string(&validated_params, "priority", "high"))
    } else {
        config_opt.as_ref().and_then(|c| c.system.thread_priority.clone())
    };
    runtime_config.thread_priority = match priority {
        Some(p) => p.parse()?,
        None => crate::runner::ThreadPriority::default(),
    };
    if runtime_config.thread_priority == crate::runner::ThreadPriority::Realtime
        && runtime_config.cpu_list.as_ref().is_some_and(|cpus| cpus.len() >= crate::cpu_topology::logical_cpu_ids().len())
    {
        fe.event(Setup::RealtimeOnEveryCpu.into());
    }
    runtime_config.enhanced_memory_strategy = enhanced_memory_strategy.clone();

    // Extract channels parameter if provided (overrides config default of 2)
    let channels_override = if params::has_param(&validated_params, "channels") {
        Some(params::get_usize(&validated_params, "channels", 2) as u32)
    } else {
        None
    };

    // The seal and its width, if given (the parser has validated them)
    let seal_override = params::has_param(&validated_params, "seal").then(|| params::get_string(&validated_params, "seal", "tmr"));
    let seal_width_override =
        params::has_param(&validated_params, "seal-width").then(|| params::get_string(&validated_params, "seal-width", "auto"));

    // Extract the mirror override if provided (the parser has validated it)
    let mirror_override = if params::has_param(&validated_params, "mirror") {
        params::get_string(&validated_params, "mirror", "whole").parse::<crate::config::MirrorMode>().ok()
    } else {
        None
    };

    // Extract pattern-mode override if provided
    let pattern_mode_override = if params::has_param(&validated_params, "pattern-mode") {
        Some(params::get_u32(&validated_params, "pattern-mode", 0))
    } else {
        None
    };

    // Mem-Sequence's passes and order, if given (the parser has checked them)
    let sequence_override = params::has_param(&validated_params, "sequence").then(|| params::get_string(&validated_params, "sequence", ""));
    let order_override = params::has_param(&validated_params, "order").then(|| params::get_string(&validated_params, "order", "forward"));

    // Extract verify-reps override if provided
    let verify_reps_override =
        if params::has_param(&validated_params, "verify-reps") { Some(params::get_u32(&validated_params, "verify-reps", 1)) } else { None };

    // Extract test-reps override if provided
    let test_reps_override =
        if params::has_param(&validated_params, "test-reps") { Some(params::get_u32(&validated_params, "test-reps", 1)) } else { None };

    // Extract write-read-cycles override if provided
    let wrc_override = if params::has_param(&validated_params, "write-read-cycles") {
        Some(params::get_u32(&validated_params, "write-read-cycles", 1))
    } else {
        None
    };

    // --startup-debug: run the startup sequence and print the test plan, but allocate no test memory and run no test
    let plan_only = args.iter().any(|a| a == "--startup-debug");

    let start_time = std::time::Instant::now();

    // Unified test execution path - works for bandwidth tests, latency tests, or mixed test_filter may be set by --ram-latency or
    // --cache-latency to run specific test subsets test_filter_param may be set by test=Pattern for CLI filtering
    let final_test_filter = test_filter.as_deref().or(test_filter_param.as_deref());

    let plan = crate::plans::resolve(plan_param, config_opt.is_some(), final_test_filter.is_some())?;
    if let Some(plan) = plan {
        fe.event(Setup::Plan { name: plan.name, about: plan.about, is_default: plan_param.is_none() }.into());
    }

    let status = run_tests_with_layout_and_timing_filtered(
        crate::runner::RunSetup {
            layout: enhanced_layout,
            error_mode,
            suite_timing,
            runtime_config,
            config: config_opt.as_ref(),
            overrides: crate::runner::TestRunOverrides {
                single_test_filter: final_test_filter,
                plan,
                seal_override: seal_override.as_deref(),
                seal_width_override: seal_width_override.as_deref(),
                mirror_override,
                pattern_mode_override,
                sequence_override: sequence_override.as_deref(),
                order_override: order_override.as_deref(),
                verify_reps_override,
                test_reps_override,
                wrc_override,
                channels_override,
                plan_only,
                command_line: &command_line,
            },
            cache_info,
        },
        &fe,
    );

    Ok(SessionEnd::Ran { status, elapsed: start_time.elapsed(), plan_only })
}

/// Why `args` can't be run, if they give `--yes` or `--no-save` to anything but a calibration (`commands::Command::Calibrate` takes them
/// first): they answer its save question, which nothing else asks.
fn calibration_flag_without_calibration(args: &[String]) -> Option<String> {
    args.iter().find(|a| *a == "--yes" || *a == "--no-save").map(|flag| {
        format!(
            "{flag} answers a calibration's save question: give it with --calibrate-cache, --calibrate-cache-ext, \
         --calibrate-seal or --calibrate-all"
        )
    })
}

/// The seal's width under `auto`, at startup (TODO 99): this machine's seal calibration (`AppConfig::valid_seal_calibration`), whose width
/// `set` makes the one `auto` resolves to (`seal::set_calibrated_width` in a run), else none, so the rule; and which it was.
fn load_seal_calibration(
    app_config: &crate::app_config::AppConfig, system_info: &crate::SystemInfo, smbios: &crate::smbios::SmbiosData,
    set: impl FnOnce(crate::seal::SealWidth),
) -> SealCalibrationState {
    let seal = app_config.valid_seal_calibration(system_info, smbios).and_then(|s| s.width().map(|w| (s, w)));
    match (seal, &app_config.seal_calibration) {
        (Some((seal, width)), _) => {
            set(width);
            SealCalibrationState::Loaded { bits: seal.width_bits, at: seal.timestamp }
        }
        (None, Some(_)) => SealCalibrationState::Stale,
        (None, None) => SealCalibrationState::None,
    }
}

/// Parsed configuration bundle produced from validated CLI parameters:
/// `(memory_strategy, error_mode, suite_timing, cputype, cpus, pinning, alloc)`.
type ParsedRunConfig = (EnhancedMemoryStrategy, ErrorMode, TestSuiteTiming, String, String, CpuPinningConfig, MemoryAllocationConfig);

/// Build configuration from validated parameters (using centralized registry). What the allocation and pinning parameters set goes to `fe`,
/// once they are read.
fn build_config_from_validated_params(
    validated: &HashMap<String, params::ParamValue>, fe: &dyn Frontend,
) -> Result<ParsedRunConfig, String> {
    // Extract basic string parameters with defaults
    let cputype = params::get_string(validated, "cputype", "threads");
    let cpus = params::get_string(validated, "cpus", "100%");

    // Build error mode
    let error_mode = match params::get_string(validated, "errors", "log").to_lowercase().as_str() {
        "halt" | "stop" => ErrorMode::Halt,
        "panic" | "debug" => ErrorMode::Panic,
        _ => ErrorMode::Log,
    };

    // Build test suite timing
    let suite_timing = if params::has_param(validated, "cycles") {
        let cycles = params::get_u32(validated, "cycles", 3);
        TestSuiteTiming::cycles_only(cycles)
    } else if params::has_param(validated, "duration") {
        let duration = params::get_u32(validated, "duration", 0);
        TestSuiteTiming::duration_only(duration)
    } else {
        TestSuiteTiming::default()
    };

    // Build memory strategy (requires special parsing)
    let enhanced_memory_strategy = if let Some(params::ParamValue::String(memory_str)) = validated.get("memory") {
        parse_enhanced_memory_parameter(memory_str)?
    } else {
        EnhancedMemoryStrategy::default()
    };

    // Build CPU pinning config. skip_spec/stride_spec are the two-stage selection specs; cpus_to_skip is resolved from skip_spec later
    // (needs the real core count).
    let mut pinning_config = CpuPinningConfig {
        skip_spec: params::get_string(validated, "skip-cores", "1"),
        stride_spec: params::get_string(validated, "cpu-stride", "1"),
        ..Default::default()
    };
    if params::get_bool(validated, "--disable-pinning", false) {
        pinning_config.enable_pinning = false;
    }

    // Build memory allocation config
    let mut alloc_config = MemoryAllocationConfig::default();
    let mut notes = Vec::new();

    // Handle page size overrides
    if let Some(params::ParamValue::String(page_str)) = validated.get("minpage") {
        alloc_config.min_page_size = page_str.to_string();
        notes.push(ParamNote::MinPage(page_str.to_string()));
    }
    if let Some(params::ParamValue::String(page_str)) = validated.get("maxpage") {
        alloc_config.max_page_size = page_str.to_string();
        notes.push(ParamNote::MaxPage(page_str.to_string()));
    }
    for key in MemoryAllocationConfig::BLOCK_PARAMS {
        if let Some(params::ParamValue::String(v)) = validated.get(key) {
            alloc_config.set_block_param(key, v);
            notes.push(ParamNote::Block(key, v.to_string()));
        }
    }

    // Handle topology (has side effect of setting global state)
    if let Some(params::ParamValue::String(topology_str)) = validated.get("topology") {
        use crate::cpu_topology::{TopologyDetectionMethod, set_topology_detection_method};
        // Validated by the parameter's parser
        let method = TopologyDetectionMethod::parse(topology_str).unwrap_or(TopologyDetectionMethod::Auto);
        set_topology_detection_method(method);
        notes.push(ParamNote::Topology(method.name()));
    }

    // Note skip-cores if specified
    if params::has_param(validated, "skip-cores") {
        notes.push(ParamNote::SkipCores(pinning_config.cpus_to_skip));
    }
    if !pinning_config.enable_pinning {
        notes.push(ParamNote::PinningOff);
    }
    if !notes.is_empty() {
        fe.event(Setup::ParamNotes(notes).into());
    }

    Ok((enhanced_memory_strategy, error_mode, suite_timing, cputype, cpus, pinning_config, alloc_config))
}

/// Parse enhanced memory parameter with clear reserve semantics
fn parse_enhanced_memory_parameter(param: &str) -> Result<crate::memory::allocation_strategy::EnhancedMemoryStrategy, String> {
    use crate::memory::allocation_strategy::{AllocationMode, EnhancedMemoryStrategy};

    Ok(EnhancedMemoryStrategy { allocation_mode: AllocationMode::parse(param)? })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `--yes` and `--no-save` go with a calibration, which takes them before this check; a run that gives either is stopped with what they
    /// are for.
    #[test]
    fn the_save_flags_go_only_with_a_calibration() {
        let args = |line: &[&str]| line.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(calibration_flag_without_calibration(&args(&["tmr", "memory=50%"])), None);
        for flag in ["--yes", "--no-save"] {
            let e = calibration_flag_without_calibration(&args(&["tmr", "memory=50%", flag])).expect(flag);
            assert!(e.starts_with(flag) && e.contains("--calibrate-all"), "{e}");
        }
        assert!(matches!(Command::parse(&args(&["tmr", "--calibrate-seal", "--yes"])), Some(Command::Calibrate(_))));
    }

    /// TODO 99: at startup, a seal calibration measured on this machine gives `auto` its width (`set`, `seal::set_calibrated_width` in a
    /// run), which the run's seal then resolves to, and is Loaded; one from other hardware gives it none and is Stale; with none, None. The
    /// line each shows is the CLI's (`render::setup`, crates/tmr-cli).
    #[test]
    fn startup_gives_auto_this_machines_seal_calibration() {
        use crate::seal::{SealWidth, SealWidthSource, TestCalibratedWidth};
        let (system_info, smbios) = (crate::tests::get_system_info(), crate::smbios::get_smbios());
        let load = |config: &crate::app_config::AppConfig| {
            let mut set = Vec::new();
            let state = load_seal_calibration(config, system_info, smbios, |width| set.push(width));
            (state, set)
        };
        let mut config = crate::app_config::AppConfig::default();
        config.set_seal_calibration(crate::calibration::test_seal_calibration(128), system_info, smbios);
        let at = config.seal_calibration.as_ref().unwrap().timestamp;
        let (state, set) = load(&config);
        assert_eq!(state, SealCalibrationState::Loaded { bits: 128, at });
        assert_eq!(set, [SealWidth::W128]);
        let _calibrated = TestCalibratedWidth::set(set.first().copied());
        assert_eq!(SealWidth::Auto.resolve_from("seal-width="), Ok((SealWidth::W128, SealWidthSource::Calibrated)));

        config.seal_calibration.as_mut().unwrap().fingerprint.machine_id = Some("another machine".to_string());
        assert_eq!(load(&config), (SealCalibrationState::Stale, vec![]));
        config.seal_calibration = None;
        assert_eq!(load(&config), (SealCalibrationState::None, vec![]));
    }
}
