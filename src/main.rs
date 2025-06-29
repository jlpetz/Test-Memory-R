use std::env;
use tmr::{create_demo_configs, load_config, ErrorMode, MemoryStrategy, AllocationMode, WindowMode, BlockMode};
use tmr::layout::MemoryLayout;
use tmr::runner::{run_tests_with_layout_and_timing, TestSuiteTiming};

fn main() {
    // Initialize logging
    env_logger::Builder::from_default_env().filter_level(log::LevelFilter::Info).init();

    println!("🚀 Test Memory R (TMR) v1.0.0 - High-Performance Memory Testing Tool");
    println!("================================================================================");

    let args: Vec<String> = env::args().collect();

    // Check for special commands
    if args.len() > 1 {
        match args[1].as_str() {
            "--create-demo-configs" => {
                if let Err(e) = create_demo_configs() {
                    println!("❌ Failed to create demo configs: {}", e);
                    return;
                }
                return;
            }
            "--help" | "-h" => {
                print_help(&args[0]);
                return;
            }
            "--version" | "-v" => {
                println!("Test Memory R (TMR) version 1.0.0");
                println!("High-Performance Memory Testing Tool with TM5 Compatibility");
                println!("Three-Stage Memory Architecture with Comprehensive Testing");
                return;
            }
            _ => {}
        }
    }

    // Check for config file parameter
    let config_file = args.iter().find(|arg| arg.starts_with("config=")).map(|arg| &arg[7..]);

    let (memory_strategy, error_mode, suite_timing, cputype, cpus) = if let Some(config_path) = config_file {
        match load_config(config_path) {
            Ok(config) => {
                println!("✅ Loaded configuration: {}", config.metadata.name);
                println!(
                    "   Format Version: {} | Application: {}",
                    config.config_format_version, config.application_name
                );
                println!(
                    "   Author: {} | Tested with: TMR v{}",
                    config.metadata.author, config.metadata.tested_with_version
                );
                if let Some(desc) = &config.metadata.description {
                    println!("   Description: {}", desc);
                }
                
                // Show test configuration summary
                let enabled_tests: Vec<_> = config.test_sequence.iter().filter(|t| t.enabled).collect();
                println!("   Tests: {} enabled tests with three-stage memory configuration", enabled_tests.len());
                
                // Show timing configuration
                if let Some(cycles) = config.system.timing.global_cycles {
                    print!("   Global Timing: {} cycles", cycles);
                    if let Some(duration) = config.system.timing.global_duration_secs {
                        println!(" (max {}s)", duration);
                    } else {
                        println!(" (no time limit)");
                    }
                } else if let Some(duration) = config.system.timing.global_duration_secs {
                    println!("   Global Timing: {}s duration (unlimited cycles)", duration);
                } else {
                    println!("   Global Timing: Unlimited cycles and duration");
                }
                
                // Show any per-test timing or window overrides
                let mut has_overrides = false;
                for test in &enabled_tests {
                    let mut override_parts = Vec::new();
                    
                    if test.cycles.is_some() || test.duration_secs.is_some() {
                        let timing_str = match (test.cycles, test.duration_secs) {
                            (Some(c), Some(d)) => format!("{}cycles/{}s", c, d),
                            (Some(c), None) => format!("{}cycles", c),
                            (None, Some(d)) => format!("{}s", d),
                            _ => String::new(),
                        };
                        if !timing_str.is_empty() {
                            override_parts.push(format!("Timing {}", timing_str));
                        }
                    }
                    
                    if test.window_size_mb.is_some() || test.window_mode.is_some() {
                        if let Some(size) = test.window_size_mb {
                            override_parts.push(format!("Window {}MB", size));
                        } else if let Some(mode) = &test.window_mode {
                            override_parts.push(format!("Window {}", mode));
                        }
                    }
                    
                    if test.block_size_mb.is_some() || test.block_mode.is_some() {
                        if let Some(size) = test.block_size_mb {
                            override_parts.push(format!("Block {}MB", size));
                        } else if let Some(mode) = &test.block_mode {
                            override_parts.push(format!("Block {}", mode));
                        }
                    }
                    
                    if test.allow_misaligned == Some(true) {
                        override_parts.push("Misaligned".to_string());
                    }
                    
                    if !override_parts.is_empty() {
                        if !has_overrides {
                            println!("   Per-test overrides detected:");
                            has_overrides = true;
                        }
                        println!("     {}: {}", test.function, override_parts.join(", "));
                    }
                }
                
                println!();

                let memory_strategy = config.to_memory_strategy();
                let error_mode = config.to_error_mode();
                let suite_timing = config.to_test_suite_timing();
                let cputype = config.system.cpu_config.cpu_type.clone();
                let cpus = format!("{}%", config.system.cpu_config.usage_percent);

                (memory_strategy, error_mode, suite_timing, cputype, cpus)
            }
            Err(e) => {
                println!("❌ Failed to load config file '{}': {}", config_path, e);
                println!("   Stopping execution. Please provide a valid config file or run without config parameter.");
                std::process::exit(1);
            }
        }
    } else {
        parse_command_line_params(&args)
    };

    // Calculate CPU/thread count
    let total_cpus = match cputype.as_str() {
        "cores" => num_cpus::get_physical(),
        _ => num_cpus::get(),
    };

    let percent = cpus.trim_end_matches('%').parse::<u32>().unwrap_or(100);
    let threads = ((total_cpus as u32 * percent) / 100).max(1).min(total_cpus as u32) as usize;

    // Display startup mode and parameters
    println!("Startup Mode & Parameters:");
    println!("  Command Line: {}", args.join(" "));

    // Check environment variables
    let rust_log = env::var("RUST_LOG").unwrap_or_else(|_| "not set".to_string());
    println!("  RUST_LOG: {}", rust_log);

    let rust_backtrace = env::var("RUST_BACKTRACE").unwrap_or_else(|_| "not set".to_string());
    if rust_backtrace != "not set" {
        println!("  RUST_BACKTRACE: {}", rust_backtrace);
    }

    // Show which parameters were explicitly set vs defaults
    let mut explicit_params = Vec::new();
    let mut default_params = Vec::new();

    for arg in &args[1..] {
        if arg.starts_with("config=") {
            explicit_params.push(arg.clone());
        } else if arg.starts_with("cputype=") {
            explicit_params.push(format!("cputype={}", cputype));
        } else if arg.starts_with("cpus=") {
            explicit_params.push(format!("cpus={}", cpus));
        } else if arg.starts_with("memory=") {
            explicit_params.push(arg.clone());
        } else if arg.starts_with("errors=") {
            explicit_params.push(arg.clone());
        } else if arg.starts_with("cycles=") {
            explicit_params.push(arg.clone());
        } else if arg.starts_with("duration=") {
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
            default_params.push("memory=10% reserve (default)".to_string());
        }
        if !args.iter().any(|a| a.starts_with("errors=")) {
            default_params.push("errors=log (default)".to_string());
        }
        if !args.iter().any(|a| a.starts_with("cycles=")) {
            default_params.push("cycles=3 (default)".to_string());
        }
    }

    if !explicit_params.is_empty() {
        println!("  Explicit Parameters: {}", explicit_params.join(", "));
    }
    if !default_params.is_empty() {
        println!("  Default Parameters: {}", default_params.join(", "));
    }
    println!();

    println!("System information and Memory Architecture detection:");
    println!("  CPU Type: {}", cputype);
    println!("  Using {}/{} {} for testing", threads, total_cpus, cputype);
    
    // Display memory strategy information
    print!("  Memory Strategy: ");
    match &memory_strategy.allocation_mode {
        AllocationMode::MaxAvailable { reserve_mb } => {
            println!("TM5-Compatible Maximum Allocation");
            println!("    Stage 1 (Allocation): Maximum available minus {} MB OS reserve", reserve_mb);
        }
        AllocationMode::PercentageReserve { reserve_percent } => {
            println!("Modern Optimal Allocation");
            println!("    Stage 1 (Allocation): Maximum available minus {:.1}% reserve", reserve_percent);
        }
        AllocationMode::FixedReserve { reserve_gib } => {
            println!("Fixed Reserve Allocation");
            println!("    Stage 1 (Allocation): Maximum available minus {:.2} GiB reserve", reserve_gib);
        }
    }
    
    match &memory_strategy.default_window_mode {
        WindowMode::FullAllocation => {
            println!("    Stage 2 (Testing Window): Full allocation per thread (maximum memory stress)");
        }
        WindowMode::FixedSize { size_mb } => {
            println!("    Stage 2 (Testing Window): {} MB total window (TM5-compatible)", size_mb);
        }
        WindowMode::CacheRelative { multiplier } => {
            println!("    Stage 2 (Testing Window): {:.1}x cache size (adaptive sizing)", multiplier);
        }
    }
    
    match &memory_strategy.default_block_mode {
        BlockMode::AutoOptimal => {
            println!("    Stage 3 (Block Size): Auto-optimized per test for SIMD and cache alignment");
        }
        BlockMode::FixedSize { size_mb } => {
            println!("    Stage 3 (Block Size): {} MB fixed blocks", size_mb);
        }
        BlockMode::WindowFraction { fraction } => {
            println!("    Stage 3 (Block Size): {:.1}% of window size per block", fraction * 100.0);
        }
    }
    
    print!("  Error Mode: ");
    match error_mode {
        ErrorMode::Log => println!("Log and continue"),
        ErrorMode::Halt => println!("Halt on first error"),
        ErrorMode::Panic => println!("Panic on error (debug mode)"),
    }
    
    // Display timing configuration
    print!("  Test Suite Timing: ");
    match (&suite_timing.global_cycles, &suite_timing.global_duration_secs) {
        (Some(cycles), Some(duration)) => println!("{} cycles or {}s max (whichever first)", cycles, duration),
        (Some(cycles), None) => println!("{} cycles (no time limit)", cycles),
        (None, Some(duration)) => println!("{}s duration (unlimited cycles)", duration),
        (None, None) => println!("Unlimited cycles and duration"),
    }

    // Check large page privilege early
    match tmr::check_large_page_privilege() {
        Ok(()) => println!("  Large Pages: ✅ Available (SeLockMemoryPrivilege enabled)"),
        Err(msg) => {
            println!("  Large Pages: ⚠️  Not available - {}", msg);
            println!("    To enable: Run as Administrator OR enable 'Lock pages in memory' in Group Policy");
            println!("    Impact: Will use standard 4KB pages instead of 2MB pages");
        }
    }

    // Detect SIMD capabilities
    let simd_caps = tmr::detect_simd_capabilities();
    println!("  SIMD Support: {}", simd_caps);

    println!();

    // Detect and display system architecture (includes CPU info and cache)
    let system_info = tmr::tests::get_system_info();
    println!("System Information:");
    println!("  CPU: {} ({})", system_info.cpu_brand, system_info.cpu_vendor);
    println!("  Family: {}, Model: {}, Stepping: {}", 
              system_info.cpu_family, system_info.cpu_model, system_info.cpu_stepping);
    println!("  Cores: {} physical, {} logical{}", 
              system_info.physical_cores, 
              system_info.logical_cores,
              if system_info.has_hyperthreading { " (Hyperthreading enabled)" } else { "" });

    let cache_info = system_info.get_cache_info();
    println!("  Cache Architecture:");
    println!("    L1 Data: {:.1} KB | L1 Instruction: {:.1} KB", 
        cache_info.l1_data_cache as f64 / 1024.0,
        cache_info.l1_instruction_cache as f64 / 1024.0);
    println!("    L2: {:.1} KB | L3: {:.1} MB | Line Size: {} bytes", 
        cache_info.l2_cache as f64 / 1024.0,
        cache_info.l3_cache as f64 / (1024.0 * 1024.0),
        cache_info.cache_line_size);
    println!("    Total Cache: {:.1} MB | Detection: {}", 
        cache_info.total_cache as f64 / (1024.0 * 1024.0),
        cache_info.detection_method);
    println!("    Memory Testing: Windows configured relative to cache for optimal stress patterns");

    println!();

    // Calculate memory layout (Stage 1 allocation only)
    let layout = MemoryLayout::calculate(memory_strategy, threads);

    println!("Starting comprehensive memory tests...");
    println!("  Stage 1: Pre-allocating maximum memory per thread");
    println!("  Stage 2: Configuring testing windows per test (full allocation or optimized)");
    println!("  Stage 3: Optimizing block sizes and alignment per test");
    println!("  Critical: StuckBitTest will scan ALL allocated memory for stuck bits");
    println!("(detailed logs available with RUST_LOG=debug)");
    println!();
    
    let start_time = std::time::Instant::now();
    let success = run_tests_with_layout_and_timing(layout, error_mode, suite_timing);
    let total_time = start_time.elapsed();

    println!();
    println!("================================================================================");
    if success {
        println!("✅ All memory tests completed successfully in {}", format_duration(total_time));
        println!("   Comprehensive testing: Full memory stuck bit detection + optimized stress tests");
        println!("   Memory pressure maintained throughout testing with three-stage architecture");
    } else {
        println!("❌ Tests failed or encountered errors in {}", format_duration(total_time));
    }

    println!();
    print_usage(&args[0]);
}

fn parse_command_line_params(args: &[String]) -> (MemoryStrategy, ErrorMode, TestSuiteTiming, String, String) {
    let mut cputype = "threads".to_string();
    let mut cpus = "100%".to_string();
    let mut memory_strategy = MemoryStrategy::default(); // Modern optimal by default
    let mut error_mode = ErrorMode::Log;
    let mut suite_timing = TestSuiteTiming::default(); // 3 cycles

    // Parse arguments
    for arg in args {
        if arg.starts_with("cputype=") {
            cputype = arg[8..].to_string();
        } else if arg.starts_with("cpus=") {
            cpus = arg[5..].to_string();
        } else if arg.starts_with("memory=") {
            memory_strategy = parse_memory_parameter(&arg[7..]);
        } else if arg.starts_with("errors=") {
            error_mode = parse_error_mode(&arg[7..]);
        } else if arg.starts_with("cycles=") {
            if let Ok(cycles) = arg[7..].parse::<u32>() {
                suite_timing = TestSuiteTiming::cycles_only(cycles);
            }
        } else if arg.starts_with("duration=") {
            if let Ok(duration) = arg[9..].parse::<u32>() {
                suite_timing = TestSuiteTiming::duration_only(duration);
            }
        }
    }

    (memory_strategy, error_mode, suite_timing, cputype, cpus)
}

fn parse_memory_parameter(param: &str) -> MemoryStrategy {
    let param = param.trim();

    if param.ends_with('%') {
        let percent_str = param.trim_end_matches('%');
        match percent_str.parse::<f64>() {
            Ok(percent) if percent >= 0.0 && percent <= 95.0 => {
                MemoryStrategy {
                    allocation_mode: AllocationMode::PercentageReserve { reserve_percent: percent },
                    default_window_mode: WindowMode::FullAllocation,
                    default_block_mode: BlockMode::AutoOptimal,
                }
            }
            Ok(percent) => {
                println!("Warning: Invalid percentage {}%, using default 10%", percent);
                MemoryStrategy::default()
            }
            Err(_) => {
                println!("Warning: Could not parse percentage '{}', using default 10%", param);
                MemoryStrategy::default()
            }
        }
    } else if param.to_lowercase().ends_with("gib") {
        let gib_str = param[..param.len() - 3].trim();
        match gib_str.parse::<f64>() {
            Ok(gib) if gib >= 0.0 => {
                MemoryStrategy {
                    allocation_mode: AllocationMode::FixedReserve { reserve_gib: gib },
                    default_window_mode: WindowMode::FullAllocation,
                    default_block_mode: BlockMode::AutoOptimal,
                }
            }
            Ok(gib) => {
                println!("Warning: Invalid GiB value {}, using default 10%", gib);
                MemoryStrategy::default()
            }
            Err(_) => {
                println!("Warning: Could not parse GiB value '{}', using default 10%", param);
                MemoryStrategy::default()
            }
        }
    } else if param.to_lowercase().ends_with("mib") {
        let mib_str = param[..param.len() - 3].trim();
        match mib_str.parse::<f64>() {
            Ok(mib) if mib >= 0.0 => {
                let gib = mib / 1024.0;
                MemoryStrategy {
                    allocation_mode: AllocationMode::FixedReserve { reserve_gib: gib },
                    default_window_mode: WindowMode::FullAllocation,
                    default_block_mode: BlockMode::AutoOptimal,
                }
            }
            Ok(mib) => {
                println!("Warning: Invalid MiB value {}, using default 10%", mib);
                MemoryStrategy::default()
            }
            Err(_) => {
                println!("Warning: Could not parse MiB value '{}', using default 10%", param);
                MemoryStrategy::default()
            }
        }
    } else if param.to_lowercase() == "tm5" {
        MemoryStrategy::tm5_compatible(880, 128)
    } else {
        println!("Warning: Unknown memory parameter format '{}', using default", param);
        println!("  Supported formats: 10%, 2GiB, 1024MiB, tm5");
        MemoryStrategy::default()
    }
}

fn parse_error_mode(param: &str) -> ErrorMode {
    match param.trim().to_lowercase().as_str() {
        "log" => ErrorMode::Log,
        "halt" | "stop" => ErrorMode::Halt,
        "panic" | "debug" => ErrorMode::Panic,
        _ => {
            println!("Warning: Unknown error mode '{}', using default 'log'", param);
            println!("  Supported modes: log, halt, panic");
            ErrorMode::Log
        }
    }
}

fn print_help(program_name: &str) {
    println!("🚀 Test Memory R (TMR) v1.0.0 - High-Performance Memory Testing Tool");
    println!("===================================================================================");
    println!();
    println!("USAGE:");
    println!("  {}                                    # Run with defaults", program_name);
    println!("  {} config=test.json                  # Load modern JSON config (v2.0)", program_name);
    println!("  {} config=legacy.cfg                 # Load legacy TestMem5 config (v1.0)", program_name);
    println!("  {} --create-demo-configs              # Create demo configuration files", program_name);
    println!("  {} --version                         # Show version information", program_name);
    println!();
    println!("For full help documentation, see the artifacts or code comments.");
}

fn print_usage(program_name: &str) {
    println!("Quick Usage Examples:");
    println!("  {} memory=10%                    # Reserve 10% of system memory", program_name);
    println!("  {} memory=2GiB                  # Reserve 2 GiB", program_name);
    println!("  {} memory=tm5                   # TM5-compatible allocation", program_name);
    println!("  {} cycles=5 duration=600        # 5 cycles OR 10 minutes max", program_name);
    println!("  {} cpus=50% cputype=cores       # Use 50% of CPU cores", program_name);
    println!("  {} errors=halt                  # Stop on first error", program_name);
    println!("  {} config=test.json             # Load comprehensive JSON config", program_name);
    println!("  {} config=legacy.cfg            # Auto-convert TM5 config + add stuck bit test", program_name);
    println!("  {} --create-demo-configs        # Create demo configurations", program_name);
    println!();
    println!("For full help: {} --help", program_name);
}

fn format_duration(duration: std::time::Duration) -> String {
    // Format runtime as HH:MM:SS
    let total_seconds = duration.as_secs();
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;
    format!("{:02}:{:02}:{:02}", hours, minutes, seconds)
}