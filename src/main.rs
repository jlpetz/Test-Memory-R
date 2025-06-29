use std::env;
use tmr::{create_demo_configs, load_config, run_tests_with_layout, ErrorMode, MemoryLayout, MemoryStrategy, DEFAULT_RESERVE_PERCENT};

fn main() {
    // Initialize logging
    env_logger::Builder::from_default_env().filter_level(log::LevelFilter::Info).init();

    println!("🚀 Test Memory R (TMR) v1.0.0 - Three-Stage Memory Testing Tool");
    println!("=======================================================================");

    let args: Vec<String> = env::args().collect();

    // Check for special commands
    if args.len() > 1 {
        match args[1].as_str() {
            "--create-demo-configs" => {
                if let Err(e) = create_demo_configs() {
                    println!("  Supported formats: 10%, 2GiB, 1024MiB");
        (MemoryStrategy::ModernOptimal { reserve_gib: None }, DEFAULT_RESERVE_PERCENT)
    }
}

fn parse_strategy_parameter(param: &str) -> MemoryStrategy {
    match param.trim().to_lowercase().as_str() {
        "tm5" | "tm5_compatible" => MemoryStrategy::TM5Compatible {
            testing_window_size_mb: 880,
            reserved_memory_mb: 128,
            test_block_size_mb: 0,
        },
        "modern" | "modern_optimal" => MemoryStrategy::ModernOptimal { reserve_gib: None },
        "custom" => MemoryStrategy::Custom {
            blocks_per_thread: 4,
            min_block_size_mb: 256,
        },
        _ => {
            println!("Warning: Unknown strategy '{}', using default 'modern_optimal'", param);
            println!("  Supported strategies: tm5_compatible, modern_optimal, custom");
            MemoryStrategy::ModernOptimal { reserve_gib: None }
        }
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
    println!("🚀 Test Memory R (TMR) v1.0.0 - Three-Stage Memory Testing Tool");
    println!("=======================================================================");
    println!();
    println!("USAGE:");
    println!("  {}                                    # Run with defaults", program_name);
    println!(
        "  {} config=test.json                  # Load modern JSON config (v2.0)",
        program_name
    );
    println!(
        "  {} config=legacy.cfg                 # Load legacy TestMem5 config (v1.0)",
        program_name
    );
    println!(
        "  {} --create-demo-configs              # Create demo configuration files",
        program_name
    );
    println!("  {} --version                         # Show version information", program_name);
    println!();
    println!("THREE-STAGE MEMORY ARCHITECTURE:");
    println!("  Stage 1 - Memory Allocation:");
    println!("    • Allocate maximum available memory per thread (like TM5's 12×4.6GB)");
    println!("    • Lock memory pages to prevent swapping during tests");
    println!("    • Create memory pressure to stress the entire system");
    println!();
    println!("  Stage 2 - Testing Windows:");
    println!("    • Use focused subsets of allocated memory for repeated access");
    println!("    • Test temporal locality and memory timing constraints");
    println!("    • Configurable per test type for optimal stress patterns");
    println!();
    println!("  Stage 3 - Block Sizing:");
    println!("    • Optimize access patterns with aligned memory blocks");
    println!("    • Auto-align for SIMD operations and cache line efficiency");
    println!("    • Support both aligned and misaligned access testing");
    println!();
    println!("COMMAND LINE PARAMETERS:");
    println!("  memory=<value>     Memory allocation (Stage 1):");
    println!("                       10%           - Reserve 10% of system memory");
    println!("                       2GiB          - Reserve 2 GiB");
    println!("                       1024MiB       - Reserve 1024 MiB");
    println!();
    println!("  strategy=<type>    Memory allocation strategy:");
    println!("                       modern_optimal - Auto-optimized three-stage (default)");
    println!("                       tm5_compatible - TM5-style with configurable windows");
    println!("                       custom         - Multiple blocks with custom sizing");
    println!();
    println!("  cpus=<percent>     CPU usage percentage (1-100, default: 100)");
    println!("  cputype=<type>     CPU type:");
    println!("                       threads       - Use logical threads (default)");
    println!("                       cores         - Use physical cores only");
    println!();
    println!("  errors=<mode>      Error handling mode:");
    println!("                       log           - Log errors and continue (default)");
    println!("                       halt          - Stop on first error");
    println!("                       panic         - Panic on error (debug mode)");
    println!();
    println!("CONFIG FILES:");
    println!("  Modern JSON format v2.0 (.json):");
    println!("    • Three-stage architecture with per-test configuration");
    println!("    • Window and block size control per test function");
    println!("    • Alignment options and misaligned access testing");
    println!("    • Auto-sizing based on cache hierarchy and SIMD requirements");
    println!();
    println!("  Legacy TestMem5 format v1.0 (.cfg):");
    println!("    • Automatically converted to three-stage TM5-compatible mode");
    println!("    • Preserves original memory allocation patterns");
    println!("    • Maintains test sequences and timing configurations");
    println!("    • Per-test block sizes preserved and enhanced");
    println!();
    println!("MEMORY ALLOCATION STRATEGIES:");
    println!("  TM5-Compatible Three-Stage:");
    println!("    Stage 1: Allocate maximum memory minus fixed OS reserve");
    println!("    Stage 2: Fixed testing window size divided among threads");
    println!("    Stage 3: Configurable block sizes per test with alignment");
    println!("    → Best for: Legacy TM5 config compatibility, predictable patterns");
    println!();
    println!("  Modern Optimal Three-Stage:");
    println!("    Stage 1: Allocate maximum memory minus percentage-based reserve");
    println!("    Stage 2: Auto-calculate windows based on cache sizes and test type");
    println!("    Stage 3: Auto-align blocks for optimal SIMD and cache performance");
    println!("    → Best for: Modern systems, maximum performance, auto-optimization");
    println!();
    println!("  Custom Three-Stage:");
    println!("    Stage 1: Multiple smaller allocations per thread");
    println!("    Stage 2: Flexible window configuration within allocations");
    println!("    Stage 3: Full control over block sizes and alignment");
    println!("    → Best for: Advanced users, specific testing scenarios");
    println!();
    println!("ENVIRONMENT VARIABLES:");
    println!("  RUST_LOG=debug     Enable detailed three-stage logging");
    println!("  RUST_LOG=trace     Enable very detailed stage-by-stage logging");
    println!();
    println!("EXAMPLES:");
    println!("  {} memory=5% cpus=75% cputype=cores", program_name);
    println!("  {} strategy=tm5_compatible memory=880MiB", program_name);
    println!("  {} config=1usmus_v3.cfg              # Auto-converts to three-stage", program_name);
    println!("  {} config=my_test.json errors=halt", program_name);
    println!("  RUST_LOG=debug {} strategy=modern_optimal", program_name);
    println!();
    println!("The three-stage architecture provides:");
    println!("  • Maximum memory pressure (Stage 1)");
    println!("  • Focused timing-sensitive testing (Stage 2)");
    println!("  • Optimized access patterns (Stage 3)");
    println!("  • TM5 compatibility with modern enhancements");
}

fn print_usage(program_name: &str) {
    println!("Usage examples:");
    println!("  {} memory=10%                    # Reserve 10% of system memory", program_name);
    println!("  {} memory=2GiB                  # Reserve 2 GiB", program_name);
    println!("  {} strategy=tm5_compatible      # Use TM5-style three-stage allocation", program_name);
    println!("  {} cpus=50% cputype=cores       # Use 50% of CPU cores", program_name);
    println!("  {} errors=halt                  # Stop on first error", program_name);
    println!("  {} config=test.json             # Load JSON config file (v2.0)", program_name);
    println!(
        "  {} config=legacy.cfg            # Load legacy TM5 config (v1.0)",
        program_name
    );
    println!("  {} --create-demo-configs        # Create demo config files", program_name);
    println!();
    println!("Three-stage architecture:");
    println!("  Stage 1: Maximum memory allocation per thread (like TM5)");
    println!("  Stage 2: Focused testing windows for temporal locality");
    println!("  Stage 3: Optimized block sizes with alignment control");
    println!();
    println!("Environment variables:");
    println!("  RUST_LOG=debug                  # Enable detailed stage logging");
    println!("  RUST_LOG=tmr=trace              # Enable very detailed logging");
    println!();
    println!("For full help: {} --help", program_name);
}

fn format_duration(duration: std::time::Duration) -> String {
    let total_seconds = duration.as_secs();
    let days = total_seconds / 86400;
    let hours = (total_seconds % 86400) / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;
    let millis = duration.subsec_millis();

    if days > 0 {
        format!("{}d {:02}h {:02}m {:02}.{:03}s", days, hours, minutes, seconds, millis)
    } else if hours > 0 {
        format!("{:02}h {:02}m {:02}.{:03}s", hours, minutes, seconds, millis)
    } else if minutes > 0 {
        format!("{:02}m {:02}.{:03}s", minutes, seconds, millis)
    } else {
        format!("{}.{:03}s", seconds, millis)
    }
}!("❌ Failed to create demo configs: {}", e);
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
                println!("Three-Stage Memory Testing Tool with TM5 Compatibility");
                println!("Architecture: Stage 1 (Allocation) → Stage 2 (Window) → Stage 3 (Blocks)");
                return;
            }
            _ => {}
        }
    }

    // Check for config file parameter
    let config_file = args.iter().find(|arg| arg.starts_with("config=")).map(|arg| &arg[7..]);

    let (memory_strategy, error_mode, cputype, cpus, reserve_percent) = if let Some(config_path) = config_file {
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
                
                // Show three-stage configuration
                match &config.to_memory_strategy() {
                    MemoryStrategy::TM5Compatible { testing_window_size_mb, reserved_memory_mb, test_block_size_mb } => {
                        println!("   Three-Stage Config:");
                        println!("     Stage 1 (Allocation): Reserve {}MB for OS", reserved_memory_mb);
                        println!("     Stage 2 (Window): {}MB total testing window", testing_window_size_mb);
                        if *test_block_size_mb > 0 {
                            println!("     Stage 3 (Blocks): {}MB default block size", test_block_size_mb);
                        } else {
                            println!("     Stage 3 (Blocks): Per-test configuration");
                        }
                    }
                    _ => {
                        println!("   Three-Stage Config: Modern auto-configuration");
                    }
                }
                
                // Count per-test configurations
                let per_test_configs = config.test_sequence.iter()
                    .filter(|t| t.window_size_mb.is_some() || t.block_size_mb.is_some())
                    .count();
                if per_test_configs > 0 {
                    println!("   Per-Test Configs: {} tests have custom window/block sizes", per_test_configs);
                }
                
                println!();

                let memory_strategy = config.to_memory_strategy();
                let error_mode = config.to_error_mode();
                let cputype = config.system.cpu_config.cpu_type.clone();
                let cpus = format!("{}%", config.system.cpu_config.usage_percent);
                
                // For TM5-compatible mode, use a default reserve percent (not used in calculation)
                let reserve_percent = match &memory_strategy {
                    MemoryStrategy::TM5Compatible { .. } => 0.0, // Not used
                    _ => 10.0, // Default fallback
                };

                (memory_strategy, error_mode, cputype, cpus, reserve_percent)
            }
            Err(e) => {
                println!("❌ Failed to load config file '{}': {}", config_path, e);
                println!("   Falling back to command line parameters...");
                println!();
                parse_command_line_params(&args)
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
        } else if arg.starts_with("strategy=") {
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
            default_params.push(format!("memory={}% (default)", DEFAULT_RESERVE_PERCENT));
        }
        if !args.iter().any(|a| a.starts_with("errors=")) {
            default_params.push("errors=log (default)".to_string());
        }
        if !args.iter().any(|a| a.starts_with("strategy=")) {
            default_params.push("strategy=modern_optimal (default)".to_string());
        }
    }

    if !explicit_params.is_empty() {
        println!("  Explicit Parameters: {}", explicit_params.join(", "));
    }
    if !default_params.is_empty() {
        println!("  Default Parameters: {}", default_params.join(", "));
    }
    println!();

    println!("Configuration:");
    println!("  CPU Type: {}", cputype);
    println!("  Using {}/{} {} for testing", threads, total_cpus, cputype);
    
    // Display three-stage memory strategy information
    print!("  Memory Strategy: ");
    match &memory_strategy {
        MemoryStrategy::TM5Compatible { testing_window_size_mb, reserved_memory_mb, test_block_size_mb } => {
            println!("TM5-Compatible Three-Stage");
            println!("    Stage 1 (Allocation): Maximum available memory minus {}MB OS reserve", reserved_memory_mb);
            println!("    Stage 2 (Window): {}MB total testing window (divided among threads)", testing_window_size_mb);
            if *test_block_size_mb > 0 {
                println!("    Stage 3 (Blocks): {}MB default block size (per-test overrides allowed)", test_block_size_mb);
            } else {
                println!("    Stage 3 (Blocks): Per-test configuration with auto-alignment");
            }
        }
        MemoryStrategy::ModernOptimal { reserve_gib } => {
            println!("Modern Optimal Three-Stage");
            println!("    Stage 1 (Allocation): Maximum available memory minus reserve");
            if let Some(gib) = reserve_gib {
                println!("      Reserve: {:.2} GiB", gib);
            } else {
                println!("      Reserve: {:.1}% of system memory", reserve_percent);
            }
            println!("    Stage 2 (Window): Auto-calculated based on cache hierarchy and test type");
            println!("    Stage 3 (Blocks): Auto-aligned based on SIMD requirements and cache lines");
        }
        MemoryStrategy::Custom { blocks_per_thread, min_block_size_mb } => {
            println!("Custom Three-Stage");
            println!("    Stage 1 (Allocation): Multiple blocks per thread");
            println!("      Blocks per thread: {}", blocks_per_thread);
            println!("      Min block size: {}MB", min_block_size_mb);
            println!("    Stage 2 (Window): Per-test configuration within blocks");
            println!("    Stage 3 (Blocks): Configurable with alignment options");
        }
    }
    
    print!("  Error Mode: ");
    match error_mode {
        ErrorMode::Log => println!("Log and continue"),
        ErrorMode::Halt => println!("Halt on first error"),
        ErrorMode::Panic => println!("Panic on error (debug mode)"),
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

    // Calculate memory layout (Stage 1 only - Stages 2&3 happen per-test)
    let layout = MemoryLayout::calculate(memory_strategy, threads, reserve_percent);

    // Show expected allocation pattern
    match &layout.strategy {
        MemoryStrategy::TM5Compatible { .. } => {
            let per_thread_gib = layout.blocks[0].size_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
            println!("Expected Allocation Pattern (like TM5):");
            println!("  {} × {:.2} GiB per thread = {:.2} GiB total",
                threads, per_thread_gib, 
                layout.allocated_memory as f64 / (1024.0 * 1024.0 * 1024.0));
            println!("  This matches TM5's large-block-per-thread approach");
        }
        _ => {
            println!("Expected Allocation Pattern:");
            println!("  Modern optimized allocation with auto-sizing");
        }
    }
    println!();

    // Run tests
    println!("Starting three-stage memory tests...");
    println!("  Stage 1: Allocate maximum memory per thread");
    println!("  Stage 2: Configure testing windows per test type");
    println!("  Stage 3: Optimize block sizes with alignment");
    println!("  (detailed logs available with RUST_LOG=debug)");
    println!();
    
    let start_time = std::time::Instant::now();
    let success = run_tests_with_layout(layout, error_mode);
    let total_time = start_time.elapsed();

    println!();
    println!("=======================================================================");
    if success {
        println!("✅ All three-stage tests completed successfully in {}", format_duration(total_time));
        println!("   Stage 1 (Allocation): Memory allocated and locked successfully");
        println!("   Stage 2 (Windows): Per-test window sizing applied optimally"); 
        println!("   Stage 3 (Blocks): Block alignment and sizing optimized per test");
    } else {
        println!("❌ Tests failed or encountered errors in {}", format_duration(total_time));
    }

    println!();
    print_usage(&args[0]);
}

fn parse_command_line_params(args: &[String]) -> (MemoryStrategy, ErrorMode, String, String, f64) {
    let mut cputype = "threads".to_string();
    let mut cpus = "100%".to_string();
    let mut memory_strategy = MemoryStrategy::ModernOptimal { reserve_gib: None };
    let mut error_mode = ErrorMode::Log;
    let mut reserve_percent = DEFAULT_RESERVE_PERCENT;

    // Parse arguments
    for arg in args {
        if arg.starts_with("cputype=") {
            cputype = arg[8..].to_string();
        } else if arg.starts_with("cpus=") {
            cpus = arg[5..].to_string();
        } else if arg.starts_with("memory=") {
            let (strategy, percent) = parse_memory_parameter(&arg[7..]);
            memory_strategy = strategy;
            reserve_percent = percent;
        } else if arg.starts_with("errors=") {
            error_mode = parse_error_mode(&arg[7..]);
        } else if arg.starts_with("strategy=") {
            memory_strategy = parse_strategy_parameter(&arg[9..]);
        }
    }

    (memory_strategy, error_mode, cputype, cpus, reserve_percent)
}

fn parse_memory_parameter(param: &str) -> (MemoryStrategy, f64) {
    let param = param.trim();

    if param.ends_with('%') {
        let percent_str = param.trim_end_matches('%');
        match percent_str.parse::<f64>() {
            Ok(percent) if percent >= 0.0 && percent <= 95.0 => {
                (MemoryStrategy::ModernOptimal { reserve_gib: None }, percent)
            }
            Ok(percent) => {
                println!(
                    "Warning: Invalid percentage {}%, using default {}%",
                    percent, DEFAULT_RESERVE_PERCENT
                );
                (MemoryStrategy::ModernOptimal { reserve_gib: None }, DEFAULT_RESERVE_PERCENT)
            }
            Err(_) => {
                println!(
                    "Warning: Could not parse percentage '{}', using default {}%",
                    param, DEFAULT_RESERVE_PERCENT
                );
                (MemoryStrategy::ModernOptimal { reserve_gib: None }, DEFAULT_RESERVE_PERCENT)
            }
        }
    } else if param.to_lowercase().ends_with("gib") {
        let gib_str = param[..param.len() - 3].trim();
        match gib_str.parse::<f64>() {
            Ok(gib) if gib >= 0.0 => {
                (MemoryStrategy::ModernOptimal { reserve_gib: Some(gib) }, 0.0)
            }
            Ok(gib) => {
                println!("Warning: Invalid GiB value {}, using default {}%", gib, DEFAULT_RESERVE_PERCENT);
                (MemoryStrategy::ModernOptimal { reserve_gib: None }, DEFAULT_RESERVE_PERCENT)
            }
            Err(_) => {
                println!(
                    "Warning: Could not parse GiB value '{}', using default {}%",
                    param, DEFAULT_RESERVE_PERCENT
                );
                (MemoryStrategy::ModernOptimal { reserve_gib: None }, DEFAULT_RESERVE_PERCENT)
            }
        }
    } else if param.to_lowercase().ends_with("mib") {
        let mib_str = param[..param.len() - 3].trim();
        match mib_str.parse::<f64>() {
            Ok(mib) if mib >= 0.0 => {
                let gib = mib / 1024.0;
                (MemoryStrategy::ModernOptimal { reserve_gib: Some(gib) }, 0.0)
            }
            Ok(mib) => {
                println!("Warning: Invalid MiB value {}, using default {}%", mib, DEFAULT_RESERVE_PERCENT);
                (MemoryStrategy::ModernOptimal { reserve_gib: None }, DEFAULT_RESERVE_PERCENT)
            }
            Err(_) => {
                println!(
                    "Warning: Could not parse MiB value '{}', using default {}%",
                    param, DEFAULT_RESERVE_PERCENT
                );
                (MemoryStrategy::ModernOptimal { reserve_gib: None }, DEFAULT_RESERVE_PERCENT)
            }
        }
    } else {
        println!(
            "Warning: Unknown memory parameter format '{}', using default {}%",
            param, DEFAULT_RESERVE_PERCENT
        );
        println
