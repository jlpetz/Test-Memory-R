use crate::{ErrorMode, MemoryLayout, ProgressTracker, BlockInfo, TestBuffer};
use crate::layout::{WindowMode, BlockMode};
use crate::tests::{TestStats, TestMemoryConfig, TestTiming, stuck_bit_test};
use crate::tests::{
    mirror_move_128_non_temporal, mirror_move_256_non_temporal, mirror_move_512_non_temporal,
    simple_test, refresh_stable, cache_busting_write_test, random_access_torture_test,
    stride_access_test, bandwidth_saturation_test, block_move_test
};
use crate::progress::{progress_reporter, TestSummary};
use crate::results::{TestRunResult, save_test_result}; // New results module
use crate::dma_memory::{DmaBuffer, DmaBufferEnhanced, DmaConfig, EnhancedTestBuffer, MemoryType, DriverHandle, remap_all_allocations_to_type, PageSize};
use crate::config::CpuPinningConfig;
use crate::{MemoryBackend, RuntimeConfig};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::thread;
use std::time::Instant;
use std::sync::OnceLock;
use windows::Win32::Foundation::HANDLE;

use windows::Win32::System::SystemInformation::{
    GetLogicalProcessorInformationEx, 
    RelationNumaNode,
    SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
};

// Global flag for graceful shutdown
static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

// Pre-allocated memory block management
pub struct AllocatedBlock {
    pub buffer: EnhancedTestBuffer,
    pub block_info: BlockInfo,
    pub memory_type: MemoryType,  // Track current type
    pub numa_node: u32,           // NUMA node (doesn't change)
	pub supports_remapping: bool,  // Add this
}

// Modern NUMA information structure
#[derive(Debug, Clone)]
pub struct NumaTopology {
    pub node_count: u32,
    pub nodes: Vec<NumaNodeInfo>,
    pub cpu_to_node: HashMap<u32, u32>,
}

#[derive(Debug, Clone)]
pub struct NumaNodeInfo {
    pub node_id: u32,
    pub group_count: u16,
    pub group_masks: Vec<GroupMask>,
    pub cpu_count: u32,
    pub cpus: Vec<u32>,
}

#[derive(Debug, Clone)]
pub struct GroupMask {
    pub group: u16,
    pub mask: usize,
}

#[derive(Debug, Clone)]
pub struct NumaMemoryInfo {
    pub node_id: u32,
    pub total_memory_kb: u64,
    pub available_memory_kb: u64,
}

// Global test suite timing configuration
#[derive(Debug, Clone)]
pub struct TestSuiteTiming {
    pub global_cycles: Option<u32>,        // Number of full test suite runs
    pub global_duration_secs: Option<u32>, // Maximum total time for all cycles
}

impl Default for TestSuiteTiming {
    fn default() -> Self {
        Self {
            global_cycles: Some(3), // Default 3 cycles like TM5
            global_duration_secs: None,
        }
    }
}

impl TestSuiteTiming {
    pub fn cycles_only(cycles: u32) -> Self {
        Self {
            global_cycles: Some(cycles),
            global_duration_secs: None,
        }
    }
    
    pub fn duration_only(duration_secs: u32) -> Self {
        Self {
            global_cycles: None,
            global_duration_secs: Some(duration_secs),
        }
    }
    
    pub fn hybrid(cycles: u32, max_duration_secs: u32) -> Self {
        Self {
            global_cycles: Some(cycles),
            global_duration_secs: Some(max_duration_secs),
        }
    }
    
    pub fn unlimited() -> Self {
        Self {
            global_cycles: None,
            global_duration_secs: None,
        }
    }
    
    pub fn should_continue_suite(&self, current_cycle: u32, elapsed_secs: u32) -> bool {
        // Check if shutdown was requested
        if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
            return false;
        }
        
        // Check maximum duration first
        if let Some(max_secs) = self.global_duration_secs {
            if elapsed_secs >= max_secs {
                return false;
            }
        }
        
        // Check cycle limit
        if let Some(max_cycles) = self.global_cycles {
            if current_cycle >= max_cycles {
                return false;
            }
        }
        
        // If no limits set, run forever
        true
    }
}

// Simple performance config for now (full version would come from config.rs)
#[derive(Debug, Clone)]
pub struct PerformanceConfig {
    pub thread_priority: ThreadPriority,
}

#[derive(Debug, Clone)]
pub enum ThreadPriority {
    Normal,
    High,
    Realtime,
}

impl Default for PerformanceConfig {
    fn default() -> Self {
        Self {
            thread_priority: ThreadPriority::High,
        }
    }
}

impl PerformanceConfig {
    pub fn apply(&self) -> Result<(), String> {
        use windows::Win32::System::Threading::{
            SetThreadPriority, GetCurrentThread,
            THREAD_PRIORITY_NORMAL, THREAD_PRIORITY_HIGHEST, 
            THREAD_PRIORITY_TIME_CRITICAL
        };
        
        unsafe {
            let priority = match self.thread_priority {
                ThreadPriority::Normal => THREAD_PRIORITY_NORMAL,
                ThreadPriority::High => THREAD_PRIORITY_HIGHEST,
                ThreadPriority::Realtime => THREAD_PRIORITY_TIME_CRITICAL,
            };
            
            if let Err(e) = SetThreadPriority(GetCurrentThread(), priority) {
				log::warn!("Failed to set thread priority: {:?}", e);
			}
        }
        
        Ok(())
    }
}

// Updated test function signatures to accept TestMemoryConfig
type TestFunctionSimple = unsafe fn(*mut u8, usize, usize, ErrorMode, &TestTiming) -> TestStats;
type TestFunctionWithStreams = unsafe fn(*mut u8, usize, usize, ErrorMode, &TestTiming, u32) -> TestStats;
type TestFunctionWithConfig = unsafe fn(*mut u8, usize, usize, ErrorMode, &TestTiming, &TestMemoryConfig) -> TestStats;

// Test function wrapper enum to handle different signatures
enum TestFunction {
    Simple(TestFunctionSimple),
    WithStreams(TestFunctionWithStreams),
    WithConfig(TestFunctionWithConfig),
}

fn detect_runtime_capabilities(use_driver_chunking: bool) -> RuntimeConfig {
    let driver_available = DmaBuffer::is_driver_available_and_compatible();
    let large_pages_available = crate::check_large_page_privilege().is_ok();
    
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


fn print_current_memory_status() {
    use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    
    unsafe {
        let mut mem_status = MEMORYSTATUSEX {
            dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
            ..Default::default()
        };
        
        if GlobalMemoryStatusEx(&mut mem_status).is_ok() {
            println!("\n📊 Current System Memory Status:");
            println!("   Total Physical: {:.2} GiB", mem_status.ullTotalPhys as f64 / (1024.0 * 1024.0 * 1024.0));
            println!("   Available Physical: {:.2} GiB ({:.1}%)", 
                    mem_status.ullAvailPhys as f64 / (1024.0 * 1024.0 * 1024.0),
                    (mem_status.ullAvailPhys as f64 / mem_status.ullTotalPhys as f64) * 100.0);
            println!("   Total Page File: {:.2} GiB", mem_status.ullTotalPageFile as f64 / (1024.0 * 1024.0 * 1024.0));
            println!("   Available Page File: {:.2} GiB", mem_status.ullAvailPageFile as f64 / (1024.0 * 1024.0 * 1024.0));
            println!("   Memory Load: {}%", mem_status.dwMemoryLoad);
        }
    }
}

pub fn run_tests_with_layout(layout: MemoryLayout, error_mode: ErrorMode) -> bool {
    // Create a default runtime config here
    let runtime_config = detect_runtime_capabilities(false);
    run_tests_with_layout_and_timing(layout, error_mode, TestSuiteTiming::default(), runtime_config)
}


pub fn run_tests_with_layout_and_timing(layout: MemoryLayout, error_mode: ErrorMode, suite_timing: TestSuiteTiming, runtime_config: RuntimeConfig) -> bool {
    // Set up CTRL+C handler
    setup_signal_handler();
    
    layout.print_layout();

    let progress = Arc::new(ProgressTracker::new());
    let success = Arc::new(AtomicBool::new(true));

    let mut thread_blocks: HashMap<usize, Vec<BlockInfo>> = HashMap::new();
    for block in layout.blocks {
        thread_blocks.entry(block.thread_id).or_default().push(block);
    }

    // Stage 1: Pre-allocate all memory blocks
    progress.set_phase("Stage 1: Allocating Memory");
    println!("Stage 1: Pre-allocating memory blocks...");

    let mut allocated_blocks = match allocate_all_blocks(&thread_blocks, &runtime_config) {
        Ok(blocks) => blocks,
        Err(e) => {
            println!("❌ Failed to allocate memory blocks: {}", e);
			// Try to print current memory status
            print_current_memory_status();
            return false;
        }
		
    };

    print_allocation_summary(&allocated_blocks);

    // Calculate progress tracking information
    let test_definitions = create_test_definitions();
    let tests_per_cycle = test_definitions.len() as u64; // One test completion = all threads finishing that test
    
    // Set up progress tracking with proper cycle information
    progress.set_cycle_info(1, suite_timing.global_cycles, tests_per_cycle);

    // Print test configuration summary
    print_test_configuration(&test_definitions, &suite_timing);

    // Start progress reporter thread
    let progress_clone = Arc::clone(&progress);
    let progress_handle = thread::spawn(move || {
        progress_reporter(progress_clone);
    });

    // Initialize test run result
    let test_run_result = Arc::new(Mutex::new(TestRunResult::new()));

    // Run test suite with timing control
    let suite_start = Instant::now();
    let mut current_cycle = 0u32;
    
    println!("\n=== Starting Test Suite ===");
    
    loop {
        current_cycle += 1;
        let cycle_start = Instant::now();
        
        println!("\n=== Test Suite Cycle {} ===", current_cycle);
        progress.start_new_cycle(current_cycle);
        progress.set_phase(&format!("Cycle {}", current_cycle));
        
        // Collect cycle test summaries
        let cycle_summaries = Arc::new(Mutex::new(Vec::new()));
        
        // Run one complete cycle of all tests
        let cycle_success = run_single_test_cycle(
            &thread_blocks,
            &mut allocated_blocks,
            &test_definitions,
            error_mode,
            Arc::clone(&progress),
            Arc::clone(&cycle_summaries),
            current_cycle,
        );
        
        if !cycle_success || SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
            success.store(false, Ordering::Relaxed);
            
            // Print cycle report before exiting
            let cycle_elapsed = cycle_start.elapsed().as_secs() as u32;
            if let Ok(summaries) = cycle_summaries.lock() {
                if !summaries.is_empty() {
                    println!("\n=== Interrupted Cycle {} Report ===", current_cycle);
                    print_cycle_report(current_cycle, cycle_elapsed, &summaries);
                }
            }
            break;
        }
        
        let cycle_elapsed = cycle_start.elapsed().as_secs() as u32;
        let total_elapsed = suite_start.elapsed().as_secs() as u32;
        
        // Print cycle report and save to result
        if let Ok(summaries) = cycle_summaries.lock() {
            print_cycle_report(current_cycle, cycle_elapsed, &summaries);
            progress.complete_cycle(current_cycle, summaries.clone());
            
            // Add cycle to test run result
            if let Ok(mut result) = test_run_result.lock() {
                result.add_cycle(current_cycle, cycle_elapsed, summaries.clone());
            }
        }
        
        // Check if we should continue to next cycle
        if !suite_timing.should_continue_suite(current_cycle, total_elapsed) {
            println!("\nTest suite timing limits reached - stopping");
            break;
        }
    }

    progress.set_phase("Completed");
    thread::sleep(std::time::Duration::from_millis(100));

    if let Err(_) = progress_handle.join() {
        log::warn!("Progress reporter thread failed to join cleanly");
    }

    // Print final summary
    let total_time = suite_start.elapsed();
    if let Ok(result) = test_run_result.lock() {
        print_final_summary(&progress, total_time, &result, &test_definitions);
        
        // Save test result to file
        if let Err(e) = save_test_result(&result) {
            log::warn!("Failed to save test result: {}", e);
        } else {
            log::info!("Test result saved to file");
        }
    }

    log::info!("Releasing all allocated memory blocks");
    success.load(Ordering::Relaxed)
}

fn setup_signal_handler() {
	ctrlc::set_handler(move || {
		println!("\n\n🛑 CTRL+C detected - initiating graceful shutdown...");
		SHUTDOWN_REQUESTED.store(true, Ordering::Relaxed);
		crate::reset_driver();
	}).expect("Error setting CTRL+C handler");
}

fn create_test_definitions() -> Vec<(&'static str, TestFunction, TestMemoryConfig)> {
    vec![
        // === CRITICAL: Full Memory Stuck Bit Test ===
        (
            "StuckBitTest", 
            TestFunction::Simple(|ptr, size, tid, em, timing| unsafe { stuck_bit_test(ptr, size, tid, em, timing) }),
            TestMemoryConfig::new(
                WindowMode::FullAllocation,           // Use entire allocation 
                BlockMode::WindowFraction { fraction: 0.0625 }, // 1/16th of window per block for efficiency
                false,  // Require alignment
                false   // Doesn't need locality - needs to test ALL memory
            ).with_timing(TestTiming::cycles_only(1)) // Run once per cycle - it's thorough
             .with_streams(1) // Single stream for stuck bit test
			 .with_memory_type(None)  // Default WriteBack
        ),
        
        // === SIMD Tests with optimal window/block sizing ===
        (
            "MirrorMove128NonTemporal", 
            TestFunction::WithStreams(|ptr, size, tid, em, timing, streams| unsafe { 
                mirror_move_128_non_temporal(ptr, size, tid, em, timing, streams) 
            }),
            TestMemoryConfig::new(
                WindowMode::FixedSize { size_mb: 64 },   // 64MB window for SIMD locality
                BlockMode::FixedSize { size_mb: 16 },    // 16MB blocks for 128-bit alignment
                false,  // Require alignment
                true    // Needs locality for SIMD efficiency
            ).with_timing(TestTiming::duration_only(10)) // 10 seconds of continuous SIMD testing
             .with_streams(1) // Default single stream
			 .with_memory_type(None)  // Default WriteBack
        ),
        (
            "MirrorMove256NonTemporal", 
            TestFunction::WithStreams(|ptr, size, tid, em, timing, streams| unsafe { 
                mirror_move_256_non_temporal(ptr, size, tid, em, timing, streams) 
            }),
            TestMemoryConfig::new(
                WindowMode::FixedSize { size_mb: 128 },  // 128MB window
                BlockMode::FixedSize { size_mb: 32 },    // 32MB blocks for 256-bit alignment
                false,
                true
            ).with_timing(TestTiming::duration_only(10))
             .with_streams(2) // Dual stream for 256-bit
			 .with_memory_type(None)  // Default WriteBack
        ),
        (
            "MirrorMove512NonTemporal", 
            TestFunction::WithStreams(|ptr, size, tid, em, timing, streams| unsafe { 
                mirror_move_512_non_temporal(ptr, size, tid, em, timing, streams) 
            }),
            TestMemoryConfig::new(
                WindowMode::FixedSize { size_mb: 256 },  // 256MB window
                BlockMode::FixedSize { size_mb: 64 },    // 64MB blocks for 512-bit alignment
                false,
                true
            ).with_timing(TestTiming::duration_only(10))
             .with_streams(4) // Quad stream for 512-bit
			 .with_memory_type(None)  // Default WriteBack
        ),
        
        // === Memory Pattern Tests ===
        (
            "SimpleTest", 
            TestFunction::WithConfig(|ptr, size, tid, em, timing, config| unsafe { 
                simple_test(ptr, size, tid, em, timing, config) 
            }),
            TestMemoryConfig::new(
                WindowMode::FullAllocation,              // Test large portions of memory
                BlockMode::FixedSize { size_mb: 4 },     // 4MB blocks
                false,
                false   // Doesn't need locality
            ).with_timing(TestTiming::hybrid(100, 30)) // 100 cycles or 30 seconds, whichever first
             .with_streams(1) // Default single stream
			 .with_memory_type(None)  // Default WriteBack
        ),
        
        // === Refresh/Retention Tests ===
        (
            "RefreshStable", 
            TestFunction::Simple(|ptr, size, tid, em, timing| unsafe { refresh_stable(ptr, size, tid, em, timing) }),
            TestMemoryConfig::new(
                WindowMode::CacheRelative { multiplier: 2.0 }, // 2x cache size for refresh testing
                BlockMode::FixedSize { size_mb: 1 },           // Small 1MB blocks
                false,
                true    // Needs locality for refresh timing
            ).with_timing(TestTiming::duration_only(15)) // 15 seconds of refresh testing
             .with_streams(1) // Single stream
			 .with_memory_type(None)  // Default WriteBack
        ),
        
        // === Cache Tests ===
        (
            "CacheBusting", 
            TestFunction::WithStreams(|ptr, size, tid, em, timing, streams| unsafe { 
                cache_busting_write_test(ptr, size, tid, em, timing, streams) 
            }),
            TestMemoryConfig::new(
                WindowMode::CacheRelative { multiplier: 0.5 }, // Half cache size
                BlockMode::FixedSize { size_mb: 1 },           // 1MB blocks for cache busting
                false,
                true    // Specifically targets cache behavior
            ).with_timing(TestTiming::duration_only(20)) // 20 seconds of cache busting
             .with_streams(4) // 4 streams for cache stress
			 .with_memory_type(Some(MemoryType::Uncached))  // Force DRAM access
        ),
        
        // === Stress Tests ===
        (
            "RandomTorture", 
            TestFunction::WithStreams(|ptr, size, tid, em, timing, streams| unsafe { 
                random_access_torture_test(ptr, size, tid, em, timing, streams) 
            }),
            TestMemoryConfig::new(
                WindowMode::FullAllocation,              // Random access across full allocation
                BlockMode::FixedSize { size_mb: 8 },     // 8MB blocks
                true,   // Allow misaligned for maximum stress
                false   // Random access - locality not needed
            ).with_timing(TestTiming::duration_only(25)) // 25 seconds of random torture
             .with_streams(8) // 8 streams for maximum chaos
			 .with_memory_type(None)  // Default WriteBack
        ),
        (
            "StrideAccess", 
            TestFunction::WithStreams(|ptr, size, tid, em, timing, streams| unsafe { 
                stride_access_test(ptr, size, tid, em, timing, streams) 
            }),
            TestMemoryConfig::new(
                WindowMode::FullAllocation,              // Test stride patterns across full memory
                BlockMode::FixedSize { size_mb: 2 },     // 2MB blocks
                false,
                false
            ).with_timing(TestTiming::cycles_only(1)) // 1 cycle of stride patterns
             .with_streams(4) // 4 streams for stride patterns
			 .with_memory_type(None)  // Default WriteBack
        ),
        
        // === Bandwidth Test ===
        (
            "BandwidthSat", 
            TestFunction::WithStreams(|ptr, size, tid, em, timing, streams| unsafe { 
                bandwidth_saturation_test(ptr, size, tid, em, timing, streams) 
            }),
            TestMemoryConfig::new(
                WindowMode::FullAllocation,              // Maximum bandwidth requires full allocation
                BlockMode::FixedSize { size_mb: 32 },    // Large 32MB blocks for bandwidth
                false,
                false
            ).with_timing(TestTiming::duration_only(15)) // 15 seconds of bandwidth saturation
             .with_streams(1) // Single stream for max bandwidth
			 .with_memory_type(Some(MemoryType::WriteCombining))  // Optimal for bandwidth!
        ),
		
		// Add this to the test definitions vector
		(
			"BlockMove", 
			TestFunction::WithStreams(|ptr, size, tid, em, timing, streams| unsafe { 
				block_move_test(ptr, size, tid, em, timing, streams) 
			}),
			TestMemoryConfig::new(
				WindowMode::FullAllocation,              // Need full allocation for src+dst
				BlockMode::FixedSize { size_mb: 16 },    // 16MB blocks
				false,  // Require alignment
				false   // Doesn't need locality
			).with_timing(TestTiming::duration_only(20)) // 20 seconds of block copying
			 .with_streams(1) // Default single stream
			 .with_memory_type(None)  // Default WriteBack
		),
    ]
}

// Update run_single_test_cycle in runner.rs for NUMA awareness
// Update run_single_test_cycle to use remapping
fn run_single_test_cycle(
    thread_blocks: &HashMap<usize, Vec<BlockInfo>>,
    allocated_blocks: &mut HashMap<usize, Vec<AllocatedBlock>>,
    test_definitions: &[(&'static str, TestFunction, TestMemoryConfig)],
    error_mode: ErrorMode,
    progress: Arc<ProgressTracker>,
    cycle_summaries: Arc<Mutex<Vec<TestSummary>>>,
    cycle_number: u32,
) -> bool {
    let thread_count = thread_blocks.len();
    let success = Arc::new(AtomicBool::new(true));
    
	let supports_remapping = allocated_blocks.values()
		.any(|blocks| blocks.iter().any(|b| b.supports_remapping));

    for (test_idx, (test_name, test_func, test_config)) in test_definitions.iter().enumerate() {
        if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
            return false;
        }
        
        progress.set_phase(&format!("{} ({}/{})", test_name, test_idx + 1, test_definitions.len()));
        
        // Determine required memory type for this test
        let required_memory_type = test_config.memory_type.unwrap_or(MemoryType::WriteBack);
        
        // Remap memory if needed (fast operation, no reallocation)
		if supports_remapping && test_config.memory_type.is_some() {
			let required_memory_type = test_config.memory_type.unwrap_or(MemoryType::WriteBack);
			
			if let Ok(driver) = DriverHandle::open() {
				let remap_start = Instant::now();
				if let Err(e) = remap_all_allocations_to_type(allocated_blocks, required_memory_type, &driver) {
					log::warn!("Failed to remap memory for test {}: {} - continuing with current memory type", test_name, e);
				} else {
					let remap_time = remap_start.elapsed();  // Move this inside the else block
					if remap_time.as_millis() > 0 {
						log::info!("Memory remapped to {:?} for {} in {:?}", 
								 required_memory_type, test_name, remap_time);
					}
				}
			}
		} else if test_config.memory_type.is_some() {
			log::debug!("Memory remapping not available - using default memory type");
		}
               
        let test_start = Instant::now();
        let barrier = Arc::new(Barrier::new(thread_count));
        let test_stats = Arc::new(Mutex::new(Vec::new()));
        
        // Extract allocated blocks
        let mut thread_allocated_data = Vec::new();
        for (thread_id, _) in thread_blocks {
            let blocks = allocated_blocks.remove(thread_id).unwrap();
            thread_allocated_data.push((*thread_id, blocks));
        }

        let mut handles = vec![];
        for (idx, (thread_id, blocks)) in thread_allocated_data.into_iter().enumerate() {
            let success_clone = Arc::clone(&success);
            let barrier_clone = Arc::clone(&barrier);
            let test_stats_clone = Arc::clone(&test_stats);
            let test_name = test_name.to_string();
            let test_func = test_func.clone();
            let test_config = test_config.clone();
			// Get CPU assignments and pinning config
			let pinning_config = CpuPinningConfig::default(); // Or get from your config
			let cpu_assignments: Vec<(usize, u32)> = (0..thread_count)
				.map(|i| (i, get_numa_node_for_cpu(i)))
				.collect();
            let (cpu_id, numa_node) = cpu_assignments[idx];

            let handle = thread::spawn(move || {
				// Apply thread priority if configured
				if let Err(e) = PerformanceConfig::default().apply() {
					log::warn!("Failed to set thread priority: {}", e);
				}
				
				// Pin thread to CPU
				if pinning_config.enable_pinning {
					match pin_thread_to_cpu_with_config(thread_id, thread_count, &pinning_config) {
						Ok(actual_cpu) => {
							log::debug!("Thread {} pinned to CPU {} (NUMA {})", 
									  thread_id, actual_cpu, numa_node);
							let thread_handle = unsafe { windows::Win32::System::Threading::GetCurrentThread() };
							let _ = set_thread_ideal_processor_ex(thread_handle, actual_cpu as u32, numa_node);
						}
						Err(e) => {
							log::warn!("Thread {} pinning failed: {}", thread_id, e);
						}
					}
				}
                
                // Wait for all threads
                barrier_clone.wait();
                
                let thread_start = Instant::now();
                let mut total_bytes = 0u64;
                let mut total_errors = 0u64;
                
                // Run test on all blocks
                for allocated_block in &blocks {
                    if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
                        success_clone.store(false, Ordering::Relaxed);
                        break;
                    }
                    
					// Run test with original config - NUMA is already handled at allocation time
					match run_test_with_memory_stages(
						&test_name,
						&test_func,
						allocated_block,
						&test_config,  // Use original test_config
						thread_id,
						error_mode,
					) {
						Ok(stats) => {
							total_bytes += stats.bytes_processed as u64;
							total_errors += stats.error_count;
							
							if stats.error_count > 0 {
								if let Err(e) = handle_test_errors(&stats, error_mode, &test_name) {
									log::error!("[Thread {} on CPU {}] {}", thread_id, cpu_id, e);
									success_clone.store(false, Ordering::Relaxed);
									break;
								}
							}
						}
						Err(e) => {
							log::error!("[Thread {} on CPU {}] Test {} failed: {}", 
									  thread_id, cpu_id, test_name, e);
							success_clone.store(false, Ordering::Relaxed);
							break;
						}
					}
                }
                
                let thread_elapsed = thread_start.elapsed().as_millis();
                if let Ok(mut stats) = test_stats_clone.lock() {
                    stats.push((thread_id, total_bytes, thread_elapsed, total_errors));
                }
                (thread_id, blocks)
            });
            handles.push(handle);
        }

        // Collect results
        for handle in handles {
            if let Ok((thread_id, blocks)) = handle.join() {
                allocated_blocks.insert(thread_id, blocks);
            } else {
                success.store(false, Ordering::Relaxed);
            }
        }
        
        // Process test statistics...
        // (rest of the function remains the same)
    }

    success.load(Ordering::Relaxed)
}

// Helper to get performance config from somewhere (config file, etc)
fn get_performance_config() -> Option<PerformanceConfig> {
    // This would come from your config system
    Some(PerformanceConfig::default())
}

// Clone implementation for TestFunction
impl Clone for TestFunction {
    fn clone(&self) -> Self {
        match self {
            TestFunction::Simple(f) => TestFunction::Simple(*f),
            TestFunction::WithStreams(f) => TestFunction::WithStreams(*f),
            TestFunction::WithConfig(f) => TestFunction::WithConfig(*f),
        }
    }
}

// Updated to pass correct parameters based on test function type
fn run_test_with_memory_stages(
    test_name: &str,
    test_func: &TestFunction,
    allocated_block: &AllocatedBlock,
    test_config: &TestMemoryConfig,
    thread_id: usize,
    error_mode: ErrorMode,
) -> Result<TestStats, String> {
    let allocated_size = allocated_block.buffer.size();
    let allocated_ptr = allocated_block.buffer.as_mut_ptr();
    
    // Stage 2: Calculate window size
    let window_size = test_config.calculate_window_size(test_name, allocated_size);
    
    // Stage 3: Calculate block size with alignment
    let block_size = test_config.calculate_block_size(test_name, window_size);
    
    // Ensure window is multiple of block size
    let final_window_size= test_config.align_window_to_blocks(window_size, block_size);
    
    // Log configuration once at start
    let allocated_mb = allocated_size as f64 / (1024.0 * 1024.0);
    let window_mb = final_window_size as f64 / (1024.0 * 1024.0);
    let block_mb = block_size as f64 / (1024.0 * 1024.0);
    let window_percent = (final_window_size as f64 / allocated_size as f64) * 100.0;
    
    // Log configuration once per test (only for thread 0 to avoid spam)
    if thread_id == 0 {
        log::info!(
            "{} - Window: {:.1}MB ({:.1}%), Block: {:.1}MB, Streams: {}{}",
            test_name, window_mb, window_percent, block_mb, test_config.streams,
            if test_config.allow_misaligned { ", misaligned" } else { "" }
        );
    }
    
    // Debug log for all threads if needed
    log::debug!(
        "[Thread {}] {} - Configuration: Window {:.1}MB of {:.1}MB ({:.1}%), Block {:.1}MB, Streams: {}{}",
        thread_id, test_name, window_mb, allocated_mb, window_percent, block_mb, test_config.streams,
        if test_config.allow_misaligned { ", misaligned" } else { "" }
    );
    
    // Run the test function with appropriate parameters
    let stats = unsafe {
        match test_func {
            TestFunction::Simple(f) => {
                f(allocated_ptr, final_window_size, thread_id, error_mode, &test_config.timing)
            }
            TestFunction::WithStreams(f) => {
                f(allocated_ptr, final_window_size, thread_id, error_mode, &test_config.timing, test_config.streams)
            }
            TestFunction::WithConfig(f) => {
                f(allocated_ptr, final_window_size, thread_id, error_mode, &test_config.timing, test_config)
            }
        }
    };
    
    Ok(stats)
}

#[derive(Debug)]
struct ThreadAllocationSummary {
    thread_id: usize,
    total_size: usize,
    huge_pages: usize,
    large_pages: usize,
    regular_bytes: usize,
}

fn allocate_all_blocks(
    thread_blocks: &HashMap<usize, Vec<BlockInfo>>, 
    runtime_config: &RuntimeConfig
) -> Result<HashMap<usize, Vec<AllocatedBlock>>, String> {
    let mut allocated_blocks = HashMap::new();
    let thread_count = thread_blocks.len();
    
    // Check DMA driver and probe for huge pages
    let dma_available = DmaBuffer::is_driver_available();
    let mut huge_pages_available = 0usize;
    
    if dma_available {
        log::info!("TMR kernel driver detected - probing for huge page availability");
        
        if let Ok(stats) = DmaBuffer::get_memory_stats() {
            log::info!("DMA Memory Stats:");
            log::info!("  Total Physical: {:.2} GiB", stats.total_physical_memory as f64 / (1024.0 * 1024.0 * 1024.0));
            log::info!("  Available Physical: {:.2} GiB", stats.available_physical_memory as f64 / (1024.0 * 1024.0 * 1024.0));
            log::info!("  Available Large Pages: {}", stats.available_large_pages);
            log::info!("  Available Huge Pages: {}", stats.available_huge_pages);
            
            huge_pages_available = stats.available_huge_pages as usize;
        }
    }
    
    // Calculate fair distribution of huge pages
    let huge_pages_per_thread = if huge_pages_available > 0 {
        huge_pages_available / thread_count
    } else {
        0
    };
    let extra_huge_pages = huge_pages_available % thread_count;
    
    if huge_pages_available > 0 {
        log::info!("Huge page distribution: {} per thread + {} extra for first threads", 
                 huge_pages_per_thread, extra_huge_pages);
    }
    
    // Enhanced tracking for summaries
    let mut total_allocated = 0usize;
    let mut huge_page_segments = 0usize;
    let mut large_page_segments = 0usize;
    let mut regular_page_segments = 0usize;
    let mut threads_allocated = 0usize;
    

    let mut thread_summaries = Vec::new();
    
    // Sort threads for consistent allocation order
    let mut sorted_threads: Vec<_> = thread_blocks.iter().collect();
    sorted_threads.sort_by_key(|(id, _)| *id);
    
    for (thread_idx, (thread_id, blocks)) in sorted_threads.into_iter().enumerate() {
        let mut thread_allocated = Vec::new();
        let mut thread_stats = ThreadAllocationSummary {
            thread_id: *thread_id,
            total_size: 0,
            huge_pages: 0,
            large_pages: 0,
            regular_bytes: 0,
        };
        
        // Calculate this thread's huge page quota
        let thread_huge_pages = huge_pages_per_thread + 
                               if thread_idx < extra_huge_pages { 1 } else { 0 };
        
        log::info!("Thread {} allocated {} huge pages quota", thread_id, thread_huge_pages);
        
        for (block_idx, block) in blocks.iter().enumerate() {
            match runtime_config.memory_backend {
                MemoryBackend::KernelDriver => {
					if runtime_config.use_driver_chunking {
						// Let driver handle chunking - single request with min/max constraints
						let config = DmaConfig {
							minimum_page_size: PageSize::Regular,
							maximum_page_size: if thread_huge_pages > 0 { PageSize::Huge } else { PageSize::Large },
							prefer_numa_node: Some(get_numa_node_for_cpu(*thread_id)),
							zero_memory: false,
							memory_type: MemoryType::WriteBack,
							contiguous: true,
							timeout_ms: 10000,
							retry_interval_ms: 10,
							max_retries: 100,
							strict_numa: false,
						};
						
						match DmaBufferEnhanced::new_with_config(block.size_bytes, config) {
							Ok(dma) => {
								// Log what the driver actually allocated
								log::info!("Thread {}: Driver allocated {} bytes with {} segments",
										 thread_id, block.size_bytes, dma.segments().len());
								
								// Update stats based on what driver gave us
								for segment in dma.segments() {
									match segment.page_size_kb {
										1048576 => {
											huge_page_segments += 1;
											thread_stats.huge_pages += 1;
										}
										2048 => {
											large_page_segments += 1;
											thread_stats.large_pages += 1;
										}
										4 => {
											regular_page_segments += segment.size / 4096;
											thread_stats.regular_bytes += segment.size;
										}
										_ => {}
									}
								}
								
								thread_stats.total_size += dma.size();
								thread_allocated.push(AllocatedBlock {
									buffer: EnhancedTestBuffer::DmaEnhanced(dma),
									block_info: block.clone(),
									memory_type: MemoryType::WriteBack,
									numa_node: get_numa_node_for_cpu(*thread_id),
									supports_remapping: true,
								});
							}
							Err(e) => {
								return Err(format!(
									"Driver-chunked allocation failed for thread {} (size: {:.2} GiB): {}",
									thread_id,
									block.size_bytes as f64 / (1024.0 * 1024.0 * 1024.0),
									e
								));
							}
						}
					} else {
						// Use the new chunked allocation
						let allocations = allocate_block_chunked(
							block.size_bytes,
							thread_id,
							thread_huge_pages,
							runtime_config
						)?;
						
						// Gather stats from allocations
						for alloc in allocations {
							thread_stats.total_size += alloc.buffer.size();
							
							if let EnhancedTestBuffer::DmaEnhanced(ref dma) = alloc.buffer {
								for segment in dma.segments() {
									match segment.page_size_kb {
										1048576 => {
											huge_page_segments += 1;
											thread_stats.huge_pages += 1;
										}
										2048 => {
											large_page_segments += 1;
											thread_stats.large_pages += 1;
										}
										4 => {
											regular_page_segments += segment.size / 4096;
											thread_stats.regular_bytes += segment.size;
										}
										_ => {}
									}
								}
							}
                        thread_allocated.push(alloc);
						}
					}
				}
				MemoryBackend::NativeLargePages => {
					match TestBuffer::new_large_pages(block.size_bytes) {
						Some(buf) => {
							let size = buf.size();
							thread_stats.total_size += size;
							
							// Native large pages are always 2MB
							if buf.uses_large_pages() {
								let large_page_count = size / (2 * 1024 * 1024);
								thread_stats.large_pages += large_page_count;
								large_page_segments += large_page_count;
							}
							
							thread_allocated.push(AllocatedBlock {
								buffer: EnhancedTestBuffer::Regular(buf),
								block_info: block.clone(),
								memory_type: MemoryType::WriteBack,
								numa_node: get_numa_node_for_cpu(*thread_id),
								supports_remapping: false,
							});
						}
						None => {
							return Err(format!(
								"Large page allocation failed for thread {} (block {}/{}, size: {:.2} GiB)",
								thread_id, 
								block_idx + 1, 
								blocks.len(),
								block.size_bytes as f64 / (1024.0 * 1024.0 * 1024.0)
							));
						}
					}
				}
				MemoryBackend::NativeRegular => {
					match TestBuffer::new_aligned(block.size_bytes) {
						Some(buf) => {
							let size = buf.size();
							thread_stats.total_size += size;
							
							// Native regular pages are always 4KB
							let regular_page_count = size / 4096;
							thread_stats.regular_bytes += size;
							regular_page_segments += regular_page_count;
							
							thread_allocated.push(AllocatedBlock {
								buffer: EnhancedTestBuffer::Regular(buf),
								block_info: block.clone(),
								memory_type: MemoryType::WriteBack,
								numa_node: get_numa_node_for_cpu(*thread_id),
								supports_remapping: false,
							});
						}
						None => {
							return Err(format!(
								"Regular allocation failed for thread {} (block {}/{}, size: {:.2} GiB)",
								thread_id, 
								block_idx + 1, 
								blocks.len(),
								block.size_bytes as f64 / (1024.0 * 1024.0 * 1024.0)
							));
						}
					}
				}
            }
        }
        
        total_allocated += thread_stats.total_size;
        thread_summaries.push(thread_stats);
        allocated_blocks.insert(*thread_id, thread_allocated);
        threads_allocated += 1;
        
        log::info!("Thread {} completed allocation successfully", thread_id);
    }
    
    // Print enhanced summary
    print_enhanced_allocation_summary(
        total_allocated,
        huge_page_segments,
        large_page_segments,
        regular_page_segments,
        &thread_summaries
    );
    
    Ok(allocated_blocks)
}

fn allocate_block_chunked(
    total_size: usize,
    thread_id: &usize,
    huge_page_quota: usize,
    _runtime_config: &RuntimeConfig,
) -> Result<Vec<AllocatedBlock>, String> {
    let mut allocations = Vec::new();
    let mut remaining_size = total_size;
    let numa_node = get_numa_node_for_cpu(*thread_id);
    
    // Phase 1: Calculate and request huge pages in ONE allocation
    if huge_page_quota > 0 && remaining_size >= 1024 * 1024 * 1024 {
        // Calculate how many whole huge pages fit
        let huge_pages_needed = remaining_size / (1024 * 1024 * 1024);
        let huge_pages_to_allocate = huge_pages_needed.min(huge_page_quota);
        let huge_bytes = huge_pages_to_allocate * 1024 * 1024 * 1024;
        
        if huge_bytes > 0 {
            log::debug!("Thread {}: Requesting {} GiB in huge pages", thread_id, huge_bytes / (1024*1024*1024));
            
            let config = DmaConfig {
                minimum_page_size: PageSize::Huge,
				maximum_page_size: PageSize::Huge,  // Force huge pages only
                prefer_numa_node: Some(numa_node),
                zero_memory: false,
                memory_type: MemoryType::WriteBack,
                contiguous: true,
                timeout_ms: 10000,
                retry_interval_ms: 10,
                max_retries: 100,
                strict_numa: false,
            };
            
            match DmaBufferEnhanced::new_with_config(huge_bytes, config) {
                Ok(dma) => {
                    // Verify we got what we asked for
                    let huge_count = dma.segments().iter()
                        .filter(|s| s.page_size_kb == 1048576)
                        .count();
                    log::info!("Thread {}: Allocated {} huge pages", thread_id, huge_count);
                    
                    allocations.push(AllocatedBlock {
                        buffer: EnhancedTestBuffer::DmaEnhanced(dma),
                        block_info: BlockInfo {
                            size_bytes: huge_bytes,
                            thread_id: *thread_id,
                        },
                        memory_type: MemoryType::WriteBack,
                        numa_node,
                        supports_remapping: true,
                    });
                    remaining_size -= huge_bytes;
                }
                Err(e) => {
                    log::debug!("Thread {}: Huge page allocation failed: {}", thread_id, e);
                    // Continue with full remaining_size for next tier
                }
            }
        }
    }
    
    // Phase 2: Calculate and request large pages in ONE allocation
    if remaining_size >= 2 * 1024 * 1024 {
        // Calculate how many whole large pages fit
        let large_pages_needed = remaining_size / (2 * 1024 * 1024);
        let large_bytes = large_pages_needed * 2 * 1024 * 1024;
        
        if large_bytes > 0 {
            log::debug!("Thread {}: Requesting {} MiB in large pages", thread_id, large_bytes / (1024*1024));
            
            let config = DmaConfig {
				minimum_page_size: PageSize::Large,
				maximum_page_size: PageSize::Large,  // Force large pages only
                prefer_numa_node: Some(numa_node),
                zero_memory: false,
                memory_type: MemoryType::WriteBack,
                contiguous: true,
                timeout_ms: 10000,
                retry_interval_ms: 10,
                max_retries: 100,
                strict_numa: false,
            };
            
            match DmaBufferEnhanced::new_with_config(large_bytes, config) {
                Ok(dma) => {
                    let large_count = dma.segments().iter()
                        .filter(|s| s.page_size_kb == 2048)
                        .count();
                    log::info!("Thread {}: Allocated {} large pages", thread_id, large_count);
                    
                    allocations.push(AllocatedBlock {
                        buffer: EnhancedTestBuffer::DmaEnhanced(dma),
                        block_info: BlockInfo {
                            size_bytes: large_bytes,
                            thread_id: *thread_id,
                        },
                        memory_type: MemoryType::WriteBack,
                        numa_node,
                        supports_remapping: true,
                    });
                    remaining_size -= large_bytes;
                }
                Err(e) => {
                    log::debug!("Thread {}: Large page allocation failed: {}", thread_id, e);
                    // Continue with full remaining_size for next tier
                }
            }
        }
    }
    
    // Phase 3: Request any remainder as regular pages in ONE allocation
    if remaining_size > 0 {
        log::debug!("Thread {}: Requesting {} bytes in regular pages", thread_id, remaining_size);
        
        let config = DmaConfig {
			minimum_page_size: PageSize::Regular,
			maximum_page_size: PageSize::Regular,  // Force regular pages only
            prefer_numa_node: Some(numa_node),
            zero_memory: false,
            memory_type: MemoryType::WriteBack,
            contiguous: true,
            timeout_ms: 10000,
            retry_interval_ms: 10,
            max_retries: 100,
            strict_numa: false,
        };
        
        match DmaBufferEnhanced::new_with_config(remaining_size, config) {
            Ok(dma) => {
                allocations.push(AllocatedBlock {
                    buffer: EnhancedTestBuffer::DmaEnhanced(dma),
                    block_info: BlockInfo {
                        size_bytes: remaining_size,
                        thread_id: *thread_id,
                    },
                    memory_type: MemoryType::WriteBack,
                    numa_node,
                    supports_remapping: true,
                });
            }
            Err(e) => {
                log::warn!("Thread {}: Regular page allocation failed: {}", thread_id, e);
            }
        }
    }
    
    // Check if we allocated everything requested
    let allocated_total = allocations.iter().map(|a| a.block_info.size_bytes).sum::<usize>();
    if allocated_total < total_size {
        if allocations.is_empty() {
            Err(format!("Thread {}: Failed to allocate any memory", thread_id))
        } else {
            log::warn!("Thread {}: Partial allocation - got {:.2} GiB of {:.2} GiB requested",
                     thread_id,
                     allocated_total as f64 / (1024.0 * 1024.0 * 1024.0),
                     total_size as f64 / (1024.0 * 1024.0 * 1024.0));
            Ok(allocations)
        }
    } else {
        Ok(allocations)
    }
}

fn print_enhanced_allocation_summary(
    total_allocated: usize,
    huge_segments: usize,
    large_segments: usize,
    regular_segments: usize,
    thread_summaries: &[ThreadAllocationSummary]
) {
    println!("\n=== Stage 1 Memory Allocation Summary ===");
    println!("Total Allocated: {:.2} GiB across {} threads",
            total_allocated as f64 / (1024.0 * 1024.0 * 1024.0),
            thread_summaries.len());
    
    // Always show page distribution section
    println!("\nPage Type Distribution:");
    if huge_segments > 0 {
        println!("  ✅ Huge Pages (1GB): {} pages = {:.2} GiB",
                huge_segments,
                huge_segments as f64);
    } else {
        println!("  ❌ Huge Pages (1GB): Not available");
    }
    
    if large_segments > 0 {
        println!("  ✅ Large Pages (2MB): {} pages = {:.2} GiB", 
                large_segments,
                (large_segments * 2) as f64 / 1024.0);
    } else {
        println!("  ❌ Large Pages (2MB): Not available");
    }
    
    if regular_segments > 0 {
        println!("  ✅ Regular Pages (4KB): ~{} pages = {:.2} MiB",
                regular_segments,
                (regular_segments * 4) as f64 / 1024.0);
    }
    
    // Per-thread breakdown
    println!("\nPer-Thread Allocation:");
    for summary in thread_summaries {
        print!("  Thread {}: {:.2} GiB total",
               summary.thread_id,
               summary.total_size as f64 / (1024.0 * 1024.0 * 1024.0));
        
        let mut components = Vec::new();
        if summary.huge_pages > 0 {
            components.push(format!("{}×1GB", summary.huge_pages));
        }
        if summary.large_pages > 0 {
            components.push(format!("{}×2MB", summary.large_pages));
        }
        if summary.regular_bytes > 0 {
            if summary.regular_bytes >= 1024 * 1024 {
                components.push(format!("{:.1}MB regular", 
                                      summary.regular_bytes as f64 / (1024.0 * 1024.0)));
            } else {
                components.push(format!("{:.1}KB regular", 
                                      summary.regular_bytes as f64 / 1024.0));
            }
        }
        
        if !components.is_empty() {
            println!(" ({})", components.join(" + "));
        } else {
            println!(" (allocation type unknown)");
        }
    }
    
    // Allocation efficiency (only if we have detailed stats)
    if huge_segments > 0 || large_segments > 0 || regular_segments > 0 {
        let huge_gib = huge_segments as f64;
        let large_gib = (large_segments * 2) as f64 / 1024.0;
        let regular_gib = (regular_segments * 4) as f64 / (1024.0 * 1024.0);
        let total_gib = huge_gib + large_gib + regular_gib;
        
        if total_gib > 0.0 {
            println!("\nMemory Efficiency:");
            if huge_segments > 0 {
                println!("  Huge Pages: {:.1}% of total", (huge_gib / total_gib) * 100.0);
            }
            if large_segments > 0 {
                println!("  Large Pages: {:.1}% of total", (large_gib / total_gib) * 100.0);
            }
            if regular_segments > 0 {
                println!("  Regular Pages: {:.1}% of total", (regular_gib / total_gib) * 100.0);
            }
        }
    }
    
    println!();
}

fn print_allocation_summary(allocated_blocks: &HashMap<usize, Vec<AllocatedBlock>>) {
    let mut total_size = 0usize;
    let mut large_page_blocks = 0;
    let mut dma_blocks = 0;
    let mut huge_page_blocks = 0;
    let mut size_per_thread = 0usize;

    for (thread_id, blocks) in allocated_blocks {
        let thread_total_size: usize = blocks.iter().map(|b| b.buffer.size()).sum();
        
        for block in blocks {
            match &block.buffer {
                EnhancedTestBuffer::Regular(regular) => {
                    if regular.uses_large_pages() {
                        large_page_blocks += 1;
                    }
                }
                EnhancedTestBuffer::Dma(_) => {
                    dma_blocks += 1;
                }
                EnhancedTestBuffer::DmaEnhanced(dma) => {
                    dma_blocks += 1;
                    if dma.has_huge_pages() {
                        huge_page_blocks += 1;
                    } else if dma.has_large_pages() {
                        large_page_blocks += 1;
                    }
                }
            }
        }

        total_size += thread_total_size;

        if *thread_id == 0 {
            size_per_thread = thread_total_size;
        }
    }

    let thread_count = allocated_blocks.len();
    let size_per_thread_gib = size_per_thread as f64 / (1024.0 * 1024.0 * 1024.0);

    println!("Stage 1 Memory Allocation Summary:");
    println!(
        "  {} × {:.2} GiB per thread = {:.2} GiB total (TM5-style maximum allocation)",
        thread_count, size_per_thread_gib,
        total_size as f64 / (1024.0 * 1024.0 * 1024.0)
    );

    if dma_blocks > 0 {
        println!("  ✅ {} blocks using DMA allocation (kernel driver - physical memory)", dma_blocks);
        if huge_page_blocks > 0 {
            println!("    - {} blocks with 1GB huge pages", huge_page_blocks);
        }
        if large_page_blocks > 0 {
            println!("    - {} blocks with 2MB large pages", large_page_blocks);
        }
    } else if large_page_blocks > 0 {
        println!("  ✅ {} blocks using large pages (2MB pages)", large_page_blocks);
    } else {
        println!("  ⚠️  Using standard 4KB pages (DMA driver not available, large pages not available)");
    }
    println!();
}

fn print_test_configuration(test_definitions: &[(&'static str, TestFunction, TestMemoryConfig)], suite_timing: &TestSuiteTiming) {
    println!("Test Configuration:");
    
    // Suite timing
    print!("  Suite Timing: ");
    match (suite_timing.global_cycles, suite_timing.global_duration_secs) {
        (Some(cycles), Some(duration)) => println!("{} cycles or {}s max", cycles, duration),
        (Some(cycles), None) => println!("{} cycles", cycles),
        (None, Some(duration)) => println!("{}s duration", duration),
        (None, None) => println!("Unlimited"),
    }
    
    // Test sequence
    println!("  Test Sequence: {} tests", test_definitions.len());
    
    // Individual tests
    for (i, (name, _, config)) in test_definitions.iter().enumerate() {
        print!("    {}. {} - ", i + 1, name);
        
        // Timing
        match (&config.timing.cycles, &config.timing.duration_secs) {
            (Some(c), Some(d)) => print!("{}cycles/{}s, ", c, d),
            (Some(c), None) => print!("{}cycles, ", c),
            (None, Some(d)) => print!("{}s, ", d),
            (None, None) => print!("unlimited, "),
        }
        
        // Streams (equivalent to TM5 jump/parameter)
        print!("{}streams", config.streams);
        
        // Window mode
        match &config.window_mode {
            WindowMode::FullAllocation => print!(", FullWindow"),
            WindowMode::FixedSize { size_mb } => print!(", Window:{}MB", size_mb),
            WindowMode::CacheRelative { multiplier } => print!(", Window:{}xCache", multiplier),
        }
        
        // Block mode
        match &config.block_mode {
            BlockMode::AutoOptimal => print!(", AutoBlock"),
            BlockMode::FixedSize { size_mb } => print!(", Block:{}MB", size_mb),
            BlockMode::WindowFraction { fraction } => print!(", Block:{:.1}%", fraction * 100.0),
        }
        
        if config.allow_misaligned {
            print!(", Misaligned");
        }
        if config.requires_locality {
            print!(", Locality");
        }
        
        println!();
    }
}

fn print_cycle_report(cycle: u32, duration_secs: u32, summaries: &[TestSummary]) {
    println!("\n--- Cycle {} Report ---", cycle);
    println!("Duration: {}s", duration_secs);
    
    let total_bytes: u64 = summaries.iter().map(|s| s.bytes_processed).sum();
    let total_errors: u64 = summaries.iter().map(|s| s.errors).sum();
    let avg_throughput_mib = if duration_secs > 0 {
        (total_bytes as f64 / (1024.0 * 1024.0)) / duration_secs as f64
    } else {
        0.0
    };
    let avg_throughput_gib = avg_throughput_mib / 1024.0;
    
    println!("Tests completed: {}", summaries.len());
    println!("Total data processed: {:.2} GiB", total_bytes as f64 / (1024.0 * 1024.0 * 1024.0));
    println!("Average throughput: {:.1} MiB/s ({:.2} GiB/s)", avg_throughput_mib, avg_throughput_gib);
    println!("Total errors: {}", total_errors);
    
    if !summaries.is_empty() {
        println!("\nTest Performance:");
        for (i, summary) in summaries.iter().enumerate() {
            let test_number = i + 1;
            let throughput_gib_s = summary.throughput_mib_s / 1024.0;
            
            println!("  {}. {} - {:.1}s, {:.2} GiB @ {:.1} MiB/s ({:.2} GiB/s){}",
                test_number,
                summary.name,
                summary.duration_ms as f64 / 1000.0,
                summary.bytes_processed as f64 / (1024.0 * 1024.0 * 1024.0),
                summary.throughput_mib_s,
                throughput_gib_s,
                if summary.errors > 0 { 
                    format!(" [⚠️ ERRORS: {}]", summary.errors) 
                } else { 
                    String::new() 
                }
            );
        }
    }
}

fn print_final_summary(progress: &ProgressTracker, total_time: std::time::Duration, test_result: &TestRunResult, test_definitions: &[(&'static str, TestFunction, TestMemoryConfig)]) {
    let cycle_stats = progress.get_cycle_stats();
    let total_bytes = progress.total_bytes_processed.load(Ordering::Relaxed);
    let total_errors = progress.total_errors.load(Ordering::Relaxed);
    
    println!("\n=== Final Test Summary ===");
    println!("Total runtime: {}", format_duration(total_time));
    println!("Cycles completed: {}", cycle_stats.len());
    println!("Total data processed: {:.2} GiB", total_bytes as f64 / (1024.0 * 1024.0 * 1024.0));
    
    // Calculate overall throughput
    let overall_throughput_mib = if total_time.as_secs() > 0 {
        (total_bytes as f64 / (1024.0 * 1024.0)) / total_time.as_secs() as f64
    } else {
        0.0
    };
    let overall_throughput_gib = overall_throughput_mib / 1024.0;
    
    println!("Overall throughput: {:.1} MiB/s ({:.2} GiB/s)", overall_throughput_mib, overall_throughput_gib);
    println!("Total errors detected: {}", total_errors);
    
    // Print per-test breakdown
    if !cycle_stats.is_empty() {
        println!("\nPer-Test Performance Summary (averaged across {} cycles):", cycle_stats.len());
        
        // Create test aggregates
        let mut test_aggregates: HashMap<String, (u64, u128, u64)> = HashMap::new(); // bytes, duration_ms, errors
        
        for cycle in &cycle_stats {
            for test_summary in &cycle.test_stats {
                let entry = test_aggregates.entry(test_summary.name.clone()).or_insert((0, 0, 0));
                entry.0 += test_summary.bytes_processed;
                entry.1 += test_summary.duration_ms;
                entry.2 += test_summary.errors;
            }
        }
        
        for (i, (test_name, _, _)) in test_definitions.iter().enumerate() {
            if let Some((total_bytes, total_duration_ms, total_errors)) = test_aggregates.get(*test_name) {
                let test_number = i + 1;
                let cycle_count = cycle_stats.len() as u64;
                let avg_bytes = *total_bytes / cycle_count;
                let avg_duration_ms = *total_duration_ms / cycle_count as u128;
                let avg_throughput_mib = if avg_duration_ms > 0 {
                    (avg_bytes as f64 / (1024.0 * 1024.0)) / (avg_duration_ms as f64 / 1000.0)
                } else {
                    0.0
                };
                let avg_throughput_gib = avg_throughput_mib / 1024.0;
                
                println!("  {}. {} - {:.1}s avg, {:.2} GiB avg @ {:.1} MiB/s ({:.2} GiB/s){}",
                    test_number,
                    test_name,
                    avg_duration_ms as f64 / 1000.0,
                    avg_bytes as f64 / (1024.0 * 1024.0 * 1024.0),
                    avg_throughput_mib,
                    avg_throughput_gib,
                    if *total_errors > 0 { 
                        format!(" [⚠️ TOTAL ERRORS: {}]", total_errors) 
                    } else { 
                        String::new() 
                    }
                );
            }
        }
    }
    
    println!("\nResult saved to: .\\{}", test_result.get_filename());
}

// Modern NUMA topology discovery for Windows 11+
pub fn discover_numa_topology() -> Result<NumaTopology, String> {
    unsafe {
        let mut buffer_size = 0u32;
        
        // Get required buffer size
        let _ = GetLogicalProcessorInformationEx(
            RelationNumaNode,
            None,
            &mut buffer_size,
        );
        
        if buffer_size == 0 {
            return Ok(NumaTopology {
                node_count: 1,
                nodes: vec![NumaNodeInfo {
                    node_id: 0,
                    group_count: 1,
                    group_masks: vec![GroupMask { group: 0, mask: !0 }],
                    cpu_count: num_cpus::get() as u32,
                    cpus: (0..num_cpus::get() as u32).collect(),
                }],
                cpu_to_node: (0..num_cpus::get() as u32).map(|cpu| (cpu, 0)).collect(),
            });
        }
        
        let mut buffer = vec![0u8; buffer_size as usize];
        
        // Get NUMA topology
        GetLogicalProcessorInformationEx(
            RelationNumaNode,
            Some(buffer.as_mut_ptr() as *mut SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX),
            &mut buffer_size,
        ).map_err(|e| format!("Failed to get NUMA topology: {:?}", e))?;
        
        let mut nodes = Vec::new();
        let mut cpu_to_node = HashMap::new();
        let mut offset = 0;
        
        while offset < buffer_size as usize {
            let info = &*(buffer.as_ptr().add(offset) as *const SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX);
            
            if info.Relationship == RelationNumaNode {
                let numa_info = &info.Anonymous.NumaNode;
                let node_id = numa_info.NodeNumber;
                
                // Extract CPU information from group affinity
                let mut cpus = Vec::new();
                let mut group_masks = Vec::new();
                
                // The group affinity is stored in the Anonymous union
                let group_affinity = &numa_info.Anonymous.GroupMask;
                let group = group_affinity.Group;
                let mask = group_affinity.Mask;
                
                group_masks.push(GroupMask { group, mask });
                
                // Extract individual CPU IDs from the mask
                for bit in 0..64 {
                    if (mask & (1u64 << bit) as usize) != 0 {
                        let cpu_id = (group as u32 * 64) + bit;
                        cpus.push(cpu_id);
                        cpu_to_node.insert(cpu_id, node_id);
                    }
                }
                
                nodes.push(NumaNodeInfo {
                    node_id,
                    group_count: 1, // Each NUMA node entry has one group in this structure
                    group_masks,
                    cpu_count: cpus.len() as u32,
                    cpus,
                });
            }
            
            offset += info.Size as usize;
        }
        
        Ok(NumaTopology {
            node_count: nodes.len() as u32,
            nodes,
            cpu_to_node,
        })
    }
}

// Get NUMA node for a specific CPU
pub fn get_numa_node_for_cpu(cpu_id: usize) -> u32 {
    static NUMA_TOPOLOGY: OnceLock<NumaTopology> = OnceLock::new();
    
    let topology = NUMA_TOPOLOGY.get_or_init(|| {
        discover_numa_topology().unwrap_or_else(|e| {
            log::warn!("Failed to discover NUMA topology: {}", e);
            // Return single node topology as fallback
            NumaTopology {
                node_count: 1,
                nodes: vec![],
                cpu_to_node: HashMap::new(),
            }
        })
    });
    
    topology.cpu_to_node.get(&(cpu_id as u32)).copied().unwrap_or(0)
}

fn get_numa_node_count() -> usize {
    unsafe {
        use windows::Win32::System::SystemInformation::{
            GetLogicalProcessorInformationEx, RelationNumaNode, 
            SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX
        };
        
        let mut buffer_size = 0u32;
        let _ = GetLogicalProcessorInformationEx(RelationNumaNode, None, &mut buffer_size);
        
        if buffer_size == 0 {
            return 1; // Single NUMA node system
        }
        
        let mut buffer = vec![0u8; buffer_size as usize];
        if GetLogicalProcessorInformationEx(
            RelationNumaNode,
            Some(buffer.as_mut_ptr() as *mut SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX),
            &mut buffer_size,
        ).is_err() {
            return 1;
        }
        
        let mut numa_count = 0;
        let mut offset = 0;
        while offset < buffer_size as usize {
            let info = &*(buffer.as_ptr().add(offset) as *const SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX);
            
            if info.Relationship == RelationNumaNode {
                numa_count += 1;
            }
            
            offset += info.Size as usize;
        }
        
        numa_count.max(1)
    }
}

fn get_cpus_for_numa_node(numa_node: u32) -> Vec<usize> {
    unsafe {
        use windows::Win32::System::SystemInformation::{
            GetLogicalProcessorInformationEx, RelationNumaNode,
            SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX
        };
        
        let mut cpus = Vec::new();
        let mut buffer_size = 0u32;
        
        let _ = GetLogicalProcessorInformationEx(RelationNumaNode, None, &mut buffer_size);
        if buffer_size == 0 {
            return cpus;
        }
        
        let mut buffer = vec![0u8; buffer_size as usize];
        if GetLogicalProcessorInformationEx(
            RelationNumaNode,
            Some(buffer.as_mut_ptr() as *mut SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX),
            &mut buffer_size,
        ).is_ok() {
            let mut offset = 0;
            while offset < buffer_size as usize {
                let info = &*(buffer.as_ptr().add(offset) as *const SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX);
                
                if info.Relationship == RelationNumaNode {
                    let numa_info = &info.Anonymous.NumaNode;
                    if numa_info.NodeNumber == numa_node {
                        let group_mask = &numa_info.Anonymous.GroupMask;
                        let group = group_mask.Group as usize;
                        
                        // Extract CPU IDs from mask
                        for bit in 0..64 {
                            if (group_mask.Mask & (1u64 << bit) as usize) != 0 {
                                cpus.push(group * 64 + bit);
                            }
                        }
                    }
                }
                
                offset += info.Size as usize;
            }
        }
        
        cpus
    }
}


/// Calculate optimal CPU assignment for a thread
fn calculate_cpu_assignment(
    thread_id: usize, 
    total_threads: usize,
    config: &CpuPinningConfig
) -> usize {
    if !config.enable_pinning {
        return thread_id;
    }
    
    let total_cpus = num_cpus::get();
    let start_cpu = if config.avoid_cpu_0 { 1 } else { 0 };
    
    if config.balance_across_numa {
        // Get NUMA topology
        let numa_nodes = get_numa_node_count();
        if numa_nodes > 1 {
            // Distribute threads round-robin across NUMA nodes
            let cpus_per_node = total_cpus / numa_nodes;
            let node_for_thread = thread_id % numa_nodes;
            let thread_in_node = thread_id / numa_nodes;
            
            start_cpu + (node_for_thread * cpus_per_node) + (thread_in_node % cpus_per_node)
        } else {
            // Single NUMA node - simple distribution
            start_cpu + (thread_id % (total_cpus - start_cpu))
        }
    } else {
        // Simple sequential assignment
        start_cpu + (thread_id % (total_cpus - start_cpu))
    }
}

/// Pin thread to CPU with configuration
pub fn pin_thread_to_cpu_with_config(
    thread_id: usize,
    total_threads: usize,
    config: &CpuPinningConfig
) -> Result<usize, String> {
    if !config.enable_pinning {
        return Ok(thread_id);
    }
    
    let cpu_id = calculate_cpu_assignment(thread_id, total_threads, config);
    
    use windows::Win32::System::Threading::{SetThreadGroupAffinity, GetCurrentThread};
    use windows::Win32::System::SystemInformation::GROUP_AFFINITY;

    unsafe {
        let thread_handle = GetCurrentThread();
        let group = (cpu_id / 64) as u16;
        let mask_bit = cpu_id % 64;
        
        let group_affinity = GROUP_AFFINITY {
            Mask: (1u64 << mask_bit) as usize,  // Cast to usize
            Group: group,
            Reserved: [0; 3],
        };

		let result = SetThreadGroupAffinity(thread_handle, &group_affinity, None);

		if result.as_bool() {
			log::info!("Thread {} → CPU {} (NUMA {})", 
					  thread_id, cpu_id, get_numa_node_for_cpu(cpu_id));
			Ok(cpu_id)
		} else {
			use windows::Win32::Foundation::GetLastError;
			let error = GetLastError();
			Err(format!("Failed to pin thread {} to CPU {}: Error {:?}", thread_id, cpu_id, error))
		}
    }
}

// In runner.rs, add near your other thread affinity functions (around line 1500)
pub fn set_thread_ideal_processor_ex(thread_handle: HANDLE, cpu_id: u32, numa_node: u32) -> Result<(), String> {
    unsafe {
        use windows::Win32::System::Threading::SetThreadIdealProcessorEx;
        use windows::Win32::System::Kernel::PROCESSOR_NUMBER;
        
        let group = (cpu_id / 64) as u16;
        let number = (cpu_id % 64) as u8;
        
        let processor = PROCESSOR_NUMBER {
            Group: group,
            Number: number,
            Reserved: 0,
        };
        
        let mut previous = PROCESSOR_NUMBER::default();
        
        SetThreadIdealProcessorEx(thread_handle, &processor, Some(&mut previous))
            .map_err(|e| format!("Failed to set ideal processor: {:?}", e))?;
        
        Ok(())
    }
}

fn handle_test_errors(stats: &TestStats, error_mode: ErrorMode, test_name: &str) -> Result<(), String> {
    match error_mode {
        ErrorMode::Panic => panic!("Memory error detected in {} (see logs)", test_name),
        ErrorMode::Halt => return Err(format!("Test halted due to {} errors in {}", stats.error_count, test_name)),
        ErrorMode::Log => {
            log::error!(
                "[Thread {}] {} errors detected in {} (continuing)",
                stats.thread_id,
                stats.error_count,
                test_name
            );
        }
    }
    Ok(())
}

fn format_duration(duration: std::time::Duration) -> String {
    let total_seconds = duration.as_secs();
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;
    format!("{:02}:{:02}:{:02}", hours, minutes, seconds)
}