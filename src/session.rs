//! A session (TODO 29): a command line's setup, then its run, shown through a frontend. This is what the CLI did before the
//! engine/presentation split, with an event (`frontend::Setup`) where it printed and an end (`SessionEnd`) where it exited, so the CLI and
//! the window take the same parameters and the console shows what it did. The question it asks about large pages (a new sign-in, the right
//! granted first when TMR can grant it, or 4 KiB pages or quit when it can't) is the frontend's to answer (`Frontend::ask_large_pages`).
//!
//! Sessions one after another in a process (TODO 109: the window's Run again), `reset_run` readying it for the next. Within a session the
//! run settings it sets (the seed, the width, the topology's typing) are first-set-wins, so the command line's hold over a config's.

use std::collections::HashMap;
use std::env;
use std::sync::Arc;
use std::time::Duration;

use crate::commands::Command;
use crate::config::{CpuPinningConfig, MemoryAllocationConfig};
use crate::cpu_topology::{get_cpu_topology, is_hybrid_cpu};
use crate::frontend::{
    CacheCalibrationState, Candidate, Frontend, LargePageAnswer, LargePageQuestion, ParamNote, SealCalibrationState, Selection, Setup,
};
use crate::memory::allocation_strategy::EnhancedMemoryStrategy;
use crate::runner::{RunStatus, TestSuiteTiming, ThreadPriority, detect_runtime_capabilities, run_tests_with_layout_and_timing_filtered};
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
    /// The user chose to sign out and back in (or restart Windows) for large pages (`LargePageAnswer::SignOut`), or the right was granted
    /// to their pick (`LargePageAnswer::Grant`), which a new sign-in brings: nothing ran
    SignOutChosen,
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
    /// The CPUs asked for can't be selected (`cpu_selection::choose`)
    CpuSelection(String),
    /// This sign-in lacks the right large pages need, the run's maxpage= allows them, and nothing said to take 4 KiB pages this time
    /// (TODO 113, the owner's rule): the user quit at the large-page question (`LargePageAnswer::Quit`), gave it no answer, or nobody
    /// could answer it, or the grant they picked failed (`Setup::LargePageGrant` said why). Nothing was allocated.
    NeedsLargePages,
}

/// Readies the process for another session (TODO 109): the run settings the last one set (the seed, the width, the bus beat, the topology's
/// typing, rescan, `patterns=`, the calibrations, the threads a core), its start stamp, which names its log and results files, and its run
/// ID, which both carry, and the stop flag, back to a new process's. A frontend running sessions one after another calls it before each,
/// and before the run's log file opens, which takes the stamp; the console runs one a process. Only between sessions: the last one's thread
/// has ended.
pub fn reset_run() {
    crate::pattern::reset_run_settings();
    crate::simd::reset_width();
    crate::seal::reset_calibrated_width();
    crate::cpu_topology::reset_typing();
    crate::tests::reset_run_settings();
    crate::run_context::reset_run_stamp();
    crate::runner::clear_stop();
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
        "--compare-results", "--show-results", "--debug-topology", "--show-topology", "--setup-large-pages", "--remove-large-pages",
        "--startup-debug", "--disable-pinning", "--no-serials", "--help", "-h", "-?", "--version", "-v",
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
            build_config_from_validated_params(&validated_params, fe.as_ref());
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
                        enhanced_memory_strategy = parse_enhanced_memory_parameter(memory_str);
                        log::debug!("CLI override: enhanced_memory_strategy");
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
                    // Resolved later against the cores (`cpu_selection::choose`)
                    pinning_config.skip_spec = params::get_string(&validated_params, "skip-cores", "1");
                    log::debug!("CLI override: skip_spec = {}", pinning_config.skip_spec);
                }
                "cpu-stride" => {
                    pinning_config.stride_spec = Some(params::get_string(&validated_params, "cpu-stride", "1"));
                    log::debug!("CLI override: stride_spec = {:?}", pinning_config.stride_spec);
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
    // As the loggers read it (TODO 105): its value, or what the console and the log file take without it
    let levels = crate::logfile::levels();
    let rust_log = levels.rust_log.clone().unwrap_or_else(|| {
        format!(
            "not set: the console shows {}, the log file takes {}",
            levels.shown.as_str().to_lowercase(),
            levels.file.as_str().to_lowercase()
        )
    });

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

    // The CPUs the threads take (TODO 108, `cpu_selection`): the cores ranked by Windows' priority within each L3 group, the groups in
    // turn; skip-cores= leaving out the end of that order (or a range of core numbers), cpus= taking from the front of the rest, or by
    // core number at cpu-stride='s spacing. Selected even when pinning is off: the list also decides which NUMA node each thread's
    // memory comes from, so a pinned/unpinned A/B differs in scheduling alone. The pool skips only the affinity call
    // (`RuntimeConfig.pin_threads`).
    let avoid_smt = cputype == "cores";
    fe.event(Setup::CpuType { cputype: cputype.clone(), avoid_smt }.into());
    let skip = match crate::cpu_selection::SkipSpec::parse(&pinning_config.skip_spec) {
        Ok(skip) => skip,
        Err(e) => return Ok(SessionEnd::Stopped(Stopped::Invalid(e))),
    };
    let request = crate::cpu_selection::Request {
        skip,
        cpus: &cpus,
        stride: pinning_config.stride_spec.as_deref(),
        count_cores: avoid_smt,
        one_per_core: pinning_config.avoid_smt_doubling || avoid_smt,
    };
    let topology = get_cpu_topology();
    let cores = crate::cpu_selection::cores(topology);
    let choice = match crate::cpu_selection::choose(&cores, &request) {
        Ok(choice) => choice,
        // Never silently right-size the request: tell the user and stop
        Err(e) => return Ok(SessionEnd::Stopped(Stopped::CpuSelection(e))),
    };
    let (order, skipped) = (&choice.order, &choice.skipped);
    match request.stride {
        None => log::info!("Core order: {order:?}, the threads from the front; skip-cores={skip} leaves out {skipped:?}"),
        Some(stride) => {
            log::info!("Core order: {order:?}; skip-cores={skip} leaves out {skipped:?}, cpu-stride={stride} takes the rest by core number")
        }
    }
    // `cpus=` is relative to the cores left: any value <= 100% is always valid, which keeps configs portable across 4/6/8/16-core
    // machines. The relative math made explicit, so `cpus=50%` after a skip isn't a surprise.
    if let Some(percent) = choice.percent {
        fe.event(Setup::CpuCount { percent, available: choice.available, threads: choice.threads }.into());
    }
    let cpu_list = choice.cpu_list.clone();
    let actual_threads = cpu_list.len();

    // The workers' priority, and the window's thread's, which the selection carries to the window
    let (thread_priority, window_priority) =
        priorities(&validated_params, config_opt.as_ref().and_then(|c| c.system.thread_priority.as_deref()))?;

    // Detect SIMD capabilities
    let simd_caps = crate::detect_simd_capabilities();
    fe.event(
        Setup::Selection(Box::new(Selection {
            error_mode,
            threads: actual_threads,
            available: choice.available,
            cputype: cputype.clone(),
            cpu_list: cpu_list.clone(),
            pinned: pinning_config.enable_pinning,
            skip,
            skipped: choice.skipped.clone(),
            avoid_smt_doubling: pinning_config.avoid_smt_doubling,
            cycles: suite_timing.global_cycles,
            duration_secs: suite_timing.global_duration_secs,
            simd: simd_caps.clone(),
            window_priority,
        }))
        .into(),
    );

    fe.event(Setup::LargePageCheck.into());

    // Without the privilege, a run whose maxpage= allows large pages (the default) is asked first (TODO 113, the owner's rule): it takes
    // 4 KiB pages only when told to, or with minpage=regular maxpage=regular, which asks nothing
    use crate::memory::allocator::PageSizeLevel;
    let wants_large = PageSizeLevel::from_config_str(&alloc_config.max_page_size) > PageSizeLevel::Regular;
    match settle_large_pages(&*fe, crate::memory::large_page_question(wants_large), crate::memory::grant_large_pages) {
        LargePages::Unasked => fe.event(Setup::LargePageDiagnostic { report: crate::memory::diagnose_large_pages() }.into()),
        LargePages::SignOut => return Ok(SessionEnd::SignOutChosen),
        // 4 KiB pages this time, both ends of the range (decision 5a): the default minpage needs large pages (TODO 88)
        LargePages::Regular => {
            alloc_config.min_page_size = "regular".to_string();
            alloc_config.max_page_size = "regular".to_string();
        }
        // Before anything is allocated: the question's answer, or the grant's failure, said why
        LargePages::Quit => return Ok(SessionEnd::Stopped(Stopped::NeedsLargePages)),
    }

    let usable = crate::check_large_page_privilege().is_ok();
    // Without the privilege, a large minpage refuses the run at allocation
    let min_needs_large = PageSizeLevel::from_config_str(&alloc_config.min_page_size) > PageSizeLevel::Regular;
    fe.event(Setup::LargePageStatus { usable, min_page: alloc_config.min_page_size.clone(), min_needs_large }.into());

    // Serials out of what the run shows and keeps (`--no-serials`), before SMBIOS is first read and logged (TODO 105)
    crate::smbios::hide_serials(args.iter().any(|a| a == "--no-serials"));

    // Get system info for later use (formatted output comes via reporter below)
    let system_info = crate::tests::get_system_info();
    let cache_info = system_info.get_cache_info();

    // The threads that share a core's caches: the most the selection put on one core, so cputype=threads with one thread a core
    // (cpus=50% on 2-way SMT) doesn't halve the cache-sized extents (TODO 108)
    crate::set_active_threads_per_core(choice.threads_per_core);

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
        crate::cpu_selection::startup_topology(&cores, &request, &choice, pinning_config.enable_pinning, is_hybrid_cpu(topology));
    fe.event(Setup::CpuTopology { report: Box::new(report), smt_p_cores, e_cores_skipped_first }.into());

    // Show memory stats
    fe.event(Setup::MemoryStatus(crate::runner::memory_status()).into());

    {
        use crate::reporting::system_info_builder::build_system_info_report;

        let report = build_system_info_report(system_info, &simd_caps);
        let hypervisor =
            cache_info.is_virtual_machine.then(|| cache_info.hypervisor_name.clone().unwrap_or_else(|| "unidentified".to_string()));
        fe.event(
            Setup::SystemInfo {
                report: Box::new(report),
                memory_modules: crate::smbios::shown_modules(&smbios.memory_modules),
                system: crate::smbios::shown_system(&smbios.system),
                bios: smbios.bios.clone(),
                hypervisor,
            }
            .into(),
        );
    }

    // Calculate memory layout using enhanced system
    // Checks the block sizes too, so a bad one stops the run before anything is allocated
    let share_rounding = alloc_config.share_rounding()?;
    let enhanced_layout = enhanced_memory_strategy.create_layout(actual_threads, share_rounding)?;

    fe.event(Setup::Starting.into());

    let mut runtime_config = detect_runtime_capabilities(&alloc_config);
    runtime_config.cpu_list = Some(cpu_list);
    runtime_config.pin_threads = pinning_config.enable_pinning;

    runtime_config.thread_priority = thread_priority;
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

/// The workers' priority (TODO 77), `priority=` over the config's `thread_priority` (`config`), else High; and the window's thread's during
/// a run (`window-priority=`, the owner's, 2026-11-02), else the workers'. Their parsers have checked them, and the config's
/// `validate_system`.
fn priorities(validated: &HashMap<String, params::ParamValue>, config: Option<&str>) -> Result<(ThreadPriority, ThreadPriority), String> {
    let given = |key| params::has_param(validated, key).then(|| params::get_string(validated, key, ""));
    let workers = match given("priority").as_deref().or(config) {
        Some(priority) => priority.parse()?,
        None => ThreadPriority::default(),
    };
    let window = match given("window-priority") {
        Some(priority) => priority.parse()?,
        None => workers,
    };
    Ok((workers, window))
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
fn build_config_from_validated_params(validated: &HashMap<String, params::ParamValue>, fe: &dyn Frontend) -> ParsedRunConfig {
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
        parse_enhanced_memory_parameter(memory_str)
    } else {
        EnhancedMemoryStrategy::default()
    };

    // Build CPU pinning config: skip_spec/stride_spec, which the selection resolves against the cores (`cpu_selection::choose`)
    let mut pinning_config = CpuPinningConfig {
        skip_spec: params::get_string(validated, "skip-cores", "1"),
        stride_spec: params::has_param(validated, "cpu-stride").then(|| params::get_string(validated, "cpu-stride", "1")),
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

    // Note skip-cores if specified: as given, which the selection resolves later (TODO 108: the count it noted was the default's)
    if params::has_param(validated, "skip-cores") {
        notes.push(ParamNote::SkipCores(pinning_config.skip_spec.clone()));
    }
    if !pinning_config.enable_pinning {
        notes.push(ParamNote::PinningOff);
    }
    if !notes.is_empty() {
        fe.event(Setup::ParamNotes(notes).into());
    }

    (enhanced_memory_strategy, error_mode, suite_timing, cputype, cpus, pinning_config, alloc_config)
}

/// What the large-page question decided (`settle_large_pages`).
#[derive(Debug, PartialEq, Eq)]
enum LargePages {
    /// There was none to ask: the diagnostic says how they stand
    Unasked,
    /// End the session, for a new sign-in: the right is in the policy, from before or granted now
    SignOut,
    /// This run on 4 KiB pages, as the user said, the right left as it is
    Regular,
    /// End the session before allocating: the user quit, or didn't answer, or the grant they picked failed (`Setup::LargePageGrant` says
    /// why), and nothing runs on 4 KiB pages unasked
    Quit,
}

/// Puts `question`, the large-page check's (`memory::large_page_question`), to `fe`, and says what the answer decides, from the session's
/// own copy of the question, so a frontend can't change whom a grant goes to. `grant` is called, with the candidate picked, only when the
/// question offered the grant and the answer picked one of its candidates; its outcome goes to `fe`, and a failed one quits. An answer
/// that isn't the question's (a grant to the sign-in's question, an index past the list) changes nothing and quits. The owner's rule on
/// large pages (2026-09-14, and TODO 113's decision 5a): pause and warn, the user decides on 4 KiB pages, nothing changes the system
/// unasked, and nothing runs on 4 KiB pages unasked.
fn settle_large_pages(
    fe: &dyn Frontend, question: Option<LargePageQuestion>, grant: impl FnOnce(&Candidate) -> Result<(), String>,
) -> LargePages {
    let Some(question) = question else {
        return LargePages::Unasked;
    };
    match (fe.ask_large_pages(&question), question) {
        (LargePageAnswer::Regular, _) => LargePages::Regular,
        (LargePageAnswer::Quit, _) => LargePages::Quit,
        (LargePageAnswer::SignOut, LargePageQuestion::SignIn { .. }) => LargePages::SignOut,
        (LargePageAnswer::Grant(i), LargePageQuestion::Grant { mut candidates, .. }) if i < candidates.len() => {
            let candidate = candidates.swap_remove(i);
            let outcome = grant(&candidate);
            let granted = outcome.is_ok();
            fe.event(Setup::LargePageGrant { account: candidate.name, outcome }.into());
            if granted { LargePages::SignOut } else { LargePages::Quit }
        }
        (answer, question) => {
            log::warn!("The large-page answer {answer:?} isn't one to {question:?}: TMR quits, granting nothing and running nothing");
            LargePages::Quit
        }
    }
}

fn parse_enhanced_memory_parameter(param: &str) -> EnhancedMemoryStrategy {
    let allocation_mode = crate::memory::allocation_strategy::AllocationMode::parse(param).expect("memory= is checked when it is parsed");
    EnhancedMemoryStrategy { allocation_mode }
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

    /// The workers' priority is `priority=`'s, else the config's, else High, and the window's is `window-priority=`'s, else the workers'
    /// (the owner, 2026-11-02): given, from `priority=`, from the config, and from neither
    #[test]
    fn the_window_takes_the_workers_priority_unless_given_its_own() {
        use ThreadPriority::{High, Normal, Realtime};
        let resolve = |line: &[&str], config: Option<&str>| {
            let args: Vec<String> = line.iter().map(|s| s.to_string()).collect();
            priorities(&params::parse_and_validate_args(&args).unwrap(), config).unwrap()
        };
        assert_eq!(resolve(&[], None), (High, High), "neither");
        assert_eq!(resolve(&["window-priority=normal"], None), (High, Normal), "given");
        assert_eq!(resolve(&["window-priority=Realtime", "priority=normal"], Some("high")), (Normal, Realtime), "given, over the workers'");
        assert_eq!(resolve(&["priority=realtime"], None), (Realtime, Realtime), "from priority=");
        assert_eq!(resolve(&["priority=normal"], Some("realtime")), (Normal, Normal), "from priority=, over the config's");
        assert_eq!(resolve(&[], Some("normal")), (Normal, Normal), "from the config");
        assert_eq!(resolve(&["window-priority=high"], Some("normal")), (Normal, High), "given, with the config's workers'");
    }

    /// Bug 4 (TODO 108): a given skip-cores= is noted as given (the note showed the count before it was resolved, the default's: "Skipping
    /// first 1 CPU(s)" for skip-cores=0), and the selection resolves it. cpu-stride= is the selection's only when given: without it the
    /// ranked order stands.
    #[test]
    fn skip_cores_is_noted_as_given_and_a_stride_only_when_given() {
        use std::sync::Mutex;

        use crate::frontend::Event;

        /// Keeps the skip-cores= notes it was shown
        struct Notes(Mutex<Vec<String>>);
        impl Frontend for Notes {
            fn event(&self, event: Event) {
                if let Event::Setup(Setup::ParamNotes(notes)) = event {
                    let skips = notes.into_iter().filter_map(|note| if let ParamNote::SkipCores(spec) = note { Some(spec) } else { None });
                    self.0.lock().unwrap().extend(skips);
                }
            }
            fn ask_large_pages(&self, _: &LargePageQuestion) -> LargePageAnswer {
                LargePageAnswer::Quit
            }
        }
        let pinning = |line: &[&str]| {
            let args: Vec<String> = line.iter().map(|s| s.to_string()).collect();
            let fe = Notes(Mutex::default());
            let (.., pinning, _) = build_config_from_validated_params(&params::parse_and_validate_args(&args).unwrap(), &fe);
            (pinning.skip_spec, pinning.stride_spec, fe.0.into_inner().unwrap())
        };
        for spec in ["0", "25%", "0-7"] {
            assert_eq!(pinning(&[&format!("skip-cores={spec}")]), (spec.to_string(), None, vec![spec.to_string()]));
        }
        assert_eq!(pinning(&[]), ("1".to_string(), None, vec![]), "the default, not noted");
        assert_eq!(pinning(&["cpu-stride=1"]).1.as_deref(), Some("1"), "given, by core number");
    }

    /// The large-page question (TODO 113): none asked when the check has none; the sign-in's yes ends the session, granting nothing; the
    /// grant made only to the candidate picked, from the session's copy of the question, its outcome shown, a failed one quitting; each
    /// question's 4 KiB pages taken only when answered so, and its quit quitting; an answer that isn't the question's (a grant to the
    /// sign-in's, an index past the list, a sign-out to the grant's or the administrator's) changes nothing and quits (decision 5a: nothing
    /// runs on 4 KiB pages unasked). The grant here is a stand-in: no test touches the security policy.
    #[test]
    fn large_pages_are_granted_only_to_the_candidate_picked() {
        use std::sync::Mutex;

        use crate::frontend::{Event, Reach, Sid};

        /// Gives `answer`, and keeps the questions and the grants' outcomes it was shown
        struct Answers {
            answer: LargePageAnswer,
            asked: Mutex<Vec<LargePageQuestion>>,
            shown: Mutex<Vec<(String, Result<(), String>)>>,
        }
        impl Frontend for Answers {
            fn event(&self, event: Event) {
                let Event::Setup(Setup::LargePageGrant { account, outcome }) = event else {
                    panic!("the question sends only the grant's outcome");
                };
                self.shown.lock().unwrap().push((account, outcome));
            }
            fn ask_large_pages(&self, question: &LargePageQuestion) -> LargePageAnswer {
                self.asked.lock().unwrap().push(question.clone());
                self.answer
            }
        }
        // What the question settles to, the questions put, the grants made (the stand-in's calls, by SID) and the outcomes shown
        let settle = |question: &LargePageQuestion, answer, outcome: Result<(), String>| {
            let fe = Answers { answer, asked: Mutex::default(), shown: Mutex::default() };
            let mut granted = Vec::new();
            let decided = settle_large_pages(&fe, Some(question.clone()), |candidate| {
                granted.push(candidate.sid().to_string());
                outcome
            });
            assert_eq!(fe.asked.into_inner().unwrap(), std::slice::from_ref(question), "asked once, the session's question");
            (decided, granted, fe.shown.into_inner().unwrap())
        };
        let candidate = |name: &str, reach, sid: &str| Candidate::new(name, reach, sid.parse::<Sid>().unwrap());
        let candidates = vec![
            candidate(r"PC\owner", Reach::You, "S-1-5-21-1-2-3-1001"),
            candidate(r"BUILTIN\Administrators", Reach::Group, "S-1-5-32-544"),
            candidate("Everyone", Reach::Automatic, "S-1-1-0"),
        ];
        let grant = LargePageQuestion::Grant { candidates, uac: false, domain: false };
        let sign_in = LargePageQuestion::SignIn { account: r"PC\owner".to_string(), via: None, uac: false, domain: false };
        let needs = LargePageQuestion::NeedsAdministrator { account: r"PC\owner".to_string(), uac: true };
        use LargePageAnswer::{Quit, Regular, SignOut};

        let fe = Answers { answer: SignOut, asked: Mutex::default(), shown: Mutex::default() };
        assert_eq!(settle_large_pages(&fe, None, |_| panic!("granted")), LargePages::Unasked, "nothing to ask");
        assert!(fe.asked.into_inner().unwrap().is_empty());

        assert_eq!(settle(&sign_in, SignOut, Ok(())), (LargePages::SignOut, vec![], vec![]), "the sign-in's yes");
        assert_eq!(settle(&sign_in, Regular, Ok(())), (LargePages::Regular, vec![], vec![]));
        assert_eq!(settle(&sign_in, LargePageAnswer::Grant(0), Ok(())), (LargePages::Quit, vec![], vec![]), "no grant offered");
        assert_eq!(settle(&sign_in, Quit, Ok(())), (LargePages::Quit, vec![], vec![]), "no answer, or quit");
        assert_eq!(
            settle(&grant, LargePageAnswer::Grant(0), Ok(())),
            (LargePages::SignOut, vec!["S-1-5-21-1-2-3-1001".to_string()], vec![(r"PC\owner".to_string(), Ok(()))]),
            "granted to the account"
        );
        assert_eq!(
            settle(&grant, LargePageAnswer::Grant(2), Ok(())),
            (LargePages::SignOut, vec!["S-1-1-0".to_string()], vec![("Everyone".to_string(), Ok(()))]),
            "granted to the candidate picked"
        );
        assert_eq!(settle(&grant, LargePageAnswer::Grant(9), Ok(())), (LargePages::Quit, vec![], vec![]), "past the list");
        assert_eq!(settle(&grant, SignOut, Ok(())), (LargePages::Quit, vec![], vec![]), "a sign-out, nothing picked");
        assert_eq!(settle(&grant, Regular, Ok(())), (LargePages::Regular, vec![], vec![]), "4 KiB pages grant nothing");
        assert_eq!(settle(&grant, Quit, Ok(())), (LargePages::Quit, vec![], vec![]), "quit, granting nothing");
        let refused = Err("LsaAddAccountRights: Access is denied. (5, NTSTATUS 0xC0000022)".to_string());
        assert_eq!(
            settle(&grant, LargePageAnswer::Grant(1), refused.clone()),
            (LargePages::Quit, vec!["S-1-5-32-544".to_string()], vec![(r"BUILTIN\Administrators".to_string(), refused)]),
            "the grant failed: nothing runs on 4 KiB pages unasked"
        );
        assert_eq!(settle(&needs, Quit, Ok(())), (LargePages::Quit, vec![], vec![]), "quit, to run as administrator");
        assert_eq!(settle(&needs, Regular, Ok(())), (LargePages::Regular, vec![], vec![]), "4 KiB pages this time");
        assert_eq!(settle(&needs, SignOut, Ok(())), (LargePages::Quit, vec![], vec![]), "not its answer");
        assert_eq!(settle(&needs, LargePageAnswer::Grant(0), Ok(())), (LargePages::Quit, vec![], vec![]), "no grant offered");
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
