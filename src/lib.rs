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

pub fn check_large_page_privilege() -> Result<(), &'static str> {
    memory::check_large_page_privilege()
}

#[derive(Debug, Clone)]
pub enum MemoryStrategy {
    SingleBlock {
        memory_spec: MemorySpec,
        test_chunk_size_mb: Option<u32>, // None = no chunking, Some(size) = TM5-style chunking
    },
    MultiBlock {
        blocks_per_thread: u32,
        min_block_size_mb: u32,
    },
}

#[derive(Debug, Clone)]
pub enum MemorySpec {
    FixedWindow { 
        size_mb: u32, 
        reserved_mb: u32 
    }, // TM5-style: fixed testing window
    PercentReserve(f64),     // Modern: percentage of system memory to reserve
    AbsoluteReserve(f64),    // Modern: absolute GiB to reserve
}

impl MemoryStrategy {
    pub fn calculate_usable_memory(&self, total_memory_bytes: usize) -> usize {
        match self {
            MemoryStrategy::SingleBlock { memory_spec, .. } => {
                match memory_spec {
                    MemorySpec::FixedWindow { size_mb, .. } => {
                        (*size_mb as usize) * 1024 * 1024
                    }
                    MemorySpec::PercentReserve(percent) => {
                        let reserve_bytes = (total_memory_bytes as f64 * percent / 100.0) as usize;
                        total_memory_bytes.saturating_sub(reserve_bytes)
                    }
                    MemorySpec::AbsoluteReserve(gib) => {
                        let reserve_bytes = (gib * 1024.0 * 1024.0 * 1024.0) as usize;
                        total_memory_bytes.saturating_sub(reserve_bytes)
                    }
                }
            }
            MemoryStrategy::MultiBlock { .. } => {
                // For multi-block, we'll use a default percentage-based approach
                let reserve_bytes = (total_memory_bytes as f64 * DEFAULT_RESERVE_PERCENT / 100.0) as usize;
                total_memory_bytes.saturating_sub(reserve_bytes)
            }
        }
    }

    pub fn strategy_name(&self) -> &'static str {
        match self {
            MemoryStrategy::SingleBlock { test_chunk_size_mb, .. } => {
                if test_chunk_size_mb.is_some() {
                    "Single Block (TM5-Compatible)"
                } else {
                    "Single Block (Modern Optimal)"
                }
            }
            MemoryStrategy::MultiBlock { .. } => "Multi Block (Legacy)",
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

pub struct MemoryLayout {
    pub total_memory: usize,
    pub usable_memory: usize,
    pub reserved_memory: usize,
    pub blocks: Vec<BlockInfo>,
    pub strategy: MemoryStrategy,
}

#[derive(Debug, Clone)]
pub struct BlockInfo {
    pub size_bytes: usize,
    pub thread_id: usize,
    pub test_chunk_size: Option<usize>, // For TM5-compatible chunking within the block
}

impl MemoryLayout {
    pub fn calculate(strategy: MemoryStrategy, thread_count: usize) -> Self {
        let total_memory = memory::get_total_system_memory();
        let usable_memory = strategy.calculate_usable_memory(total_memory);
        let reserved_memory = total_memory - usable_memory;

        let blocks = Self::distribute_memory_blocks(&strategy, usable_memory, thread_count);

        MemoryLayout {
            total_memory,
            usable_memory,
            reserved_memory,
            blocks,
            strategy,
        }
    }

    fn distribute_memory_blocks(strategy: &MemoryStrategy, usable_memory: usize, thread_count: usize) -> Vec<BlockInfo> {
        match strategy {
            MemoryStrategy::SingleBlock { test_chunk_size_mb, .. } => {
                // Single large block per thread (TM5-style or Modern)
                let memory_per_thread = usable_memory / thread_count;
                let test_chunk_size = test_chunk_size_mb.map(|mb| mb as usize * 1024 * 1024);

                (0..thread_count)
                    .map(|thread_id| BlockInfo {
                        size_bytes: memory_per_thread,
                        thread_id,
                        test_chunk_size,
                    })
                    .collect()
            }
            MemoryStrategy::MultiBlock { blocks_per_thread, min_block_size_mb } => {
                // Multiple blocks per thread (legacy approach)
                let min_block_bytes = *min_block_size_mb as usize * 1024 * 1024;
                let memory_per_thread = usable_memory / thread_count;
                let actual_blocks_per_thread = (*blocks_per_thread as usize).max(1);
                let block_size = (memory_per_thread / actual_blocks_per_thread).max(min_block_bytes);

                let mut blocks = Vec::new();
                for thread_id in 0..thread_count {
                    for _ in 0..actual_blocks_per_thread {
                        blocks.push(BlockInfo {
                            size_bytes: block_size,
                            thread_id,
                            test_chunk_size: None,
                        });
                    }
                }
                blocks
            }
        }
    }

    pub fn print_layout(&self) {
        log::info!("Memory Layout:");
        log::info!(
            "  Total System Memory: {:.2} GiB",
            self.total_memory as f64 / (1024.0 * 1024.0 * 1024.0)
        );
        log::info!(
            "  Reserved Memory: {:.2} GiB",
            self.reserved_memory as f64 / (1024.0 * 1024.0 * 1024.0)
        );
        log::info!("  Usable Memory: {:.2} GiB", self.usable_memory as f64 / (1024.0 * 1024.0 * 1024.0));
        log::info!("  Strategy: {}", self.strategy.strategy_name());
        log::info!("  Total Blocks: {}", self.blocks.len());

        let mut thread_blocks: HashMap<usize, Vec<&BlockInfo>> = HashMap::new();
        for block in &self.blocks {
            thread_blocks.entry(block.thread_id).or_default().push(block);
        }

        for (thread_id, blocks) in &thread_blocks {
            let total_size: usize = blocks.iter().map(|b| b.size_bytes).sum();
            let total_gib = total_size as f64 / (1024.0 * 1024.0 * 1024.0);
            log::info!("  Thread {}: {} blocks, {:.2} GiB total", thread_id, blocks.len(), total_gib);
            
            // Show test chunk size if applicable
            if let Some(test_chunk_size) = blocks[0].test_chunk_size {
                let test_chunk_mb = test_chunk_size / (1024 * 1024);
                log::info!("    Test chunk size: {} MB (TM5-compatible mode)", test_chunk_mb);
            }
        }
    }
}

// Global progress tracking
pub struct ProgressTracker {
    pub total_tests: AtomicU64,
    pub completed_tests: AtomicU64,
    pub total_errors: AtomicU64,
    pub current_phase: Mutex<String>,
    pub current_throughput: AtomicU64, // Store as bytes/sec * 1000 for precision
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

        // Calculate throughput (bytes/sec * 1000)
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

    // Pre-allocate all memory blocks before starting tests
    progress.set_phase("Allocating Memory");
    println!("Pre-allocating memory blocks...");

    let mut allocated_blocks = match allocate_all_blocks(&thread_blocks) {
        Ok(blocks) => blocks,
        Err(e) => {
            println!("❌ Failed to allocate memory blocks: {}", e);
            return false;
        }
    };

    // Print allocation summary
    print_allocation_summary(&allocated_blocks, &layout.strategy);

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

    // Signal progress reporter to stop
    progress.set_phase("Completed");
    thread::sleep(std::time::Duration::from_millis(100)); // Give reporter time to show final status

    if let Err(_) = progress_handle.join() {
        log::warn!("Progress reporter thread failed to join cleanly");
    }

    // Memory blocks will be automatically freed when they go out of scope
    log::info!("Releasing all allocated memory blocks");

    success.load(Ordering::Relaxed)
}

fn progress_reporter(progress: Arc<ProgressTracker>) {
    let mut last_update = Instant::now();

    loop {
        thread::sleep(std::time::Duration::from_millis(1000)); // Check every second

        let (completed, total, errors, phase, throughput) = progress.get_status();

        // Update every 5 seconds or when phase changes to "Completed"
        if last_update.elapsed().as_secs() >= 5 || phase == "Completed" {
            let progress_pct = if total > 0 { (completed * 100) / total } else { 0 };

            print!("\r\x1b[K"); // Clear line
            print!(
                "Progress: {}/{} ({}%) | Errors: {} | Phase: {} | Speed: {:.2} GiB/s",
                completed, total, progress_pct, errors, phase, throughput
            );

            if phase == "Completed" {
                println!(); // New line at completion
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

            // Check if we got large pages
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
        "Memory allocation completed: {} blocks, {:.2} GiB total, {} using large pages",
        total_blocks,
        total_allocated as f64 / (1024.0 * 1024.0 * 1024.0),
        large_page_blocks
    );

    Ok(allocated_blocks)
}

fn print_allocation_summary(allocated_blocks: &HashMap<usize, Vec<AllocatedBlock>>, strategy: &MemoryStrategy) {
    let mut total_blocks = 0;
    let mut total_size = 0usize;
    let mut large_page_blocks = 0;
    let mut blocks_per_thread = 0;
    let mut size_per_thread = 0usize;

    for (thread_id, blocks) in allocated_blocks {
        let thread_total_size: usize = blocks.iter().map(|b| b.buffer.size()).sum();
        let thread_large_pages = blocks.iter().filter(|b| b.buffer.uses_large_pages()).count();

        total_blocks += blocks.len();
        total_size += thread_total_size;
        large_page_blocks += thread_large_pages;

        if *thread_id == 0 {
            // Use first thread as template
            blocks_per_thread = blocks.len();
            size_per_thread = thread_total_size;
        }
    }

    let thread_count = allocated_blocks.len();
    let size_per_block_gib = if total_blocks > 0 {
        (size_per_thread / blocks_per_thread) as f64 / (1024.0 * 1024.0 * 1024.0)
    } else {
        0.0
    };

    println!("Memory Allocation Summary:");
    println!(
        "  {} blocks of {:.2} GiB per thread, {} threads ({})",
        blocks_per_thread, size_per_block_gib, thread_count, strategy.strategy_name()
    );
    println!(
        "  Total: {} blocks, {:.2} GiB",
        total_blocks,
        total_size as f64 / (1024.0 * 1024.0 * 1024.0)
    );

    if large_page_blocks > 0 {
        println!("  ✅ {} blocks using large pages (2MB pages)", large_page_blocks);
    } else {
        println!("  ⚠️  Using standard 4KB pages (large pages not available)");
    }
    println!();
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

    // Use non-temporal loads/stores to bypass cache
    for i in 0..(len / 2) {
        let val = _mm_load_si128(base.add(i));
        // Non-temporal store bypasses cache
        _mm_stream_si128(base.add(len - 1 - i), val);
    }

    // Ensure all stores complete
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

    // Define test functions in order
    let test_functions: Vec<(&str, unsafe fn(*mut u8, usize, usize, ErrorMode) -> TestStats)> = vec![
        ("MirrorMove128NonTemporal", mirror_move_128_non_temporal),
        ("MirrorMove256NonTemporal", mirror_move_256_non_temporal),
        ("MirrorMove512NonTemporal", mirror_move_512_non_temporal),
        ("SimpleTest", simple_test),
        ("RefreshStable", refresh_stable),
        ("CacheBusting", cache_busting_write_test),
        ("RandomTorture", random_access_torture_test),
        ("StrideAccess", stride_access_test),
        ("BandwidthSat", bandwidth_saturation_test),
    ];

    // Run each test type across all blocks before moving to next test type
    for (test_name, test_func) in test_functions {
        progress.set_phase(test_name);

        // Wait for all threads to reach this test phase
        barrier.wait();

        for (block_idx, allocated_block) in allocated_blocks.iter().enumerate() {
            let ptr = allocated_block.buffer.as_mut_ptr();
            let size = allocated_block.buffer.size();
            let size_gib = size as f64 / (1024.0 * 1024.0 * 1024.0);

            log::debug!(
                "[Thread {}] Block {}/{} - {:.2} GiB - Running {}",
                thread_id,
                block_idx + 1,
                allocated_blocks.len(),
                size_gib,
                test_name
            );

            // Handle test chunk processing for TM5-compatible mode
            if let Some(test_chunk_size) = allocated_block.block_info.test_chunk_size {
                // Run test in chunks as per TM5 behavior
                let chunk_count = size / test_chunk_size;
                log::debug!("[Thread {}] Running {} in {} chunks of {} MB", 
                    thread_id, test_name, chunk_count, test_chunk_size / (1024 * 1024));
                
                for chunk_idx in 0..chunk_count {
                    let chunk_ptr = unsafe { ptr.add(chunk_idx * test_chunk_size) };
                    let stats = unsafe { test_func(chunk_ptr, test_chunk_size, thread_id, error_mode) };
                    log_stats(&stats);
                    progress.complete_test(&stats);

                    if stats.error_count > 0 {
                        handle_test_errors(&stats, error_mode, test_name)?;
                    }
                }
            } else {
                // Run test on full block (Modern Optimal mode)
                let stats = unsafe { test_func(ptr, size, thread_id, error_mode) };
                log_stats(&stats);
                progress.complete_test(&stats);

                if stats.error_count > 0 {
                    handle_test_errors(&stats, error_mode, test_name)?;
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

// Memory testing implementations (same as before)

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