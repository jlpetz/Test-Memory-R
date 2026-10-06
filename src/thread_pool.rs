use crate::{ErrorMode, AllocationBlock};
use crate::tests::{TestMemoryConfig, TestProgress};
use crate::runner::{TestFunction, ThreadPriority};
use crate::cpu_topology::get_numa_node_for_cpu;
use crate::latency_tests::LatencyTestStats;
use crate::seal::SealKernel;
use crate::tests::Stage;
use std::collections::HashMap;
use std::sync::{Arc, Barrier};
use std::sync::mpsc::{channel, Sender, Receiver};
use std::thread;
use std::time::Instant;

/// A worker thread's CPU assignment: `(thread_id, logical_cpu, numa_node)`.
pub type CpuAssignment = (usize, usize, u32);

#[derive(Debug)]
pub enum WorkItem {
    RunTest {
        test_name: &'static str,  // Keep as static throughout
        test_func: TestFunction,
        test_config: Box<TestMemoryConfig>,  // Boxed: the variant would otherwise be ~300 bytes against a unit `Shutdown`
        error_mode: ErrorMode,
        barrier: Arc<Barrier>,
        /// Where the step is in the run, for its error lines: `step 7 (Test 12, Mem-SimpleV2), cycle 2`
        context: String,
    },
    /// Before a step in a sealed run, the seal work between steps (TODO 74, `seal_before_step`):
    /// its own dispatch, so the step's time doesn't include it.
    SealBeforeStep {
        test_name: &'static str,
        test_config: Box<TestMemoryConfig>,
        barrier: Arc<Barrier>,
        context: String,
    },
    /// A run-wide seal stage over the worker's whole span (TODO 74).
    Seal {
        op: SealOp,
        kernel: SealKernel,
        barrier: Arc<Barrier>,
        /// As `RunTest`'s, e.g. `cycle 2`
        context: String,
    },
    Shutdown,
}

/// The seal's run-wide stages (TODO 74).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SealOp {
    /// Seal all of memory, at a cycle's start.
    SealAll,
    /// Check all of memory still holds the seal, at a cycle's end.
    FinalCheck,
}

// Result from worker thread for standard tests
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
    pub latency_stats: Option<LatencyTestStats>,  // Set for latency tests - includes percentiles
    /// Bad words the seal checks found during the step: before its chunks, or before a step that
    /// never takes the seal overwrote them (TODO 74). For a final check, its errors.
    pub seal_errors: u64,
}

// Thread pool worker context
pub struct WorkerContext {
    pub thread_id: usize,
    pub cpu_id: usize,
    pub allocated_blocks: Vec<AllocationBlock>,
    pub receiver: Receiver<WorkItem>,
    pub result_sender: Sender<WorkResult>,
    /// Every worker's live-progress slot; this worker publishes into `live[thread_id]`.
    pub live: Arc<[TestProgress]>,
    /// In a sealed run, the first bytes of the span that don't hold the seal: what the steps that
    /// never take it have written since the last reseal (TODO 74). All of it until the first seal.
    pub unsealed: usize,
}

// Thread pool for persistent workers
pub struct ThreadPool {
    workers: Vec<thread::JoinHandle<Vec<AllocationBlock>>>,
    senders: Vec<Sender<WorkItem>>,
    cpu_assignments: Vec<CpuAssignment>, // (thread_id, logical_cpu, numa_node)
    live: Arc<[TestProgress]>,
}

impl ThreadPool {
    pub fn new(
        mut thread_blocks: HashMap<usize, Vec<AllocationBlock>>,
        pin_threads: bool,
        thread_priority: ThreadPriority,
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

        // One live-progress slot per worker, indexed by thread_id. They outlive every test, so the
        // progress ticker can read the running test's figures while its workers publish them.
        let live: Arc<[TestProgress]> =
            (0..cpu_list_final.len().min(thread_count)).map(|_| TestProgress::new()).collect();
        
        // Create persistent worker threads
        for (thread_id, &cpu_id) in cpu_list_final.iter().enumerate().take(thread_count) {
            let (work_sender, work_receiver) = channel();
            senders.push(work_sender.clone());
            let numa_node = get_numa_node_for_cpu(cpu_id);
            cpu_assignments.push((thread_id, cpu_id, numa_node));
            
            // Take ownership of this thread's blocks
            let blocks = thread_blocks.remove(&thread_id).unwrap_or_default();
            
            let result_sender = result_sender.clone();
            let live = Arc::clone(&live);
                        
            let handle = thread::spawn(move || {
                // Pin thread to CPU once at creation
                if pin_threads {
                    match crate::runner::pin_thread_to_cpu_with_config(thread_id, cpu_id, pin_threads) {
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
                
                if let Err(e) = thread_priority.apply() {
                    log::warn!("Thread {} priority not set: {}", thread_id, e);
                }
                
                let mut context = WorkerContext {
                    thread_id,
                    cpu_id,
                    allocated_blocks: blocks,
                    receiver: work_receiver,
                    result_sender,
                    live,
                    unsealed: usize::MAX,
                };
                
                // Worker loop - process work items until shutdown
                worker_thread_loop(&mut context);
                
                // Return the allocated blocks when thread exits
                context.allocated_blocks
            });

            workers.push(handle);
        }
        
        (ThreadPool { workers, senders, cpu_assignments, live }, result_receiver)
    }

    pub fn get_cpu_assignments(&self) -> &[CpuAssignment] {
        &self.cpu_assignments
    }

    /// The workers' live-progress slots, for the progress ticker. Reset one only while no test is
    /// running.
    pub fn live_progress(&self) -> Arc<[TestProgress]> {
        Arc::clone(&self.live)
    }
    
    pub fn execute_test(
        &self,
        test_name: &'static str,  // Must be static for WorkItem
        test_func: &TestFunction,
        test_config: &TestMemoryConfig,
        error_mode: ErrorMode,
        context: &str,
    ) {
        let thread_count = self.senders.len();
        let barrier = Arc::new(Barrier::new(thread_count));

        // Send work to all threads
        for sender in &self.senders {
            let work_item = WorkItem::RunTest {
                test_name,  // Already &'static str
                test_func: test_func.clone(),
                test_config: Box::new(test_config.clone()),
                error_mode,
                barrier: Arc::clone(&barrier),
                context: context.to_string(),
            };

            if sender.send(work_item).is_err() {
                log::error!("Failed to send work to thread");
            }
        }
    }

    /// The seal work before a step, on every worker (TODO 74); each sends one `WorkResult`.
    pub fn execute_seal_before_step(&self, test_name: &'static str, test_config: &TestMemoryConfig, context: &str) {
        let barrier = Arc::new(Barrier::new(self.senders.len()));
        for sender in &self.senders {
            let work_item = WorkItem::SealBeforeStep {
                test_name,
                test_config: Box::new(test_config.clone()),
                barrier: Arc::clone(&barrier),
                context: context.to_string(),
            };
            if sender.send(work_item).is_err() {
                log::error!("Failed to send work to thread");
            }
        }
    }

    /// Run a seal stage on every worker's span (TODO 74); each sends one `WorkResult`.
    pub fn execute_seal(&self, op: SealOp, kernel: SealKernel, context: &str) {
        let barrier = Arc::new(Barrier::new(self.senders.len()));
        for sender in &self.senders {
            let work_item = WorkItem::Seal { op, kernel, barrier: Arc::clone(&barrier), context: context.to_string() };
            if sender.send(work_item).is_err() {
                log::error!("Failed to send work to thread");
            }
        }
    }

    // Modified shutdown to return assignments along with blocks
    pub fn shutdown(self) -> (HashMap<usize, Vec<AllocationBlock>>, Vec<CpuAssignment>) {
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
        ErrorMode::Halt => return Err(format!("Test halted due to {} errors + {} seal in {}", stats.error_count, stats.seal_errors, test_name)),
        ErrorMode::Log => {
            log::error!(
                "[Thread {}] {} errors + {} seal detected in {} (continuing)",
                stats.thread_id,
                stats.error_count,
                stats.seal_errors,
                test_name
            );
        }
    }
    Ok(())
}

/// Before a step in a sealed run (TODO 74), on the worker's span: a step that takes the seal gets
/// back the seal on what the steps before it that don't left unsealed; one that doesn't has the
/// sealed part of what it will overwrite checked first. Returns the bad words that check found.
pub(crate) fn seal_before_step(blocks: &[AllocationBlock], config: &TestMemoryConfig, test_name: &str,
                    progress: &TestProgress, unsealed: &mut usize) -> u64 {
    let seal = config.seal;
    if !seal.on {
        return 0;
    }
    let span = crate::test_memory::extent(blocks, usize::MAX);
    let base = span.ptr as *mut u64;
    *unsealed = (*unsealed).min(span.test_size);
    let mut errors = 0;
    // In 64 MiB pieces, so Ctrl+C is seen within one
    const PIECE: usize = 64 << 20;
    let stop = || crate::runner::SHUTDOWN_REQUESTED.load(std::sync::atomic::Ordering::Relaxed);
    if seal.expects {
        if *unsealed > 0 {
            progress.set_stage(Stage::Resealing);
            let mut at = 0;
            while at < *unsealed && !stop() {
                let end = (at + PIECE).min(*unsealed);
                // SAFETY: the prefix of this worker's own span
                unsafe { seal.kernel.fill(base, at / 8, end / 8) };
                at = end;
            }
            // A reseal cut short leaves the prefix counted unsealed; resealing it again is harmless
            if at >= *unsealed {
                *unsealed = 0;
            }
        }
    } else {
        let (extent, _) = crate::test_scaffolding::test_extent(blocks, config, test_name);
        if extent.test_size > *unsealed {
            progress.set_stage(Stage::CheckingSeal);
            let mut at = *unsealed;
            while at < extent.test_size && !stop() {
                let end = (at + PIECE).min(extent.test_size);
                // SAFETY: as above
                errors += unsafe { seal.kernel.check(base, at / 8, end / 8, "seal check before") };
                at = end;
            }
            *unsealed = extent.test_size;
        }
    }
    progress.set_stage(Stage::Testing);
    errors
}

/// A run-wide seal stage on the worker's whole span, in 64 MiB pieces so Ctrl+C and the ticker see
/// it move (TODO 74).
fn run_seal_stage(blocks: &[AllocationBlock], op: SealOp, kernel: SealKernel, progress: &TestProgress,
                  unsealed: &mut usize, thread_id: usize) -> WorkResult {
    const PIECE: usize = 64 << 20;
    let start = Instant::now();
    let span = crate::test_memory::extent(blocks, usize::MAX);
    let base = span.ptr as *mut u64;
    let (from, stage) = match op {
        SealOp::SealAll => (0, Stage::SealingMemory),
        SealOp::FinalCheck => ((*unsealed).min(span.test_size), Stage::FinalSealCheck),
    };
    progress.set_stage(stage);
    let mut errors = 0;
    let mut at = from;
    while at < span.test_size && !crate::runner::SHUTDOWN_REQUESTED.load(std::sync::atomic::Ordering::Relaxed) {
        let end = (at + PIECE).min(span.test_size);
        // SAFETY: this worker's own span
        unsafe {
            match op {
                SealOp::SealAll => kernel.fill(base, at / 8, end / 8),
                SealOp::FinalCheck => errors += kernel.check(base, at / 8, end / 8, "final seal check of"),
            }
        }
        at = end;
        progress.bytes_processed.store((at - from) as u64, std::sync::atomic::Ordering::Relaxed);
        progress.last_update_ms.store(start.elapsed().as_millis().max(1) as u64, std::sync::atomic::Ordering::Relaxed);
    }
    if op == SealOp::SealAll {
        // Sealed up to where it got: a stage cut short leaves the rest unsealed
        *unsealed = if at >= span.test_size { 0 } else { span.test_size };
    }
    progress.set_stage(Stage::Testing);
    WorkResult {
        thread_id,
        total_bytes: (at - from) as u64,
        elapsed_ms: start.elapsed().as_millis(),
        total_errors: 0,
        total_operations: 0,
        cycles_completed: 1,
        cycle_limit: Some(1),
        stopped_by_time_limit: false,
        latency_stats: None,
        seal_errors: errors,
    }
}

// Worker thread main loop
fn worker_thread_loop(context: &mut WorkerContext) {
    loop {
        match context.receiver.recv() {
            Ok(WorkItem::RunTest { test_name, test_func, test_config, error_mode, barrier, context: step }) => {
                // Wait for all threads to be ready
                barrier.wait();

                let thread_start = Instant::now();
                crate::error_context::set(format!("{step}, thread {}", context.thread_id));

                // MultiBlock tests receive ALL allocated blocks and handle interleaving internally
                let blocks_slice = &context.allocated_blocks[..];

                let progress = &context.live[context.thread_id];

                // Execute test based on function type
                let (stats, latency_stats) = match &test_func {
                    crate::runner::TestFunction::MultiBlock(f) => {
                        // Regular bandwidth test - returns TestStats only
                        let stats = unsafe {
                            f(blocks_slice, context.thread_id, error_mode, &test_config.timing, &test_config, Some(progress))
                        };
                        (stats, None)
                    }
                    crate::runner::TestFunction::Latency(f) => {
                        // Latency test - returns LatencyTestStats with percentiles
                        let latency_stats = unsafe {
                            f(blocks_slice, context.thread_id, error_mode, &test_config.timing, &test_config, Some(progress))
                        };
                        (latency_stats.basic_stats.clone(), Some(latency_stats))
                    }
                };

                // Handle any errors from the test, the seal's around its chunks too
                if stats.error_count + stats.seal_errors > 0
                    && let Err(e) = handle_test_errors(&stats, error_mode, test_name) {
                        log::error!("[Thread {} on CPU {}] {}", context.thread_id, context.cpu_id, e);
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
                    latency_stats,  // Include detailed latency stats when available
                    seal_errors: stats.seal_errors,
                };

                if context.result_sender.send(result).is_err() {
                    log::error!("Failed to send result from thread {}", context.thread_id);
                }
            }
            Ok(WorkItem::SealBeforeStep { test_name, test_config, barrier, context: step }) => {
                barrier.wait();
                let start = Instant::now();
                crate::error_context::set(format!("{step}, thread {}", context.thread_id));
                let progress = &context.live[context.thread_id];
                let errors = seal_before_step(&context.allocated_blocks, &test_config, test_name, progress, &mut context.unsealed);
                let result = WorkResult {
                    thread_id: context.thread_id,
                    total_bytes: 0,
                    elapsed_ms: start.elapsed().as_millis(),
                    total_errors: 0,
                    total_operations: 0,
                    cycles_completed: 1,
                    cycle_limit: Some(1),
                    stopped_by_time_limit: false,
                    latency_stats: None,
                    seal_errors: errors,
                };
                if context.result_sender.send(result).is_err() {
                    log::error!("Failed to send result from thread {}", context.thread_id);
                }
            }
            Ok(WorkItem::Seal { op, kernel, barrier, context: cycle }) => {
                barrier.wait();
                crate::error_context::set(format!("{cycle}, thread {}", context.thread_id));
                let progress = &context.live[context.thread_id];
                let result = run_seal_stage(&context.allocated_blocks, op, kernel, progress, &mut context.unsealed, context.thread_id);
                if context.result_sender.send(result).is_err() {
                    log::error!("Failed to send result from thread {}", context.thread_id);
                }
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