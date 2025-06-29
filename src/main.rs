use std::env;
use tmr::{create_demo_configs, load_config, run_tests_with_layout, ErrorMode, MemoryLayout, MemoryStrategy, DEFAULT_RESERVE_PERCENT};

fn main() {
    // Initialize logging
    env_logger::Builder::from_default_env().filter_level(log::LevelFilter::Info).init();

    println!("🚀 Test Memory R (TMR) v1.0.0 - High-Performance Three-Stage Memory Testing Tool");
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
                println!("High-Performance Three-Stage Memory Testing Tool with TM5 Compatibility");
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
                
                // Show test configuration summary
                let enabled_tests: Vec<_> = config.test_sequence.iter().filter(|t| t.enabled).collect();
                println!("   Tests: {} enabled tests with per-test memory configuration", enabled_tests.len());
                
                // Show any per-test window or block overrides
                let mut has_overrides = false;
                for test in &enabled_tests {
                    if test.window_size_mb.is_some() || test.block_size_mb.is_some() {
                        if !has_overrides {
                            println!("   Per-test overrides detected:");
                            has_overrides = true;
                        }
                        println!("     {}: {}{}{}",
                            test.function,
                            test.window_size_mb.map(|w| format!("Window {}MB ", w)).unwrap_or_default(),
                            test.block_size_mb.map(|b| format!("Block {}MB ", b)).unwrap_or_default(),
                            test.allow_misaligned.map(|m| if m { "Misaligned" } else { "Aligned" }).unwrap_or("")
                        );
                    }
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

    println!("Three-Stage Memory Architecture Configuration:");
    println!("  CPU Type: {}", cputype);
    println!("  Using {}/{} {} for testing", threads, total_cpus, cputype);
    
    // Display memory strategy information with three-stage breakdown
    print!("  Memory Strategy: ");
    match &memory_strategy {
        MemoryStrategy::TM5Compatible { testing_window_size_mb, reserved_memory_mb, test_block_size_mb } => {
            println!("TM5-Compatible Three-Stage");
            println!("    Stage 1 (Allocation): Maximum available minus {} MB OS reserve", reserved_memory_mb);
            println!("    Stage 2 (Testing Window): {} MB total window across all threads", testing_window_size_mb);
            if *test_block_size_mb > 0 {
                println!("    Stage 3 (Block Size): {} MB default blocks (per-test overrides allowed)", test_block_size_mb);
            } else {
                println!("    Stage 3 (Block Size): Auto-calculated per test (per-test overrides allowed)");
            }
        }
        MemoryStrategy::ModernOptimal { reserve_gib } => {
            println!("Modern Optimal Three-Stage");
            if let Some(gib) = reserve_gib {
                println!("    Stage 1 (Allocation): Maximum available minus {:.2} GiB reserve", gib);
            } else {
                println!("    Stage 1 (Allocation): Maximum available minus {:.1}% reserve", reserve_percent);
            }
            println!("    Stage 2 (Testing Window): Auto-sized per test based on cache hierarchy");
            println!("    Stage 3 (Block Size): Auto-aligned per test for optimal SIMD performance");
        }
        MemoryStrategy::Custom { blocks_per_thread, min_block_size_mb } => {
            println!("Custom Three-Stage");
            println!("    Stage 1 (Allocation): Multiple blocks per thread ({} blocks)", blocks_per_thread);
            println!("    Stage 2 (Testing Window): Configurable per test");
            println!("    Stage 3 (Block Size): Minimum {} MB (per-test overrides allowed)", min_block_size_mb);
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
	
	// Detect SIMD capabilities
    let simd_caps = tmr::detect_simd_capabilities();
    println!("  SIMD Support: {}", simd_caps);

    // Detect and display cache architecture
    let cache_info = tmr::tests::get_cache_info();
    println!("  Cache Architecture:");
    println!("    L1 Data: {:.1} KB | L1 Instruction: {:.1} KB", 
        cache_info.l1_data_cache as f64 / 1024.0,
        cache_info.l1_instruction_cache as f64 / 1024.0);
    println!("    L2: {:.1} KB | L3: {:.1} MB | Line Size: {} bytes", 
        cache_info.l2_cache as f64 / 1024.0,
        cache_info.l3_cache as f64 / (1024.0 * 1024.0),
        cache_info.cache_line_size);
    println!("    Detection: {}", cache_info.detection_method);

    println!();
	

    // Calculate memory layout (Stage 1 allocation only)
    let layout = MemoryLayout::calculate(memory_strategy, threads, reserve_percent);

    // Run tests with three-stage architecture
    println!("Starting three-stage memory tests...");
    println!("  Stage 1: Pre-allocating maximum memory per thread");
    println!("  Stage 2: Configuring testing windows per test");
    println!("  Stage 3: Optimizing block sizes and alignment per test");
    println!("(detailed logs available with RUST_LOG=debug)");
    println!();
    let start_time = std::time::Instant::now();
    let success = run_tests_with_layout(layout, error_mode);
    let total_time = start_time.elapsed();

    println!();
    println!("================================================================================");
    if success {
        println!("✅ All three-stage memory tests completed successfully in {}", format_duration(total_time));
        println!("   Memory pressure maintained throughout testing with focused window access");
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
    println!("🚀 Test Memory R (TMR) v1.0.0 - High-Performance Three-Stage Memory Testing Tool");
    println!("===================================================================================");
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
    println!("  Stage 1: Memory Allocation");
    println!("  Stage 1: Memory Allocation");
    println!("    - Allocate maximum available memory per thread (like TM5's 12 x 4.6GB)");
    println!("    - Create memory pressure on the system");
    println!("    - Lock pages to prevent swapping during tests");
    println!();
    println!("  Stage 2: Testing Window");
    println!("    - Focus testing on subset of allocation for temporal locality");
    println!("    - Test read-after-write and write-after-read timing");
    println!("    - Configurable per test (e.g., 64MB window within 4.6GB allocation)");
    println!();
    println!("  Stage 3: Block/Chunk Size");
    println!("    - Control memory controller behavior and access patterns");
    println!("    - Auto-aligned for SIMD operations (128-bit, 256-bit, 512-bit)");
    println!("    - Configurable per test with misaligned access option");
    println!();
    println!("INTELLIGENT CACHE-AWARE CONFIGURATION:");
    println!("  TMR automatically detects your CPU's cache architecture at runtime:");
    println!("    • L1, L2, L3 cache sizes via CPUID instruction");
    println!("    • Cache line size for optimal alignment");
    println!("    • Fallback to empirical timing detection if CPUID fails");
    println!("    • Auto-configures Stage 2 windows based on detected cache sizes");
    println!("    • Optimizes Stage 3 block alignment to cache line boundaries");
    println!();
    println!("  Cache-Aware Test Optimizations:");
    println!("    • CacheBusting: Window sized to L3/2, blocks aligned to cache lines");
    println!("    • RandomTorture: Window sized to L3*2 for maximum stress");
    println!("    • SIMD tests: Large windows with SIMD-aligned blocks");
    println!("    • Bandwidth tests: Windows exceed all cache levels");
    println!();
    println!("COMMAND LINE PARAMETERS:");
    println!("  memory=<value>     Memory allocation (Stage 1):");
    println!("                       10%           - Reserve 10% of system memory");
    println!("                       2GiB          - Reserve 2 GiB");
    println!("                       1024MiB       - Reserve 1024 MiB");
    println!();
    println!("  strategy=<type>    Memory allocation strategy:");
    println!("                       modern_optimal - Auto-sized windows and blocks (default)");
    println!("                       tm5_compatible - TM5-style allocation with configurable window");
    println!("                       custom         - Multiple smaller blocks per thread");
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
    println!("    - Three-stage memory configuration with per-test overrides");
    println!("    - Version tracking and compatibility checks");
    println!("    - Window size and block size configuration per test");
    println!("    - Alignment control and misaligned access options");
    println!();
    println!("  Legacy TestMem5 format v1.0 (.cfg):");
    println!("    - Compatible with original TestMem5 configs");
    println!("    - Automatically converted to three-stage architecture");
    println!("    - Preserves test sequences, patterns, and timing");
    println!("    - Maps legacy block sizes to Stage 3 configuration");
    println!();
    println!("MEMORY ALLOCATION STRATEGIES:");
    println!("  TM5-Compatible (tm5_compatible):");
    println!("    - Stage 1: Allocate maximum memory like TM5 (e.g., 12 x 4.6GB)");
    println!("    - Stage 2: Configurable testing window (e.g., 880MB total)");
    println!("    - Stage 3: Per-test block sizes with auto-alignment");
    println!("    - Best for: Backwards compatibility with TM5 configs");
    println!();
    println!("  Modern Optimal (modern_optimal):");
    println!("    - Stage 1: Optimized allocation based on available memory");
    println!("    - Stage 2: Auto-sized windows based on cache hierarchy");
    println!("    - Stage 3: Auto-aligned blocks for SIMD performance");
    println!("    - Best for: Modern systems with intelligent auto-configuration");
    println!();
    println!("  Custom (custom):");
    println!("    - Stage 1: Multiple configurable blocks per thread");
    println!("    - Stage 2: Fully configurable testing windows");
    println!("    - Stage 3: Manual block size and alignment control");
    println!("    - Best for: Advanced users requiring specific memory layouts");
    println!();
    println!("ENVIRONMENT VARIABLES:");
    println!("  RUST_LOG=debug     Enable detailed three-stage logging");
    println!("  RUST_LOG=trace     Enable very detailed logging with alignment info");
    println!();
    println!("EXAMPLES:");
    println!("  {} memory=5% cpus=75% cputype=cores", program_name);
    println!("  {} strategy=tm5_compatible memory=880MiB", program_name);
    println!("  {} config=1usmus_v3.cfg  # Auto-converts to three-stage", program_name);
    println!("  {} config=my_test.json errors=halt", program_name);
    println!("  RUST_LOG=debug {} strategy=modern_optimal", program_name);
    println!();
    println!("UNDERSTANDING THE LOGS:");
    println!("  Stage 1 logs: Show total memory allocation per thread");
    println!("  Stage 2 logs: Show testing window size for each test");
    println!("  Stage 3 logs: Show block size and alignment adjustments");
}

fn print_usage(program_name: &str) {
    println!("Quick Usage Examples:");
    println!("  {} memory=10%                    # Reserve 10% of system memory", program_name);
    println!("  {} memory=2GiB                  # Reserve 2 GiB", program_name);
    println!("  {} strategy=tm5_compatible      # Use TM5-style three-stage allocation", program_name);
    println!("  {} cpus=50% cputype=cores       # Use 50% of CPU cores", program_name);
    println!("  {} errors=halt                  # Stop on first error", program_name);
    println!("  {} config=test.json             # Load JSON config with three-stage settings", program_name);
    println!(
        "  {} config=legacy.cfg            # Auto-convert legacy TM5 config to three-stage",
        program_name
    );
    println!("  {} --create-demo-configs        # Create demo three-stage config files", program_name);
    println!();
    println!("Three-Stage Architecture Benefits:");
    println!("  • Maximum memory pressure (Stage 1) like TM5's large allocations");
    println!("  • Focused temporal testing (Stage 2) for timing-sensitive errors");
    println!("  • Optimized access patterns (Stage 3) for SIMD and cache behavior");
    println!();
    println!("Environment variables:");
    println!("  RUST_LOG=debug                  # See Stage 2 & 3 configuration details");
    println!("  RUST_LOG=tmr=trace              # See memory alignment adjustments");
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
}
