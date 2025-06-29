pub mod config;
mod memory;

use memory::TestBuffer;
use std::arch::x86_64::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::thread;
use std::time::Instant;

pub use config::*;

pub const DEFAULT_RESERVE_PERCENT: f64 = 10.0;
const CACHE_BUSTING_STRIDE: usize = 4096;

// Cache size detection for modern window sizing
const L1_CACHE_SIZE: usize = 32 * 1024;      // 32KB typical
const L2_CACHE_SIZE: usize = 256 * 1024;     // 256KB typical  
const L3_CACHE_SIZE: usize = 32 * 1024 * 1024; // 32MB typical

pub fn check_large_page_privilege() -> Result<(), &'static str> {
    memory::check_large_page_privilege()
}

#[derive(Debug, Clone)]
pub enum MemoryStrategy {
    TM5Compatible {
        testing_window_size_mb: u32,
        reserved_memory_mb: u32,
        test_block_size_mb: u32, // 0 = use full window per thread
    },
    ModernOptimal {
        reserve_gib: Option<f64>, // None = use percentage
    },
    Custom {
        blocks_per_thread: u32,
        min_block_size_mb: u32,
    },
}

impl MemoryStrategy {
    pub fn calculate_stage1_allocation(&self, total_memory_bytes: usize, reserve_percent: f64, thread_count: usize) -> usize {
        match self {
            MemoryStrategy::TM5Compatible { reserved_memory_mb, .. } => {
                // Stage 1: Allocate maximum memory minus OS reserve (TM5 style)
                let os_reserve = (*reserved_memory_mb as usize) * 1024 * 1024;
                let max_memory_for_testing = total_memory_bytes.saturating_sub(os_reserve);
                max_memory_for_testing / thread_count
            }
            MemoryStrategy::ModernOptimal { reserve_gib } => {
                if let Some(gib) = reserve_gib {
                    let reserve_bytes = (gib * 1024.0 * 1024.0 * 1024.0) as usize;
                    let usable = total_memory_bytes.saturating_sub(reserve_bytes);
                    usable / thread_count
                } else {
                    let reserve_bytes = (total_memory_bytes as f64 * reserve_percent / 100.0) as usize;
                    let usable = total_memory_bytes.saturating_sub(reserve_bytes);
                    usable / thread_count
                }
            }
            MemoryStrategy::Custom { .. } => {
                let reserve_bytes = (total_memory_bytes as f64 * reserve_percent / 100.0) as usize;
                let usable = total_memory_bytes.saturating_sub(reserve_bytes);
                usable / thread_count
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum ErrorMode {
    Log,   // Log errors and continue (default)
    Halt,  // Stop current test on first error
    Panic, // Panic on first error (for debugging)
}

#[repr(C)]
#[derive(Clone, Copy)]
enum TestAction {
    Read,
    Write,
    ReadWrite,
    WriteVerify,
    Copy,
    Verify,
    WriteWaitVerify,
    CacheBusting,
    RandomAccess,
}

impl TestAction {
    fn label(&self) -> &'static str {
        match self {
            TestAction::Read => "Read",
            TestAction::Write => "Write",
            TestAction::ReadWrite => "Read/Write",
            TestAction::WriteVerify => "Write + Verify",
            TestAction::Copy => "Copy",
            TestAction::Verify => "Verify",
            TestAction::WriteWaitVerify => "Write + Wait + Verify",
            TestAction::CacheBusting => "Cache Busting",
            TestAction::RandomAccess => "Random Access",
        }
    }
}

#[repr(C)]
pub struct TestStats {
    name: &'static str,
    action: TestAction,
    bytes_processed: usize,
    elapsed_ms: u128,
    thread_id: usize,
    error_count: u64,
}

// Three-stage memory configuration
#[derive(Debug, Clone)]
pub struct TestMemoryConfig {
    pub window_size_bytes: usize,        // Stage 2: Testing window within allocation
    pub block_size_bytes: usize,         // Stage 3: Block/chunk size for access patterns
    pub allow_misaligned: bool,          // Allow unaligned accesses for stress testing
    pub auto_adjust_alignment: bool,     // Auto-adjust for optimal alignment
}

impl TestMemoryConfig {
    pub fn new(window_size_mb: Option<u32>, block_size_mb: Option<u32>, allow_misaligned: bool) -> Self {
        let window_size_bytes = window_size_mb.map(|mb| mb as usize * 1024 * 1024).unwrap_or(0);
        let block_size_bytes = block_size_mb.map(|mb| mb as usize * 1024 * 1024).unwrap_or(0);
        
        Self {
            window_size_bytes,
            block_size_bytes,
            allow_misaligned,
            auto_adjust_alignment: true,
        }
    }

    // Calculate optimal window size for modern systems
    pub fn calculate_optimal_window_size(&self, test_name: &str, allocated_size: usize) -> usize {
        match test_name {
            // Cache-focused tests use smaller windows
            "CacheBusting" => (L3_CACHE_SIZE * 2).min(allocated_size / 4),
            "RandomTorture" => (L3_CACHE_SIZE * 4).min(allocated_size / 2),
            
            // Memory bandwidth tests use larger windows
            "BandwidthSat" => allocated_size / 2,
            "MirrorMove128NonTemporal" | "MirrorMove256NonTemporal" | "MirrorMove512NonTemporal" => {
                allocated_size / 3 // Large but not full allocation
            }
            
            // General tests use moderate windows
            "SimpleTest" | "RefreshStable" => (allocated_size / 4).max(64 * 1024 * 1024), // At least 64MB
            "StrideAccess" => (allocated_size / 8).max(32 * 1024 * 1024), // At least 32MB
            
            _ => if self.window_size_bytes > 0 {
                self.window_size_bytes
            } else {
                allocated_size / 4 // Default to 25% of allocation
            }
        }
    }

    // Calculate optimal block size with alignment
    pub fn calculate_optimal_block_size(&self, test_name: &str, window_size: usize) -> (usize, bool) {
        let optimal_block_size = match test_name {
            // SIMD tests need specific alignments
            "MirrorMove128NonTemporal" => {
                let base_size = if self.block_size_bytes > 0 { self.block_size_bytes } else { 16 * 1024 * 1024 };
                align_to_boundary(base_size, 128 / 8) // 128-bit alignment
            }
            "MirrorMove256NonTemporal" => {
                let base_size = if self.block_size_bytes > 0 { self.block_size_bytes } else { 32 * 1024 * 1024 };
                align_to_boundary(base_size, 256 / 8) // 256-bit alignment  
            }
            "MirrorMove512NonTemporal" => {
                let base_size = if self.block_size_bytes > 0 { self.block_size_bytes } else { 64 * 1024 * 1024 };
                align_to_boundary(base_size, 512 / 8) // 512-bit alignment
            }
            
            // Cache tests use cache-line aligned blocks
            "CacheBusting" => {
                let base_size = if self.block_size_bytes > 0 { self.block_size_bytes } else { 1 * 1024 * 1024 };
                align_to_boundary(base_size, 64) // Cache line alignment
            }
            
            // Memory controller tests use page-aligned blocks
            "SimpleTest" | "RefreshStable" => {
                let base_size = if self.block_size_bytes > 0 { self.block_size_bytes } else { 4 * 1024 * 1024 };
                align_to_boundary(base_size, 4096) // Page alignment
            }
            
            _ => {
                let base_size = if self.block_size_bytes > 0 { self.block_size_bytes } else { 8 * 1024 * 1024 };
                if self.allow_misaligned {
                    base_size // No alignment adjustment
                } else {
                    align_to_boundary(base_size, 64) // Cache line alignment default
                }
            }
        };

        let was_adjusted = optimal_block_size != self.block_size_bytes;
        (optimal_block_size.min(window_size), was_adjusted)
    }

    // Ensure window size is multiple of block size
    pub fn align_window_to_blocks(&self, window_size: usize, block_size: usize) -> (usize, bool) {
        if block_size == 0 || window_size == 0 {
            return (window_size, false);
        }
        
        let aligned_window = (window_size / block_size) * block_size;
        let was_adjusted = aligned_window != window_size;
        
        // Ensure we have at least one full block
        let final_window = if aligned_window < block_size {
            block_size
        } else {
            aligned_window
        };
        
        (final_window, was_adjusted || final_window != window_size)
    }
}

fn align_to_boundary(size: usize, alignment: usize) -> usize {
    ((size + alignment - 1) / alignment) * alignment
}

pub struct MemoryLayout {
    pub total_memory: usize,
    pub allocated_memory: usize,      // Stage 1: Total allocated (e.g., 55GB)
    pub reserved_memory: usize,
    pub blocks: Vec<BlockInfo>,
    pub strategy: MemoryStrategy,
}

#[derive(Debug, Clone)]
pub struct BlockInfo {
    pub size_bytes: usize,           // Stage 1: Full allocation per thread (e.g., 4.6GB)
    pub thread_id: usize,
}

impl MemoryLayout {
    pub fn calculate(strategy: MemoryStrategy, thread_count: usize, reserve_percent: f64) -> Self {
        let total_memory = memory::get_total_system_memory();
        
        // Stage 1: Calculate maximum allocation per thread
        let allocated_per_thread = strategy.calculate_stage1_allocation(total_memory, reserve_percent, thread_count);
        let total_allocated = allocated_per_thread * thread_count;
        let reserved_memory = total_memory - total_allocated;

        let blocks = (0..thread_count)
            .map(|thread_id| BlockInfo {
                size_bytes: allocated_per_thread,
                thread_id,
            })
            .collect();

        MemoryLayout {
            total_memory,
            allocated_memory: total_allocated,
            reserved_memory,
            blocks,
            strategy,
        }
    }

    pub fn print_layout(&self) {
        log::info!("Memory Layout (Three-Stage Architecture):");
        log::info!(
            "  Total System Memory: {:.2} GiB",
            self.total_memory as f64 / (1024.0 * 1024.0 * 1024.0)
        );
        log::info!(
            "  Stage 1 - Total Allocated: {:.2} GiB ({:.1}% of system)",
            self.allocated_memory as f64 / (1024.0 * 1024.0 * 1024.0),
            (self.allocated_memory as f64 / self.total_memory as f64) * 100.0
        );
        log::info!(
            "  Reserved for OS: {:.2} GiB",
            self.reserved_memory as f64 / (1024.0 * 1024.0 * 1024.0)
        );
        log::info!("  Strategy: {:?}", self.strategy);
        log::info!("  Threads: {}", self.blocks.len());

        for block in &self.blocks {
            let size_gib = block.size_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
            log::info!("  Thread {}: {:.2} GiB allocated", block.thread_id, size_gib);
        }
        
        log::info!("  Note: Stage 2 (window size) and Stage 3 (block size) will be");
        log::info!("        configured per test within these allocations");
    }
}

// Global progress tracking
pub struct ProgressTracker {
    pub total_tests: AtomicU64,
    pub completed_tests: AtomicU64,
    pub total_errors: AtomicU64,
    pub current_phase: Mutex<String>,
    pub current_throughput: AtomicU64,
}

impl ProgressTracker {
    pub fn new() -> Self {
        Self {
            total_tests: AtomicU64::new(0),
            completed_tests: AtomicU64::new(0),
            total_errors: AtomicU64::new(0),
            current_phase: Mutex::new("Initializing".to_string()),
            current_throughput: AtomicU64::new(0),
        }
    }

    pub fn add_errors(&self, count: u64) {
        self.total_errors.fetch_add(count, Ordering::Relaxed);
    }

    pub fn complete_test(&self, stats: &TestStats) {
        self.completed_tests.fetch_add(1, Ordering::Relaxed);
        self.add_errors(stats.error_count);

        if stats.elapsed_ms > 0 {
            let throughput = (stats.bytes_processed as u128 * 1000 * 1000) / stats.elapsed_ms;
            self.current_throughput.store(throughput as u64, Ordering::Relaxed);
        }
    }

    pub fn set_phase(&self, phase: &str) {
        if let Ok(mut current) = self.current_phase.lock() {
            *current = phase.to_string();
        }
    }

    pub fn get_status(&self) -> (u64, u64, u64, String, f64) {
        let completed = self.completed_tests.load(Ordering::Relaxed);
        let total = self.total_tests.load(Ordering::Relaxed);
        let errors = self.total_errors.load(Ordering::Relaxed);
        let phase = self
            .current_phase
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_else(|_| "Unknown".to_string());
        let throughput_raw = self.current_throughput.load(Ordering::Relaxed);
        let throughput_gib_s = (throughput_raw as f64) / (1000.0 * 1024.0 * 1024.0 * 1024.0);

        (completed, total, errors, phase, throughput_gib_s)
    }
}

pub fn run_tests_with_layout(layout: MemoryLayout, error_mode: ErrorMode) -> bool {
    layout.print_layout();

    let progress = Arc::new(ProgressTracker::new());
    let success = Arc::new(AtomicBool::new(true));

    // Calculate total tests
    let test_types = 9;
    let total_tests = layout.blocks.len() * test_types;
    progress.total_tests.store(total_tests as u64, Ordering::Relaxed);

    let mut thread_blocks: HashMap<usize, Vec<BlockInfo>> = HashMap::new();
    for block in layout.blocks {
        thread_blocks.entry(block.thread_id).or_default().push(block);
    }

    // Stage 1: Pre-allocate all memory blocks
    progress.set_phase("Stage 1: Allocating Memory");
    println!("Stage 1: Pre-allocating memory blocks...");

    let mut allocated_blocks = match allocate_all_blocks(&thread_blocks) {
        Ok(blocks) => blocks,
        Err(e) => {
            println!("❌ Failed to allocate memory blocks: {}", e);
            return false;
        }
    };

    print_allocation_summary(&allocated_blocks);

    // Start progress reporter thread
    let progress_clone = Arc::clone(&progress);
    let progress_handle = thread::spawn(move || {
        progress_reporter(progress_clone);
    });

    let thread_count = thread_blocks.len();
    let barrier = Arc::new(Barrier::new(thread_count));
    let mut handles = vec![];

    for (thread_id, _) in thread_blocks {
        let success_clone = Arc::clone(&success);
        let progress_clone = Arc::clone(&progress);
        let barrier_clone = Arc::clone(&barrier);
        let thread_allocated_blocks = allocated_blocks.remove(&thread_id).unwrap();

        let handle = thread::spawn(move || {
            if let Err(e) =
                run_thread_tests_with_allocated_blocks(thread_id, thread_allocated_blocks, error_mode, progress_clone, barrier_clone)
            {
                log::error!("[Thread {}] Error: {}", thread_id, e);
                success_clone.store(false, Ordering::Relaxed);
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        if handle.join().is_err() {
            success.store(false, Ordering::Relaxed);
        }
    }

    progress.set_phase("Completed");
    thread::sleep(std::time::Duration::from_millis(100));

    if let Err(_) = progress_handle.join() {
        log::warn!("Progress reporter thread failed to join cleanly");
    }

    log::info!("Releasing all allocated memory blocks");
    success.load(Ordering::Relaxed)
}

fn progress_reporter(progress: Arc<ProgressTracker>) {
    let mut last_update = Instant::now();

    loop {
        thread::sleep(std::time::Duration::from_millis(1000));

        let (completed, total, errors, phase, throughput) = progress.get_status();

        if last_update.elapsed().as_secs() >= 5 || phase == "Completed" {
            let progress_pct = if total > 0 { (completed * 100) / total } else { 0 };

            print!("\r\x1b[K");
            print!(
                "Progress: {}/{} ({}%) | Errors: {} | Phase: {} | Speed: {:.2} GiB/s",
                completed, total, progress_pct, errors, phase, throughput
            );

            if phase == "Completed" {
                println!();
                break;
            }

            std::io::Write::flush(&mut std::io::stdout()).unwrap_or(());
            last_update = Instant::now();
        }

        if phase == "Completed" {
            break;
        }
    }
}

// Pre-allocated memory block management
pub struct AllocatedBlock {
    pub buffer: TestBuffer,
    pub block_info: BlockInfo,
}

fn allocate_all_blocks(thread_blocks: &HashMap<usize, Vec<BlockInfo>>) -> Result<HashMap<usize, Vec<AllocatedBlock>>, String> {
    let mut allocated_blocks = HashMap::new();
    let mut total_allocated = 0usize;
    let mut total_blocks = 0usize;
    let mut large_page_blocks = 0usize;

    for (thread_id, blocks) in thread_blocks {
        let mut thread_allocated = Vec::new();

        for block in blocks {
            let buffer = TestBuffer::new_large_pages(block.size_bytes)
                .or_else(|| TestBuffer::new_aligned(block.size_bytes))
                .ok_or_else(|| format!("Failed to allocate buffer of {} bytes for thread {}", block.size_bytes, thread_id))?;

            if buffer.uses_large_pages() {
                large_page_blocks += 1;
            }

            total_allocated += buffer.size();
            total_blocks += 1;

            thread_allocated.push(AllocatedBlock {
                buffer,
                block_info: block.clone(),
            });
        }

        allocated_blocks.insert(*thread_id, thread_allocated);
    }

    log::info!(
        "Stage 1 allocation completed: {} blocks, {:.2} GiB total, {} using large pages",
        total_blocks,
        total_allocated as f64 / (1024.0 * 1024.0 * 1024.0),
        large_page_blocks
    );

    Ok(allocated_blocks)
}

fn print_allocation_summary(allocated_blocks: &HashMap<usize, Vec<AllocatedBlock>>) {
    let mut total_blocks = 0;
    let mut total_size = 0usize;
    let mut large_page_blocks = 0;
    let mut size_per_thread = 0usize;

    for (thread_id, blocks) in allocated_blocks {
        let thread_total_size: usize = blocks.iter().map(|b| b.buffer.size()).sum();
        let thread_large_pages = blocks.iter().filter(|b| b.buffer.uses_large_pages()).count();

        total_blocks += blocks.len();
        total_size += thread_total_size;
        large_page_blocks += thread_large_pages;

        if *thread_id == 0 {
            size_per_thread = thread_total_size;
        }
    }

    let thread_count = allocated_blocks.len();
    let size_per_thread_gib = size_per_thread as f64 / (1024.0 * 1024.0 * 1024.0);

    println!("Stage 1 Memory Allocation Summary:");
    println!(
        "  {} × {:.2} GiB per thread = {:.2} GiB total (TM5-style)",
        thread_count, size_per_thread_gib,
        total_size as f64 / (1024.0 * 1024.0 * 1024.0)
    );

    if large_page_blocks > 0 {
        println!("  ✅ {} blocks using large pages (2MB pages)", large_page_blocks);
    } else {
        println!("  ⚠️  Using standard 4KB pages (large pages not available)");
    }
    println!();
}

// Test function with three-stage memory management
fn run_test_with_memory_stages(
    test_name: &str,
    test_func: unsafe fn(*mut u8, usize, usize, ErrorMode) -> TestStats,
    allocated_block: &AllocatedBlock,
    test_config: &TestMemoryConfig,
    thread_id: usize,
    error_mode: ErrorMode,
) -> Result<TestStats, String> {
    let allocated_size = allocated_block.buffer.size();
    let allocated_ptr = allocated_block.buffer.as_mut_ptr();
    
    // Stage 2: Calculate optimal window size within allocation
    let optimal_window_size = test_config.calculate_optimal_window_size(test_name, allocated_size);
    let (window_size, window_adjusted) = if test_config.window_size_bytes > 0 {
        (test_config.window_size_bytes.min(allocated_size), false)
    } else {
        (optimal_window_size, true)
    };
    
    // Stage 3: Calculate optimal block size with alignment
    let (block_size, block_adjusted) = test_config.calculate_optimal_block_size(test_name, window_size);
    
    // Ensure window is multiple of block size
    let (final_window_size, window_aligned) = test_config.align_window_to_blocks(window_size, block_size);
    
    // Log Stage 2 & 3 configuration
    let window_mb = final_window_size as f64 / (1024.0 * 1024.0);
    let block_mb = block_size as f64 / (1024.0 * 1024.0);
    let allocated_mb = allocated_size as f64 / (1024.0 * 1024.0);
    
    log::info!(
        "[Thread {}] {} - Stage 2: Window {:.1}MB/{:.1}MB allocated{}{}",
        thread_id, test_name, window_mb, allocated_mb,
        if window_adjusted { " (auto-sized)" } else { "" },
        if window_aligned { " (aligned to blocks)" } else { "" }
    );
    
    log::info!(
        "[Thread {}] {} - Stage 3: Block size {:.1}MB{}",
        thread_id, test_name, block_mb,
        if block_adjusted { " (auto-aligned)" } else { "" }
    );
    
    // Run the test function on the configured window
    let stats = unsafe {
        test_func(allocated_ptr, final_window_size, thread_id, error_mode)
    };
    
    Ok(stats)
}

// SIMD-optimized memory testing implementations
unsafe fn mirror_move_128_non_temporal(ptr: *mut u8, size: usize, thread_id: usize, _error_mode: ErrorMode) -> TestStats {
    let start = Instant::now();
    if !is_x86_feature_detected!("sse2") {
        return TestStats {
            name: "MirrorMove128NonTemporal",
            action: TestAction::ReadWrite,
            bytes_processed: 0,
            elapsed_ms: 0,
            thread_id,
            error_count: 0,
        };
    }

    let len = size / std::mem::size_of::<__m128i>();
    let base = ptr as *mut __m128i;

    for i in 0..(len / 2) {
        let val = _mm_load_si128(base.add(i));
        _mm_stream_si128(base.add(len - 1 - i), val);
    }

    _mm_sfence();

    let elapsed = start.elapsed().as_millis();
    TestStats {
        name: "MirrorMove128NonTemporal",
        action: TestAction::ReadWrite,
        bytes_processed: size,
        elapsed_ms: elapsed,
        thread_id,
        error_count: 0,
    }
}

unsafe fn mirror_move_256_non_temporal(ptr: *mut u8, size: usize, thread_id: usize, _error_mode: ErrorMode) -> TestStats {
    let start = Instant::now();
    if !is_x86_feature_detected!("avx2") {
        return TestStats {
            name: "MirrorMove256NonTemporal",
            action: TestAction::ReadWrite,
            bytes_processed: 0,
            elapsed_ms: 0,
            thread_id,
            error_count: 0,
        };
    }

    let len = size / std::mem::size_of::<__m256i>();
    let base = ptr as *mut __m256i;

    for i in 0..(len / 2) {
        let val = _mm256_load_si256(base.add(i));
        _mm256_stream_si256(base.add(len - 1 - i), val);
    }

    _mm_sfence();

    let elapsed = start.elapsed().as_millis();
    TestStats {
        name: "MirrorMove256NonTemporal",
        action: TestAction::ReadWrite,
        bytes_processed: size,
        elapsed_ms: elapsed,
        thread_id,
        error_count: 0,
    }
}

unsafe fn mirror_move_512_non_temporal(ptr: *mut u8, size: usize, thread_id: usize, _error_mode: ErrorMode) -> TestStats {
    let start = Instant::now();
    if !is_x86_feature_detected!("avx512f") {
        return TestStats {
            name: "MirrorMove512NonTemporal",
            action: TestAction::ReadWrite,
            bytes_processed: 0,
            elapsed_ms: 0,
            thread_id,
            error_count: 0,
        };
    }

    let len = size / std::mem::size_of::<__m512i>();
    let base = ptr as *mut __m512i;

    for i in 0..(len / 2) {
        let val = _mm512_load_si512(base.add(i));
        _mm512_stream_si512(base.add(len - 1 - i), val);
    }

    _mm_sfence();

    let elapsed = start.elapsed().as_millis();
    TestStats {
        name: "MirrorMove512NonTemporal",
        action: TestAction::ReadWrite,
        bytes_processed: size,
        elapsed_ms: elapsed,
        thread_id,
        error_count: 0,
    }
}

fn run_thread_tests_with_allocated_blocks(
    thread_id: usize,
    allocated_blocks: Vec<AllocatedBlock>,
    error_mode: ErrorMode,
    progress: Arc<ProgressTracker>,
    barrier: Arc<Barrier>,
) -> Result<(), String> {
    log::info!(
        "[Thread {}] Starting tests with {} pre-allocated blocks",
        thread_id,
        allocated_blocks.len()
    );

    // Define test functions with their configurations
    let test_definitions: Vec<(&str, unsafe fn(*mut u8, usize, usize, ErrorMode) -> TestStats, TestMemoryConfig)> = vec![
        (
            "MirrorMove128NonTemporal", 
            mirror_move_128_non_temporal,
            TestMemoryConfig::new(Some(64), Some(16), false) // 64MB window, 16MB blocks
        ),
        (
            "MirrorMove256NonTemporal", 
            mirror_move_256_non_temporal,
            TestMemoryConfig::new(Some(128), Some(32), false) // 128MB window, 32MB blocks
        ),
        (
            "MirrorMove512NonTemporal", 
            mirror_move_512_non_temporal,
            TestMemoryConfig::new(Some(256), Some(64), false) // 256MB window, 64MB blocks
        ),
        (
            "SimpleTest", 
            simple_test,
            TestMemoryConfig::new(None, Some(4), false) // Auto window, 4MB blocks
        ),
        (
            "RefreshStable", 
            refresh_stable,
            TestMemoryConfig::new(Some(32), Some(1), false) // 32MB window, 1MB blocks
        ),
        (
            "CacheBusting", 
            cache_busting_write_test,
            TestMemoryConfig::new(None, Some(1), false) // Auto cache-sized window, 1MB blocks
        ),
        (
            "RandomTorture", 
            random_access_torture_test,
            TestMemoryConfig::new(None, Some(8), false) // Auto window, 8MB blocks
        ),
        (
            "StrideAccess", 
            stride_access_test,
            TestMemoryConfig::new(None, Some(2), false) // Auto window, 2MB blocks
        ),
        (
            "BandwidthSat", 
            bandwidth_saturation_test,
            TestMemoryConfig::new(None, Some(32), false) // Auto large window, 32MB blocks
        ),
    ];

    // Run each test type across all blocks before moving to next test type
    for (test_name, test_func, test_config) in test_definitions {
        progress.set_phase(&format!("Stage 2&3: {}", test_name));

        // Wait for all threads to reach this test phase
        barrier.wait();

        for (block_idx, allocated_block) in allocated_blocks.iter().enumerate() {
            log::debug!(
                "[Thread {}] Block {}/{} - Running {} with Stage 2&3 configuration",
                thread_id,
                block_idx + 1,
                allocated_blocks.len(),
                test_name
            );

            match run_test_with_memory_stages(
                test_name,
                test_func,
                allocated_block,
                &test_config,
                thread_id,
                error_mode,
            ) {
                Ok(stats) => {
                    log_stats(&stats);
                    progress.complete_test(&stats);

                    if stats.error_count > 0 {
                        handle_test_errors(&stats, error_mode, test_name)?;
                    }
                }
                Err(e) => {
                    log::error!("[Thread {}] Test {} failed: {}", thread_id, test_name, e);
                    return Err(e);
                }
            }
        }

        // Wait for all threads to complete this test phase
        barrier.wait();
    }

    Ok(())
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

fn log_stats(stats: &TestStats) {
    let gib = stats.bytes_processed as f64 / (1024.0 * 1024.0 * 1024.0);
    let secs = stats.elapsed_ms as f64 / 1000.0;
    let throughput = if secs > 0.0 { gib / secs } else { 0.0 };
    log::debug!(
        "[T{}] [{}] ({}) completed in {} ms — {:.2} GiB/s — {} errors",
        stats.thread_id,
        stats.name,
        stats.action.label(),
        stats.elapsed_ms,
        throughput,
        stats.error_count
    );
}

// SIMD capability detection
pub fn detect_simd_capabilities() -> String {
    let mut capabilities = Vec::new();

    if is_x86_feature_detected!("sse") {
        capabilities.push("SSE");
    }
    if is_x86_feature_detected!("sse2") {
        capabilities.push("SSE2");
    }
    if is_x86_feature_detected!("sse3") {
        capabilities.push("SSE3");
    }
    if is_x86_feature_detected!("sse4.1") {
        capabilities.push("SSE4.1");
    }
    if is_x86_feature_detected!("sse4.2") {
        capabilities.push("SSE4.2");
    }
    if is_x86_feature_detected!("avx") {
        capabilities.push("AVX");
    }
    if is_x86_feature_detected!("avx2") {
        capabilities.push("AVX2");
    }
    if is_x86_feature_detected!("avx512f") {
        capabilities.push("AVX-512F");
    }
    if is_x86_feature_detected!("avx512bw") {
        capabilities.push("AVX-512BW");
    }
    if is_x86_feature_detected!("avx512vl") {
        capabilities.push("AVX-512VL");
    }

    if capabilities.is_empty() {
        "None detected".to_string()
    } else {
        capabilities.join(", ")
    }
}

// Memory testing focused implementations with error handling

unsafe fn simple_test(ptr: *mut u8, size: usize, thread_id: usize, error_mode: ErrorMode) -> TestStats {
    let start = Instant::now();
    let base = ptr as *mut u64;
    let len = size / std::mem::size_of::<u64>();
    let mut error_count = 0;

    // Write phase
    for i in 0..len {
        base.add(i).write(i as u64 ^ 0xDEADBEEFDEADBEEF);
    }

    std::sync::atomic::fence(Ordering::SeqCst);

    // Verify phase
    for i in 0..len {
        let v = base.add(i).read();
        let expected = i as u64 ^ 0xDEADBEEFDEADBEEF;
        if v != expected {
            error_count += 1;
            match error_mode {
                ErrorMode::Panic => panic!("simple_test: memory error at index {}", i),
                ErrorMode::Halt => break,
                ErrorMode::Log => {
                    log::error!("simple_test: memory error at index {} - expected {:#x}, got {:#x}", i, expected, v);
                }
            }
        }
    }

    let elapsed = start.elapsed().as_millis();
    TestStats {
        name: "SimpleTest",
        action: TestAction::WriteVerify,
        bytes_processed: size,
        elapsed_ms: elapsed,
        thread_id,
        error_count,
    }
}

unsafe fn refresh_stable(ptr: *mut u8, size: usize, thread_id: usize, error_mode: ErrorMode) -> TestStats {
    let start = Instant::now();
    let base = ptr as *mut u64;
    let len = size / std::mem::size_of::<u64>();
    let mut error_count = 0;

    for i in 0..len {
        base.add(i).write(0xA5A5A5A5A5A5A5A5);
    }

    std::sync::atomic::fence(Ordering::SeqCst);
    std::thread::sleep(std::time::Duration::from_millis(50));

    for i in 0..len {
        let v = base.add(i).read();
        if v != 0xA5A5A5A5A5A5A5A5 {
            error_count += 1;
            match error_mode {
                ErrorMode::Panic => panic!("refresh_stable: memory error at index {}", i),
                ErrorMode::Halt => break,
                ErrorMode::Log => {
                    log::error!(
                        "refresh_stable: memory error at index {} - expected {:#x}, got {:#x}",
                        i,
                        0xA5A5A5A5A5A5A5A5u64,
                        v
                    );
                }
            }
        }
    }

    let elapsed = start.elapsed().as_millis();
    TestStats {
        name: "RefreshStable",
        action: TestAction::WriteWaitVerify,
        bytes_processed: size,
        elapsed_ms: elapsed,
        thread_id,
        error_count,
    }
}

unsafe fn cache_busting_write_test(ptr: *mut u8, size: usize, thread_id: usize, error_mode: ErrorMode) -> TestStats {
    let start = Instant::now();
    let base = ptr as *mut u64;
    let len = size / std::mem::size_of::<u64>();
    let mut error_count = 0;

    // Write with large strides to bust cache
    let stride = CACHE_BUSTING_STRIDE / std::mem::size_of::<u64>();
    let pattern = 0x0123456789ABCDEFu64.wrapping_add(thread_id as u64);

    for offset in 0..stride.min(len) {
        let mut i = offset;
        while i < len {
            base.add(i).write(pattern.wrapping_add(i as u64));
            i += stride;
        }
    }

    std::sync::atomic::fence(Ordering::SeqCst);

    // Verify with same stride pattern
    for offset in 0..stride.min(len) {
        let mut i = offset;
        while i < len {
            let v = base.add(i).read();
            let expected = pattern.wrapping_add(i as u64);
            if v != expected {
                error_count += 1;
                match error_mode {
                    ErrorMode::Panic => panic!("cache_busting_write_test: memory error at index {}", i),
                    ErrorMode::Halt => break,
                    ErrorMode::Log => {
                        log::error!(
                            "cache_busting_write_test: memory error at index {} - expected {:#x}, got {:#x}",
                            i,
                            expected,
                            v
                        );
                    }
                }
            }
            i += stride;
        }
    }

    let elapsed = start.elapsed().as_millis();
    TestStats {
        name: "CacheBusting",
        action: TestAction::CacheBusting,
        bytes_processed: size,
        elapsed_ms: elapsed,
        thread_id,
        error_count,
    }
}

unsafe fn random_access_torture_test(ptr: *mut u8, size: usize, thread_id: usize, error_mode: ErrorMode) -> TestStats {
    let start = Instant::now();
    let base = ptr as *mut u64;
    let len = size / std::mem::size_of::<u64>();
    let mut error_count = 0;

    // Initialize with known pattern
    for i in 0..len {
        base.add(i).write(i as u64);
    }

    std::sync::atomic::fence(Ordering::SeqCst);

    // Random access torture
    let mut rng_state = 0x123456789ABCDEFu64.wrapping_add(thread_id as u64);
    let iterations = (len / 1000).max(5000).min(50000);

    // Phase 1: Random read verification of initial pattern
    for iteration in 0..iterations {
        rng_state ^= rng_state << 13;
        rng_state ^= rng_state >> 17;
        rng_state ^= rng_state << 5;

        let idx = (rng_state as usize) % len;
        let expected = idx as u64;
        let actual = base.add(idx).read();

        if actual != expected {
            error_count += 1;
            match error_mode {
                ErrorMode::Panic => panic!(
                    "random_access_torture_test: Phase 1 memory error at index {}, iteration {}, expected {}, actual {}",
                    idx, iteration, expected, actual
                ),
                ErrorMode::Halt => break,
                ErrorMode::Log => {
                    log::error!(
                        "random_access_torture_test: Phase 1 memory error at index {}, iteration {}, expected {}, actual {}",
                        idx,
                        iteration,
                        expected,
                        actual
                    );
                }
            }
        }
    }

    let elapsed = start.elapsed().as_millis();
    TestStats {
        name: "RandomTorture",
        action: TestAction::RandomAccess,
        bytes_processed: (iterations * 2 + len) * std::mem::size_of::<u64>(),
        elapsed_ms: elapsed,
        thread_id,
        error_count,
    }
}

unsafe fn stride_access_test(ptr: *mut u8, size: usize, thread_id: usize, error_mode: ErrorMode) -> TestStats {
    let start = Instant::now();
    let base = ptr as *mut u64;
    let len = size / std::mem::size_of::<u64>();
    let mut error_count = 0;

    // Test various stride patterns that defeat caching
    let strides = [1, 16, 64, 256, 1024, 4096];
    let pattern_base = 0xFEDCBA9876543210u64.wrapping_add(thread_id as u64);

    for &stride in &strides {
        if stride >= len {
            continue;
        }

        // Write with this stride
        for start_offset in 0..stride.min(len) {
            let mut i = start_offset;
            while i < len {
                let pattern = pattern_base.wrapping_add((stride as u64) << 32).wrapping_add(i as u64);
                base.add(i).write(pattern);
                i += stride;
            }
        }

        std::sync::atomic::fence(Ordering::SeqCst);

        // Verify with same stride
        for start_offset in 0..stride.min(len) {
            let mut i = start_offset;
            while i < len {
                let expected = pattern_base.wrapping_add((stride as u64) << 32).wrapping_add(i as u64);
                let actual = base.add(i).read();
                if actual != expected {
                    error_count += 1;
                    match error_mode {
                        ErrorMode::Panic => panic!("stride_access_test: memory error at stride {} index {}", stride, i),
                        ErrorMode::Halt => break,
                        ErrorMode::Log => {
                            log::error!(
                                "stride_access_test: memory error at stride {} index {} - expected {:#x}, got {:#x}",
                                stride,
                                i,
                                expected,
                                actual
                            );
                        }
                    }
                }
                i += stride;
            }
        }
    }

    let elapsed = start.elapsed().as_millis();
    TestStats {
        name: "StrideAccess",
        action: TestAction::ReadWrite,
        bytes_processed: size * strides.len(),
        elapsed_ms: elapsed,
        thread_id,
        error_count,
    }
}

unsafe fn bandwidth_saturation_test(ptr: *mut u8, size: usize, thread_id: usize, _error_mode: ErrorMode) -> TestStats {
    let start = Instant::now();
    let base = ptr as *mut u64;
    let len = size / std::mem::size_of::<u64>();

    // Pure memory bandwidth test - large sequential writes
    let pattern = 0x5555AAAA5555AAAAu64.wrapping_add(thread_id as u64);

    // Write phase
    for i in 0..len {
        base.add(i).write(pattern.wrapping_add(i as u64));
    }

    std::sync::atomic::fence(Ordering::SeqCst);

    // Read phase
    let mut checksum = 0u64;
    for i in 0..len {
        checksum = checksum.wrapping_add(base.add(i).read());
    }

    // Prevent optimization
    std::ptr::write_volatile(&mut checksum, checksum);

    let elapsed = start.elapsed().as_millis();
    TestStats {
        name: "BandwidthSat",
        action: TestAction::ReadWrite,
        bytes_processed: size * 2, // Read + Write
        elapsed_ms: elapsed,
        thread_id,
        error_count: 0, // This test doesn't verify individual values
    }
}
