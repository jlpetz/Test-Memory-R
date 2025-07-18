use std::env;
use tmr::{create_demo_configs, load_config, ErrorMode, MemoryStrategy, AllocationMode, WindowMode, BlockMode, MemoryBackend, RuntimeConfig};
use tmr::layout::MemoryLayout;
use tmr::runner::{run_tests_with_layout_and_timing, TestSuiteTiming, get_numa_node_for_cpu};
use tmr::results::compare_results_command;
use tmr::{dma_memory::{DmaBuffer, DriverHandle, CompatibilityFlags}, reset_driver, display_driver_info, check_and_display_driver_status, DriverStatus, refresh_driver_status, is_driver_connected, display_driver_stats};
use tmr::config::{MemoryAllocationConfig, CpuPinningConfig};
use log::LevelFilter;
use env_logger::Builder;
use std::io::Write;
use std::sync::{Arc, Mutex};

// Global file logger for dual console+file logging
static FILE_LOGGER: std::sync::OnceLock<Arc<Mutex<Option<std::fs::File>>>> = std::sync::OnceLock::new();

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize enhanced logging with file output
    setup_logging();

    println!("🚀 Test Memory R (TMR) v1.0.0 - High-Performance Memory Testing Tool");
    println!("================================================================================");

    let args: Vec<String> = env::args().collect();

    // Check for special commands
    if args.len() > 1 {
        match args[1].as_str() {
            "--create-demo-configs" => {
                if let Err(e) = create_demo_configs() {
                    println!("❌ Failed to create demo configs: {}", e);
                    return Ok(());
                }
                return Ok(());
            }
            "--compare-results" => {
                if args.len() < 4 {
                    println!("❌ Usage: {} --compare-results <baseline.json> <current.json> [output.json]", args[0]);
                    return Ok(());
                }
                let baseline = &args[2];
                let current = &args[3];
                let output = args.get(4).map(|s| s.as_str());
                
                match compare_results_command(baseline, current, output) {
                    Ok(()) => println!("✅ Comparison completed successfully"),
                    Err(e) => println!("❌ Comparison failed: {}", e),
                }
                return Ok(());
            }
            "--help" | "-h" => {
                print_help(&args[0]);
                return Ok(());
            }
            "--version" | "-v" => {
                println!("Test Memory R (TMR) version 1.0.0");
                println!("High-Performance Memory Testing Tool with TM5 Compatibility");
                println!("Three-Stage Memory Architecture with Comprehensive Testing");
                return Ok(());
            }
            _ => {}
        }
    }

    // Check for config file parameter
    let config_file = args.iter().find(|arg| arg.starts_with("config=")).map(|arg| &arg[7..]);

	let (memory_strategy, error_mode, suite_timing, cputype, cpus, pinning_config, alloc_config) = 
		if let Some(config_path) = config_file {
			let config = load_config(config_path)?;
			(
				config.to_memory_strategy(),
				config.to_error_mode(),
				config.to_test_suite_timing(),
				config.system.cpu_config.cpu_type.clone(),
				format!("{}%", config.system.cpu_config.usage_percent),
				config.system.cpu_pinning.clone(),
				config.system.memory_allocation.clone(),
			)
		} else {
			// Command line defaults
			(
				MemoryStrategy::default(),
				ErrorMode::Log,
				TestSuiteTiming::default(),
				"threads".to_string(),
				"100%".to_string(),
				CpuPinningConfig::default(),
				MemoryAllocationConfig::default(),
			)
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
    let rust_log = env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string());
    println!("  RUST_LOG: {} (logs to .\\logs\\TMR_YYYY-MM-DD_HH-MM-SS.log)", rust_log);

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
            default_params.push("memory=20% reserve (default)".to_string());
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
	print!("  Error Mode: ");
    match error_mode {
        ErrorMode::Log => println!("Log and continue"),
        ErrorMode::Halt => println!("Halt on first error"),
        ErrorMode::Panic => println!("Panic on error (debug mode)"),
    }
    println!();
	
	// Check large page privilege early
    match tmr::check_large_page_privilege() {
        Ok(()) => println!("  Large Pages: ✅ Available (SeLockMemoryPrivilege enabled)"),
        Err(msg) => {
            println!("  Large Pages: ⚠️  Not available - {}", msg);
            println!("    To enable: Run as Administrator OR enable 'Lock pages in memory' in Group Policy");
            println!("    Impact: Will use standard 4KB pages instead of 2MB pages");
        }
    }
	let use_driver_chunking = args.iter().any(|arg| arg == "--driver-chunking");
	if use_driver_chunking {
		println!("  Use Driver Chunking: ❌ Enabled (for driver testing, driver controls page allocation strategy)");
	} else {
		println!("  Use Driver Chunking: ✅ Disabled (default/good, application controls page allocation strategy)");
	}

	// Check and display DMA driver status
	display_driver_info();
	// Reset allocations if driver is available
	if matches!(check_and_display_driver_status(), DriverStatus::Available(_)) {
		reset_driver();
	}
	println!();
	
    println!("System information and Memory Architecture detection:");
    println!("  CPU Type: {}", cputype);
    println!("  Using {}/{} {} for testing", threads, total_cpus, cputype);
	// Detect SIMD capabilities
    let simd_caps = tmr::detect_simd_capabilities();
    println!("  SIMD Support: {}", simd_caps);
    println!();
    
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
    
    // Display timing configuration
    print!("  Test Suite Timing: ");
    match (&suite_timing.global_cycles, &suite_timing.global_duration_secs) {
        (Some(cycles), Some(duration)) => println!("{} cycles or {}s max (whichever first)", cycles, duration),
        (Some(cycles), None) => println!("{} cycles (no time limit)", cycles),
        (None, Some(duration)) => println!("{}s duration (unlimited cycles)", duration),
        (None, None) => println!("Unlimited cycles and duration"),
    }

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
    println!("  Cache Architecture ({}):", cache_info.detection_method);
    println!("    L1 Data: {:.1} KB total ({:.1} KB × {} cores)", 
        cache_info.l1_data_cache as f64 / 1024.0,
        cache_info.per_core_l1d as f64 / 1024.0,
        cache_info.core_count);
    println!("    L1 Instruction: {:.1} KB total ({:.1} KB × {} cores)", 
        cache_info.l1_instruction_cache as f64 / 1024.0,
        cache_info.per_core_l1i as f64 / 1024.0,
        cache_info.core_count);
    println!("    L2: {:.1} KB total ({:.1} KB × {} cores)", 
        cache_info.l2_cache as f64 / 1024.0,
        cache_info.per_core_l2 as f64 / 1024.0,
        cache_info.core_count);
    println!("    L3: {:.1} MB (shared)", 
        cache_info.l3_cache as f64 / (1024.0 * 1024.0));
    println!("    Line Size: {} bytes | Total Cache: {:.1} MB", 
        cache_info.cache_line_size,
        cache_info.total_cache as f64 / (1024.0 * 1024.0));
    println!();

    // Calculate memory layout (Stage 1 allocation only)
    let layout = MemoryLayout::calculate(memory_strategy, threads);

    println!("Starting comprehensive memory tests...");
    println!("  Stage 1: Pre-allocating maximum memory per thread");
    println!("  Stage 2: Configuring testing windows per test (full allocation or optimized)");
    println!("  Stage 3: Optimizing block sizes and alignment per test");
    println!("  Critical: StuckBitTest will scan ALL allocated memory for stuck bits");
    println!("  Use CTRL+C for graceful shutdown with final report");
    println!("(detailed logs available with RUST_LOG=debug)");
    println!();
	
	let runtime_config = detect_memory_capabilities(use_driver_chunking);
    
    let start_time = std::time::Instant::now();
	let success = run_tests_with_layout_and_timing(layout, error_mode, suite_timing, runtime_config);
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
	
	if is_driver_connected() {
		display_driver_stats(); // Gets fresh stats from existing handle
	}

	reset_driver(); // Reset_driver, this tells the driver we are done and any allocation not already released should be released(fail-safe).
	
	// This will completely reset the driver handle and re-check version, for when we implement a GUI
	let new_status = refresh_driver_status();
	
	if is_driver_connected() {
		display_driver_stats(); // Gets fresh stats from existing handle
	}
	
	println!();
    print_usage(&args[0]);
	Ok(())
}

// Example usage in main.rs
pub fn check_dma_driver_status() {
    match DriverHandle::open_with_version_check() {
        Ok(driver) => {
            if let Ok(version) = driver.check_version_compatibility() {
                println!("  DMA Driver: ✅ Available and compatible");
                println!("    Version: {}.{}.{}.{}", 
                         version.driver_version_major,
                         version.driver_version_minor,
                         version.driver_version_build,
                         version.driver_version_revision);
            }
        }
        Err(e) => {
            println!("  DMA Driver: ❌ {}", e);
            if e.contains("version incompatible") {
                println!("    Action: Update TMR or the kernel driver to matching versions");
            }
        }
    }
}

fn setup_logging() {
    // Create logs directory if it doesn't exist
    if let Err(e) = std::fs::create_dir_all("logs") {
        eprintln!("Warning: Failed to create logs directory: {}", e);
    }

    let start_time = chrono::Local::now();
    let log_filename = format!("logs/TMR_{}.log", start_time.format("%Y-%m-%d_%H-%M-%S"));
    
    let log_level = env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string());
    let level_filter = match log_level.to_lowercase().as_str() {
        "error" => LevelFilter::Error,
        "warn" => LevelFilter::Warn,
        "info" => LevelFilter::Info,
        "debug" => LevelFilter::Debug,
        "trace" => LevelFilter::Trace,
        _ => LevelFilter::Info,
    };

    // Initialize file logger for dual logging
    if let Ok(log_file) = std::fs::File::create(&log_filename) {
        FILE_LOGGER.set(Arc::new(Mutex::new(Some(log_file)))).unwrap_or(());
    }

    // Set up env_logger for console with original nice formatting (colors + module info)
    Builder::new()
        .filter_level(level_filter)
        .format(|buf, record| {
            // Clear any existing progress line and ensure we start on a new line
            // This prevents log messages from getting merged with progress output
            if record.level() <= log::Level::Warn {
                // For warnings and errors, always clear the line and add emphasis
                print!("\r\x1b[K\n"); // Clear line and add newline for visibility
            } else {
                // For info/debug, just clear the current line
                print!("\r\x1b[K");
            }
            let _ = std::io::stdout().flush();
            
            // Write to console with original env_logger format
            let console_result = writeln!(
                buf,
                "\x1b[{}m[{} {} {}]\x1b[0m {}",
                match record.level() {
                    log::Level::Error => "31", // Red
                    log::Level::Warn => "33",  // Yellow
                    log::Level::Info => "32",  // Green
                    log::Level::Debug => "36", // Cyan
                    log::Level::Trace => "35", // Magenta
                },
                chrono::Local::now().format("%Y-%m-%dT%H:%M:%SZ"),
                record.level(),
                record.module_path().unwrap_or("unknown"),
                record.args()
            );

            // Also write to file without colors
            if let Some(file_logger) = FILE_LOGGER.get() {
                if let Ok(mut file_guard) = file_logger.lock() {
                    if let Some(ref mut file) = *file_guard {
                        let _ = writeln!(
                            file,
                            "[{} {} {}] {}",
                            chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f"),
                            record.level(),
                            record.module_path().unwrap_or("unknown"),
                            record.args()
                        );
                        let _ = file.flush();
                    }
                }
            }

            console_result
        })
        .target(env_logger::Target::Stdout)
        .init();

    // Log startup info
    log::info!("TMR v1.0.0 started - console logging with colors and module info restored");
    log::info!("Log level: {} - detailed logs also saved to {}", log_level, log_filename);
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
                println!("Warning: Invalid percentage {}%, using default 20%", percent);
                MemoryStrategy::default()
            }
            Err(_) => {
                println!("Warning: Could not parse percentage '{}', using default 20%", param);
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
                println!("Warning: Invalid GiB value {}, using default 20%", gib);
                MemoryStrategy::default()
            }
            Err(_) => {
                println!("Warning: Could not parse GiB value '{}', using default 20%", param);
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
                println!("Warning: Invalid MiB value {}, using default 20%", mib);
                MemoryStrategy::default()
            }
            Err(_) => {
                println!("Warning: Could not parse MiB value '{}', using default 20%", param);
                MemoryStrategy::default()
            }
        }
    } else if param.to_lowercase() == "tm5" {
        MemoryStrategy::tm5_compatible(880, 128)
    } else {
        println!("Warning: Unknown memory parameter format '{}', using default", param);
        println!("  Supported formats: 20%, 2GiB, 1024MiB, tm5");
        MemoryStrategy::default()
    }
}

// Add NUMA system info display
fn display_numa_info() {
    let cpu_count = num_cpus::get();
    let physical_cores = num_cpus::get_physical();
    
    println!("  CPU Configuration:");
    println!("    Logical CPUs: {}", cpu_count);
    println!("    Physical cores: {}", physical_cores);
    
    // Estimate NUMA nodes based on system size
    let estimated_numa_nodes = if cpu_count >= 32 { 2 } else { 1 };
    println!("    Estimated NUMA nodes: {}", estimated_numa_nodes);
    
    // Show CPU to NUMA mapping for first few CPUs
    if estimated_numa_nodes > 1 {
        println!("  CPU to NUMA mapping (estimated):");
        for cpu in 0..8.min(cpu_count) {
            let node = get_numa_node_for_cpu(cpu);
            println!("    CPU {} -> NUMA node {}", cpu, node);
        }
        if cpu_count > 8 {
            println!("    ... and {} more CPUs", cpu_count - 8);
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

fn detect_memory_capabilities(use_driver_chunking: bool) -> RuntimeConfig {
    let driver_available = tmr::dma_memory::DmaBuffer::is_driver_available_and_compatible();
    let large_pages_available = tmr::check_large_page_privilege().is_ok();
    
    let memory_backend = if driver_available {
        MemoryBackend::KernelDriver
    } else if large_pages_available {
        MemoryBackend::NativeLargePages
    } else {
        MemoryBackend::NativeRegular
    };
    
    RuntimeConfig {
        memory_backend,
        driver_available,
        large_pages_available,
		use_driver_chunking,
    }
}


fn format_duration(duration: std::time::Duration) -> String {
    // Format runtime as HH:MM:SS
    let total_seconds = duration.as_secs();
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;
    format!("{:02}:{:02}:{:02}", hours, minutes, seconds)
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
    println!("  {} --compare-results baseline.json current.json [output.json]", program_name);
    println!("                                          # Compare two test results from .\\results\\");
    println!("  {} --version                         # Show version information", program_name);
    println!();
    println!("COMMAND LINE PARAMETERS:");
    println!("  memory=20%                           # Reserve 20% of system memory");
    println!("  memory=2GiB                         # Reserve 2 GiB");
    println!("  memory=tm5                          # TM5-compatible allocation");
    println!("  cycles=5                            # Run 5 complete test cycles");
    println!("  duration=600                        # Maximum 10 minutes runtime");
    println!("  cpus=50%                            # Use 50% of available CPUs");
    println!("  cputype=cores                       # Use physical cores (vs threads)");
    println!("  errors=halt                         # Stop on first error");
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

fn print_usage(program_name: &str) {
    println!("Quick Usage Examples:");
    println!("  {} memory=20%                   # Reserve 20% of system memory", program_name);
    println!("  {} memory=2GiB                  # Reserve 2 GiB", program_name);
    println!("  {} memory=tm5                   # TM5-compatible allocation", program_name);
    println!("  {} cycles=5 duration=600        # 5 cycles OR 10 minutes max", program_name);
    println!("  {} cpus=50% cputype=cores       # Use 50% of CPU cores", program_name);
    println!("  {} errors=halt                  # Stop on first error", program_name);
    println!("  {} config=test.json             # Load comprehensive JSON config", program_name);
    println!("  {} config=legacy.cfg            # Auto-convert TM5 config + add stuck bit test", program_name);
    println!("  {} --create-demo-configs        # Create demo configurations", program_name);
    println!("  {} --compare-results old.json new.json # Compare results from .\\results\\", program_name);
    println!();
    println!("For full help: {} --help", program_name);
}