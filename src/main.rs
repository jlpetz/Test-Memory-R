use std::env;
use std::collections::HashMap;
use std::io::{stdin, stdout};
use std::io::Write;						// Needed for flush()
use std::sync::{Arc, Mutex};
use log::LevelFilter;
use env_logger::Builder;
use windows::Win32::Storage::FileSystem::{GetFileVersionInfoW, GetFileVersionInfoSizeW, VerQueryValueW};

use tmr::{create_demo_configs, load_config, ErrorMode};
use tmr::params;  // Centralized parameter registry
// Note: Legacy MemoryLayout still needed for runner interface
use tmr::memory::allocation_strategy::EnhancedMemoryStrategy;
use tmr::runner::{run_tests_with_layout_and_timing_filtered, TestSuiteTiming, print_current_memory_status, detect_runtime_capabilities};
use tmr::cpu_topology::{display_cpu_topology, get_cpu_topology, is_hybrid_cpu, CoreType};
use tmr::results::compare_results_command;
use tmr::{reset_driver, check_and_display_driver_status, DriverStatus, refresh_driver_status, is_driver_connected, display_driver_stats, compare_app_vs_driver_stats, reset_app_driver_stats};
use tmr::driver::DriverHandle;
use tmr::config::{MemoryAllocationConfig, CpuPinningConfig};

// Global file logger for dual console+file logging
static FILE_LOGGER: std::sync::OnceLock<Arc<Mutex<Option<std::fs::File>>>> = std::sync::OnceLock::new();

/// Vectored Exception Handler — catches hardware crashes (access violations, illegal
/// instructions, stack overflows) that Rust's panic handler cannot see.
/// Logs the faulting instruction, target address, and access type before the process dies.
unsafe extern "system" fn crash_exception_handler(
    info: *mut windows::Win32::System::Diagnostics::Debug::EXCEPTION_POINTERS,
) -> i32 {
    use windows::Win32::Foundation::{
        EXCEPTION_ACCESS_VIOLATION, STATUS_ILLEGAL_INSTRUCTION,
        STATUS_STACK_OVERFLOW, STATUS_STACK_BUFFER_OVERRUN,
    };

    if info.is_null() { return 0; } // EXCEPTION_CONTINUE_SEARCH
    let ptrs = unsafe { &*info };
    if ptrs.ExceptionRecord.is_null() { return 0; }
    let record = unsafe { &*ptrs.ExceptionRecord };

    let code = record.ExceptionCode;
    let ip = record.ExceptionAddress as usize;

    // Only handle fatal exceptions — let debugger breakpoints etc. pass through
    let label = if code == EXCEPTION_ACCESS_VIOLATION {
        "ACCESS_VIOLATION"
    } else if code == STATUS_ILLEGAL_INSTRUCTION {
        "ILLEGAL_INSTRUCTION"
    } else if code == STATUS_STACK_OVERFLOW {
        "STACK_OVERFLOW"
    } else if code == STATUS_STACK_BUFFER_OVERRUN {
        "STACK_BUFFER_OVERRUN"
    } else {
        return 0; // Not our problem — pass to next handler
    };

    // Compute RVA from module base (works regardless of ASLR)
    let module_base = unsafe {
        windows::Win32::System::LibraryLoader::GetModuleHandleA(None)
            .map(|h| h.0 as usize).unwrap_or(0)
    };
    let rva = if module_base > 0 && ip >= module_base { ip - module_base } else { ip };

    // For access violations, ExceptionInformation[0] = access type, [1] = target address
    if code == EXCEPTION_ACCESS_VIOLATION && record.NumberParameters >= 2 {
        let access_type = match record.ExceptionInformation[0] {
            0 => "READ from",
            1 => "WRITE to",
            8 => "DEP violation at",
            _ => "UNKNOWN access at",
        };
        let target_addr = record.ExceptionInformation[1];
        eprintln!("\n!!! FATAL: {} (0x{:08X}) at IP={:#x} (RVA={:#x})", label, code.0, ip, rva);
        eprintln!("!!!   {} address {:#x}", access_type, target_addr);
    } else {
        eprintln!("\n!!! FATAL: {} (0x{:08X}) at IP={:#x} (RVA={:#x})", label, code.0, ip, rva);
    }
    eprintln!("!!! Module base: {:#x}", module_base);

    // Return EXCEPTION_CONTINUE_SEARCH (0) — let the default handler terminate the process
    // after we've logged. Using 1 (EXCEPTION_CONTINUE_EXECUTION) would loop forever.
    0
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // --- Crash handling: install BEFORE anything else ---

    // 1. Rust panic hook — ensures panic messages reach stderr + log before abort
    std::panic::set_hook(Box::new(|info| {
        let msg = if let Some(s) = info.payload().downcast_ref::<&str>() {
            s.to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "unknown panic".to_string()
        };
        let location = info.location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "unknown location".to_string());
        eprintln!("\n!!! PANIC at {}: {}", location, msg);
        log::error!("PANIC at {}: {}", location, msg);
    }));

    // 2. Windows VEH — catches hardware exceptions (segfaults, illegal instructions)
    unsafe {
        windows::Win32::System::Diagnostics::Debug::AddVectoredExceptionHandler(
            1, // first=1: our handler runs BEFORE any other handlers
            Some(crash_exception_handler),
        );
    }

    // Initialize enhanced logging with file output
    setup_logging();

    println!("🚀 Test Memory R (TMR) v1.0.0 - High-Performance Memory Testing Tool");
    println!("================================================================================");

    let mut args: Vec<String> = env::args().collect();

    // Handle test= parameter for filtering tests by pattern
    let mut test_filter_param: Option<String> = None;
    for arg in &args {
        if let Some(test_pattern) = arg.strip_prefix("test=") {
            test_filter_param = Some(test_pattern.to_string());
            println!("🎯 Test Filter: {}", test_pattern);
            println!("  Running tests matching pattern");
            println!("  Default: cycles=1 for quick targeted runs");
            println!("  CLI overrides supported (e.g. duration=60 cycles=5)\n");
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

    // Validate that all -- flags are recognized before processing
    // This catches typos and deprecated flags early with helpful error messages
    let known_flags = [
        "--ram-latency", "--cache-latency", "--quick-test", "--calibrate-cache", "--calibrate-cache-ext",
        "--calibration-file", "--output",
        "--create-demo-configs", "--compare-results", "--debug-topology",
        "--show-topology", "--setup-large-pages", "--help", "-h", "--version", "-v"
    ];

    let unknown_flags: Vec<String> = args.iter()
        .filter(|arg| arg.starts_with("--") || arg.starts_with("-"))
        .filter(|arg| {
            // Check if it matches any known flag (handle --flag=value style)
            !known_flags.iter().any(|known| {
                arg.starts_with(known) || arg == known
            })
        })
        .cloned()
        .collect();

    if !unknown_flags.is_empty() {
        println!("❌ Error: Unknown parameter(s) detected:");
        for flag in &unknown_flags {
            println!("  {}", flag);
        }
        println!();
        println!("Note: --latency-test has been renamed to --ram-latency");
        println!();
        params::print_help(&args[0]);
        return Ok(());
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
            ("memory", "10%-from-available:start=split:auto"),
            ("cpus", "50%"),  // Multi-threaded for realistic DRAM behavior
            ("cycles", "1"),  // Single cycle for quick latency measurement
        ];

        for (key, default_value) in ram_latency_defaults.drain(..) {
            let param_prefix = format!("{}=", key);
            if !args.iter().any(|a| a.starts_with(&param_prefix)) {
                args.push(format!("{}={}", key, default_value));
            }
        }

        // Set filter to run DRAM latency tests (6 tests: DRAM-* and DRAMFull-*)
        test_filter = Some("DRAM*".to_string());
    }

    // Handle --cache-latency: Full cache hierarchy diagnostic (single-thread)
    // Tests: Read/Write/Copy × (L1, L2, L3, DRAM) = 12 tests
    // Uses 1 CPU for clean single-threaded measurements
    let cache_latency_idx = args.iter().position(|a| a == "--cache-latency");
    if cache_latency_idx.is_some() {
        args.retain(|a| a != "--cache-latency");

        // Inject cache-latency defaults: single thread for clean measurements
        let mut cache_latency_defaults = vec![
            ("memory", "10%-from-available:start=split:auto"),
            ("cpus", "1"),  // Single physical core for diagnostic-quality measurements
            ("cycles", "1"),  // Single cycle for quick latency measurement
        ];

        for (key, default_value) in cache_latency_defaults.drain(..) {
            let param_prefix = format!("{}=", key);
            if !args.iter().any(|a| a.starts_with(&param_prefix)) {
                args.push(format!("{}={}", key, default_value));
            }
        }

        // Set filter to run cache hierarchy tests (12 tests: L1-*, L2-*, L3-*, DRAM-*)
        // Note: Excludes DRAMFull-* tests (those are for --ram-latency)
        test_filter = Some("L*,DRAM-*".to_string());
    }

    // Handle --quick-test by injecting defaults BEFORE parameter parsing
    // This allows CLI overrides to work: --quick-test cycles=2 skip-cores=0
    let has_quick_test = args.iter().any(|a| a == "--quick-test");
    if has_quick_test {
        println!("🚀 TMR Quick Test Mode");
        println!("  Quick test uses reduced memory (60% reserve) for faster validation");
        println!("  All diagnostics and checks included, CLI overrides supported\n");

        // Inject quick-test defaults (only if not already specified)
        let mut quick_defaults = vec![
            ("memory", "60%-from-available:start=split:auto"),
            ("cycles", "1"),
            ("skip-cores", "1"),
            ("cpus", "100%"),
        ];

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
			"--debug-topology" => {
                println!("🔍 Debugging CPU Topology Detection");
                tmr::cpu_topology::debug_topology_detection();
                return Ok(());
            }
			"--show-topology" => {
				println!("🔍 Complete CPU Topology Mapping");
				tmr::cpu_topology::show_complete_topology_mapping();
				return Ok(());
			}
			// --cache-latency and --ram-latency are handled above
			// They use the unified test flow with filters
			"--setup-large-pages" => {
				println!("🔧 TMR Large Page Setup Tool");
				println!("=============================\n");
				
				let diagnostic = tmr::memory::diagnose_and_setup_large_pages();
				println!("{}", diagnostic);
				
				// Test final state
				match tmr::check_large_page_privilege() {
					Ok(()) => {
						println!("\n🎉 SUCCESS: Large pages are now enabled and working!");
						println!("You can now run TMR normally for optimal performance.");
					}
					Err(e) => {
						println!("\n⚠️  Large pages are still not available: {}", e);
						println!("TMR will still work but will use standard pages.");
						println!("This may require a system restart if privileges were just granted.");
					}
				}
				return Ok(());
			}
			"--calibrate-cache" => {
				println!("🔬 TMR Adaptive Cache Calibration");
				println!("==================================\n");
				
				// Detect cache info
				let cache_info = tmr::CacheInfo::detect();
				cache_info.print_info();
				println!();
				
				// Check for maxpage= override in remaining args
				let mut config = tmr::calibration::CalibrationConfig::default();
				for arg in &args[2..] {
					if let Some(page_str) = arg.strip_prefix("maxpage=") {
						config.page_size = page_str.to_lowercase();
						println!("📄 Page size override: {}\n", config.page_size);
					}
				}
				
				// Run calibration
				let calibration_test = tmr::calibration::CalibrationTest::with_config(cache_info.clone(), config);
				match calibration_test.run() {
					Ok(results) => {
						tmr::calibration::CalibrationTest::display_results(&results, &cache_info);
						
						// Check for --output flag to save results
						if let Some(output_idx) = args.iter().position(|a| a == "--output") {
							if let Some(output_path) = args.get(output_idx + 1) {
								match serde_json::to_string_pretty(&results) {
									Ok(json) => {
										match std::fs::write(output_path, json) {
											Ok(()) => println!("✅ Results saved to: {}", output_path),
											Err(e) => println!("❌ Failed to write results: {}", e),
										}
									}
									Err(e) => println!("❌ Failed to serialize results: {}", e),
								}
							} else {
								println!("❌ --output requires a file path");
							}
						}
					}
					Err(e) => {
						println!("❌ Calibration failed: {}", e);
					}
				}
				return Ok(());
			}
			"--calibrate-cache-ext" => {
				println!("🔬 TMR Adaptive Cache Calibration (Extended - Phase 2)");
				println!("======================================================\n");
				
				// Detect cache info
				let cache_info = tmr::CacheInfo::detect();
				cache_info.print_info();
				println!();
				
				// Check for maxpage= override in remaining args
				let mut config = tmr::calibration::CalibrationConfig::default();
				for arg in &args[2..] {
					if let Some(page_str) = arg.strip_prefix("maxpage=") {
						config.page_size = page_str.to_lowercase();
						println!("📄 Page size override: {}\n", config.page_size);
					}
				}
				
				// Run extended calibration (phase 1 sweep + phase 2 fine-grain)
				let calibration_test = tmr::calibration::CalibrationTest::with_config(cache_info.clone(), config);
				match calibration_test.run_extended() {
					Ok(results) => {
						tmr::calibration::CalibrationTest::display_results(&results, &cache_info);
						
						// Check for --output flag to save results
						if let Some(output_idx) = args.iter().position(|a| a == "--output") {
							if let Some(output_path) = args.get(output_idx + 1) {
								match serde_json::to_string_pretty(&results) {
									Ok(json) => {
										match std::fs::write(output_path, json) {
											Ok(()) => println!("✅ Results saved to: {}", output_path),
											Err(e) => println!("❌ Failed to write results: {}", e),
										}
									}
									Err(e) => println!("❌ Failed to serialize results: {}", e),
								}
							} else {
								println!("❌ --output requires a file path");
							}
						}
					}
					Err(e) => {
						println!("❌ Extended calibration failed: {}", e);
					}
				}
				return Ok(());
			}
            "--help" | "-h" => {
                params::print_help(&args[0]);
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
	

    // Parse and validate ALL command-line arguments using centralized registry
    let validated_params = match params::parse_and_validate_args(&args[1..]) {
        Ok(params) => params,
        Err(e) => {
            eprintln!("{}", e);
            std::process::exit(1);
        }
    };

    // Check for config file parameter
    let config_file = params::get_string(&validated_params, "config", "");
    let config_file = if config_file.is_empty() { None } else { Some(config_file) };

	let (mut enhanced_memory_strategy, mut error_mode, mut suite_timing, mut cputype, mut cpus, mut pinning_config, mut alloc_config, config_opt) =
		if let Some(ref config_path) = config_file {
			let config = load_config(config_path)?;
			(
				config.to_memory_strategy(),
				config.to_error_mode(),
				config.to_test_suite_timing(),
				config.system.cpu_config.cpu_type.clone(),
				format!("{}%", config.system.cpu_config.usage_percent),
				config.system.cpu_pinning.clone(),
				config.system.memory_allocation.clone(), // Use the config's allocation settings
				Some(config) // Keep the full config for test sequence
			)
		} else {
			// Build config from validated command-line parameters
			let (enhanced_memory_strategy, error_mode, suite_timing, cputype, cpus, pinning_config, alloc_config) =
				build_config_from_validated_params(&validated_params)?;
			(enhanced_memory_strategy, error_mode, suite_timing, cputype, cpus, pinning_config, alloc_config, None)
		};

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
								println!("❌ Invalid memory override '{}': {}", memory_str, e);
								std::process::exit(1);
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
					pinning_config.cpus_to_skip = params::get_usize(&validated_params, "skip-cores", 1);
					log::debug!("CLI override: cpus_to_skip = {}", pinning_config.cpus_to_skip);
				}
				"--disable-pinning" => {
					pinning_config.enable_pinning = false;
					log::debug!("CLI override: enable_pinning = false");
				}
				"allocator" => {
					if let params::ParamValue::String(allocator_str) = value {
						use tmr::memory::allocator::AllocationStrategy;
						match allocator_str.parse::<AllocationStrategy>() {
							Ok(strategy) => {
								alloc_config.allocation_strategy = strategy.to_string();
								log::debug!("CLI override: allocation_strategy = {}", strategy);
							}
							Err(e) => {
								println!("❌ Invalid allocator override '{}': {}", allocator_str, e);
								std::process::exit(1);
							}
						}
					}
				}
				"topology" => {
					if let params::ParamValue::String(topology_str) = value {
						use tmr::cpu_topology::{set_topology_detection_method, TopologyDetectionMethod};
						let method = match topology_str.to_lowercase().as_str() {
							"windows" | "windowsapi" => TopologyDetectionMethod::WindowsApi,
							"windowsv2" | "v2" => TopologyDetectionMethod::WindowsApiV2,
							"cpuid" => TopologyDetectionMethod::CpuidBased,
							"auto" => TopologyDetectionMethod::Auto,
							_ => TopologyDetectionMethod::Auto,
						};
						set_topology_detection_method(method);
						log::debug!("CLI override: topology_detection_method = {:?}", method);
					}
				}
				"--driver-chunking" => {
					alloc_config.driver_chunking = true;
					log::debug!("CLI override: driver_chunking = true");
				}
				"--batch-remap" => {
					alloc_config.remap_mode = "batch".to_string();
					log::debug!("CLI override: remap_mode = batch");
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
				_ => {} // Ignore unknown overrides
			}
		}
	}

    // Display startup mode and parameters
    println!("Startup Mode & Parameters:");
    println!("  Command Line: {}", args.join(" "));

    // Display build type (helps identify performance issues in logs)
    let (build_type, build_icon) = if cfg!(debug_assertions) {
        ("Debug (unoptimized)", "❌")
    } else {
        ("Release (optimized)", "✅")
    };
    println!("  Build Type: {} {}", build_icon, build_type);
    log::info!("TMR v1.0.0 starting - {} build on {}", build_type, std::env::consts::ARCH);

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
            default_params.push("memory=10%-from-available:start=split:auto (default)".to_string());
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
	
    // Calculate CPU/thread count
	let total_cpus = match cputype.as_str() {
		"cores" => num_cpus::get_physical(),
		_ => num_cpus::get(),
	};

	// When using cores mode, we should automatically avoid SMT doubling
	let avoid_smt = cputype == "cores";
	if avoid_smt {
		println!("  CPU Type: {} (avoiding SMT/Hyperthreading)", cputype);
	} else {
		println!("  CPU Type: {}", cputype);
	}

	// Calculate available CPUs after accounting for skipped cores
	let available_cpus = if pinning_config.cpus_to_skip > 0 {
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
		let skipped_logical: usize = sorted_cores.iter()
			.take(pinning_config.cpus_to_skip)
			.map(|(_, count)| *count)
			.sum();
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

	let threads = if cpus.ends_with('%') {
		// Percentage of available CPUs
		let percent = cpus.trim_end_matches('%').parse::<u32>().unwrap_or(100);
		// Round to nearest instead of truncating: +50 before dividing by 100
		((available_cpus as u32 * percent + 50) / 100).max(1).min(available_cpus as u32) as usize
	} else {
		// Absolute count
		cpus.parse::<usize>().unwrap_or(available_cpus).min(available_cpus).max(1)
	};
		
	let (actual_threads, cpu_list) = if pinning_config.enable_pinning {
		// Override avoid_smt_doubling if cputype=cores
		let effective_avoid_smt = pinning_config.avoid_smt_doubling || avoid_smt;
		
		calculate_thread_allocation(
			threads,
			pinning_config.cpus_to_skip,
			effective_avoid_smt
		)
	} else {
		// No pinning, use all requested threads
		(threads, (0..threads).collect())
	};
	
	print!("  Error Mode: ");
    match error_mode {
        ErrorMode::Log => println!("Log and continue"),
        ErrorMode::Halt => println!("Halt on first error"),
        ErrorMode::Panic => println!("Panic on error (debug mode)"),
    }
	
	println!("  Using {}/{} {} for testing", actual_threads, available_cpus, cputype);
	if pinning_config.enable_pinning {
		println!("  CPU Assignment: {:?}", cpu_list);
		if pinning_config.cpus_to_skip > 0 {
			println!("  Skipping first {} CPU(s) for system responsiveness", pinning_config.cpus_to_skip);
		}
		if pinning_config.avoid_smt_doubling {
			println!("  Avoiding SMT doubling (using physical cores only)");
		}
	}

	let use_batch_remap = args.iter().any(|arg| arg == "--batch-remap");
	if use_batch_remap {
		tmr::set_use_remap_all(false);
		println!("  Remap Mode: Batch remapping (original implementation)");
	} else {
		tmr::set_use_remap_all(true);
		println!("  Remap Mode: Remap all (optimized for TMR)");
	}
	
	// Display timing configuration
    print!("  Test Suite Timing: ");
    match (&suite_timing.global_cycles, &suite_timing.global_duration_secs) {
        (Some(cycles), Some(duration)) => println!("{} cycles or {}s max (whichever first)", cycles, duration),
        (Some(cycles), None) => println!("{} cycles (no time limit)", cycles),
        (None, Some(duration)) => println!("{}s duration (unlimited cycles)", duration),
        (None, None) => println!("Unlimited cycles and duration"),
    }
	// Detect SIMD capabilities
    let simd_caps = tmr::detect_simd_capabilities();
    println!("  SIMD Support: {}", simd_caps);
    println!();
		
	println!("Checking Large Page access");

	// Check for restart needed scenario first
	if tmr::check_restart_needed() {
		println!("⚠️  LARGE PAGE SETUP REQUIRES RESTART");
		println!("=====================================");
		println!("The Large Page privilege has been configured but requires a restart to take effect.");
		println!();
		println!("Choose your preferred action:");
		println!("  A) Exit TMR and restart system (RECOMMENDED for optimal performance)");
		println!("  B) Continue with standard 4KB pages (5-10% performance impact)");
		println!();
		print!("Enter your choice (A/B): ");
		stdout().flush().unwrap();
		
		let mut input = String::new();
		stdin().read_line(&mut input).unwrap();
		let choice = input.trim().to_uppercase();
		
		match choice.as_str() {
			"A" => {
				println!();
				println!("✅ Restart recommended for optimal performance.");
				println!("After restart, TMR will automatically use large pages.");
				println!();
				println!("To restart:");
				println!("  • Windows: shutdown /r /t 0");
				println!("  • Or use Start Menu → Power → Restart");
				println!();
				println!("TMR will exit now.");
				return Ok(());
			}
			"B" => {
				println!();
				println!("⚠️  Continuing with standard 4KB pages.");
				println!("   Performance impact: ~5-10% slower than large pages");
				println!("   Test accuracy: Unaffected (still comprehensive)");
				println!();
				println!("💡 TIP: Restart TMR after rebooting for optimal performance");
				println!();
			}
			_ => {
				println!();
				println!("❌ Invalid choice. Defaulting to continue with 4KB pages.");
				println!();
			}
		}
	} else {
		// Normal large page setup flow
		let large_page_diagnostic = tmr::memory::diagnose_and_setup_large_pages();
		println!("{}", large_page_diagnostic);
	}

	match tmr::check_large_page_privilege() {
		Ok(()) => {
			println!("🎉 Large Pages: ✅ ENABLED and tested successfully!");
			println!("   TMR will use 2MB large pages for optimal performance");
		},
		Err(_) => {
			println!("⚠️  Large Pages: Not available - TMR will use standard 4KB pages");
			println!("   Performance impact: ~5-10% slower memory allocation");
			println!("   This is normal on some systems and doesn't affect test accuracy");
		}
	}
	
	// Check KMDF version before attempting to use the driver
	let kmdf_compatible = match verify_kmdf_compatibility() {
		Ok((major, minor)) => {
			println!("  KMDF Framework: ✅ v{}.{} (Compatible)", major, minor);
			true
		}
		Err(e) => {
			println!("  KMDF Framework: ⚠️  {}", e);
			println!("    Impact: TMR kernel driver will not be available");
			println!("    Note: Driver requires Windows 10 version 2004 or later (KMDF 1.33+)");
			false
		}
	};
	
	let use_driver_chunking = args.iter().any(|arg| arg == "--driver-chunking");
	if use_driver_chunking {
		println!("  Use Driver Chunking: ❌ Enabled (for driver testing, driver controls page allocation strategy)");
	} else {
		println!("  Use Driver Chunking: ✅ Disabled (default/good, application controls page allocation strategy)");
	}

	// Only check driver status if KMDF is compatible
	if kmdf_compatible {
		// Use formal reporting system for driver status
		{
			use tmr::reporting::{create_console_reporter, models::{DriverStatusReport, DriverVersion}};
			
			let driver_status = check_and_display_driver_status();
			
			let (available, version, error_message, statistics) = match &driver_status {
				DriverStatus::Available(ver) => {
					let driver_version = Some(DriverVersion {
						major: ver.driver_version_major as u16,
						minor: ver.driver_version_minor as u16,
						build: ver.driver_version_build as u16,
						revision: ver.driver_version_revision as u16,
					});
					
					// Statistics not easily available from current interface, so None for now
					(true, driver_version, None, None)
				},
				DriverStatus::NotFound => {
					(false, None, Some("TMR kernel driver not found or not accessible".to_string()), None)
				},
				DriverStatus::VersionMismatch { .. } => {
					(false, None, Some("Driver version incompatible with application".to_string()), None)
				},
				DriverStatus::Error(e) => {
					(false, None, Some(e.clone()), None)
				},
			};
			
			let report = DriverStatusReport {
				available,
				version,
				error_message,
				statistics,
			};
			
			let mut reporter = create_console_reporter();
			if let Err(e) = reporter.report_driver_status(&report) {
				log::error!("Failed to display driver status report: {}", e);
				// Fallback to basic output
				if available {
					println!("  DMA Driver: ✅ Available");
				} else {
					println!("  DMA Driver: ❌ Not Found");
				}
			}
		}
		
		// Reset allocations if driver is available
		if matches!(check_and_display_driver_status(), DriverStatus::Available(_)) {
			reset_driver();
		}
	} else {
		println!("  DMA Driver: ⚠️  Skipped (KMDF version too old)");
	}
	println!();
	
    // Get system info for later use (formatted output comes via reporter below)
    let system_info = tmr::tests::get_system_info();
    let cache_info = system_info.get_cache_info();

	// Display CPU topology right after system information
	// Add this after the cache architecture display (around line 220-230):
	if pinning_config.enable_pinning {
		display_cpu_topology(&cpu_list, pinning_config.cpus_to_skip, avoid_smt);
	} else {
		println!("\nCPU Thread Assignment: No pinning - threads will be scheduled by OS");
	}
	println!();
	
	// Show memory stats and get cached memory info
	let cached_memory_info = print_current_memory_status();
	
	// Add formal system info reporting using cached memory info (no duplicate Windows API calls)
	if let Some(ref memory_info) = cached_memory_info {
		use tmr::reporting::{create_console_reporter, system_info_builder::build_system_info_report};
		
		let report = build_system_info_report(
			system_info,
			memory_info,
			get_cpu_topology(),
			&simd_caps,
			&cpu_list
		);
		
		let mut reporter = create_console_reporter();
		if let Err(e) = reporter.report_system_info(&report) {
			log::error!("Failed to display system info report: {}", e);
			// Manual system info already displayed above as fallback
		}
	}
	
	println!();
    
    // Calculate memory layout using enhanced system
    let enhanced_layout = enhanced_memory_strategy.create_layout(actual_threads)?;

    // Display memory strategy information
    println!("  Memory Strategy: {}", enhanced_layout.allocation_result.allocation_type);
    println!("    Allocation: {:.2} GiB, Reserve: {:.2} GiB", 
             enhanced_layout.allocation_result.allocation_bytes as f64 / 1024_f64.powi(3),
             enhanced_layout.allocation_result.reserve_bytes as f64 / 1024_f64.powi(3));
    
    println!("    Window/Chunk Modes: Per-test configuration (see test sequence below)");
	println!("  ");

    println!("Starting comprehensive memory tests... Use CTRL+C for graceful shutdown with final report");
    println!("(detailed logs available with RUST_LOG=debug)");
    println!();

	// Register CTRL+C handler for graceful shutdown
	// This allows tests to stop at next checkpoint and print final reports
	ctrlc::set_handler(move || {
		use std::sync::atomic::Ordering;
		println!("\n\n🛑 CTRL+C received - initiating graceful shutdown...");
		println!("   Tests will stop at next checkpoint and print final report");
		println!("   (Press CTRL+C again to force immediate exit)\n");
		tmr::runner::SHUTDOWN_REQUESTED.store(true, Ordering::Relaxed);
	}).expect("Error setting CTRL+C handler");

	let mut runtime_config = detect_runtime_capabilities(&alloc_config);
	runtime_config.cpu_list = Some(cpu_list);
	runtime_config.enhanced_memory_strategy = enhanced_memory_strategy.clone();

	// Extract streams parameter if provided (overrides hard-coded config)
	let streams_override = if params::has_param(&validated_params, "streams") {
		Some(params::get_usize(&validated_params, "streams", 1))
	} else {
		None
	};

	// Extract parameter override if provided (overrides subblock/stride config)
	let parameter_override = if params::has_param(&validated_params, "parameter") {
		Some(params::get_string(&validated_params, "parameter", "none"))
	} else {
		None
	};

	// Extract pattern-mode override if provided
	let pattern_mode_override = if params::has_param(&validated_params, "pattern-mode") {
		Some(params::get_u32(&validated_params, "pattern-mode", 0))
	} else {
		None
	};

	// Extract verify-reps override if provided
	let verify_reps_override = if params::has_param(&validated_params, "verify-reps") {
		Some(params::get_u32(&validated_params, "verify-reps", 1))
	} else {
		None
	};

	// Extract test-reps override if provided
	let test_reps_override = if params::has_param(&validated_params, "test-reps") {
		Some(params::get_u32(&validated_params, "test-reps", 1))
	} else {
		None
	};

	// Extract write-read-cycles override if provided
	let wrc_override = if params::has_param(&validated_params, "write-read-cycles") {
		Some(params::get_u32(&validated_params, "write-read-cycles", 1))
	} else {
		None
	};

    let start_time = std::time::Instant::now();

    // Unified test execution path - works for bandwidth tests, latency tests, or mixed
    // test_filter may be set by --ram-latency or --cache-latency to run specific test subsets
    // test_filter_param may be set by test=Pattern for CLI filtering
    let final_test_filter = test_filter.as_deref().or(test_filter_param.as_deref());

    let success = run_tests_with_layout_and_timing_filtered(
        enhanced_layout,
        error_mode,
        suite_timing,
        runtime_config,
        config_opt.as_ref(),
        final_test_filter,
        streams_override,
        parameter_override.as_deref(),
        pattern_mode_override,
        verify_reps_override,
        test_reps_override,
        wrc_override,
        cache_info,
    );

    let total_time = start_time.elapsed();

    println!();
    println!("================================================================================");
    if success {
        println!("✅ All memory tests completed successfully in {}", format_duration(total_time));
        println!("   Comprehensive testing completed with unified execution pipeline");
        println!("   Memory pressure maintained throughout testing with three-stage architecture");
    } else {
        println!("❌ Tests failed or encountered errors in {}", format_duration(total_time));
    }

    println!();
	
	if is_driver_connected() {
		// Gets fresh stats from existing handle
		if let Err(e) = compare_app_vs_driver_stats() {
			log::warn!("Failed to compare driver stats: {}", e);
		}
		
		reset_driver(); // Reset_driver, this tells the driver we are done and any allocation not already released should be released(fail-safe).
		
		// This will completely reset the apps driver handle and re-check version, for when we implement a GUI
		let new_status = refresh_driver_status();
		log::debug!("Driver status after reset: {:?}", new_status);
		reset_app_driver_stats(); // Reset the app stats since we reset the Driver, we want stats to align at zero

		// display_driver_stats(); // We can probably comment this out, as the below compare_app_vs_driver_stats will display both app and driver after reset.
		if let Err(e) = display_driver_stats() {
			log::warn!("Failed to display driver stats: {}", e);
		}
		
		// Gets fresh stats from existing handle, because we did a reset, things should be ZEROed
		if let Err(e) = compare_app_vs_driver_stats() {
			log::warn!("Failed to compare driver stats: {}", e);
		}
	}
	
	println!();
    params::print_usage(&args[0]);
	Ok(())
}

// Example usage in main.rs
pub fn check_dma_driver_status() {
    match DriverHandle::open() {
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

// In main.rs - Pre-calculate thread allocation
fn calculate_thread_allocation(
    requested_threads: usize,
    cpus_to_skip: usize,
    avoid_smt_doubling: bool,
) -> (usize, Vec<usize>) {
    let topology = get_cpu_topology(); // Use v2 here
    let is_hybrid = is_hybrid_cpu(topology);
    
    // Group logical CPUs by physical core
    let mut cores_map: HashMap<usize, Vec<(usize, CoreType)>> = HashMap::new();
    for cpu in topology {
        cores_map.entry(cpu.physical_core_id)
            .or_default()
            .push((cpu.logical_id, cpu.core_type));
    }
    
    // Separate P-cores and E-cores
	let mut p_cores: Vec<_> = cores_map.iter()
		.filter(|(_, cpus)| cpus.iter().any(|(_, t)| matches!(t, CoreType::Performance(_))))
		.map(|(id, cpus)| (*id, cpus.clone()))
		.collect();

	let mut e_cores: Vec<_> = cores_map.iter()
		.filter(|(_, cpus)| cpus.iter().any(|(_, t)| matches!(t, CoreType::Efficiency(_))))
		.map(|(id, cpus)| (*id, cpus.clone()))
		.collect();
    
    p_cores.sort_by_key(|(id, _)| *id);
    e_cores.sort_by_key(|(id, _)| *id);
    
    let mut selected_cpus = Vec::new();
    let mut cores_to_skip = cpus_to_skip;
    
    if is_hybrid {
        // Skip E-cores first (if we have any to skip)
        let e_cores_to_skip = cores_to_skip.min(e_cores.len());
        let e_cores_to_use = e_cores.iter().skip(e_cores_to_skip);
        
        // Update remaining cores to skip
        cores_to_skip = cores_to_skip.saturating_sub(e_cores.len());
        
        // Then skip some P-cores if needed
        let p_cores_to_skip = cores_to_skip.min(p_cores.len());
        let p_cores_to_use = p_cores.iter().skip(p_cores_to_skip);
        
        // Round-robin assignment: spread threads across physical cores first,
        // then fill in SMT siblings if more threads are needed.
        // This ensures better utilization when using partial CPU counts (e.g., cpus=50%)

        // Collect cores we'll use (respecting skip settings)
        let p_cores_vec: Vec<_> = p_cores_to_use.collect();
        let e_cores_vec: Vec<_> = e_cores_to_use.collect();

        // Pass 1: Take first thread from each P-core
        for (_, logical_cpus) in &p_cores_vec {
            if selected_cpus.len() >= requested_threads { break; }
            if let Some((cpu_id, _)) = logical_cpus.first() {
                selected_cpus.push(*cpu_id);
            }
        }

        // Pass 2: Take first thread from each E-core
        for (_, logical_cpus) in &e_cores_vec {
            if selected_cpus.len() >= requested_threads { break; }
            if let Some((cpu_id, _)) = logical_cpus.first() {
                selected_cpus.push(*cpu_id);
            }
        }

        // Pass 3 & 4: If not avoiding SMT and still need more, take SMT siblings
        if !avoid_smt_doubling && selected_cpus.len() < requested_threads {
            // Take SMT siblings from P-cores
            for (_, logical_cpus) in &p_cores_vec {
                for (cpu_id, _) in logical_cpus.iter().skip(1) {
                    if selected_cpus.len() >= requested_threads { break; }
                    selected_cpus.push(*cpu_id);
                }
            }

            // Take SMT siblings from E-cores
            for (_, logical_cpus) in &e_cores_vec {
                for (cpu_id, _) in logical_cpus.iter().skip(1) {
                    if selected_cpus.len() >= requested_threads { break; }
                    selected_cpus.push(*cpu_id);
                }
            }
        }
    } else {
        // Non-hybrid CPU - use round-robin assignment
        let mut physical_cores: Vec<_> = cores_map.keys().cloned().collect();
        physical_cores.sort();

        // Skip the first N cores
        let cores_to_use: Vec<_> = physical_cores.iter().skip(cores_to_skip).cloned().collect();

        // Pass 1: Take first thread from each physical core (round-robin)
        for physical_core in &cores_to_use {
            if selected_cpus.len() >= requested_threads { break; }
            if let Some(logical_cpus) = cores_map.get(physical_core) {
                if let Some((cpu_id, _)) = logical_cpus.first() {
                    selected_cpus.push(*cpu_id);
                }
            }
        }

        // Pass 2: If not avoiding SMT and still need more, take SMT siblings
        if !avoid_smt_doubling && selected_cpus.len() < requested_threads {
            for physical_core in &cores_to_use {
                if let Some(logical_cpus) = cores_map.get(physical_core) {
                    for (cpu_id, _) in logical_cpus.iter().skip(1) {
                        if selected_cpus.len() >= requested_threads { break; }
                        selected_cpus.push(*cpu_id);
                    }
                }
            }
        }
    }
    
    (selected_cpus.len(), selected_cpus)
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
                // For warnings and errors, clear the line (removed extra newline)
                print!("\r\x1b[K");
            } else {
                // For info/debug, just clear the current line
                print!("\r\x1b[K");
            }
            let _ = stdout().flush();
            
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
            if let Some(file_logger) = FILE_LOGGER.get()
                && let Ok(mut file_guard) = file_logger.lock()
                    && let Some(ref mut file) = *file_guard {
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

            console_result
        })
        .target(env_logger::Target::Stdout)
        .init();

    // Log startup info
    log::info!("TMR v1.0.0 started - console logging with colors and module info restored");
    log::info!("Log level: {} - detailed logs also saved to {}", log_level, log_filename);
}

/// Build configuration from validated parameters (using centralized registry)
fn build_config_from_validated_params(
    validated: &HashMap<String, params::ParamValue>
) -> Result<(EnhancedMemoryStrategy, ErrorMode, TestSuiteTiming, String, String, CpuPinningConfig, MemoryAllocationConfig), String> {
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

    // Build CPU pinning config
    let mut pinning_config = CpuPinningConfig::default();
    pinning_config.cpus_to_skip = params::get_usize(validated, "skip-cores", 1);
    if params::get_bool(validated, "--disable-pinning", false) {
        pinning_config.enable_pinning = false;
    }

    // Build memory allocation config
    let mut alloc_config = MemoryAllocationConfig::default();

    // Handle allocator if specified (requires special parsing)
    if let Some(params::ParamValue::String(allocator_str)) = validated.get("allocator") {
        use tmr::memory::allocator::AllocationStrategy;
        match allocator_str.parse::<AllocationStrategy>() {
            Ok(strategy) => {
                alloc_config.allocation_strategy = strategy.to_string();
                println!("  Allocation Strategy: {}", strategy);
            }
            Err(e) => {
                return Err(format!("Invalid allocator '{}': {}", allocator_str, e));
            }
        }
    }

    // Handle driver flags
    if params::get_bool(validated, "--driver-chunking", false) {
        alloc_config.driver_chunking = true;
    }
    if params::get_bool(validated, "--batch-remap", false) {
        alloc_config.remap_mode = "batch".to_string();
    }

    // Handle page size overrides
    if let Some(params::ParamValue::String(page_str)) = validated.get("minpage") {
        alloc_config.min_page_size = page_str.to_string();
        println!("  Minimum Page Size: {}", page_str);
    }
    if let Some(params::ParamValue::String(page_str)) = validated.get("maxpage") {
        alloc_config.max_page_size = page_str.to_string();
        println!("  Maximum Page Size: {}", page_str);
    }

    // Handle topology (has side effect of setting global state)
    if let Some(params::ParamValue::String(topology_str)) = validated.get("topology") {
        use tmr::cpu_topology::{set_topology_detection_method, TopologyDetectionMethod};
        let method = match topology_str.to_lowercase().as_str() {
            "windows" | "windowsapi" => TopologyDetectionMethod::WindowsApi,
            "windowsv2" | "v2" => TopologyDetectionMethod::WindowsApiV2,
            "cpuid" => TopologyDetectionMethod::CpuidBased,
            "auto" => TopologyDetectionMethod::Auto,
            _ => TopologyDetectionMethod::Auto,
        };
        set_topology_detection_method(method);
        println!("  Topology Detection: {:?}", method);
    }

    // Print skip-cores if specified
    if params::has_param(validated, "skip-cores") {
        println!("  CPU Pinning: Skipping first {} CPU(s)", pinning_config.cpus_to_skip);
    }
    if !pinning_config.enable_pinning {
        println!("  CPU Pinning: Disabled");
    }

    Ok((enhanced_memory_strategy, error_mode, suite_timing, cputype, cpus, pinning_config, alloc_config))
}

// Add these functions to main.rs
fn check_kmdf_version() -> Result<(u16, u16), String> {
    unsafe {
        let file_path = windows::core::w!("C:\\Windows\\System32\\drivers\\Wdf01000.sys");
        
        // Get the size of version info
        let size = GetFileVersionInfoSizeW(file_path, None);
        if size == 0 {
            return Err("Failed to get KMDF version info size".to_string());
        }
        
        // Allocate buffer for version info
        let mut buffer = vec![0u8; size as usize];
        
        // Get version info
        if GetFileVersionInfoW(
            file_path,
            None,
            size,
            buffer.as_mut_ptr() as *mut std::ffi::c_void,
        ).is_err() {
            return Err("Failed to get KMDF version info".to_string());
        }
        
        // Query for VS_FIXEDFILEINFO
        let mut file_info_ptr: *mut std::ffi::c_void = std::ptr::null_mut();
        let mut len = 0u32;
        
        if !VerQueryValueW(
            buffer.as_ptr() as *const std::ffi::c_void,
            windows::core::w!("\\"),
            &mut file_info_ptr,
            &mut len,
        ).as_bool() {  // BOOL type uses .as_bool()
            return Err("Failed to query KMDF version value".to_string());
        }
        
        if file_info_ptr.is_null() || len == 0 {
            return Err("Invalid KMDF version info pointer".to_string());
        }
        
        // Cast to VS_FIXEDFILEINFO structure
        #[repr(C)]
        struct VS_FIXEDFILEINFO {
            dw_signature: u32,
            dw_struct_version: u32,
            dw_file_version_ms: u32,
            dw_file_version_ls: u32,
            dw_product_version_ms: u32,
            dw_product_version_ls: u32,
            dw_file_flags_mask: u32,
            dw_file_flags: u32,
            dw_file_os: u32,
            dw_file_type: u32,
            dw_file_subtype: u32,
            dw_file_date_ms: u32,
            dw_file_date_ls: u32,
        }
        
        let file_info = &*(file_info_ptr as *const VS_FIXEDFILEINFO);
        
        // Extract major and minor version from product version
        let major = (file_info.dw_product_version_ms >> 16) as u16;
        let minor = (file_info.dw_product_version_ms & 0xFFFF) as u16;
        
        Ok((major, minor))
    }
}

fn verify_kmdf_compatibility() -> Result<(u16, u16), String> {
    const REQUIRED_MAJOR: u16 = 1;
    const REQUIRED_MINOR: u16 = 33;
    
    match check_kmdf_version() {
        Ok((major, minor)) => {
            log::info!("KMDF version detected: {}.{}", major, minor);
            
            if major > REQUIRED_MAJOR || (major == REQUIRED_MAJOR && minor >= REQUIRED_MINOR) {
                log::info!("KMDF version {}.{} meets minimum requirement ({}.{})", 
                         major, minor, REQUIRED_MAJOR, REQUIRED_MINOR);
                Ok((major, minor))
            } else {
                Err(format!(
                    "KMDF version {}.{} is too old. Minimum required: {}.{}",
                    major, minor, REQUIRED_MAJOR, REQUIRED_MINOR
                ))
            }
        }
        Err(e) => Err(format!("Failed to check KMDF version: {}", e))
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


/// Parse enhanced memory parameter with clear reserve semantics
fn parse_enhanced_memory_parameter(param: &str) -> Result<tmr::memory::allocation_strategy::EnhancedMemoryStrategy, String> {
    use tmr::memory::allocation_strategy::{EnhancedMemoryStrategy, AllocationMode};
    
    let (allocation_mode, start_address_mode) = AllocationMode::parse(param)?;
    
    Ok(EnhancedMemoryStrategy {
        allocation_mode,
        start_address_mode,
    })
}
