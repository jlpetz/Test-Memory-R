use crate::{ErrorMode, MemoryLayout, ProgressTracker, BlockInfo, TestBuffer};
use crate::layout::{WindowMode, BlockMode};
use crate::{MemoryStrategy, MemoryAllocationConfig};  // Add this line
use crate::tests::{TestStats, TestMemoryConfig, TestTiming, TestAction};
use crate::tests::{
    mirror_move_128_non_temporal, mirror_move_256_non_temporal, mirror_move_512_non_temporal,
    simple_test, refresh_stable, cache_busting_write_test, random_access_torture_test,
    stride_access_test, bandwidth_saturation_test, block_move_test, stuck_bit_test
};
use crate::progress::{progress_reporter, TestSummary};
use crate::results::{TestRunResult, save_test_result};
use crate::dma_memory::{DmaBuffer, DmaBufferEnhanced, DmaConfig, EnhancedTestBuffer, MemoryType, DriverHandle, PageSize};
use crate::config::CpuPinningConfig;
use crate::{MemoryBackend, RuntimeConfig};
use crate::utils::{TableBuilder, Alignment};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::thread;
use std::time::Instant;
use windows::Win32::Foundation::HANDLE;
use crate::cpu_topology::{
        get_cpu_topology,
		get_numa_node_for_cpu,
    };

use std::sync::mpsc::{channel, Sender, Receiver};

// Global flag for graceful shutdown
static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

// Pre-allocated memory block management
pub struct AllocatedBlock {
    pub buffer: EnhancedTestBuffer,
    pub block_info: BlockInfo,
    pub memory_type: MemoryType,
    pub numa_node: u32,
    pub supports_remapping: bool,
}

// Work item for thread pool
enum WorkItem {
    RunTest {
        test_name: String,
        test_func: TestFunction,
        test_config: TestMemoryConfig,
        error_mode: ErrorMode,
        barrier: Arc<Barrier>,
    },
	RemapMemory {
        new_memory_type: MemoryType,
        barrier: Arc<Barrier>,
    },
    Shutdown,
}

// Result from worker thread
struct WorkResult {
    thread_id: usize,
    total_bytes: u64,
    elapsed_ms: u128,
    total_errors: u64,
}

// Thread pool worker context
struct WorkerContext {
    thread_id: usize,
    cpu_id: usize,
    numa_node: u32,
    allocated_blocks: Vec<AllocatedBlock>,
    receiver: Receiver<WorkItem>,
    result_sender: Sender<WorkResult>,
}

type TestStatsTuple = (usize, usize, u64, u128, u64);	 // thread_id, cpu_id, bytes, elapsed, errors

// Thread pool for persistent workers
struct ThreadPool {
    workers: Vec<thread::JoinHandle<Vec<AllocatedBlock>>>,
    senders: Vec<Sender<WorkItem>>,
	cpu_assignments: Vec<(usize, usize, u32)>, // (thread_id, logical_cpu, numa_node)
}

impl ThreadPool {
    fn new(
        mut thread_blocks: HashMap<usize, Vec<AllocatedBlock>>,
        pinning_config: &CpuPinningConfig,
        thread_count: usize,
		cpu_list: Option<&[usize]>,
    ) -> (Self, Receiver<WorkResult>) {
        let (result_sender, result_receiver) = channel();
        let mut senders = Vec::new();
        let mut workers = Vec::new();
		let mut cpu_assignments = Vec::new();
        
        // Get CPU assignments from provided list or calculate
        let cpu_list_final: Vec<usize> = if let Some(list) = cpu_list {
            list.to_vec()
        } else {
            (0..thread_count).collect()
        };
        
        // Create persistent worker threads
        for thread_id in 0..thread_count {
            let (work_sender, work_receiver) = channel();
            senders.push(work_sender.clone());
            
            let cpu_id = cpu_list_final[thread_id];
            let numa_node = get_numa_node_for_cpu(cpu_id);
			cpu_assignments.push((thread_id, cpu_id, numa_node));
            
            // Take ownership of this thread's blocks
            let blocks = thread_blocks.remove(&thread_id).unwrap_or_else(Vec::new);
            
            let result_sender = result_sender.clone();
            let pinning_config = pinning_config.clone();
						
			// Replace the worker thread creation section with this:
			let handle = thread::spawn(move || {
				// Pin thread to CPU once at creation
                if pinning_config.enable_pinning {
                    match pin_thread_to_cpu_with_config(thread_id, &pinning_config) {
                        Ok(actual_cpu) => {
                            log::debug!("Thread {} pinned to CPU {}", thread_id, actual_cpu);
                            let thread_handle = unsafe { windows::Win32::System::Threading::GetCurrentThread() };
                            let _ = set_thread_ideal_processor_ex(thread_handle, actual_cpu as u32);
                        }
                        Err(e) => {
                            log::warn!("Thread {} pinning failed: {}", thread_id, e);
                        }
                    }
                }
				
				// Apply thread priority
				if let Err(e) = PerformanceConfig::default().apply() {
					log::warn!("Failed to set thread priority: {}", e);
				}
				
				let mut context = WorkerContext {
					thread_id,
					cpu_id,
					numa_node,
					allocated_blocks: blocks,
					receiver: work_receiver,
					result_sender,
				};
				
				// Worker loop - process work items until shutdown
				worker_thread_loop(&mut context);
				
				// Return the allocated blocks when thread exits
				context.allocated_blocks
			});

            workers.push(handle);
        }
        
        (ThreadPool { workers, senders, cpu_assignments }, result_receiver)
    }

    fn get_cpu_assignments(&self) -> &[(usize, usize, u32)] {
        &self.cpu_assignments
    }
    
	fn execute_test(
		&self,
		test_name: &str,
		test_func: &TestFunction,
		test_config: &TestMemoryConfig,
		error_mode: ErrorMode,
	) {
		let thread_count = self.senders.len();
		let barrier = Arc::new(Barrier::new(thread_count));
		
		// Send work to all threads
		for sender in &self.senders {
			let work_item = WorkItem::RunTest {
				test_name: test_name.to_string(),
				test_func: test_func.clone(),
				test_config: test_config.clone(),
				error_mode,
				barrier: Arc::clone(&barrier),
			};
			
			if sender.send(work_item).is_err() {
				log::error!("Failed to send work to thread");
			}
		}
	}
    
    // Modified shutdown to return assignments along with blocks
    fn shutdown(self) -> (HashMap<usize, Vec<AllocatedBlock>>, Vec<(usize, usize, u32)>) {
        let cpu_assignments = self.cpu_assignments;
        
        // Send shutdown signal to all workers
        for sender in &self.senders {
            let _ = sender.send(WorkItem::Shutdown);
        }
        
        // Wait for all workers and collect their allocated blocks
        let mut thread_blocks = HashMap::new();
        for (idx, worker) in self.workers.into_iter().enumerate() {
            if let Ok(blocks) = worker.join() {
                thread_blocks.insert(idx, blocks);
            }
        }
        
        (thread_blocks, cpu_assignments)
    }
}

// Worker thread main loop
fn worker_thread_loop(context: &mut WorkerContext) {
    loop {
        match context.receiver.recv() {
            Ok(WorkItem::RunTest { test_name, test_func, test_config, error_mode, barrier }) => {
                // Wait for all threads to be ready
                barrier.wait();
                
                let thread_start = Instant::now();
                let mut total_bytes = 0u64;
                let mut total_errors = 0u64;
                
                // Run test on all blocks
				log::debug!("Thread {} on NUMA node {} processing", context.thread_id, context.numa_node);
                for allocated_block in &context.allocated_blocks {
                    if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
                        break;
                    }
                    
                    match run_test_with_memory_stages(
                        &test_name,
                        &test_func,
                        allocated_block,
                        &test_config,
                        context.thread_id,
                        error_mode,
                    ) {
                        Ok(stats) => {
                            total_bytes += stats.bytes_processed as u64;
                            total_errors += stats.error_count;
                            
                            if stats.error_count > 0 {
                                if let Err(e) = handle_test_errors(&stats, error_mode, &test_name) {
                                    log::error!("[Thread {} on CPU {}] {}", context.thread_id, context.cpu_id, e);
                                    break;
                                }
								else {
									log::debug!("Thread {} on CPU {} finished ok", context.thread_id, context.cpu_id);
								}
                            }
                        }
                        Err(e) => {
                            log::error!("[Thread {} on CPU {}] Test {} failed: {}", 
                                      context.thread_id, context.cpu_id, test_name, e);
                            break;
                        }
                    }
                }
                
                let thread_elapsed = thread_start.elapsed().as_millis();
                
                // Send result back
                let result = WorkResult {
                    thread_id: context.thread_id,
                    total_bytes,
                    elapsed_ms: thread_elapsed,
                    total_errors,
                };
                
                if context.result_sender.send(result).is_err() {
                    log::error!("Failed to send result from thread {}", context.thread_id);
                }
            }
			Ok(WorkItem::RemapMemory { new_memory_type, barrier }) => {
				// Wait for all threads to be ready
				barrier.wait();
				
				// Update memory type for all blocks owned by this thread
				for block in &mut context.allocated_blocks {
					block.memory_type = new_memory_type;
					// Note: The actual memory remapping happens at the driver level. This just updates our local tracking.
				}
				
				log::debug!("Thread {} updated memory type tracking to {:?} for {} blocks", 
						  context.thread_id, new_memory_type, context.allocated_blocks.len());
			}
            Ok(WorkItem::Shutdown) => {
                log::debug!("Thread {} shutting down", context.thread_id);
                break;
            }
            Err(_) => {
                log::error!("Thread {} receiver error, shutting down", context.thread_id);
                break;
            }
        }
    }
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
    pub global_cycles: Option<u32>,
    pub global_duration_secs: Option<u32>,
}

impl Default for TestSuiteTiming {
    fn default() -> Self {
        Self {
            global_cycles: Some(1),
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
        if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
            return false;
        }
        
        if let Some(max_secs) = self.global_duration_secs {
            if elapsed_secs >= max_secs {
                return false;
            }
        }
        
        if let Some(max_cycles) = self.global_cycles {
            if current_cycle >= max_cycles {
                return false;
            }
        }
        
        true
    }
}

// Simple performance config
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

// Test function signatures
type TestFunctionSimple = unsafe fn(*mut u8, usize, usize, ErrorMode, &TestTiming) -> TestStats;
type TestFunctionWithStreams = unsafe fn(*mut u8, usize, usize, ErrorMode, &TestTiming, u32) -> TestStats;
type TestFunctionWithConfig = unsafe fn(*mut u8, usize, usize, ErrorMode, &TestTiming, &TestMemoryConfig) -> TestStats;

// Test function wrapper enum
#[derive(Clone)]
enum TestFunction {
    Simple(TestFunctionSimple),
    WithStreams(TestFunctionWithStreams),
    WithConfig(TestFunctionWithConfig),
}



// In runner.rs
pub fn detect_runtime_capabilities(alloc_config: &MemoryAllocationConfig) -> RuntimeConfig {
    let driver_available = DmaBuffer::is_driver_available_and_compatible();
    let large_pages_available = crate::check_large_page_privilege().is_ok();
    
    // Determine memory backend based on config preferences and availability
    let memory_backend = if let Some(use_driver) = alloc_config.use_driver {
        // User explicitly requested driver on/off
        if use_driver && driver_available {
            MemoryBackend::KernelDriver
        } else if use_driver && !driver_available {
            log::warn!("Driver requested but not available, falling back");
            if large_pages_available {
                MemoryBackend::NativeLargePages
            } else {
                MemoryBackend::NativeRegular
            }
        } else {
            // User explicitly disabled driver
            if large_pages_available {
                MemoryBackend::NativeLargePages
            } else {
                MemoryBackend::NativeRegular
            }
        }
    } else {
        // Auto-detect (current behavior)
        if driver_available {
            MemoryBackend::KernelDriver
        } else if large_pages_available {
            MemoryBackend::NativeLargePages
        } else {
            MemoryBackend::NativeRegular
        }
    };
    
    RuntimeConfig {
        memory_backend,
        driver_available,
        large_pages_available,
        use_driver_chunking: alloc_config.driver_chunking,
        cpu_list: None,
        memory_strategy: MemoryStrategy::default(), // Will be set by caller
        memory_allocation: alloc_config.clone(),    // Store the config
    }
}

pub fn print_current_memory_status() {
    use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    use crate::utils::{TableBuilder, Alignment};
    
    unsafe {
        let mut mem_status = MEMORYSTATUSEX {
            dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
            ..Default::default()
        };
        
        if GlobalMemoryStatusEx(&mut mem_status).is_ok() {
            println!("📊 Current System Memory Status:");
            
            let total_phys_gib = mem_status.ullTotalPhys as f64 / (1024.0 * 1024.0 * 1024.0);
            let avail_phys_gib = mem_status.ullAvailPhys as f64 / (1024.0 * 1024.0 * 1024.0);
            let total_page_gib = mem_status.ullTotalPageFile as f64 / (1024.0 * 1024.0 * 1024.0);
            let avail_page_gib = mem_status.ullAvailPageFile as f64 / (1024.0 * 1024.0 * 1024.0);
            let phys_percent = (mem_status.ullAvailPhys as f64 / mem_status.ullTotalPhys as f64) * 100.0;
            
            let table = TableBuilder::new()
                .add_header("Memory Type", Alignment::Left)
                .add_header("Total", Alignment::Right)
                .add_header("Available", Alignment::Right)
                .add_header("Used", Alignment::Right)
                .add_header("Status", Alignment::Center)
                .add_row(vec![
                    "Physical RAM".to_string(),
                    format!("{:.2} GiB", total_phys_gib),
                    format!("{:.2} GiB", avail_phys_gib),
                    format!("{:.2} GiB", total_phys_gib - avail_phys_gib),
                    format!("{:.1}% free", phys_percent),
                ])
                .add_row(vec![
                    "Page File".to_string(),
                    format!("{:.2} GiB", total_page_gib),
                    format!("{:.2} GiB", avail_page_gib),
                    format!("{:.2} GiB", total_page_gib - avail_page_gib),
                    format!("{:.1}% load", mem_status.dwMemoryLoad as f64),
                ]);
            
            table.print();
        } else {
            println!("📊 Unable to retrieve system memory status");
        }
    }
}

pub fn run_tests_with_layout(layout: MemoryLayout, error_mode: ErrorMode) -> bool {
    let alloc_config = MemoryAllocationConfig::default();
    let runtime_config = detect_runtime_capabilities(&alloc_config);
    run_tests_with_layout_and_timing(layout, error_mode, TestSuiteTiming::default(), runtime_config)
}

pub fn run_tests_with_layout_and_timing(
    layout: MemoryLayout, 
    error_mode: ErrorMode, 
    suite_timing: TestSuiteTiming, 
    runtime_config: RuntimeConfig
) -> bool {
    setup_signal_handler();
    
    layout.print_layout();

    let progress = Arc::new(ProgressTracker::new());
    let success = Arc::new(AtomicBool::new(true));
	let all_test_cpu_stats: Arc<Mutex<HashMap<String, Vec<TestStatsTuple>>>> = Arc::new(Mutex::new(HashMap::new()));

    let mut thread_blocks: HashMap<usize, Vec<BlockInfo>> = HashMap::new();
    for block in layout.blocks {
        thread_blocks.entry(block.thread_id).or_default().push(block);
    }

    // Stage 1: Pre-allocate all memory blocks
    progress.set_phase("Stage 1: Allocating Memory");
    println!("Stage 1: Pre-allocating memory blocks...");

    let allocated_blocks = match allocate_all_blocks(&thread_blocks, &runtime_config) {
        Ok(blocks) => blocks,
        Err(e) => {
            println!("❌ Failed to allocate memory blocks: {}\n", e);
            print_current_memory_status();
            return false;
        }
    };

    print_allocation_summary(&allocated_blocks);

    // Calculate progress tracking information
    let test_definitions = create_test_definitions();
    let tests_per_cycle = test_definitions.len() as u64;
    
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

    // Create thread pool with pre-allocated blocks
    let thread_count = allocated_blocks.len();
    let pinning_config = CpuPinningConfig::default();
    
    println!("\nCreating thread pool with {} persistent workers...", thread_count);
    let (thread_pool, result_receiver) = ThreadPool::new(
        allocated_blocks, 
        &pinning_config, 
        thread_count,
        runtime_config.cpu_list.as_deref()  // Pass the CPU list
    );
	
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
        
        let cycle_summaries = Arc::new(Mutex::new(Vec::new()));
        
        // Run one complete cycle of all tests using the thread pool
        let cycle_success = run_single_test_cycle_with_pool(
            &thread_pool,
            &result_receiver,
            &test_definitions,
            error_mode,
            Arc::clone(&progress),
            Arc::clone(&cycle_summaries),
            current_cycle,
            thread_count,
            &runtime_config,
			Arc::clone(&all_test_cpu_stats),
        );
        
        if !cycle_success || SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
            success.store(false, Ordering::Relaxed);
            
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
        
        if let Ok(summaries) = cycle_summaries.lock() {
            print_cycle_report(current_cycle, cycle_elapsed, &summaries);
            progress.complete_cycle(current_cycle, summaries.clone());
            
            if let Ok(mut result) = test_run_result.lock() {
                result.add_cycle(current_cycle, cycle_elapsed, summaries.clone());
            }
        }
        
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

    // Shutdown thread pool and recover allocated blocks
    let (recovered_blocks, final_cpu_assignments) = thread_pool.shutdown();
    log::info!("Thread pool shut down, recovered {} thread block sets", recovered_blocks.len());
	
    // Print CPU performance analysis
	if let Ok(cpu_stats) = all_test_cpu_stats.lock() {
		if !cpu_stats.is_empty() {
			print_cpu_performance_summary(&cpu_stats, &final_cpu_assignments);
		}
	}

    // Print final summary
    let total_time = suite_start.elapsed();
    if let Ok(result) = test_run_result.lock() {
        print_final_summary(&progress, total_time, &result, &test_definitions);
        
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

// New function to run tests using the thread pool - updated for memory remapping
fn run_single_test_cycle_with_pool(
    thread_pool: &ThreadPool,
    result_receiver: &Receiver<WorkResult>,
    test_definitions: &[(&'static str, TestFunction, TestMemoryConfig)],
    error_mode: ErrorMode,
    progress: Arc<ProgressTracker>,
    cycle_summaries: Arc<Mutex<Vec<TestSummary>>>,
    cycle_number: u32,
    thread_count: usize,
    runtime_config: &RuntimeConfig,
	all_test_cpu_stats: Arc<Mutex<HashMap<String, Vec<TestStatsTuple>>>>,
) -> bool {
    let success = Arc::new(AtomicBool::new(true));
    let supports_remapping = runtime_config.driver_available;
    let mut current_memory_type = MemoryType::WriteBack;

    for (test_idx, (test_name, test_func, test_config)) in test_definitions.iter().enumerate() {
        if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
            return false;
        }
        
        progress.set_phase(&format!("{} ({}/{})", test_name, test_idx + 1, test_definitions.len()));
        
        // Handle memory remapping if needed
		if supports_remapping && test_config.memory_type.is_some() {
			let required_memory_type = test_config.memory_type.unwrap_or(MemoryType::WriteBack);
			
			if current_memory_type != required_memory_type {
				if let Ok(driver) = DriverHandle::open() {
					let remap_start = Instant::now();
					
					// Check which mode to use
					if crate::dma_memory::get_use_remap_all() {
						// Use simplified remap-all approach
						match driver.remap_all_memory_type(required_memory_type) {
							Ok(count) => {
								let remap_elapsed = remap_start.elapsed();
								log::info!("Remapped {} allocations to {:?} in {:?} (remap all mode)", 
										 count, required_memory_type, remap_elapsed);
								current_memory_type = required_memory_type;
								
								// Notify all threads to update their memory type tracking
								let barrier = Arc::new(Barrier::new(thread_count));
								for sender in &thread_pool.senders {
									let _ = sender.send(WorkItem::RemapMemory {
										new_memory_type: required_memory_type,
										barrier: Arc::clone(&barrier),
									});
								}
							}
							Err(e) => {
								log::warn!("Failed to remap memory type: {}", e);
							}
						}
					} else {
						// Use batch remap approach
						// Note: This is more complex with thread pool since we don't have direct access to blocks
						// For now, just use the driver's batch capability
						log::info!("Batch remap mode selected but using driver remap_all for thread pool");
						match driver.remap_all_memory_type(required_memory_type) {
							Ok(count) => {
								let remap_elapsed = remap_start.elapsed();
								log::info!("Remapped {} allocations to {:?} in {:?} (batch mode via driver)", 
										 count, required_memory_type, remap_elapsed);
								current_memory_type = required_memory_type;
								
								// Notify all threads to update their memory type tracking
								let barrier = Arc::new(Barrier::new(thread_count));
								for sender in &thread_pool.senders {
									let _ = sender.send(WorkItem::RemapMemory {
										new_memory_type: required_memory_type,
										barrier: Arc::clone(&barrier),
									});
								}
							}
							Err(e) => {
								log::warn!("Failed to remap memory type: {}", e);
							}
						}
					}
				}
			}
		}
        
        let test_start = Instant::now();
        
        // Execute test on all threads
        thread_pool.execute_test(test_name, test_func, test_config, error_mode);
        
		// Collect results from all threads
		let mut test_stats = Vec::new();
		let mut total_bytes_for_test = 0u64;
		let mut total_errors_for_test = 0u64;

		for _ in 0..thread_count {
			match result_receiver.recv() {
				Ok(result) => {
					// Look up CPU assignment for this thread
					if let Some(&(_, cpu_id, _)) = thread_pool.get_cpu_assignments()
						.iter()
						.find(|(tid, _, _)| *tid == result.thread_id) 
					{
						// Store simplified stats: (thread_id, cpu_id, bytes, elapsed, errors)
						test_stats.push((result.thread_id, cpu_id, 
									   result.total_bytes, result.elapsed_ms, result.total_errors));
					}
					total_bytes_for_test += result.total_bytes;
					total_errors_for_test += result.total_errors;
					
					if result.total_errors > 0 {
						success.store(false, Ordering::Relaxed);
					}
				}
				Err(e) => {
					log::error!("Failed to receive result: {:?}", e);
					success.store(false, Ordering::Relaxed);
				}
			}
		}
				
        // Process test statistics
        let test_duration = test_start.elapsed();
        let throughput_mib_s = if test_duration.as_millis() > 0 {
            (total_bytes_for_test as f64 / (1024.0 * 1024.0)) / test_duration.as_secs_f64()
        } else {
            0.0
        };
		
		// ADD THIS: Update progress tracker with the combined test results
		let test_duration = test_start.elapsed();
		let combined_stats = TestStats {
			name: test_name,
			action: TestAction::ReadWrite,  // You might need to determine this properly
			bytes_processed: total_bytes_for_test as usize,
			elapsed_ms: test_duration.as_millis(),
			thread_id: 0,  // Combined result, not thread-specific
			error_count: total_errors_for_test,
		};
		progress.complete_test(&combined_stats);
        
        let test_summary = TestSummary {
            name: test_name.to_string(),
            duration_ms: test_duration.as_millis(),
            bytes_processed: total_bytes_for_test,
            throughput_mib_s,
            errors: total_errors_for_test,
        };
        
        if let Ok(mut summaries) = cycle_summaries.lock() {
            summaries.push(test_summary);
        }
        
        log::info!("Cycle {} - {} completed: {:.2} GiB in {:.1}s @ {:.1} MiB/s{}",
                  cycle_number,
                  test_name,
                  total_bytes_for_test as f64 / (1024.0 * 1024.0 * 1024.0),
                  test_duration.as_secs_f64(),
                  throughput_mib_s,
                  if total_errors_for_test > 0 {
                      format!(" [⚠️ {} ERRORS]", total_errors_for_test)
                  } else {
                      String::new()
                  }
        );

        if let Ok(mut all_stats) = all_test_cpu_stats.lock() {
            all_stats.insert(test_name.to_string(), test_stats.clone());
        }
        
        // Analyze thread timing deviation
        analyze_thread_timing(&test_stats, test_name);
    }
    
    success.load(Ordering::Relaxed)
}


// Update analyze_thread_timing to use the cached topology
fn analyze_thread_timing(stats: &[TestStatsTuple], test_name: &str) {
    if stats.is_empty() {
        return;
    }
    
    let topology = get_cpu_topology();  // Use the wrapper
    let cpu_to_physical: HashMap<usize, usize> = topology.iter()
        .map(|info| (info.logical_id, info.physical_core_id))
        .collect();
     
    let total_elapsed: u128 = stats.iter().map(|(_, _, _, elapsed, _)| elapsed).sum();
    let avg_elapsed = total_elapsed / stats.len() as u128;
    
    let mut deviations: Vec<(usize, usize, u64, u128, u64, i128)> = Vec::new();
    
    for &(thread_id, cpu_id, bytes, elapsed, errors) in stats.iter() {
        let deviation = elapsed as i128 - avg_elapsed as i128;
        deviations.push((thread_id, cpu_id, bytes, elapsed, errors, deviation));
    }
    
    // Always show timing deviation report
    log::info!("Thread timing deviation for {} - Avg: {:.1}s", 
              test_name, avg_elapsed as f64 / 1000.0);
    
    let mut table = TableBuilder::new()
        .add_header("Thread", Alignment::Right)
        .add_header("Logical CPU", Alignment::Right)
        .add_header("Physical Core", Alignment::Right)
        .add_header("NUMA Node", Alignment::Center)
        .add_header("Runtime", Alignment::Right)
        .add_header("Deviation", Alignment::Right)
        .add_header("Data", Alignment::Right)
        .add_header("Throughput", Alignment::Right)
        .add_header("Errors", Alignment::Right);
    
    // Sort by deviation descending (highest deviation first)
    deviations.sort_by(|a, b| b.5.cmp(&a.5));
    
    for (thread_id, cpu_id, bytes, elapsed, errors, deviation) in deviations {
        let physical_core = cpu_to_physical.get(&cpu_id).unwrap_or(&0);
        let numa_node = get_numa_node_for_cpu(cpu_id);
        
        // Calculate throughput
        let throughput_mib_s = if elapsed > 0 {
            (bytes as f64 / (1024.0 * 1024.0)) / (elapsed as f64 / 1000.0)
        } else {
            0.0
        };
        
        // Format data size
        let data_str = if bytes >= 1024 * 1024 * 1024 {
            format!("{:.2} GiB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
        } else {
            format!("{:.2} MiB", bytes as f64 / (1024.0 * 1024.0))
        };
        
        table = table.add_row(vec![
            format!("{}", thread_id),
            format!("{}", cpu_id),
            format!("{}", physical_core),
            format!("{}", numa_node),
            format_duration_ms(elapsed),
            format!("{:+}ms", deviation),
            data_str,
            format!("{:.1} MiB/s", throughput_mib_s),
            format!("{}", errors),
        ]);
    }
    
    table.print();
}

// Create test definitions
fn create_test_definitions() -> Vec<(&'static str, TestFunction, TestMemoryConfig)> {
    vec![
        // === CRITICAL: Full Memory Stuck Bit Test ===
        (
            "StuckBitTest", 
            TestFunction::Simple(|ptr, size, tid, em, timing| unsafe { stuck_bit_test(ptr, size, tid, em, timing) }),
            TestMemoryConfig::new(
                WindowMode::FullAllocation,
                BlockMode::WindowFraction { fraction: 0.0625 },
                false,
                false
            ).with_timing(TestTiming::cycles_only(1))
             .with_streams(1)
             .with_memory_type(None)
        ),
        
        // === SIMD Tests with optimal window/block sizing ===
        (
            "MirrorMove128NonTemporal", 
            TestFunction::WithStreams(|ptr, size, tid, em, timing, streams| unsafe { 
                mirror_move_128_non_temporal(ptr, size, tid, em, timing, streams) 
            }),
            TestMemoryConfig::new(
                WindowMode::FixedSize { size_mb: 64 },
                BlockMode::FixedSize { size_mb: 16 },
                false,
                true
            ).with_timing(TestTiming::duration_only(10))
             .with_streams(1)
             .with_memory_type(None)
        ),
        (
            "MirrorMove256NonTemporal", 
            TestFunction::WithStreams(|ptr, size, tid, em, timing, streams| unsafe { 
                mirror_move_256_non_temporal(ptr, size, tid, em, timing, streams) 
            }),
            TestMemoryConfig::new(
                WindowMode::FixedSize { size_mb: 128 },
                BlockMode::FixedSize { size_mb: 32 },
                false,
                true
            ).with_timing(TestTiming::duration_only(10))
             .with_streams(2)
             .with_memory_type(None)
        ),
        (
            "MirrorMove512NonTemporal", 
            TestFunction::WithStreams(|ptr, size, tid, em, timing, streams| unsafe { 
                mirror_move_512_non_temporal(ptr, size, tid, em, timing, streams) 
            }),
            TestMemoryConfig::new(
                WindowMode::FixedSize { size_mb: 256 },
                BlockMode::FixedSize { size_mb: 64 },
                false,
                true
            ).with_timing(TestTiming::duration_only(10))
             .with_streams(4)
             .with_memory_type(None)
        ),
        
        // === Memory Pattern Tests ===
        (
            "SimpleTest", 
            TestFunction::WithConfig(|ptr, size, tid, em, timing, config| unsafe { 
                simple_test(ptr, size, tid, em, timing, config) 
            }),
            TestMemoryConfig::new(
                WindowMode::FullAllocation,
                BlockMode::FixedSize { size_mb: 4 },
                false,
                false
            ).with_timing(TestTiming::hybrid(100, 30))
             .with_streams(1)
             .with_memory_type(None)
        ),
        
        // === Refresh/Retention Tests ===
        (
            "RefreshStable", 
            TestFunction::Simple(|ptr, size, tid, em, timing| unsafe { refresh_stable(ptr, size, tid, em, timing) }),
            TestMemoryConfig::new(
                WindowMode::CacheRelative { multiplier: 2.0 },
                BlockMode::FixedSize { size_mb: 1 },
                false,
                true
            ).with_timing(TestTiming::duration_only(15))
             .with_streams(1)
             .with_memory_type(None)
        ),
        
        // === Cache Tests ===
        (
            "CacheBusting", 
            TestFunction::WithStreams(|ptr, size, tid, em, timing, streams| unsafe { 
                cache_busting_write_test(ptr, size, tid, em, timing, streams) 
            }),
            TestMemoryConfig::new(
                WindowMode::CacheRelative { multiplier: 0.5 },
                BlockMode::FixedSize { size_mb: 1 },
                false,
                true
            ).with_timing(TestTiming::duration_only(20))
             .with_streams(4)
             .with_memory_type(Some(MemoryType::Uncached))
        ),
        
        // === Stress Tests ===
        (
            "RandomTorture", 
            TestFunction::WithStreams(|ptr, size, tid, em, timing, streams| unsafe { 
                random_access_torture_test(ptr, size, tid, em, timing, streams) 
            }),
            TestMemoryConfig::new(
                WindowMode::FullAllocation,
                BlockMode::FixedSize { size_mb: 8 },
                true,
                false
            ).with_timing(TestTiming::duration_only(25))
             .with_streams(8)
             .with_memory_type(None)
        ),
        (
            "StrideAccess", 
            TestFunction::WithStreams(|ptr, size, tid, em, timing, streams| unsafe { 
                stride_access_test(ptr, size, tid, em, timing, streams) 
            }),
            TestMemoryConfig::new(
                WindowMode::FullAllocation,
                BlockMode::FixedSize { size_mb: 2 },
                false,
                false
            ).with_timing(TestTiming::cycles_only(1))
             .with_streams(4)
             .with_memory_type(None)
        ),
        
        // === Bandwidth Test ===
        (
            "BandwidthSat", 
            TestFunction::WithStreams(|ptr, size, tid, em, timing, streams| unsafe { 
                bandwidth_saturation_test(ptr, size, tid, em, timing, streams) 
            }),
            TestMemoryConfig::new(
                WindowMode::FullAllocation,
                BlockMode::FixedSize { size_mb: 32 },
                false,
                false
            ).with_timing(TestTiming::duration_only(15))
             .with_streams(1)
             .with_memory_type(Some(MemoryType::WriteCombining))
        ),
        
        // === Block Move Test ===
        (
            "BlockMove", 
            TestFunction::WithStreams(|ptr, size, tid, em, timing, streams| unsafe { 
                block_move_test(ptr, size, tid, em, timing, streams) 
            }),
            TestMemoryConfig::new(
                WindowMode::FullAllocation,
                BlockMode::FixedSize { size_mb: 16 },
                false,
                false
            ).with_timing(TestTiming::duration_only(20))
             .with_streams(1)
             .with_memory_type(None)
        ),
    ]
}

// Keep all the other functions unchanged...
// (All the allocation, NUMA, helper functions remain the same)

// Thread allocation summary structure
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
    
    let mut total_allocated = 0usize;
    let mut huge_page_segments = 0usize;
    let mut large_page_segments = 0usize;
    let mut regular_page_segments = 0usize;
    let mut threads_allocated = 0usize;
    let mut thread_summaries = Vec::new();
    
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
        
        let thread_huge_pages = huge_pages_per_thread + 
                               if thread_idx < extra_huge_pages { 1 } else { 0 };
        
        log::info!("Thread {} allocated {} huge pages quota", thread_id, thread_huge_pages);
        
        for (block_idx, block) in blocks.iter().enumerate() {
            match runtime_config.memory_backend {
                MemoryBackend::KernelDriver => {
                    if runtime_config.use_driver_chunking {
                        let config = DmaConfig {
							minimum_page_size: MemoryAllocationConfig::parse_page_size(&runtime_config.memory_allocation.min_page_size)?,
							maximum_page_size: MemoryAllocationConfig::parse_page_size(&runtime_config.memory_allocation.max_page_size)?,
							prefer_numa_node: Some(get_numa_node_for_cpu(*thread_id)),
							zero_memory: runtime_config.memory_allocation.zero_memory,
							memory_type: MemoryAllocationConfig::parse_memory_type(&runtime_config.memory_allocation.default_memory_type)?,
							contiguous: runtime_config.memory_allocation.require_contiguous,
							timeout_ms: runtime_config.memory_allocation.allocation_timeout_ms,
							retry_interval_ms: runtime_config.memory_allocation.retry_interval_ms,
							max_retries: runtime_config.memory_allocation.max_retries,
							strict_numa: runtime_config.memory_allocation.strict_numa,
                        };
                        
                        match DmaBufferEnhanced::new_with_config(block.size_bytes, config) {
                            Ok(dma) => {
                                log::info!("Thread {}: Driver allocated {} bytes with {} segments",
                                         thread_id, block.size_bytes, dma.segments().len());
                                
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
									"Driver-chunked allocation failed for thread {} (size: {:.2} GiB): {}\n  Progress: {} threads succeeded, 1 failed, {} not attempted",
									thread_id,
									block.size_bytes as f64 / (1024.0 * 1024.0 * 1024.0),
									e,
									threads_allocated,
									thread_count - threads_allocated - 1
								));
							}
                        }
                    } else {
                        let allocations = allocate_block_chunked(
                            block.size_bytes,
                            thread_id,
                            thread_huge_pages,
                            runtime_config
                        )?;
                        
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
								"Large page allocation failed for thread {} (block {}/{}, size: {:.2} GiB)\n  Progress: {} threads succeeded, 1 failed, {} not attempted",
								thread_id, 
								block_idx + 1, 
								blocks.len(),
								block.size_bytes as f64 / (1024.0 * 1024.0 * 1024.0),
								threads_allocated,
								thread_count - threads_allocated - 1
							));
                        }
                    }
                }
                MemoryBackend::NativeRegular => {
                    match TestBuffer::new_aligned(block.size_bytes) {
                        Some(buf) => {
                            let size = buf.size();
                            thread_stats.total_size += size;
                            
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
								"Regular allocation failed for thread {} (block {}/{}, size: {:.2} GiB)\n  Progress: {} threads succeeded, 1 failed, {} not attempted",
								thread_id, 
								block_idx + 1, 
								blocks.len(),
								block.size_bytes as f64 / (1024.0 * 1024.0 * 1024.0),
								threads_allocated,
								thread_count - threads_allocated - 1
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
		log::info!("Thread {} completed allocation successfully ({}/{})", 
				  thread_id, threads_allocated, thread_count);
    }
    
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
    runtime_config: &RuntimeConfig,
) -> Result<Vec<AllocatedBlock>, String> {
    let mut allocations = Vec::new();
    let mut remaining_size = total_size;
    let numa_node = get_numa_node_for_cpu(*thread_id);
    
    if huge_page_quota > 0 && remaining_size >= 1024 * 1024 * 1024 {
        let huge_pages_needed = remaining_size / (1024 * 1024 * 1024);
        let huge_pages_to_allocate = huge_pages_needed.min(huge_page_quota);
        let huge_bytes = huge_pages_to_allocate * 1024 * 1024 * 1024;
        
        if huge_bytes > 0 {
            log::debug!("Thread {}: Requesting {} GiB in huge pages", thread_id, huge_bytes / (1024*1024*1024));
            
            let config = DmaConfig {
				minimum_page_size: PageSize::Huge,
				maximum_page_size: PageSize::Huge,
				prefer_numa_node: Some(numa_node),
				zero_memory: runtime_config.memory_allocation.zero_memory,
				memory_type: MemoryAllocationConfig::parse_memory_type(&runtime_config.memory_allocation.default_memory_type)?,
				contiguous: runtime_config.memory_allocation.require_contiguous,
				timeout_ms: runtime_config.memory_allocation.allocation_timeout_ms,
				retry_interval_ms: runtime_config.memory_allocation.retry_interval_ms,
				max_retries: runtime_config.memory_allocation.max_retries,
				strict_numa: runtime_config.memory_allocation.strict_numa,
            };
            
            match DmaBufferEnhanced::new_with_config(huge_bytes, config) {
                Ok(dma) => {
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
                }
            }
        }
    }
    
    if remaining_size >= 2 * 1024 * 1024 {
        let large_pages_needed = remaining_size / (2 * 1024 * 1024);
        let large_bytes = large_pages_needed * 2 * 1024 * 1024;
        
        if large_bytes > 0 {
            log::debug!("Thread {}: Requesting {} MiB in large pages", thread_id, large_bytes / (1024*1024));
            
            let config = DmaConfig {
                minimum_page_size: PageSize::Large,
                maximum_page_size: PageSize::Large,
				prefer_numa_node: Some(numa_node),
				zero_memory: runtime_config.memory_allocation.zero_memory,
				memory_type: MemoryAllocationConfig::parse_memory_type(&runtime_config.memory_allocation.default_memory_type)?,
				contiguous: runtime_config.memory_allocation.require_contiguous,
				timeout_ms: runtime_config.memory_allocation.allocation_timeout_ms,
				retry_interval_ms: runtime_config.memory_allocation.retry_interval_ms,
				max_retries: runtime_config.memory_allocation.max_retries,
				strict_numa: runtime_config.memory_allocation.strict_numa,
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
                }
            }
        }
    }
    
    if remaining_size > 0 {
        log::debug!("Thread {}: Requesting {} bytes in regular pages", thread_id, remaining_size);
        
        let config = DmaConfig {
            minimum_page_size: PageSize::Regular,
            maximum_page_size: PageSize::Regular,
			prefer_numa_node: Some(numa_node),
			zero_memory: runtime_config.memory_allocation.zero_memory,
			memory_type: MemoryAllocationConfig::parse_memory_type(&runtime_config.memory_allocation.default_memory_type)?,
			contiguous: runtime_config.memory_allocation.require_contiguous,
			timeout_ms: runtime_config.memory_allocation.allocation_timeout_ms,
			retry_interval_ms: runtime_config.memory_allocation.retry_interval_ms,
			max_retries: runtime_config.memory_allocation.max_retries,
			strict_numa: runtime_config.memory_allocation.strict_numa,
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
    
    print!("  Suite Timing: ");
    match (suite_timing.global_cycles, suite_timing.global_duration_secs) {
        (Some(cycles), Some(duration)) => println!("{} cycles or {}s max", cycles, duration),
        (Some(cycles), None) => println!("{} cycles", cycles),
        (None, Some(duration)) => println!("{}s duration", duration),
        (None, None) => println!("Unlimited"),
    }
    
    println!("  Test Sequence: {} tests", test_definitions.len());
    
    for (i, (name, _, config)) in test_definitions.iter().enumerate() {
        print!("    {}. {} - ", i + 1, name);
        
        match (&config.timing.cycles, &config.timing.duration_secs) {
            (Some(c), Some(d)) => print!("{}cycles/{}s, ", c, d),
            (Some(c), None) => print!("{}cycles, ", c),
            (None, Some(d)) => print!("{}s, ", d),
            (None, None) => print!("unlimited, "),
        }
        
        print!("{}streams", config.streams);
        
        match &config.window_mode {
            WindowMode::FullAllocation => print!(", FullWindow"),
            WindowMode::FixedSize { size_mb } => print!(", Window:{}MB", size_mb),
            WindowMode::CacheRelative { multiplier } => print!(", Window:{}xCache", multiplier),
        }
        
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
    
    let overall_throughput_mib = if total_time.as_secs() > 0 {
        (total_bytes as f64 / (1024.0 * 1024.0)) / total_time.as_secs() as f64
    } else {
        0.0
    };
    let overall_throughput_gib = overall_throughput_mib / 1024.0;
    
    println!("Overall throughput: {:.1} MiB/s ({:.2} GiB/s)", overall_throughput_mib, overall_throughput_gib);
    println!("Total errors detected: {}", total_errors);
    
    if !cycle_stats.is_empty() {
        println!("\nPer-Test Performance Summary (averaged across {} cycles):", cycle_stats.len());
        
        let mut test_aggregates: HashMap<String, (u64, u128, u64)> = HashMap::new();
        
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


fn get_numa_node_count() -> usize {
    unsafe {
        use windows::Win32::System::SystemInformation::{
            GetLogicalProcessorInformationEx, RelationNumaNode, 
            SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX
        };
        
        let mut buffer_size = 0u32;
        let _ = GetLogicalProcessorInformationEx(RelationNumaNode, None, &mut buffer_size);
        
        if buffer_size == 0 {
            return 1;
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

fn calculate_cpu_assignment(
    thread_id: usize, 
    config: &CpuPinningConfig
) -> usize {
    if !config.enable_pinning {
        return thread_id;
    }
    
    let total_cpus = num_cpus::get();
    let start_cpu = config.cpus_to_skip;  // Use new field
    let available_cpus = total_cpus.saturating_sub(start_cpu);
    
    if available_cpus == 0 {
        log::warn!("No CPUs available after skipping {}, using CPU 0", start_cpu);
        return 0;
    }
    
    if config.balance_across_numa {
        let numa_nodes = get_numa_node_count();
        if numa_nodes > 1 {
            // Distribute threads across NUMA nodes
            let cpus_per_node = available_cpus / numa_nodes;
            let node_for_thread = thread_id % numa_nodes;
            let thread_in_node = thread_id / numa_nodes;
            
            start_cpu + (node_for_thread * cpus_per_node) + (thread_in_node % cpus_per_node)
        } else {
            start_cpu + (thread_id % available_cpus)
        }
    } else {
        start_cpu + (thread_id % available_cpus)
    }
}

pub fn pin_thread_to_cpu_with_config(
    thread_id: usize,
    config: &CpuPinningConfig
) -> Result<usize, String> {
    if !config.enable_pinning {
        return Ok(thread_id);
    }
    
    let cpu_id = calculate_cpu_assignment(thread_id, config);
    
    use windows::Win32::System::Threading::{SetThreadGroupAffinity, GetCurrentThread};
    use windows::Win32::System::SystemInformation::GROUP_AFFINITY;

    unsafe {
        let thread_handle = GetCurrentThread();
        let group = (cpu_id / 64) as u16;
        let mask_bit = cpu_id % 64;
        
        let group_affinity = GROUP_AFFINITY {
            Mask: (1u64 << mask_bit) as usize,
            Group: group,
            Reserved: [0; 3],
        };

        let result = SetThreadGroupAffinity(thread_handle, &group_affinity, None);

        if result.as_bool() {
            Ok(cpu_id)
        } else {
            use windows::Win32::Foundation::GetLastError;
            let error = GetLastError();
            Err(format!("Failed to pin thread {} to CPU {}: Error {:?}", thread_id, cpu_id, error))
        }
    }
}

pub fn set_thread_ideal_processor_ex(thread_handle: HANDLE, cpu_id: u32) -> Result<(), String> {
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
    
    let window_size = test_config.calculate_window_size(test_name, allocated_size);
    let block_size = test_config.calculate_block_size(test_name, window_size);
    let final_window_size= test_config.align_window_to_blocks(window_size, block_size);
    
    let allocated_mb = allocated_size as f64 / (1024.0 * 1024.0);
    let window_mb = final_window_size as f64 / (1024.0 * 1024.0);
    let block_mb = block_size as f64 / (1024.0 * 1024.0);
    let window_percent = (final_window_size as f64 / allocated_size as f64) * 100.0;
    
    if thread_id == 0 {
        log::info!(
            "{} - Window: {:.1}MB ({:.1}%), Block: {:.1}MB, Streams: {}{}",
            test_name, window_mb, window_percent, block_mb, test_config.streams,
            if test_config.allow_misaligned { ", misaligned" } else { "" }
        );
    }
    
    log::debug!(
        "[Thread {}] {} - Configuration: Window {:.1}MB of {:.1}MB ({:.1}%), Block {:.1}MB, Streams: {}{}",
        thread_id, test_name, window_mb, allocated_mb, window_percent, block_mb, test_config.streams,
        if test_config.allow_misaligned { ", misaligned" } else { "" }
    );
    
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

pub fn print_cpu_performance_summary(
    all_test_stats: &HashMap<String, Vec<TestStatsTuple>>,
    cpu_assignments: &[(usize, usize, u32)] // (thread_id, logical_cpu, numa_node)
) {
    println!("\n=== CPU Performance Summary ===");
    
    // Get topology info for logical -> physical mapping
    let topology = get_cpu_topology();
    let cpu_to_physical: HashMap<usize, usize> = topology.iter()
        .map(|info| (info.logical_id, info.physical_core_id))
        .collect();
    
    // Build a map of thread -> logical CPU for easy lookup
    let thread_to_cpu: HashMap<usize, (usize, u32)> = cpu_assignments.iter()
        .map(|&(thread_id, cpu_id, numa_node)| (thread_id, (cpu_id, numa_node)))
        .collect();
    
    // Section 1: By Thread
    println!("\nPerformance by Thread:");
    let mut table = TableBuilder::new()
        .add_header("Thread", Alignment::Center)
        .add_header("Logical CPU", Alignment::Center)
        .add_header("Physical Core", Alignment::Center)
        .add_header("NUMA", Alignment::Center)
        .add_header("Total Time", Alignment::Right)
        .add_header("Throughput", Alignment::Right)
        .add_header("Errors", Alignment::Right);
    
    // Aggregate stats by thread across all tests
    let mut thread_totals: HashMap<usize, (u64, u128, u64)> = HashMap::new();
    
    for test_stats in all_test_stats.values() {
        for &(thread_id, _, bytes, elapsed, errors) in test_stats {
            let entry = thread_totals.entry(thread_id).or_insert((0, 0, 0));
            entry.0 += bytes;
            entry.1 += elapsed;
            entry.2 += errors;
        }
    }
    
    // Sort by thread ID and display
    let mut thread_ids: Vec<_> = thread_totals.keys().cloned().collect();
    thread_ids.sort();
    
    for thread_id in thread_ids {
        let (bytes, elapsed_ms, errors) = thread_totals[&thread_id];
        
        if let Some(&(cpu_id, numa_node)) = thread_to_cpu.get(&thread_id) {
            let physical_core = cpu_to_physical.get(&cpu_id).unwrap_or(&0);
            let throughput_mib_s = if elapsed_ms > 0 {
                (bytes as f64 / (1024.0 * 1024.0)) / (elapsed_ms as f64 / 1000.0)
            } else {
                0.0
            };
            
            table = table.add_row(vec![
                format!("{}", thread_id),
                format!("{}", cpu_id),
                format!("{}", physical_core),
                format!("{}", numa_node),
                format_duration_ms(elapsed_ms),
                format!("{:.1} MiB/s", throughput_mib_s),
                format!("{}", errors),
            ]);
        }
    }
    
    table.print();
    
    // Section 2: By Logical CPU
    println!("\nPerformance by Logical CPU:");
    let mut cpu_stats_map: HashMap<usize, CpuPerformanceStats> = HashMap::new();
    
    for test_stats in all_test_stats.values() {
        for &(_, cpu_id, bytes, elapsed, errors) in test_stats {
            let entry = cpu_stats_map.entry(cpu_id).or_insert(CpuPerformanceStats {
                thread_count: 0,
                total_elapsed_ms: 0,
                total_bytes: 0,
                total_errors: 0,
                numa_node: get_numa_node_for_cpu(cpu_id),
            });
            
            entry.thread_count = 1; // In your design, one thread per CPU
            entry.total_elapsed_ms += elapsed;
            entry.total_bytes += bytes;
            entry.total_errors += errors;
        }
    }
    
    let mut cpu_table = TableBuilder::new()
        .add_header("Logical CPU", Alignment::Center)
        .add_header("Physical Core", Alignment::Center)
        .add_header("NUMA", Alignment::Center)
        .add_header("Total Time", Alignment::Right)
        .add_header("Throughput", Alignment::Right)
        .add_header("Errors", Alignment::Right);
    
    let mut cpu_ids: Vec<_> = cpu_stats_map.keys().cloned().collect();
    cpu_ids.sort();
    
    for cpu_id in cpu_ids {
        let stats = &cpu_stats_map[&cpu_id];
        let physical_core = cpu_to_physical.get(&cpu_id).unwrap_or(&0);
        let throughput_mib_s = if stats.total_elapsed_ms > 0 {
            (stats.total_bytes as f64 / (1024.0 * 1024.0)) / (stats.total_elapsed_ms as f64 / 1000.0)
        } else {
            0.0
        };
        
        cpu_table = cpu_table.add_row(vec![
            format!("{}", cpu_id),
            format!("{}", physical_core),
            format!("{}", stats.numa_node),
            format_duration_ms(stats.total_elapsed_ms),
            format!("{:.1} MiB/s", throughput_mib_s),
            format!("{}", stats.total_errors),
        ]);
    }
    
    cpu_table.print();
    
    // Section 3: By Physical Core
    println!("\nPerformance by Physical Core:");
    let mut core_stats_map: HashMap<usize, PhysicalCoreStats> = HashMap::new();
    
    for test_stats in all_test_stats.values() {
        for &(_, cpu_id, bytes, elapsed, errors) in test_stats {
            if let Some(&physical_core) = cpu_to_physical.get(&cpu_id) {
                let entry = core_stats_map.entry(physical_core).or_insert(PhysicalCoreStats {
                    logical_cpus: Vec::new(),
                    total_elapsed_ms: 0,
                    total_bytes: 0,
                    total_errors: 0,
                });
                
                if !entry.logical_cpus.contains(&cpu_id) {
                    entry.logical_cpus.push(cpu_id);
                }
                entry.total_elapsed_ms += elapsed;
                entry.total_bytes += bytes;
                entry.total_errors += errors;
            }
        }
    }
    
    let mut core_table = TableBuilder::new()
        .add_header("Physical Core", Alignment::Center)
        .add_header("Logical CPUs", Alignment::Center)
        .add_header("Total Time", Alignment::Right)
        .add_header("Throughput", Alignment::Right)
        .add_header("Errors", Alignment::Right);
    
    let mut core_ids: Vec<_> = core_stats_map.keys().cloned().collect();
    core_ids.sort();
    
    for core_id in core_ids {
        let stats = &core_stats_map[&core_id];
        let throughput_mib_s = if stats.total_elapsed_ms > 0 {
            (stats.total_bytes as f64 / (1024.0 * 1024.0)) / (stats.total_elapsed_ms as f64 / 1000.0)
        } else {
            0.0
        };
        
        let cpu_list = stats.logical_cpus.iter()
            .map(|cpu| cpu.to_string())
            .collect::<Vec<_>>()
            .join(",");
        
        core_table = core_table.add_row(vec![
            format!("{}", core_id),
            cpu_list,
            format_duration_ms(stats.total_elapsed_ms),
            format!("{:.1} MiB/s", throughput_mib_s),
            format!("{}", stats.total_errors),
        ]);
    }
    
    core_table.print();
    
    // Call the variance analysis
    print_cpu_variance_analysis(&cpu_stats_map);
}

// Helper structures
#[derive(Debug)]
struct CpuPerformanceStats {
    thread_count: usize,
    total_elapsed_ms: u128,
    total_bytes: u64,
    total_errors: u64,
    numa_node: u32,
}

#[derive(Debug)]
struct PhysicalCoreStats {
    logical_cpus: Vec<usize>,
    total_elapsed_ms: u128,
    total_bytes: u64,
    total_errors: u64,
}

fn print_cpu_variance_analysis(cpu_stats: &HashMap<usize, CpuPerformanceStats>) {
    println!("\nCPU Performance Variance:");
    
    // Calculate average performance per CPU
    let total_elapsed: u128 = cpu_stats.values().map(|s| s.total_elapsed_ms).sum();
    let total_bytes: u64 = cpu_stats.values().map(|s| s.total_bytes).sum();
    let avg_throughput = if total_elapsed > 0 {
        (total_bytes as f64 / (1024.0 * 1024.0)) / (total_elapsed as f64 / 1000.0)
    } else {
        0.0
    };
    
    let mut variances: Vec<(usize, f64, f64)> = Vec::new(); // (cpu_id, throughput, variance_pct)
    
    for (cpu_id, stats) in cpu_stats {
        let cpu_throughput = if stats.total_elapsed_ms > 0 {
            (stats.total_bytes as f64 / (1024.0 * 1024.0)) / (stats.total_elapsed_ms as f64 / 1000.0)
        } else {
            0.0
        };
        
        let variance_pct = ((cpu_throughput - avg_throughput) / avg_throughput) * 100.0;
        variances.push((*cpu_id, cpu_throughput, variance_pct));
    }
    
    // Sort by variance
    variances.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
    
    println!("  Average throughput: {:.1} MiB/s", avg_throughput);
    println!("  Best performing CPUs:");
    for (cpu_id, throughput, variance) in variances.iter().take(3) {
        if *variance > 0.0 {
            println!("    CPU {}: {:.1} MiB/s (+{:.1}%)", cpu_id, throughput, variance);
        }
    }
    
    println!("  Worst performing CPUs:");
    for (cpu_id, throughput, variance) in variances.iter().rev().take(3) {
        if *variance < 0.0 {
            println!("    CPU {}: {:.1} MiB/s ({:.1}%)", cpu_id, throughput, variance);
        }
    }
}

fn format_duration_ms(ms: u128) -> String {
    if ms >= 60000 {
        let minutes = ms / 60000;
        let seconds = (ms % 60000) / 1000;
        format!("{}m {:02}s", minutes, seconds)
    } else if ms >= 1000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{}ms", ms)
    }
}
