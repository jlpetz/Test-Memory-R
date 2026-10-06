//! The command-line front end: argument handling, logging setup and the top-level run modes.

use std::env;
use std::collections::HashMap;
use std::io::{stdin, stdout};
use std::io::Write;						// Needed for flush()
use std::sync::{Arc, Mutex};
use log::LevelFilter;
use env_logger::Builder;

use crate::{create_demo_configs, load_config, ErrorMode};
use crate::params;  // Centralized parameter registry
// Note: Legacy MemoryLayout still needed for runner interface
use crate::memory::allocation_strategy::EnhancedMemoryStrategy;
use crate::runner::{run_tests_with_layout_and_timing_filtered, RunStatus, TestSuiteTiming, print_current_memory_status, detect_runtime_capabilities};
use crate::cpu_topology::{display_cpu_topology, get_cpu_topology, is_hybrid_cpu, CoreType};
use crate::results::compare_results_command;
use crate::config::{MemoryAllocationConfig, CpuPinningConfig};

/// `--ram-latency`: the 6 DRAM latency tests, DRAM-* and DRAMFull-* (TODO 92)
pub(crate) const RAM_LATENCY_FILTER: &str = "Lat-DRAM-*,Lat-DRAMFull-*";
/// `--cache-latency`: Read/Write/Copy at L1, L2, L3 and DRAM, 12 tests; DRAMFull is
/// `--ram-latency`'s (TODO 92)
pub(crate) const CACHE_LATENCY_FILTER: &str = "Lat-L1-*,Lat-L2-*,Lat-L3-*,Lat-DRAM-*";

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

/// Verify the running CPU has the instruction-set features TMR is built against.
///
/// TMR compiles at the `x86-64-v3` baseline (AVX2) and emits CLFLUSHOPT unconditionally
/// in the cache-flush path. CLFLUSHOPT is a standalone CPUID feature (not part of any
/// psABI microarch level), so it must be checked at runtime. Missing either feature means
/// the binary would hit an illegal-instruction fault during testing — so we detect it up
/// front, explain why, and exit cleanly. This excludes pre-2015 CPUs (Intel pre-Skylake,
/// incl. Haswell/Broadwell which have AVX2 but no CLFLUSHOPT; AMD pre-Excavator/pre-Zen),
/// all of which are DDR3/DDR4-era and out of scope for a DDR5 tester.
fn require_cpu_features() {
    let mut missing: Vec<&str> = Vec::new();
    if !is_x86_feature_detected!("avx2") {
        missing.push("AVX2");
    }
    // clflushopt detection landed in std behind the unstable `clflushopt_target_feature`
    // gate (rustc PR #157098) — gated at the crate root (`lib.rs`).
    if !is_x86_feature_detected!("clflushopt") {
        missing.push("CLFLUSHOPT");
    }

    if !missing.is_empty() {
        eprintln!("FATAL: this CPU is missing required instruction set features: {}", missing.join(", "));
        eprintln!("TMR is built for x86-64-v3 (AVX2) + CLFLUSHOPT — a 2015-or-newer CPU");
        eprintln!("(Intel Skylake+ / AMD Excavator+ / any Zen). Pre-2015 CPUs are not supported.");
        std::process::exit(RunStatus::NotRun.exit_code());
    }
}

/// Any error here stops TMR before testing, so it exits 2 like every other "stopped before
/// testing" case (`RunStatus::NotRun`); returned to `main.rs` it would exit 1, which means "tests
/// failed". The console gets it once, the log file too.
pub fn main() -> Result<(), Box<dyn std::error::Error>> {
    if let Err(e) = run() {
        log::error!(target: crate::console::FILE_ONLY_TARGET, "{e}");
        eprintln!("Error: {e}");
        std::process::exit(RunStatus::NotRun.exit_code());
    }
    Ok(())
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
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

    // 3. CPU capability gate — TMR is built for x86-64-v3 (AVX2) and requires CLFLUSHOPT
    //    (not part of any psABI level, so the compiler baseline can't guarantee it). Reject
    //    unsupported CPUs cleanly here rather than letting them #UD-fault mid-test.
    require_cpu_features();

    // VT escape processing, for the log colours and the progress ticker (see console.rs).
    crate::console::init();

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
        "--calibration-file", "--output", "--no-calibration",
        "--create-demo-configs", "--compare-results", "--debug-topology",
        "--show-topology", "--setup-large-pages", "--startup-debug", "--disable-pinning",
        "--help", "-h", "-?", "--version", "-v"
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
        std::process::exit(RunStatus::NotRun.exit_code());
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
            ("cpus", "50%"),  // Multi-threaded for realistic DRAM behavior
            ("cycles", "1"),  // Single cycle for quick latency measurement
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
            ("cpus", "1"),  // Single physical core for diagnostic-quality measurements
            ("cycles", "1"),  // Single cycle for quick latency measurement
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
        println!("🚀 TMR Quick Test Mode");
        println!("  Quick test uses reduced memory (60% reserve) for faster validation");
        println!("  All diagnostics and checks included, CLI overrides supported\n");

        // Inject quick-test defaults (only if not already specified)
        let mut quick_defaults = vec![
            ("memory", "60%-from-available"),
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
                    std::process::exit(RunStatus::NotRun.exit_code());
                }
                return Ok(());
            }
            "--compare-results" => {
                if args.len() < 4 {
                    println!("❌ Usage: {} --compare-results <baseline.json> <current.json> [output.json]", args[0]);
                    std::process::exit(RunStatus::NotRun.exit_code());
                }
                let baseline = &args[2];
                let current = &args[3];
                let output = args.get(4).map(|s| s.as_str());
                
                match compare_results_command(baseline, current, output) {
                    Ok(()) => println!("✅ Comparison completed successfully"),
                    Err(e) => {
                        println!("❌ Comparison failed: {}", e);
                        std::process::exit(RunStatus::NotRun.exit_code());
                    }
                }
                return Ok(());
            }
			"--debug-topology" => {
                println!("🔍 Debugging CPU Topology Detection");
                crate::cpu_topology::debug_topology_detection();
                return Ok(());
            }
			"--show-topology" => {
				println!("🔍 Complete CPU Topology Mapping");
				crate::cpu_topology::show_complete_topology_mapping();
				return Ok(());
			}
			// --cache-latency and --ram-latency are handled above
			// They use the unified test flow with filters
			"--setup-large-pages" => {
				println!("🔧 TMR Large Page Setup Tool");
				println!("=============================\n");
				
				let diagnostic = crate::memory::diagnose_and_setup_large_pages();
				println!("{}", diagnostic);
				
				// Test final state
				match crate::check_large_page_privilege() {
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

				// Detect system info once (includes cache info, TSC, hypervisor)
				let system_info = crate::SystemInfo::detect();
				let cache_info = system_info.get_cache_info().clone();
				cache_info.print_info();
				let smbios = crate::smbios::get_smbios();
				println!();

				// Check for maxpage= override in remaining args
				let mut config = crate::calibration::CalibrationConfig::default();
				for arg in &args[2..] {
					if let Some(page_str) = arg.strip_prefix("maxpage=") {
						config.page_size = page_str.to_lowercase();
						println!("📄 Page size override: {}\n", config.page_size);
					}
				}

				// Run calibration
				let calibration_test = crate::calibration::CalibrationTest::with_config(cache_info.clone(), config);
				match calibration_test.run() {
					Ok(results) => {
						crate::calibration::CalibrationTest::display_results(&results, &cache_info);

						// Save to tmr-cfg.json — compare with existing if present
						let cfg_path = crate::app_config::AppConfig::default_path();
						let mut app_config = crate::app_config::AppConfig::load(&cfg_path);
						let should_save = prompt_calibration_save(&app_config.calibration, &results, "standard");
						if should_save {
							app_config.update_machine_id(&system_info, smbios);
							app_config.calibration = Some(results.clone());
							match app_config.save(&cfg_path) {
								Ok(()) => println!("Calibration saved to {}", cfg_path.display()),
								Err(e) => println!("Warning: Failed to save calibration: {}", e),
							}
						} else {
							println!("Calibration not saved (kept existing).");
						}

						// Check for --output flag to save results
						if let Some(output_idx) = args.iter().position(|a| a == "--output") {
							if let Some(output_path) = args.get(output_idx + 1) {
								match serde_json::to_string_pretty(&results) {
									Ok(json) => {
										match std::fs::write(output_path, json) {
											Ok(()) => println!("Results saved to: {}", output_path),
											Err(e) => println!("Failed to write results: {}", e),
										}
									}
									Err(e) => println!("Failed to serialize results: {}", e),
								}
							} else {
								println!("--output requires a file path");
							}
						}
					}
					Err(e) => {
						println!("Calibration failed: {}", e);
						std::process::exit(RunStatus::NotRun.exit_code());
					}
				}
				return Ok(());
			}
			"--calibrate-cache-ext" => {
				println!("🔬 TMR Adaptive Cache Calibration (Extended - Phase 2)");
				println!("======================================================\n");

				// Detect system info once (includes cache info, TSC, hypervisor)
				let system_info = crate::SystemInfo::detect();
				let cache_info = system_info.get_cache_info().clone();
				cache_info.print_info();
				let smbios = crate::smbios::get_smbios();
				println!();

				// Check for maxpage= override in remaining args
				let mut config = crate::calibration::CalibrationConfig::default();
				for arg in &args[2..] {
					if let Some(page_str) = arg.strip_prefix("maxpage=") {
						config.page_size = page_str.to_lowercase();
						println!("📄 Page size override: {}\n", config.page_size);
					}
				}

				// Run extended calibration (phase 1 sweep + phase 2 fine-grain)
				let calibration_test = crate::calibration::CalibrationTest::with_config(cache_info.clone(), config);
				match calibration_test.run_extended() {
					Ok(results) => {
						crate::calibration::CalibrationTest::display_results(&results, &cache_info);

						// Save to tmr-cfg.json — compare with existing if present
						let cfg_path = crate::app_config::AppConfig::default_path();
						let mut app_config = crate::app_config::AppConfig::load(&cfg_path);
						let should_save = prompt_calibration_save(&app_config.calibration_extended, &results, "extended");
						if should_save {
							app_config.update_machine_id(&system_info, smbios);
							app_config.calibration_extended = Some(results.clone());
							match app_config.save(&cfg_path) {
								Ok(()) => println!("Extended calibration saved to {}", cfg_path.display()),
								Err(e) => println!("Warning: Failed to save calibration: {}", e),
							}
						} else {
							println!("Calibration not saved (kept existing).");
						}

						// Check for --output flag to save results
						if let Some(output_idx) = args.iter().position(|a| a == "--output") {
							if let Some(output_path) = args.get(output_idx + 1) {
								match serde_json::to_string_pretty(&results) {
									Ok(json) => {
										match std::fs::write(output_path, json) {
											Ok(()) => println!("Results saved to: {}", output_path),
											Err(e) => println!("Failed to write results: {}", e),
										}
									}
									Err(e) => println!("Failed to serialize results: {}", e),
								}
							} else {
								println!("--output requires a file path");
							}
						}
					}
					Err(e) => {
						println!("Extended calibration failed: {}", e);
						std::process::exit(RunStatus::NotRun.exit_code());
					}
				}
				return Ok(());
			}
            "--help" | "-h" | "/?" | "-?" => {
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
            std::process::exit(RunStatus::NotRun.exit_code());
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
								std::process::exit(RunStatus::NotRun.exit_code());
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
						use crate::cpu_topology::{set_topology_detection_method, TopologyDetectionMethod};
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
            default_params.push("memory=10%-from-available (default)".to_string());
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

	// Stage 1 FILTER: resolve the skip-cores spec ("N" | "N%" | "A-B") into a leading-skip
	// count and/or an excluded core-id range. The range form is how you isolate one memory
	// domain on a multi-CCD part (e.g. skip-cores=0-7 tests only cores 8+).
	let (resolved_skip, skip_excluded_range) =
		crate::cpu_selection::resolve_skip_spec(&pinning_config.skip_spec, total_cpus);
	pinning_config.cpus_to_skip = resolved_skip;
	if let Some((lo, hi)) = skip_excluded_range {
		println!("  CPU Filter: excluding core id range {}-{} (Stage 1)", lo, hi);
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
				topo.iter()
					.filter(|c| c.physical_core_id >= lo && c.physical_core_id <= hi)
					.count()
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

	// Subtract cores removed by an excluded id-range to get the true Available pool.
	let available_cpus = available_cpus_before_range
		.saturating_sub(range_excluded_count)
		.max(1);

	// `cpus=` is RELATIVE to the post-filter Available pool: any value <= 100% is therefore
	// always valid, which keeps configs portable across 4/6/8/16-core machines.
	let threads = if cpus.ends_with('%') {
		// Percentage of available CPUs
		let percent = cpus.trim_end_matches('%').parse::<u32>().unwrap_or(100);
		// Round to nearest instead of truncating: +50 before dividing by 100
		let n = ((available_cpus as u32 * percent + 50) / 100).max(1).min(available_cpus as u32) as usize;
		// Make the relative math explicit so `cpus=50%` after a skip isn't a surprise.
		println!("  CPU Count: {}% of {} available = {} thread(s)", percent, available_cpus, n);
		n
	} else {
		// Absolute count
		cpus.parse::<usize>().unwrap_or(available_cpus).min(available_cpus).max(1)
	};
		
	// Selected even when pinning is off: the list also decides which NUMA node each thread's
	// memory comes from, so a pinned/unpinned A/B differs in scheduling alone. The pool skips
	// only the affinity call (`RuntimeConfig.pin_threads`).
	// Override avoid_smt_doubling if cputype=cores
	let effective_avoid_smt = pinning_config.avoid_smt_doubling || avoid_smt;
	let (actual_threads, cpu_list) = match crate::cpu_selection::calculate_thread_allocation(
		threads,
		pinning_config.cpus_to_skip,
		effective_avoid_smt,
		skip_excluded_range,
		&pinning_config.stride_spec,
	) {
		Ok(result) => result,
		Err(e) => {
			// Never silently right-size the request — tell the user and stop.
			eprintln!("❌ CPU selection error: {}", e);
			std::process::exit(RunStatus::NotRun.exit_code());
		}
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
	} else {
		println!("  CPU Assignment: {:?} ⚠️ NUMA placement only, pinning is off", cpu_list);
	}
	if pinning_config.cpus_to_skip > 0 {
		println!("  Skipping first {} CPU(s) for system responsiveness", pinning_config.cpus_to_skip);
	}
	if pinning_config.avoid_smt_doubling {
		println!("  Avoiding SMT doubling (using physical cores only)");
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
    let simd_caps = crate::detect_simd_capabilities();
    println!("  SIMD Support: {}", simd_caps);
    println!();
		
	println!("Checking Large Page access");

	// Check for restart needed scenario first
	if crate::check_restart_needed() {
		println!("⚠️  LARGE PAGE SETUP REQUIRES RESTART");
		println!("=====================================");
		println!("The Large Page privilege has been configured but requires a restart to take effect.");
		println!();
		println!("Choose your preferred action:");
		println!("  A) Exit TMR and restart system (RECOMMENDED for optimal performance)");
		println!("  B) Continue with standard 4KB pages (~5-10% slower, so less memory stress)");
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
				// The default minpage needs large pages, so say 4 KB for this run (TODO 88)
				alloc_config.min_page_size = "regular".to_string();
				println!();
				println!("⚠️  Continuing with standard 4KB pages (minpage=regular for this run).");
				println!("   Performance impact: ~5-10% slower than large pages");
				println!("   Every test still runs and every pattern is still checked, but slower");
				println!("   memory traffic is less stress, so marginal errors that only appear at");
				println!("   full bandwidth may go undetected");
				println!();
				println!("💡 TIP: Restart TMR after rebooting for optimal performance");
				println!();
			}
			_ => {
				alloc_config.min_page_size = "regular".to_string();
				println!();
				println!("❌ Invalid choice. Defaulting to continue with 4KB pages (minpage=regular for this run).");
				println!();
			}
		}
	} else {
		// Normal large page setup flow
		let large_page_diagnostic = crate::memory::diagnose_and_setup_large_pages();
		println!("{}", large_page_diagnostic);
	}

	match crate::check_large_page_privilege() {
		Ok(()) => {
			println!("🎉 Large Pages: ✅ ENABLED and tested successfully!");
			println!("   TMR will use 2MB large pages for optimal performance");
		},
		Err(_) if crate::memory::allocator::PageSizeLevel::from_config_str(&alloc_config.min_page_size)
			> crate::memory::allocator::PageSizeLevel::Regular => {
			// Without the privilege, a large minpage (the default) refuses the run at allocation
			println!("⚠️  Large Pages: Not available, and minpage={} needs them, so the run will stop", alloc_config.min_page_size);
			println!("   before allocating. Set them up (tmr.exe --setup-large-pages, elevated), or rerun");
			println!("   with minpage=regular to test on standard 4KB pages: every test still runs, but");
			println!("   slower memory traffic is less stress, so marginal errors that only appear at");
			println!("   full bandwidth may go undetected");
		}
		Err(_) => {
			println!("⚠️  Large Pages: Not available - TMR will use standard 4KB pages");
			println!("   Performance impact: ~5-10% slower memory allocation");
			println!("   Every test still runs and every pattern is still checked, but slower");
			println!("   memory traffic is less stress, so marginal errors that only appear at");
			println!("   full bandwidth may go undetected");
		}
	}

	println!();
	
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
				let cal_type = if app_config.calibration_extended.is_some() { "extended" } else { "standard" };
				let timestamp = cal.timestamp.format("%Y-%m-%d %H:%M");
				println!("  Calibration: Loaded ({}, {})", cal_type, timestamp);
				crate::set_calibration_data(Some(cal.clone()));
			} else {
				println!("  Calibration: None (run --calibrate-cache to calibrate)");
				crate::set_calibration_data(None);
			}
		} else if app_config.machine_id.is_some() {
			println!("  Calibration: Stale (hardware changed, re-run --calibrate-cache)");
			crate::set_calibration_data(None);
		} else {
			println!("  Calibration: None (run --calibrate-cache to calibrate)");
			crate::set_calibration_data(None);
		}
	} else {
		println!("  Calibration: Disabled (--no-calibration)");
		crate::set_calibration_data(None);
	}

	// Display CPU topology right after system information
	// Add this after the cache architecture display (around line 220-230):
	display_cpu_topology(&cpu_list, pinning_config.cpus_to_skip, avoid_smt, pinning_config.enable_pinning);
	println!();
	
	// Show memory stats
	print_current_memory_status();
	
	{
		use crate::reporting::{create_console_reporter, system_info_builder::build_system_info_report};

		let report = build_system_info_report(system_info, &simd_caps);

		let mut reporter = create_console_reporter();
		if let Err(e) = reporter.report_system_info(&report) {
			log::error!("Failed to display system info report: {}", e);
			// Manual system info already displayed above as fallback
		}
	}
	
	println!();
    
    // Calculate memory layout using enhanced system
    // Checks the block sizes too, so a bad one stops the run before anything is allocated
    let share_rounding = alloc_config.share_rounding()?;
    let enhanced_layout = enhanced_memory_strategy.create_layout(actual_threads, share_rounding)?;

    println!("Starting comprehensive memory tests... Use CTRL+C for graceful shutdown with final report");
    println!("(detailed logs available with RUST_LOG=debug)");
    println!();

	// Register CTRL+C handler for graceful shutdown
	// This allows tests to stop at next checkpoint and print final reports
	// A flag of the handler's own, not SHUTDOWN_REQUESTED: a halt sets that too, and the first
	// CTRL+C after a halt must still be the graceful one.
	let mut pressed = false;
	ctrlc::set_handler(move || {
		use std::sync::atomic::Ordering;
		if pressed {
			// Second press: the graceful stop is stuck or too slow. Windows frees every page the
			// process holds, large pages included, when it exits. stderr rather than print_above,
			// because a stuck shutdown may be one that is holding the console lock.
			eprintln!("\n\n🛑 CTRL+C again - exiting now, without the final report or results file");
			log::logger().flush();
			// The status Windows itself gives a process killed by CTRL+C, so a script can tell this apart
			// from a run that finished.
			std::process::exit(windows::Win32::Foundation::STATUS_CONTROL_C_EXIT.0);
		}
		pressed = true;
		// Above the progress ticker, which this thread would otherwise print into the middle of.
		crate::console::print_above(
			"\n🛑 CTRL+C received - initiating graceful shutdown...\n\
			 \x20  Tests will stop at next checkpoint and print final report\n\
			 \x20  (Press CTRL+C again to exit immediately, without the report or results file)\n\n",
		);
		crate::runner::SHUTDOWN_REQUESTED.store(true, Ordering::Relaxed);
	}).expect("Error setting CTRL+C handler");

	let mut runtime_config = detect_runtime_capabilities(&alloc_config);
	runtime_config.cpu_list = Some(cpu_list);
	runtime_config.pin_threads = pinning_config.enable_pinning;
	runtime_config.enhanced_memory_strategy = enhanced_memory_strategy.clone();

	// Extract channels parameter if provided (overrides config default of 2)
	let channels_override = if params::has_param(&validated_params, "channels") {
		Some(params::get_usize(&validated_params, "channels", 2) as u32)
	} else {
		None
	};

	// The seal and its width, if given (the parser has validated them)
	let seal_override = params::has_param(&validated_params, "seal").then(|| params::get_string(&validated_params, "seal", "tmr"));
	let seal_width_override = params::has_param(&validated_params, "seal-width").then(|| params::get_string(&validated_params, "seal-width", "auto"));

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

    // --startup-debug: run the startup sequence and print the test plan, but allocate no test memory and
    // run no test
    let plan_only = args.iter().any(|a| a == "--startup-debug");

    let start_time = std::time::Instant::now();

    // Unified test execution path - works for bandwidth tests, latency tests, or mixed
    // test_filter may be set by --ram-latency or --cache-latency to run specific test subsets
    // test_filter_param may be set by test=Pattern for CLI filtering
    let final_test_filter = test_filter.as_deref().or(test_filter_param.as_deref());

    let status = run_tests_with_layout_and_timing_filtered(
        enhanced_layout,
        error_mode,
        suite_timing,
        runtime_config,
        config_opt.as_ref(),
        crate::runner::TestRunOverrides {
            single_test_filter: final_test_filter,
            seal_override: seal_override.as_deref(),
            seal_width_override: seal_width_override.as_deref(),
            mirror_override,
            pattern_mode_override,
            verify_reps_override,
            test_reps_override,
            wrc_override,
            channels_override,
            plan_only,
        },
        cache_info,
    );

    if plan_only {
        if status == RunStatus::Passed {
            println!("\n================================================================================");
            println!("  --startup-debug: Startup sequence and test plan complete. No test memory allocated, no test run.");
            println!("================================================================================");
        }
    } else {
        let total_time = start_time.elapsed();

        println!();
        println!("================================================================================");
        match status {
            RunStatus::Passed => println!("✅ All memory tests completed successfully in {}", format_duration(total_time)),
            RunStatus::Failed => println!("❌ Tests failed or encountered errors in {}", format_duration(total_time)),
            RunStatus::NotRun => println!("❌ Stopped before testing: see the message above"),
            RunStatus::Interrupted => println!("⚠️  Interrupted after {}: only the tests that finished were checked", format_duration(total_time)),
        }
    }
    // Exit status for scripts: 0 passed, 1 failed or found errors, 2 stopped before testing
    if status != RunStatus::Passed {
        std::process::exit(status.exit_code());
    }
	Ok(())
}

/// Compare new calibration results with existing and prompt user to accept/reject.
/// Returns true if the new results should be saved.
/// If no existing results, saves automatically (no prompt).
fn prompt_calibration_save(
    existing: &Option<crate::calibration::CalibrationResults>,
    new_results: &crate::calibration::CalibrationResults,
    cal_type: &str,
) -> bool {
    let existing = match existing {
        Some(e) => e,
        None => {
            println!("\nNo existing {} calibration — saving new results.", cal_type);
            return true;
        }
    };

    // Show comparison table
    println!("\n📊 Calibration Comparison (existing vs new)");
    println!("───────────────────────────────────────────────────────────────────────────");
    println!("{:<10} {:>12} {:>12} {:>8}  {:>10} {:>10} {:>8}",
        "Tier", "Old Size", "New Size", "Delta", "Old Lat", "New Lat", "Delta");
    println!("───────────────────────────────────────────────────────────────────────────");

    for tier in crate::calibration::CacheTier::all_tiers() {
        let old_tier = existing.tiers.get(tier);
        let new_tier = new_results.tiers.get(tier);

        match (old_tier, new_tier) {
            (Some(old), Some(new)) => {
                let size_pct = if old.optimal_size > 0 {
                    ((new.optimal_size as f64 / old.optimal_size as f64) - 1.0) * 100.0
                } else { 0.0 };
                let lat_pct = if old.median_latency_ns > 0.0 {
                    ((new.median_latency_ns / old.median_latency_ns) - 1.0) * 100.0
                } else { 0.0 };

                println!("{:<10} {:>12} {:>12} {:>+7.1}%  {:>9.1}ns {:>9.1}ns {:>+7.1}%",
                    tier.name(),
                    format_calibration_size(old.optimal_size),
                    format_calibration_size(new.optimal_size),
                    size_pct,
                    old.median_latency_ns,
                    new.median_latency_ns,
                    lat_pct,
                );
            }
            (None, Some(new)) => {
                println!("{:<10} {:>12} {:>12} {:>8}  {:>10} {:>9.1}ns {:>8}",
                    tier.name(), "-", format_calibration_size(new.optimal_size), "new",
                    "-", new.median_latency_ns, "new");
            }
            (Some(old), None) => {
                println!("{:<10} {:>12} {:>12} {:>8}  {:>9.1}ns {:>10} {:>8}",
                    tier.name(), format_calibration_size(old.optimal_size), "-", "gone",
                    old.median_latency_ns, "-", "gone");
            }
            (None, None) => {}
        }
    }
    println!("───────────────────────────────────────────────────────────────────────────");
    println!("  Existing: {} ({})", existing.timestamp.format("%Y-%m-%d %H:%M"), existing.page_size);
    println!("  New:      {} ({})", new_results.timestamp.format("%Y-%m-%d %H:%M"), new_results.page_size);

    // Prompt
    print!("\nSave new {} calibration? [Y/n]: ", cal_type);
    let _ = stdout().flush();
    let mut input = String::new();
    match stdin().read_line(&mut input) {
        Ok(_) => {
            let answer = input.trim().to_lowercase();
            answer.is_empty() || answer == "y" || answer == "yes"
        }
        Err(_) => true, // On read error, default to saving
    }
}

fn format_calibration_size(bytes: usize) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{} B", bytes)
    }
}

// In main.rs - Pre-calculate thread allocation

fn setup_logging() {
    // Create logs directory if it doesn't exist
    if let Err(e) = std::fs::create_dir_all("logs") {
        eprintln!("Warning: Failed to create logs directory: {}", e);
    }

    // Shared with the result filename so a run's log and result can be paired by name; see
    // `run_context::run_start`. Local time — this file is read on the machine that wrote it.
    let log_filename = format!("logs/{}.log", crate::run_context::run_file_stem());
    
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
    if let Ok(mut log_file) = std::fs::File::create(&log_filename) {
        // Stamp the zone once here rather than on every line: every line in the file has the same
        // offset, so repeating it thousands of times buys nothing, but a reader (or a later
        // consolidation pass) still needs to know these are local times and which local that was.
        // The UTC equivalent is recorded so this file can be lined up with results from other boxes.
        let started = crate::run_context::run_start();
        let _ = writeln!(
            log_file,
            "# TMR {} — run started {} (UTC {}); all timestamps below are local time\n\
             # paired result file: results/{}.json",
            env!("CARGO_PKG_VERSION"),
            started.format("%Y-%m-%d %H:%M:%S %:z"),
            started.with_timezone(&chrono::Utc).format("%Y-%m-%d %H:%M:%S"),
            crate::run_context::run_file_stem(),
        );
        FILE_LOGGER.set(Arc::new(Mutex::new(Some(log_file)))).unwrap_or(());
    }

    // Set up env_logger for console with original nice formatting (colors + module info)
    Builder::new()
        .filter_level(level_filter)
        .format(|buf, record| {
            // Write to console with original env_logger format. `LogSink` below prints it above
            // the progress ticker, so the two never merge. A file-only record writes nothing here,
            // and `LogSink` drops an empty write.
            let console_result = if record.target() == crate::console::FILE_ONLY_TARGET {
                Ok(())
            } else {
                writeln!(
                    buf,
                    "\x1b[{}m[{} {} {}]\x1b[0m {}",
                    match record.level() {
                        log::Level::Error => "31", // Red
                        log::Level::Warn => "33",  // Yellow
                        log::Level::Info => "32",  // Green
                        log::Level::Debug => "36", // Cyan
                        log::Level::Trace => "35", // Magenta
                    },
                    // Plain local time, no zone marker. This used to be `…%SZ` on a `Local::now()` —
                    // printing local while claiming UTC, which was the actual bug. A bare stamp claims
                    // nothing, which is both honest and easier to read; the zone is stated once in the
                    // log file's header for anything that gets archived. Matches the file format below.
                    chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
                    record.level(),
                    record.module_path().unwrap_or("unknown"),
                    record.args()
                )
            };

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
        // Through the console module, so a log line can never land inside the progress ticker.
        // A pipe target gets no colour detection of its own (env_logger would strip the escapes
        // above), so decide here as the stdout target used to: colours on a console, none when
        // redirected.
        .target(env_logger::Target::Pipe(Box::new(crate::console::LogSink)))
        .write_style(if std::io::IsTerminal::is_terminal(&stdout()) {
            env_logger::WriteStyle::Always
        } else {
            env_logger::WriteStyle::Never
        })
        .init();

    // Log startup info
    log::info!("TMR v1.0.0 started - console logging with colors and module info restored");
    log::info!("Log level: {} - detailed logs also saved to {}", log_level, log_filename);
}

/// Parsed configuration bundle produced from validated CLI parameters:
/// `(memory_strategy, error_mode, suite_timing, cputype, cpus, pinning, alloc)`.
type ParsedRunConfig = (
    EnhancedMemoryStrategy,
    ErrorMode,
    TestSuiteTiming,
    String,
    String,
    CpuPinningConfig,
    MemoryAllocationConfig,
);

/// Build configuration from validated parameters (using centralized registry)
fn build_config_from_validated_params(
    validated: &HashMap<String, params::ParamValue>
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

    // Build CPU pinning config. skip_spec/stride_spec are the two-stage selection specs;
    // cpus_to_skip is resolved from skip_spec later (needs the real core count).
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


    // Handle page size overrides
    if let Some(params::ParamValue::String(page_str)) = validated.get("minpage") {
        alloc_config.min_page_size = page_str.to_string();
        println!("  Minimum Page Size: {}", page_str);
    }
    if let Some(params::ParamValue::String(page_str)) = validated.get("maxpage") {
        alloc_config.max_page_size = page_str.to_string();
        println!("  Maximum Page Size: {}", page_str);
    }
    for key in MemoryAllocationConfig::BLOCK_PARAMS {
        if let Some(params::ParamValue::String(v)) = validated.get(key) {
            alloc_config.set_block_param(key, v);
            println!("  {}: {}", key, v);
        }
    }

    // Handle topology (has side effect of setting global state)
    if let Some(params::ParamValue::String(topology_str)) = validated.get("topology") {
        use crate::cpu_topology::{set_topology_detection_method, TopologyDetectionMethod};
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

fn format_duration(duration: std::time::Duration) -> String {
    // Format runtime as HH:MM:SS
    let total_seconds = duration.as_secs();
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;
    format!("{:02}:{:02}:{:02}", hours, minutes, seconds)
}


/// Parse enhanced memory parameter with clear reserve semantics
fn parse_enhanced_memory_parameter(param: &str) -> Result<crate::memory::allocation_strategy::EnhancedMemoryStrategy, String> {
    use crate::memory::allocation_strategy::{EnhancedMemoryStrategy, AllocationMode};
    
    Ok(EnhancedMemoryStrategy {
        allocation_mode: AllocationMode::parse(param)?,
    })
}
