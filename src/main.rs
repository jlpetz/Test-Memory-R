use std::env;
use tmr::{create_demo_configs, load_config, run_tests_with_layout, ErrorMode, MemoryLayout, MemoryReserve, DEFAULT_RESERVE_PERCENT};

fn main() {
    // Initialize logging - detailed logs go to stderr/file
    env_logger::Builder::from_default_env().filter_level(log::LevelFilter::Info).init();

    println!("🚀 Test Memory R (TMR) v1.0.0 - High-Performance Memory Testing Tool");
    println!("=======================================================================");

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
                println!("High-Performance DDR5 Memory Testing Tool");
                return;
            }
            _ => {}
        }
    }

    // Check for config file parameter
    let config_file = args.iter().find(|arg| arg.starts_with("config=")).map(|arg| &arg[7..]);

    let (memory_reserve, error_mode, cputype, cpus) = if let Some(config_path) = config_file {
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
                println!();

                let memory_reserve = config.to_memory_reserve();
                let error_mode = config.to_error_mode();
                let cputype = config.system.cpu_config.cpu_type.clone();
                let cpus = format!("{}%", config.system.cpu_config.usage_percent);

                (memory_reserve, error_mode, cputype, cpus)
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
        // Skip program name
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
    print!("  Memory Reserve: ");
    match &memory_reserve {
        MemoryReserve::PercentFree(p) => println!("{:.1}% of system memory", p),
        MemoryReserve::GiB(g) => println!("{:.2} GiB", g),
        MemoryReserve::MiB(m) => println!("{:.2} MiB", m),
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

    // Calculate memory layout
    let layout = MemoryLayout::calculate(memory_reserve, threads);

    // Run tests
    println!("Starting memory tests... (detailed logs available with RUST_LOG=debug)");
    println!();
    let start_time = std::time::Instant::now();
    let success = run_tests_with_layout(layout, error_mode);
    let total_time = start_time.elapsed();

    println!();
    println!("=======================================================================");
    if success {
        println!("✅ All tests completed successfully in {}", format_duration(total_time));
    } else {
        println!("❌ Tests failed or encountered errors in {}", format_duration(total_time));
    }

    println!();
    print_usage(&args[0]);
}

fn parse_command_line_params(args: &[String]) -> (MemoryReserve, ErrorMode, String, String) {
    let mut cputype = "threads".to_string(); // default
    let mut cpus = "100%".to_string();
    let mut memory_reserve = MemoryReserve::PercentFree(DEFAULT_RESERVE_PERCENT);
    let mut error_mode = ErrorMode::Log; // default to log errors

    // Parse arguments
    for arg in args {
        if arg.starts_with("cputype=") {
            cputype = arg[8..].to_string();
        } else if arg.starts_with("cpus=") {
            cpus = arg[5..].to_string();
        } else if arg.starts_with("memory=") {
            memory_reserve = parse_memory_parameter(&arg[7..]);
        } else if arg.starts_with("errors=") {
            error_mode = parse_error_mode(&arg[7..]);
        }
    }

    (memory_reserve, error_mode, cputype, cpus)
}

fn parse_memory_parameter(param: &str) -> MemoryReserve {
    let param = param.trim();

    if param.ends_with('%') {
        let percent_str = param.trim_end_matches('%');
        match percent_str.parse::<f64>() {
            Ok(percent) if percent >= 0.0 && percent <= 95.0 => MemoryReserve::PercentFree(percent),
            Ok(percent) => {
                println!(
                    "Warning: Invalid percentage {}%, using default {}%",
                    percent, DEFAULT_RESERVE_PERCENT
                );
                MemoryReserve::PercentFree(DEFAULT_RESERVE_PERCENT)
            }
            Err(_) => {
                println!(
                    "Warning: Could not parse percentage '{}', using default {}%",
                    param, DEFAULT_RESERVE_PERCENT
                );
                MemoryReserve::PercentFree(DEFAULT_RESERVE_PERCENT)
            }
        }
    } else if param.to_lowercase().ends_with("gib") {
        let gib_str = param[..param.len() - 3].trim();
        match gib_str.parse::<f64>() {
            Ok(gib) if gib >= 0.0 => MemoryReserve::GiB(gib),
            Ok(gib) => {
                println!("Warning: Invalid GiB value {}, using default {}%", gib, DEFAULT_RESERVE_PERCENT);
                MemoryReserve::PercentFree(DEFAULT_RESERVE_PERCENT)
            }
            Err(_) => {
                println!(
                    "Warning: Could not parse GiB value '{}', using default {}%",
                    param, DEFAULT_RESERVE_PERCENT
                );
                MemoryReserve::PercentFree(DEFAULT_RESERVE_PERCENT)
            }
        }
    } else if param.to_lowercase().ends_with("mib") {
        let mib_str = param[..param.len() - 3].trim();
        match mib_str.parse::<f64>() {
            Ok(mib) if mib >= 0.0 => MemoryReserve::MiB(mib),
            Ok(mib) => {
                println!("Warning: Invalid MiB value {}, using default {}%", mib, DEFAULT_RESERVE_PERCENT);
                MemoryReserve::PercentFree(DEFAULT_RESERVE_PERCENT)
            }
            Err(_) => {
                println!(
                    "Warning: Could not parse MiB value '{}', using default {}%",
                    param, DEFAULT_RESERVE_PERCENT
                );
                MemoryReserve::PercentFree(DEFAULT_RESERVE_PERCENT)
            }
        }
    } else {
        println!(
            "Warning: Unknown memory parameter format '{}', using default {}%",
            param, DEFAULT_RESERVE_PERCENT
        );
        println!("  Supported formats: 10%, 2GiB, 1024MiB");
        MemoryReserve::PercentFree(DEFAULT_RESERVE_PERCENT)
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
    println!("COMMAND LINE PARAMETERS:");
    println!("  memory=<value>     Memory allocation:");
    println!("                       10%           - Reserve 10% of system memory");
    println!("                       2GiB          - Reserve 2 GiB");
    println!("                       1024MiB       - Reserve 1024 MiB");
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
    println!("    - Version tracking and compatibility checks");
    println!("    - Full parameter control and SIMD test configuration");
    println!("    - Structured, easy to edit with metadata");
    println!();
    println!("  Legacy TestMem5 format v1.0 (.cfg):");
    println!("    - Compatible with original TestMem5 configs");
    println!("    - Automatic function name mapping");
    println!("    - Preserves test sequences and patterns");
    println!();
    println!("ENVIRONMENT VARIABLES:");
    println!("  RUST_LOG=debug     Enable detailed logging");
    println!("  RUST_LOG=trace     Enable very detailed logging");
    println!();
    println!("EXAMPLES:");
    println!("  {} memory=5% cpus=75% cputype=cores", program_name);
    println!("  {} config=1usmus_v3.cfg", program_name);
    println!("  {} config=my_test.json errors=halt", program_name);
    println!("  RUST_LOG=debug {} memory=2GiB", program_name);
}

fn print_usage(program_name: &str) {
    println!("Usage examples:");
    println!("  {} memory=10%                    # Reserve 10% of system memory", program_name);
    println!("  {} memory=2GiB                  # Reserve 2 GiB", program_name);
    println!("  {} memory=1024MiB               # Reserve 1024 MiB", program_name);
    println!("  {} cpus=50% cputype=cores       # Use 50% of CPU cores", program_name);
    println!("  {} errors=halt                  # Stop on first error", program_name);
    println!("  {} config=test.json             # Load JSON config file (v2.0)", program_name);
    println!(
        "  {} config=legacy.cfg            # Load legacy TestMem5 config (v1.0)",
        program_name
    );
    println!("  {} --create-demo-configs        # Create demo config files", program_name);
    println!();
    println!("Environment variables:");
    println!("  RUST_LOG=debug                  # Enable detailed logging");
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
}
