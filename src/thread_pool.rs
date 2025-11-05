use crate::{ErrorMode, AllocationBlock};
use crate::tests::TestMemoryConfig;
use crate::runner::TestFunction;
use crate::config::CpuPinningConfig;
use crate::driver::MemoryType;
use crate::cpu_topology::get_numa_node_for_cpu;
use std::collections::HashMap;
use std::sync::{Arc, Barrier};
use std::sync::mpsc::{channel, Sender, Receiver};
use std::thread;
use std::time::Instant;


#[derive(Debug)]
pub enum WorkItem {
    RunTest {
        test_name: &'static str,  // Keep as static throughout
        test_func: TestFunction,
        test_config: TestMemoryConfig,
        error_mode: ErrorMode,
        barrier: Arc<Barrier>,
    },
    ChangeMemoryType {
        new_memory_type: MemoryType,
        barrier: Arc<Barrier>,
    },
    UpdateAllocations {
        new_blocks: Vec<AllocationBlock>,
        barrier: Arc<Barrier>,
    },
    Shutdown,
}

// Result from worker thread
#[derive(Debug)]
pub struct WorkResult {
    pub thread_id: usize,
    pub total_bytes: u64,
    pub elapsed_ms: u128,
    pub total_errors: u64,
    pub total_operations: u64,  // Add operations tracking from runner.rs
    pub cycles_completed: u32,  // Cycles completed (from last block processed)
    pub cycle_limit: Option<u32>,  // Planned cycles (None = unlimited)
    pub stopped_by_time_limit: bool,  // True if stopped due to time limit
}

// Thread pool worker context
pub struct WorkerContext {
    pub thread_id: usize,
    pub cpu_id: usize,
    pub numa_node: u32,
    pub allocated_blocks: Vec<AllocationBlock>,
    pub receiver: Receiver<WorkItem>,
    pub result_sender: Sender<WorkResult>,
}

pub type TestStatsTuple = (usize, usize, u64, u128, u64, u64); // thread_id, cpu_id, bytes, elapsed, errors, operations

// Thread pool for persistent workers
pub struct ThreadPool {
    workers: Vec<thread::JoinHandle<Vec<AllocationBlock>>>,
    senders: Vec<Sender<WorkItem>>,
    cpu_assignments: Vec<(usize, usize, u32)>, // (thread_id, logical_cpu, numa_node)
}

impl ThreadPool {
    pub fn new(
        mut thread_blocks: HashMap<usize, Vec<AllocationBlock>>,
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
        for (thread_id, &cpu_id) in cpu_list_final.iter().enumerate().take(thread_count) {
            let (work_sender, work_receiver) = channel();
            senders.push(work_sender.clone());
            let numa_node = get_numa_node_for_cpu(cpu_id);
            cpu_assignments.push((thread_id, cpu_id, numa_node));
            
            // Take ownership of this thread's blocks
            let blocks = thread_blocks.remove(&thread_id).unwrap_or_default();
            
            let result_sender = result_sender.clone();
            let pinning_config = pinning_config.clone();
                        
            let handle = thread::spawn(move || {
                // Pin thread to CPU once at creation
                if pinning_config.enable_pinning {
                    match crate::runner::pin_thread_to_cpu_with_config(thread_id, cpu_id, pinning_config.enable_pinning) {
                        Ok(actual_cpu) => {
                            log::debug!("Thread {} pinned to CPU {}", thread_id, actual_cpu);
                            let thread_handle = unsafe { windows::Win32::System::Threading::GetCurrentThread() };
                            let _ = crate::runner::set_thread_ideal_processor_ex(thread_handle, actual_cpu as u32);
                        }
                        Err(e) => {
                            log::warn!("Thread {} pinning failed: {}", thread_id, e);
                        }
                    }
                }
                
                // Apply thread priority
                if let Err(e) = crate::runner::PerformanceConfig::default().apply() {
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

    pub fn get_cpu_assignments(&self) -> &[(usize, usize, u32)] {
        &self.cpu_assignments
    }
    
    pub fn execute_test(
        &self,
        test_name: &'static str,  // Must be static for WorkItem
        test_func: &TestFunction,
        test_config: &TestMemoryConfig,
        error_mode: ErrorMode,
    ) {
        let thread_count = self.senders.len();
        let barrier = Arc::new(Barrier::new(thread_count));
        
        // Send work to all threads
        for sender in &self.senders {
            let work_item = WorkItem::RunTest {
                test_name,  // Already &'static str
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
    
    pub fn execute_memory_type_change(&self, new_memory_type: MemoryType) {
        let thread_count = self.senders.len();
        let memory_change_barrier = Arc::new(Barrier::new(thread_count));
        
        // Send memory type change work to all threads (free + reallocate)
        for sender in &self.senders {
            let work_item = WorkItem::ChangeMemoryType {
                new_memory_type,
                barrier: Arc::clone(&memory_change_barrier),
            };
            
            if sender.send(work_item).is_err() {
                log::error!("Failed to send memory type change work to thread");
            }
        }
    }
    
    // Modified shutdown to return assignments along with blocks
    pub fn shutdown(self) -> (HashMap<usize, Vec<AllocationBlock>>, Vec<(usize, usize, u32)>) {
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

fn handle_test_errors(stats: &crate::tests::TestStats, error_mode: ErrorMode, test_name: &str) -> Result<(), String> {
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

/// Calculate which blocks to test based on window size
/// Window limits TOTAL memory tested, not per-block
/// Always includes complete blocks (rounds up to block boundary)

// Worker thread main loop
fn worker_thread_loop(context: &mut WorkerContext) {
    loop {
        match context.receiver.recv() {
            Ok(WorkItem::RunTest { test_name, test_func, test_config, error_mode, barrier }) => {
                // Wait for all threads to be ready
                barrier.wait();
                
                let thread_start = Instant::now();

                // Extract the MultiBlock test function
                let f = match &test_func {
                    crate::runner::TestFunction::MultiBlock(f) => f,
                    _ => unreachable!("Only MultiBlock tests are registered"),
                };

                // MultiBlock tests receive ALL allocated blocks and handle interleaving internally
                let blocks_slice = &context.allocated_blocks[..];

                // Create progress tracker
                let progress = crate::tests::TestProgress::new();

                // Run the multi-block test with ALL blocks
                let stats = unsafe {
                    f(blocks_slice, context.thread_id, error_mode, &test_config.timing, &test_config, Some(&progress))
                };

                // Handle any errors from the test
                if stats.error_count > 0 {
                    if let Err(e) = handle_test_errors(&stats, error_mode, &test_name) {
                        log::error!("[Thread {} on CPU {}] {}", context.thread_id, context.cpu_id, e);
                    }
                }

                let elapsed_ms = thread_start.elapsed().as_millis();

                let result = WorkResult {
                    thread_id: context.thread_id,
                    total_bytes: stats.bytes_processed as u64,
                    elapsed_ms,
                    total_errors: stats.error_count,
                    total_operations: stats.total_operations,
                    cycles_completed: stats.cycles_completed,
                    cycle_limit: stats.cycles_planned,
                    stopped_by_time_limit: stats.stopped_by_time_limit,
                };
                
                if context.result_sender.send(result).is_err() {
                    log::error!("Failed to send result from thread {}", context.thread_id);
                }
            }
            Ok(WorkItem::ChangeMemoryType { new_memory_type, barrier }) => {
                log::info!("Thread {} stopping for memory type change to {:?}", context.thread_id, new_memory_type);
                
                // Wait at barrier - main thread will handle reallocation and send new blocks
                barrier.wait();
                
                log::info!("Thread {} resumed after memory type change to {:?}", context.thread_id, new_memory_type);
            }
            Ok(WorkItem::UpdateAllocations { new_blocks, barrier }) => {
                log::info!("Thread {} received {} new memory allocations", context.thread_id, new_blocks.len());
                
                // Replace old allocations with new ones
                context.allocated_blocks = new_blocks;
                
                // Wait at barrier to ensure all threads have updated before continuing
                barrier.wait();
                
                log::info!("Thread {} allocation update complete", context.thread_id);
            }
            Ok(WorkItem::Shutdown) => {
                log::debug!("Thread {} shutting down", context.thread_id);
                break;
            }
            Err(_) => {
                log::debug!("Thread {} channel closed, shutting down", context.thread_id);
                break;
            }
        }
    }
}