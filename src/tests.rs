use crate::ErrorMode;
use crate::cache::{CacheInfo, SystemInfo};
use crate::layout::{WindowMode, BlockMode};
use std::arch::x86_64::*;
use std::sync::atomic::Ordering;
use std::time::Instant;
use std::sync::OnceLock;

const CACHE_BUSTING_STRIDE: usize = 4096;

// Global system info that gets detected once at runtime
static SYSTEM_INFO: OnceLock<SystemInfo> = OnceLock::new();

pub fn get_system_info() -> &'static SystemInfo {
    SYSTEM_INFO.get_or_init(|| {
        let system_info = SystemInfo::detect();
        system_info.print_system_info();
        system_info
    })
}

pub fn get_cache_info() -> &'static CacheInfo {
    get_system_info().get_cache_info()
}

#[repr(C)]
#[derive(Clone, Copy)]
pub enum TestAction {
    Read,
    Write,
    ReadWrite,
    WriteVerify,
    Copy,
    Verify,
    WriteWaitVerify,
    CacheBusting,
    RandomAccess,
    StuckBitTest,
}

impl TestAction {
    pub fn label(&self) -> &'static str {
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
            TestAction::StuckBitTest => "Stuck Bit Test",
        }
    }
}

#[repr(C)]
pub struct TestStats {
    pub name: &'static str,
    pub action: TestAction,
    pub bytes_processed: usize,
    pub elapsed_ms: u128,
    pub thread_id: usize,
    pub error_count: u64,
}

// Test execution timing configuration
#[derive(Debug, Clone)]
pub struct TestTiming {
    pub cycles: Option<u32>,           // Number of times to repeat the test (None = unlimited)
    pub duration_secs: Option<u32>,    // Maximum time to run test (None = unlimited)
    pub min_duration_secs: Option<u32>, // Minimum time even if cycles complete
}

impl Default for TestTiming {
    fn default() -> Self {
        Self {
            cycles: Some(1),
            duration_secs: None,
            min_duration_secs: None,
        }
    }
}

impl TestTiming {
    pub fn cycles_only(cycles: u32) -> Self {
        Self {
            cycles: Some(cycles),
            duration_secs: None,
            min_duration_secs: None,
        }
    }
    
    pub fn duration_only(duration_secs: u32) -> Self {
        Self {
            cycles: None,
            duration_secs: Some(duration_secs),
            min_duration_secs: None,
        }
    }
    
    pub fn hybrid(cycles: u32, max_duration_secs: u32) -> Self {
        Self {
            cycles: Some(cycles),
            duration_secs: Some(max_duration_secs),
            min_duration_secs: None,
        }
    }
    
    pub fn should_continue(&self, current_cycle: u32, elapsed_secs: u32) -> bool {
        // Check minimum duration first
        if let Some(min_secs) = self.min_duration_secs {
            if elapsed_secs < min_secs {
                return true;
            }
        }
        
        // Check maximum duration
        if let Some(max_secs) = self.duration_secs {
            if elapsed_secs >= max_secs {
                return false;
            }
        }
        
        // Check cycle limit
        if let Some(max_cycles) = self.cycles {
            if current_cycle >= max_cycles {
                return false;
            }
        }
        
        // If no limits set, run forever (or until externally stopped)
        true
    }
}

// Three-stage memory configuration with corrected window logic
#[derive(Debug, Clone)]
pub struct TestMemoryConfig {
    pub window_mode: WindowMode,
    pub block_mode: BlockMode,
    pub allow_misaligned: bool,
    pub requires_locality: bool,    // True if test needs temporal locality (small window)
    pub timing: TestTiming,
    pub streams: u32,               // Number of access streams (equivalent to TM5 jump/parameter)
    pub pattern_mode: Option<u32>,  // TM5 pattern mode
    pub pattern_param0: Option<u64>, // TM5 pattern parameter 0
    pub pattern_param1: Option<u64>, // TM5 pattern parameter 1
}

impl TestMemoryConfig {
    pub fn new(window_mode: WindowMode, block_mode: BlockMode, allow_misaligned: bool, requires_locality: bool) -> Self {
        Self {
            window_mode,
            block_mode,
            allow_misaligned,
            requires_locality,
            timing: TestTiming::default(),
            streams: 1, // Default to single stream (equivalent to TM5 jump=1)
            pattern_mode: None,
            pattern_param0: None,
            pattern_param1: None,
        }
    }
    
    pub fn with_timing(mut self, timing: TestTiming) -> Self {
        self.timing = timing;
        self
    }
    
    pub fn with_streams(mut self, streams: u32) -> Self {
        self.streams = streams;
        self
    }
    
    pub fn with_pattern_config(mut self, mode: Option<u32>, param0: Option<u64>, param1: Option<u64>) -> Self {
        self.pattern_mode = mode;
        self.pattern_param0 = param0;
        self.pattern_param1 = param1;
        self
    }

    // Calculate window size with corrected logic
    pub fn calculate_window_size(&self, test_name: &str, allocated_size: usize) -> usize {
        match &self.window_mode {
            WindowMode::FullAllocation => {
                // Use full allocation unless test specifically requires locality
                if self.requires_locality {
                    self.calculate_locality_window_size(test_name, allocated_size)
                } else {
                    allocated_size
                }
            }
            WindowMode::FixedSize { size_mb } => {
                let fixed_size = (*size_mb as usize) * 1024 * 1024;
                fixed_size.min(allocated_size)
            }
            WindowMode::CacheRelative { multiplier } => {
                let cache_info = get_cache_info();
                let cache_based_size = (cache_info.total_cache as f64 * multiplier) as usize;
                cache_based_size.min(allocated_size)
            }
        }
    }
    
    // Calculate locality-specific window for tests that need it
    fn calculate_locality_window_size(&self, test_name: &str, allocated_size: usize) -> usize {
        let cache_info = get_cache_info();
        
        let optimal_size = match test_name {
            "CacheBusting" => (cache_info.l3_cache / 2).max(cache_info.l2_cache * 4),
            "RefreshStable" => cache_info.l2_cache * 2, // Small window for refresh testing
            _ => cache_info.total_cache * 2,
        };
        
        optimal_size.min(allocated_size)
    }

    // Calculate optimal block size with alignment
    pub fn calculate_block_size(&self, test_name: &str, window_size: usize) -> (usize, bool) {
        let cache_info = get_cache_info();
        
        let optimal_block_size = match &self.block_mode {
            BlockMode::FixedSize { size_mb } => {
                let fixed_size = (*size_mb as usize) * 1024 * 1024;
                if self.allow_misaligned {
                    fixed_size
                } else {
                    align_to_boundary(fixed_size, cache_info.cache_line_size)
                }
            }
            BlockMode::WindowFraction { fraction } => {
                let fraction_size = (window_size as f64 * fraction) as usize;
                if self.allow_misaligned {
                    fraction_size
                } else {
                    align_to_boundary(fraction_size, cache_info.cache_line_size)
                }
            }
            BlockMode::AutoOptimal => {
                self.calculate_optimal_block_for_test(test_name, window_size, cache_info)
            }
        };

        let was_adjusted = match &self.block_mode {
            BlockMode::FixedSize { size_mb } => {
                let original = (*size_mb as usize) * 1024 * 1024;
                optimal_block_size != original
            }
            _ => false, // Auto-calculated, so not "adjusted"
        };
        
        (optimal_block_size.min(window_size), was_adjusted)
    }
    
    fn calculate_optimal_block_for_test(&self, test_name: &str, window_size: usize, cache_info: &CacheInfo) -> usize {
        match test_name {
            // SIMD tests need specific alignments
            "MirrorMove128NonTemporal" => {
                align_to_boundary(16 * 1024 * 1024, 128 / 8) // 128-bit alignment
            }
            "MirrorMove256NonTemporal" => {
                align_to_boundary(32 * 1024 * 1024, 256 / 8) // 256-bit alignment  
            }
            "MirrorMove512NonTemporal" => {
                align_to_boundary(64 * 1024 * 1024, 512 / 8) // 512-bit alignment
            }
            
            // Full memory tests should use large blocks for efficiency
            "StuckBitTest" | "FullMemoryPattern" => {
                align_to_boundary(window_size / 16, cache_info.cache_line_size).max(1024 * 1024)
            }
            
            // Cache tests use cache-line aligned blocks
            "CacheBusting" => {
                align_to_boundary(1 * 1024 * 1024, cache_info.cache_line_size)
            }
            
            // Small blocks for refresh testing
            "RefreshStable" => {
                align_to_boundary(512 * 1024, cache_info.cache_line_size)
            }
            
            _ => {
                if self.allow_misaligned {
                    8 * 1024 * 1024 // 8MB default unaligned
                } else {
                    align_to_boundary(8 * 1024 * 1024, cache_info.cache_line_size)
                }
            }
        }
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

// === NEW: Full Memory Stuck Bit Test ===
pub unsafe fn stuck_bit_test(ptr: *mut u8, size: usize, thread_id: usize, error_mode: ErrorMode, timing: &TestTiming) -> TestStats {
    let test_name = "StuckBitTest";
    let start = Instant::now();
    let len = size / std::mem::size_of::<u64>();
    let mut total_error_count = 0u64;
    let mut total_bytes_processed = 0usize;
    let base = ptr as *mut u64;

    log::info!("[Thread {}] Running {} on {:.2} MB of memory", 
              thread_id, test_name, size as f64 / (1024.0 * 1024.0));

    let mut cycle = 0u32;
    let test_start = Instant::now();
    
    loop {
        cycle += 1;
        let mut cycle_errors = 0u64;
        
        // Phase 1: Write 0xAAAAAAAAAAAAAAAA (10101010...)
        let pattern1 = 0xAAAAAAAAAAAAAAAAu64;
        for i in 0..len {
            base.add(i).write(pattern1);
        }

        std::sync::atomic::fence(Ordering::SeqCst);

        // Phase 1 Verify
        for i in 0..len {
            let v = base.add(i).read();
            if v != pattern1 {
                cycle_errors += 1;
                match error_mode {
                    ErrorMode::Panic => panic!("stuck_bit_test: Phase 1 error at index {} - expected {:#x}, got {:#x}", i, pattern1, v),
                    ErrorMode::Halt => break,
                    ErrorMode::Log => {
                        log::error!("stuck_bit_test: Phase 1 error at index {} - expected {:#x}, got {:#x}", i, pattern1, v);
                    }
                }
            }
        }

        // Phase 2: Write 0x5555555555555555 (01010101...)
        let pattern2 = 0x5555555555555555u64;
        for i in 0..len {
            base.add(i).write(pattern2);
        }

        std::sync::atomic::fence(Ordering::SeqCst);

        // Phase 2 Verify
        for i in 0..len {
            let v = base.add(i).read();
            if v != pattern2 {
                cycle_errors += 1;
                match error_mode {
                    ErrorMode::Panic => panic!("stuck_bit_test: Phase 2 error at index {} - expected {:#x}, got {:#x}", i, pattern2, v),
                    ErrorMode::Halt => break,
                    ErrorMode::Log => {
                        log::error!("stuck_bit_test: Phase 2 error at index {} - expected {:#x}, got {:#x}", i, pattern2, v);
                    }
                }
            }
        }

        // Phase 3: Write back to 0xAAAAAAAAAAAAAAAA
        for i in 0..len {
            base.add(i).write(pattern1);
        }

        std::sync::atomic::fence(Ordering::SeqCst);

        // Phase 3 Verify
        for i in 0..len {
            let v = base.add(i).read();
            if v != pattern1 {
                cycle_errors += 1;
                match error_mode {
                    ErrorMode::Panic => panic!("stuck_bit_test: Phase 3 error at index {} - expected {:#x}, got {:#x}", i, pattern1, v),
                    ErrorMode::Halt => break,
                    ErrorMode::Log => {
                        log::error!("stuck_bit_test: Phase 3 error at index {} - expected {:#x}, got {:#x}", i, pattern1, v);
                    }
                }
            }
        }
        
        total_error_count += cycle_errors;
        total_bytes_processed += size * 6; // 3 writes + 3 reads
        
        // Check if should continue based on timing
        let elapsed_secs = test_start.elapsed().as_secs() as u32;
        if !timing.should_continue(cycle, elapsed_secs) {
            break;
        }
    }

    let elapsed = start.elapsed().as_millis();
    TestStats {
        name: "StuckBitTest",
        action: TestAction::StuckBitTest,
        bytes_processed: total_bytes_processed,
        elapsed_ms: elapsed,
        thread_id,
        error_count: total_error_count,
    }
}

// === UPDATED TEST FUNCTIONS WITH STREAM SUPPORT ===

pub unsafe fn mirror_move_128_non_temporal(ptr: *mut u8, size: usize, thread_id: usize, error_mode: ErrorMode, timing: &TestTiming, streams: u32) -> TestStats {
    let test_name = "MirrorMove128NonTemporal";
    let start = Instant::now();
    
    if !is_x86_feature_detected!("sse2") {
        return TestStats {
            name: test_name,
            action: TestAction::ReadWrite,
            bytes_processed: 0,
            elapsed_ms: 0,
            thread_id,
            error_count: 0,
        };
    }

    let mut total_bytes_processed = 0usize;
    let mut cycle = 0u32;
    let test_start = Instant::now();
    
    loop {
        cycle += 1;
        
        // Execute pattern based on number of streams (equivalent to TM5 jump parameter)
        match streams {
            2 => {
                // Two-stream pattern: split memory in half
                let half_size = size / 2;
                let base = ptr as *mut __m128i;
                let len = half_size / std::mem::size_of::<__m128i>();
                
                // Stream 1: First half
                for i in 0..(len / 2) {
                    let val = _mm_load_si128(base.add(i));
                    _mm_stream_si128(base.add(len - 1 - i), val);
                }
                
                // Stream 2: Second half
                let second_half_base = base.add(size / (2 * std::mem::size_of::<__m128i>()));
                for i in 0..(len / 2) {
                    let val = _mm_load_si128(second_half_base.add(i));
                    _mm_stream_si128(second_half_base.add(len - 1 - i), val);
                }
            }
            4 => {
                // Four-stream pattern: split memory into quarters
                let quarter_size = size / 4;
                let base = ptr as *mut __m128i;
                let len = quarter_size / std::mem::size_of::<__m128i>();
                
                for stream in 0..4 {
                    let stream_base = base.add(stream * quarter_size / std::mem::size_of::<__m128i>());
                    for i in 0..(len / 2) {
                        let val = _mm_load_si128(stream_base.add(i));
                        _mm_stream_si128(stream_base.add(len - 1 - i), val);
                    }
                }
            }
            _ => {
                // Default single stream pattern (streams=1 or any other value)
                let len = size / std::mem::size_of::<__m128i>();
                let base = ptr as *mut __m128i;
                
                for i in 0..(len / 2) {
                    let val = _mm_load_si128(base.add(i));
                    _mm_stream_si128(base.add(len - 1 - i), val);
                }
            }
        }

        _mm_sfence();
        
        total_bytes_processed += size;
        
        // Check if should continue based on timing
        let elapsed_secs = test_start.elapsed().as_secs() as u32;
        if !timing.should_continue(cycle, elapsed_secs) {
            break;
        }
    }

    let elapsed = start.elapsed().as_millis();
    TestStats {
        name: test_name,
        action: TestAction::ReadWrite,
        bytes_processed: total_bytes_processed,
        elapsed_ms: elapsed,
        thread_id,
        error_count: 0,
    }
}

pub unsafe fn mirror_move_256_non_temporal(ptr: *mut u8, size: usize, thread_id: usize, error_mode: ErrorMode, timing: &TestTiming, streams: u32) -> TestStats {
    let test_name = "MirrorMove256NonTemporal";
    let start = Instant::now();
    
    if !is_x86_feature_detected!("avx2") {
        return TestStats {
            name: test_name,
            action: TestAction::ReadWrite,
            bytes_processed: 0,
            elapsed_ms: 0,
            thread_id,
            error_count: 0,
        };
    }

    let mut total_bytes_processed = 0usize;
    let mut cycle = 0u32;
    let test_start = Instant::now();
    
    loop {
        cycle += 1;
        
        // Execute pattern based on number of streams
        match streams {
            2 => {
                // Two-stream pattern
                let half_size = size / 2;
                let base = ptr as *mut __m256i;
                let len = half_size / std::mem::size_of::<__m256i>();
                
                // Stream 1: First half
                for i in 0..(len / 2) {
                    let val = _mm256_load_si256(base.add(i));
                    _mm256_stream_si256(base.add(len - 1 - i), val);
                }
                
                // Stream 2: Second half
                let second_half_base = base.add(size / (2 * std::mem::size_of::<__m256i>()));
                for i in 0..(len / 2) {
                    let val = _mm256_load_si256(second_half_base.add(i));
                    _mm256_stream_si256(second_half_base.add(len - 1 - i), val);
                }
            }
            4 => {
                // Four-stream pattern
                let quarter_size = size / 4;
                let base = ptr as *mut __m256i;
                let len = quarter_size / std::mem::size_of::<__m256i>();
                
                for stream in 0..4 {
                    let stream_base = base.add(stream * quarter_size / std::mem::size_of::<__m256i>());
                    for i in 0..(len / 2) {
                        let val = _mm256_load_si256(stream_base.add(i));
                        _mm256_stream_si256(stream_base.add(len - 1 - i), val);
                    }
                }
            }
            _ => {
                // Default single stream pattern
                let len = size / std::mem::size_of::<__m256i>();
                let base = ptr as *mut __m256i;
                
                for i in 0..(len / 2) {
                    let val = _mm256_load_si256(base.add(i));
                    _mm256_stream_si256(base.add(len - 1 - i), val);
                }
            }
        }

        _mm_sfence();
        
        total_bytes_processed += size;
        
        let elapsed_secs = test_start.elapsed().as_secs() as u32;
        if !timing.should_continue(cycle, elapsed_secs) {
            break;
        }
    }

    let elapsed = start.elapsed().as_millis();
    TestStats {
        name: test_name,
        action: TestAction::ReadWrite,
        bytes_processed: total_bytes_processed,
        elapsed_ms: elapsed,
        thread_id,
        error_count: 0,
    }
}

pub unsafe fn mirror_move_512_non_temporal(ptr: *mut u8, size: usize, thread_id: usize, error_mode: ErrorMode, timing: &TestTiming, streams: u32) -> TestStats {
    let test_name = "MirrorMove512NonTemporal";
    let start = Instant::now();
    
    if !is_x86_feature_detected!("avx512f") {
        return TestStats {
            name: test_name,
            action: TestAction::ReadWrite,
            bytes_processed: 0,
            elapsed_ms: 0,
            thread_id,
            error_count: 0,
        };
    }

    let mut total_bytes_processed = 0usize;
    let mut cycle = 0u32;
    let test_start = Instant::now();
    
    loop {
        cycle += 1;
        
        // Execute pattern based on number of streams
        match streams {
            2 => {
                // Two-stream pattern
                let half_size = size / 2;
                let base = ptr as *mut __m512i;
                let len = half_size / std::mem::size_of::<__m512i>();
                
                // Stream 1: First half
                for i in 0..(len / 2) {
                    let val = _mm512_load_si512(base.add(i));
                    _mm512_stream_si512(base.add(len - 1 - i), val);
                }
                
                // Stream 2: Second half
                let second_half_base = base.add(size / (2 * std::mem::size_of::<__m512i>()));
                for i in 0..(len / 2) {
                    let val = _mm512_load_si512(second_half_base.add(i));
                    _mm512_stream_si512(second_half_base.add(len - 1 - i), val);
                }
            }
            4 => {
                // Four-stream pattern
                let quarter_size = size / 4;
                let base = ptr as *mut __m512i;
                let len = quarter_size / std::mem::size_of::<__m512i>();
                
                for stream in 0..4 {
                    let stream_base = base.add(stream * quarter_size / std::mem::size_of::<__m512i>());
                    for i in 0..(len / 2) {
                        let val = _mm512_load_si512(stream_base.add(i));
                        _mm512_stream_si512(stream_base.add(len - 1 - i), val);
                    }
                }
            }
            _ => {
                // Default single stream pattern
                let len = size / std::mem::size_of::<__m512i>();
                let base = ptr as *mut __m512i;
                
                for i in 0..(len / 2) {
                    let val = _mm512_load_si512(base.add(i));
                    _mm512_stream_si512(base.add(len - 1 - i), val);
                }
            }
        }

        _mm_sfence();
        
        total_bytes_processed += size;
        
        let elapsed_secs = test_start.elapsed().as_secs() as u32;
        if !timing.should_continue(cycle, elapsed_secs) {
            break;
        }
    }

    let elapsed = start.elapsed().as_millis();
    TestStats {
        name: test_name,
        action: TestAction::ReadWrite,
        bytes_processed: total_bytes_processed,
        elapsed_ms: elapsed,
        thread_id,
        error_count: 0,
    }
}

pub unsafe fn simple_test(ptr: *mut u8, size: usize, thread_id: usize, error_mode: ErrorMode, timing: &TestTiming, config: &TestMemoryConfig) -> TestStats {
    let test_name = "Simple Test";
    let start = Instant::now();
    let base = ptr as *mut u64;
    let len = size / std::mem::size_of::<u64>();
    let mut total_error_count = 0u64;
    let mut total_bytes_processed = 0usize;
    
    // Use pattern parameters if provided (TM5 compatibility)
    let pattern_base = if let (Some(mode), Some(param0), Some(param1)) = (config.pattern_mode, config.pattern_param0, config.pattern_param1) {
        match mode {
            1 => param0 ^ param1, // Pattern mode 1
            2 => (param0 << 32) | (param1 & 0xFFFFFFFF), // Pattern mode 2
            _ => 0xDEADBEEFDEADBEEF, // Default pattern
        }
    } else {
        0xDEADBEEFDEADBEEF
    };
    
    let mut cycle = 0u32;
    let test_start = Instant::now();
    
    loop {
        cycle += 1;
        let mut cycle_errors = 0u64;
        
        // Apply stream pattern
        match config.streams {
            1 => {
                // Standard single stream pattern
                // Write phase
                for i in 0..len {
                    base.add(i).write(i as u64 ^ pattern_base);
                }

                std::sync::atomic::fence(Ordering::SeqCst);

                // Verify phase
                for i in 0..len {
                    let v = base.add(i).read();
                    let expected = i as u64 ^ pattern_base;
                    if v != expected {
                        cycle_errors += 1;
                        match error_mode {
                            ErrorMode::Panic => panic!("{}: memory error at index {}", test_name, i),
                            ErrorMode::Halt => break,
                            ErrorMode::Log => {
                                log::error!("{}: memory error at index {} - expected {:#x}, got {:#x}", test_name, i, expected, v);
                            }
                        }
                    }
                }
            }
            2 => {
                // Two-stream pattern: alternate between two regions
                let half = len / 2;
                
                // Write both halves with different patterns
                for i in 0..half {
                    base.add(i).write(i as u64 ^ pattern_base);
                    base.add(half + i).write((half + i) as u64 ^ !pattern_base);
                }
                
                std::sync::atomic::fence(Ordering::SeqCst);
                
                // Verify both halves
                for i in 0..half {
                    let v1 = base.add(i).read();
                    let v2 = base.add(half + i).read();
                    let expected1 = i as u64 ^ pattern_base;
                    let expected2 = (half + i) as u64 ^ !pattern_base;
                    
                    if v1 != expected1 {
                        cycle_errors += 1;
                        handle_error(error_mode, test_name, i, expected1, v1);
                    }
                    if v2 != expected2 {
                        cycle_errors += 1;
                        handle_error(error_mode, test_name, half + i, expected2, v2);
                    }
                }
            }
            4 => {
                // Four-stream pattern: interleaved access
                let quarter = len / 4;
                
                // Write four regions with different patterns
                for i in 0..quarter {
                    base.add(i).write(i as u64 ^ pattern_base);
                    base.add(quarter + i).write((quarter + i) as u64 ^ (pattern_base.rotate_left(16)));
                    base.add(2 * quarter + i).write((2 * quarter + i) as u64 ^ (pattern_base.rotate_left(32)));
                    base.add(3 * quarter + i).write((3 * quarter + i) as u64 ^ (pattern_base.rotate_left(48)));
                }
                
                std::sync::atomic::fence(Ordering::SeqCst);
                
                // Verify four regions
                for i in 0..quarter {
                    for stream in 0..4 {
                        let idx = stream * quarter + i;
                        let v = base.add(idx).read();
                        let expected = idx as u64 ^ pattern_base.rotate_left(stream as u32 * 16);
                        
                        if v != expected {
                            cycle_errors += 1;
                            handle_error(error_mode, test_name, idx, expected, v);
                        }
                    }
                }
            }
            _ => {
                // For higher stream counts, use strided access
                let stride = len / config.streams as usize;
                
                // Write with multiple streams
                for stream in 0..config.streams as usize {
                    let pattern = pattern_base.rotate_left((stream * 8) as u32);
                    for i in 0..stride {
                        let idx = stream * stride + i;
                        if idx < len {
                            base.add(idx).write(idx as u64 ^ pattern);
                        }
                    }
                }
                
                std::sync::atomic::fence(Ordering::SeqCst);
                
                // Verify with multiple streams
                for stream in 0..config.streams as usize {
                    let pattern = pattern_base.rotate_left((stream * 8) as u32);
                    for i in 0..stride {
                        let idx = stream * stride + i;
                        if idx < len {
                            let v = base.add(idx).read();
                            let expected = idx as u64 ^ pattern;
                            if v != expected {
                                cycle_errors += 1;
                                handle_error(error_mode, test_name, idx, expected, v);
                            }
                        }
                    }
                }
            }
        }
        
        total_error_count += cycle_errors;
        total_bytes_processed += size * 2; // write + read
        
        let elapsed_secs = test_start.elapsed().as_secs() as u32;
        if !timing.should_continue(cycle, elapsed_secs) {
            break;
        }
    }

    let elapsed = start.elapsed().as_millis();
    TestStats {
        name: test_name,
        action: TestAction::WriteVerify,
        bytes_processed: total_bytes_processed,
        elapsed_ms: elapsed,
        thread_id,
        error_count: total_error_count,
    }
}

// Helper function for error handling
#[inline(always)]
unsafe fn handle_error(error_mode: ErrorMode, test_name: &str, index: usize, expected: u64, actual: u64) {
    match error_mode {
        ErrorMode::Panic => panic!("{}: memory error at index {} - expected {:#x}, got {:#x}", test_name, index, expected, actual),
        ErrorMode::Halt => {},
        ErrorMode::Log => {
            log::error!("{}: memory error at index {} - expected {:#x}, got {:#x}", test_name, index, expected, actual);
        }
    }
}

pub unsafe fn refresh_stable(ptr: *mut u8, size: usize, thread_id: usize, error_mode: ErrorMode, timing: &TestTiming) -> TestStats {
    let test_name = "Refresh Stable";
    let start = Instant::now();
    let base = ptr as *mut u64;
    let len = size / std::mem::size_of::<u64>();
    let mut total_error_count = 0u64;
    let mut total_bytes_processed = 0usize;
    
    let mut cycle = 0u32;
    let test_start = Instant::now();
    
    loop {
        cycle += 1;
        let mut cycle_errors = 0u64;
        
        for i in 0..len {
            base.add(i).write(0xA5A5A5A5A5A5A5A5);
        }

        std::sync::atomic::fence(Ordering::SeqCst);
        std::thread::sleep(std::time::Duration::from_millis(50));

        for i in 0..len {
            let v = base.add(i).read();
            if v != 0xA5A5A5A5A5A5A5A5 {
                cycle_errors += 1;
                match error_mode {
                    ErrorMode::Panic => panic!("{}: memory error at index {}", test_name, i),
                    ErrorMode::Halt => break,
                    ErrorMode::Log => {
                        log::error!(
                            "{}: memory error at index {} - expected {:#x}, got {:#x}",
                            test_name,
                            i,
                            0xA5A5A5A5A5A5A5A5u64,
                            v
                        );
                    }
                }
            }
        }
        
        total_error_count += cycle_errors;
        total_bytes_processed += size * 2;
        
        let elapsed_secs = test_start.elapsed().as_secs() as u32;
        if !timing.should_continue(cycle, elapsed_secs) {
            break;
        }
    }

    let elapsed = start.elapsed().as_millis();
    TestStats {
        name: test_name,
        action: TestAction::WriteWaitVerify,
        bytes_processed: total_bytes_processed,
        elapsed_ms: elapsed,
        thread_id,
        error_count: total_error_count,
    }
}

pub unsafe fn cache_busting_write_test(ptr: *mut u8, size: usize, thread_id: usize, error_mode: ErrorMode, timing: &TestTiming, streams: u32) -> TestStats {
    let test_name = "Cache Busting Write";
    let start = Instant::now();
    let base = ptr as *mut u64;
    let len = size / std::mem::size_of::<u64>();
    let mut total_error_count = 0u64;
    let mut total_bytes_processed = 0usize;
    
    let mut cycle = 0u32;
    let test_start = Instant::now();
    
    loop {
        cycle += 1;
        let mut cycle_errors = 0u64;
        
        // Apply stream-based access patterns
        match streams {
            1 => {
                // Single stream with large strides to bust cache
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
                            cycle_errors += 1;
                            handle_error(error_mode, test_name, i, expected, v);
                        }
                        i += stride;
                    }
                }
            }
            _ => {
                // Multiple streams with different stride offsets
                let base_stride = CACHE_BUSTING_STRIDE / std::mem::size_of::<u64>();
                let stream_offset = base_stride / streams as usize;
                
                for stream in 0..streams as usize {
                    let pattern = 0x0123456789ABCDEFu64
                        .wrapping_add(thread_id as u64)
                        .wrapping_add((stream as u64) * 0x1111111111111111u64);
                    
                    let mut i = stream * stream_offset;
                    while i < len {
                        base.add(i).write(pattern.wrapping_add(i as u64));
                        i += base_stride;
                    }
                }
                
                std::sync::atomic::fence(Ordering::SeqCst);
                
                // Verify all streams
                for stream in 0..streams as usize {
                    let pattern = 0x0123456789ABCDEFu64
                        .wrapping_add(thread_id as u64)
                        .wrapping_add((stream as u64) * 0x1111111111111111u64);
                    
                    let mut i = stream * stream_offset;
                    while i < len {
                        let v = base.add(i).read();
                        let expected = pattern.wrapping_add(i as u64);
                        if v != expected {
                            cycle_errors += 1;
                            handle_error(error_mode, test_name, i, expected, v);
                        }
                        i += base_stride;
                    }
                }
            }
        }
        
        total_error_count += cycle_errors;
        total_bytes_processed += size * 2;
        
        let elapsed_secs = test_start.elapsed().as_secs() as u32;
        if !timing.should_continue(cycle, elapsed_secs) {
            break;
        }
    }

    let elapsed = start.elapsed().as_millis();
    TestStats {
        name: "CacheBusting",
        action: TestAction::CacheBusting,
        bytes_processed: total_bytes_processed,
        elapsed_ms: elapsed,
        thread_id,
        error_count: total_error_count,
    }
}

pub unsafe fn random_access_torture_test(ptr: *mut u8, size: usize, thread_id: usize, error_mode: ErrorMode, timing: &TestTiming, streams: u32) -> TestStats {
    let test_name = "Random Access Torture";
    let start = Instant::now();
    let base = ptr as *mut u64;
    let len = size / std::mem::size_of::<u64>();
    let mut total_error_count = 0u64;
    let mut total_bytes_processed = 0usize;
    
    let mut cycle = 0u32;
    let test_start = Instant::now();
    
    // Initialize with known pattern once
    for i in 0..len {
        base.add(i).write(i as u64);
    }
    std::sync::atomic::fence(Ordering::SeqCst);
    total_bytes_processed = total_bytes_processed.saturating_add(size);
    
    loop {
        cycle += 1;
        let mut cycle_errors = 0u64;
        
        // Random access torture with configurable streams
        // Ensure we don't overflow by using checked arithmetic
        let base_iterations = (len / 1000).max(5000).min(50000);
        let iterations_per_stream = (base_iterations / streams.max(1) as usize).max(1); // Ensure at least 1 iteration
        
        //log::debug!("[Thread {}] RandomTorture: len={}, streams={}, iterations_per_stream={}", 
        //           thread_id, len, streams, iterations_per_stream);
        
        for stream in 0..streams {
            let mut rng_state = 0x123456789ABCDEFu64
                .wrapping_add(thread_id as u64)
                .wrapping_add(cycle as u64)
                .wrapping_add((stream as u64).wrapping_mul(0x8765432187654321u64));

            // Random read verification for this stream
            for iteration in 0..iterations_per_stream {
                rng_state ^= rng_state << 13;
                rng_state ^= rng_state >> 17;
                rng_state ^= rng_state << 5;

                let idx = (rng_state as usize) % len;
                let expected = idx as u64;
                let actual = base.add(idx).read();

                if actual != expected {
                    cycle_errors += 1;
                    match error_mode {
                        ErrorMode::Panic => panic!(
                            "{}: memory error at index {}, iteration {}, stream {}, expected {}, actual {}",
                            test_name, idx, iteration, stream, expected, actual
                        ),
                        ErrorMode::Halt => break,
                        ErrorMode::Log => {
                            log::error!(
                                "{}: memory error at index {}, iteration {}, stream {}, expected {}, actual {}",
                                test_name,
                                idx,
                                iteration,
                                stream,
                                expected,
                                actual
                            );
                        }
                    }
                }
            }
        }
        
        total_error_count += cycle_errors;
        // Use saturating arithmetic to prevent overflow
        let bytes_this_cycle = iterations_per_stream
            .saturating_mul(streams as usize)
            .saturating_mul(std::mem::size_of::<u64>());
        total_bytes_processed = total_bytes_processed.saturating_add(bytes_this_cycle);
        
        let elapsed_secs = test_start.elapsed().as_secs() as u32;
        if !timing.should_continue(cycle, elapsed_secs) {
            break;
        }
    }

    let elapsed = start.elapsed().as_millis();
    TestStats {
        name: test_name,
        action: TestAction::RandomAccess,
        bytes_processed: total_bytes_processed,
        elapsed_ms: elapsed,
        thread_id,
        error_count: total_error_count,
    }
}

pub unsafe fn stride_access_test(ptr: *mut u8, size: usize, thread_id: usize, error_mode: ErrorMode, timing: &TestTiming, streams: u32) -> TestStats {
    let test_name = "Stride Access";
    let start = Instant::now();
    let base = ptr as *mut u64;
    let len = size / std::mem::size_of::<u64>();
    let mut total_error_count = 0u64;
    let mut total_bytes_processed = 0usize;
    
    let mut cycle = 0u32;
    let test_start = Instant::now();
    
    loop {
        cycle += 1;
        let mut cycle_errors = 0u64;
        
        // Test various stride patterns that defeat caching
        let strides = [1, 16, 64, 256, 1024, 4096];
        let pattern_base = 0xFEDCBA9876543210u64.wrapping_add(thread_id as u64).wrapping_add(cycle as u64);

        for &stride in &strides {
            if stride >= len {
                continue;
            }

            // For stride access, partition memory between streams rather than offsets
            let elements_per_stream = len / streams.max(1) as usize;
            
            log::debug!("[Thread {}] StrideAccess: stride={}, streams={}, elements_per_stream={}", 
                       thread_id, stride, streams, elements_per_stream);
            
            // Write phase: each stream works on its own memory region
            for stream in 0..streams as usize {
                let pattern = pattern_base
                    .wrapping_add((stride as u64) << 32)
                    .wrapping_add((stream as u64) << 48);

                let stream_start = stream * elements_per_stream;
                let stream_end = if stream == streams as usize - 1 {
                    len // Last stream handles any remainder
                } else {
                    (stream + 1) * elements_per_stream
                };

                // Write with stride pattern within this stream's region
                let mut pos = stream_start;
                while pos < stream_end {
                    base.add(pos).write(pattern.wrapping_add(pos as u64));
                    pos += stride;
                }
            }

            std::sync::atomic::fence(Ordering::SeqCst);

            // Verify phase: each stream verifies its own memory region
            for stream in 0..streams as usize {
                let pattern = pattern_base
                    .wrapping_add((stride as u64) << 32)
                    .wrapping_add((stream as u64) << 48);

                let stream_start = stream * elements_per_stream;
                let stream_end = if stream == streams as usize - 1 {
                    len
                } else {
                    (stream + 1) * elements_per_stream
                };

                // Verify with same stride pattern
                let mut pos = stream_start;
                while pos < stream_end {
                    let expected = pattern.wrapping_add(pos as u64);
                    let actual = base.add(pos).read();
                    if actual != expected {
                        cycle_errors += 1;
                        handle_error(error_mode, test_name, pos, expected, actual);
                    }
                    pos += stride;
                }
            }
        }
        
        // Calculate approximate bytes processed
        // For each stride, we access approximately len/stride elements
        // Each element is 8 bytes, and we do both read and write
        let mut bytes_this_cycle = 0;
        for &stride in &strides {
            if stride < len {
                let elements_accessed = len / stride;
                bytes_this_cycle += elements_accessed * std::mem::size_of::<u64>() * 2; // read + write
            }
        }
        
        total_error_count += cycle_errors;
        total_bytes_processed = total_bytes_processed.saturating_add(bytes_this_cycle);
        
        let elapsed_secs = test_start.elapsed().as_secs() as u32;
        if !timing.should_continue(cycle, elapsed_secs) {
            break;
        }
    }

    let elapsed = start.elapsed().as_millis();
    TestStats {
        name: test_name,
        action: TestAction::ReadWrite,
        bytes_processed: total_bytes_processed,
        elapsed_ms: elapsed,
        thread_id,
        error_count: total_error_count,
    }
}

pub unsafe fn bandwidth_saturation_test(ptr: *mut u8, size: usize, thread_id: usize, _error_mode: ErrorMode, timing: &TestTiming, streams: u32) -> TestStats {
    let test_name = "Bandwidth Saturation";
    let start = Instant::now();
    let base = ptr as *mut u64;
    let len = size / std::mem::size_of::<u64>();
    let mut total_bytes_processed = 0usize;
    
    let mut cycle = 0u32;
    let test_start = Instant::now();
    
    loop {
        cycle += 1;
        
        // Pure memory bandwidth test with configurable streams
        match streams {
            1 => {
                // Single stream - maximum sequential bandwidth
                let pattern = 0x5555AAAA5555AAAAu64.wrapping_add(thread_id as u64).wrapping_add(cycle as u64);

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
            }
            _ => {
                // Multiple streams - interleaved access for bandwidth
                let stream_size = len / streams as usize;
                
                // Write phase with multiple streams
                for stream in 0..streams as usize {
                    let pattern = 0x5555AAAA5555AAAAu64
                        .wrapping_add(thread_id as u64)
                        .wrapping_add(cycle as u64)
                        .wrapping_add((stream as u64) << 32);
                    
                    let start = stream * stream_size;
                    let end = ((stream + 1) * stream_size).min(len);
                    
                    for i in start..end {
                        base.add(i).write(pattern.wrapping_add(i as u64));
                    }
                }

                std::sync::atomic::fence(Ordering::SeqCst);

                // Read phase with multiple streams
                let mut checksums = vec![0u64; streams as usize];
                for stream in 0..streams as usize {
                    let start = stream * stream_size;
                    let end = ((stream + 1) * stream_size).min(len);
                    
                    for i in start..end {
                        checksums[stream] = checksums[stream].wrapping_add(base.add(i).read());
                    }
                }

                // Prevent optimization
                for checksum in &mut checksums {
                    std::ptr::write_volatile(checksum, *checksum);
                }
            }
        }
        
        total_bytes_processed += size * 2; // Read + Write
        
        let elapsed_secs = test_start.elapsed().as_secs() as u32;
        if !timing.should_continue(cycle, elapsed_secs) {
            break;
        }
    }

    let elapsed = start.elapsed().as_millis();
    TestStats {
        name: test_name,
        action: TestAction::ReadWrite,
        bytes_processed: total_bytes_processed,
        elapsed_ms: elapsed,
        thread_id,
        error_count: 0, // This test doesn't verify individual values
    }
}

pub unsafe fn block_move_test(ptr: *mut u8, size: usize, thread_id: usize, error_mode: ErrorMode, timing: &TestTiming, streams: u32) -> TestStats {
    let test_name = "BlockMove";
    let start = Instant::now();
    
    // Divide memory in half - first half is source, second half is destination
    let half_size = size / 2;
    let src_base = ptr as *mut u64;
    let dst_base = (ptr as *mut u64).add(half_size / std::mem::size_of::<u64>());
    let len = half_size / std::mem::size_of::<u64>();
    
    let mut total_bytes_processed = 0usize;
    let mut total_error_count = 0u64;
    let mut cycle = 0u32;
    let test_start = Instant::now();
    
    // Initialize source with test pattern
    let pattern_base = 0xDEADBEEFCAFEBABEu64;
    for i in 0..len {
        src_base.add(i).write(pattern_base.wrapping_add(i as u64));
    }
    std::sync::atomic::fence(Ordering::SeqCst);
    
    loop {
        cycle += 1;
        let mut cycle_errors = 0u64;
        
        match streams {
            1 => {
                // Single stream: Simple forward copy
                for i in 0..len {
                    let val = src_base.add(i).read();
                    dst_base.add(i).write(val);
                }
                
                std::sync::atomic::fence(Ordering::SeqCst);
                
                // Verify
                for i in 0..len {
                    let expected = pattern_base.wrapping_add(i as u64);
                    let actual = dst_base.add(i).read();
                    if actual != expected {
                        cycle_errors += 1;
                        handle_error(error_mode, test_name, i, expected, actual);
                    }
                }
            }
            2 => {
                // Two streams: Copy forward and backward simultaneously
                let mid = len / 2;
                
                // Stream 1: Copy first half forward
                for i in 0..mid {
                    let val = src_base.add(i).read();
                    dst_base.add(i).write(val);
                }
                
                // Stream 2: Copy second half backward
                for i in 0..mid {
                    let src_idx = len - 1 - i;
                    let dst_idx = len - 1 - i;
                    let val = src_base.add(src_idx).read();
                    dst_base.add(dst_idx).write(val);
                }
                
                std::sync::atomic::fence(Ordering::SeqCst);
                
                // Verify both halves
                for i in 0..len {
                    let expected = pattern_base.wrapping_add(i as u64);
                    let actual = dst_base.add(i).read();
                    if actual != expected {
                        cycle_errors += 1;
                        handle_error(error_mode, test_name, i, expected, actual);
                    }
                }
            }
            4 => {
                // Four streams: Interleaved block copy
                let block_size = len / 4;
                
                for stream in 0..4 {
                    let start_idx = stream * block_size;
                    let end_idx = ((stream + 1) * block_size).min(len);
                    
                    // Copy with different patterns per stream
                    match stream % 4 {
                        0 => {
                            // Forward copy
                            for i in start_idx..end_idx {
                                let val = src_base.add(i).read();
                                dst_base.add(i).write(val);
                            }
                        }
                        1 => {
                            // Backward copy within block
                            for i in 0..(end_idx - start_idx) {
                                let src_idx = end_idx - 1 - i;
                                let dst_idx = end_idx - 1 - i;
                                let val = src_base.add(src_idx).read();
                                dst_base.add(dst_idx).write(val);
                            }
                        }
                        2 => {
                            // Skip pattern copy (every other element)
                            for i in (start_idx..end_idx).step_by(2) {
                                let val = src_base.add(i).read();
                                dst_base.add(i).write(val);
                            }
                            for i in ((start_idx + 1)..end_idx).step_by(2) {
                                let val = src_base.add(i).read();
                                dst_base.add(i).write(val);
                            }
                        }
                        _ => {
                            // Block copy
                            for i in start_idx..end_idx {
                                let val = src_base.add(i).read();
                                dst_base.add(i).write(val);
                            }
                        }
                    }
                }
                
                std::sync::atomic::fence(Ordering::SeqCst);
                
                // Verify
                for i in 0..len {
                    let expected = pattern_base.wrapping_add(i as u64);
                    let actual = dst_base.add(i).read();
                    if actual != expected {
                        cycle_errors += 1;
                        handle_error(error_mode, test_name, i, expected, actual);
                    }
                }
            }
            _ => {
                // Many streams: Strided copy pattern
                let stride = streams as usize;
                
                for offset in 0..stride.min(len) {
                    let mut i = offset;
                    while i < len {
                        let val = src_base.add(i).read();
                        dst_base.add(i).write(val);
                        i += stride;
                    }
                }
                
                std::sync::atomic::fence(Ordering::SeqCst);
                
                // Verify
                for i in 0..len {
                    let expected = pattern_base.wrapping_add(i as u64);
                    let actual = dst_base.add(i).read();
                    if actual != expected {
                        cycle_errors += 1;
                        handle_error(error_mode, test_name, i, expected, actual);
                    }
                }
            }
        }
        
        // Each cycle processes: read from source + write to destination + read for verify
        total_bytes_processed += half_size * 3;
        total_error_count += cycle_errors;
        
        let elapsed_secs = test_start.elapsed().as_secs() as u32;
        if !timing.should_continue(cycle, elapsed_secs) {
            break;
        }
    }

    let elapsed = start.elapsed().as_millis();
    TestStats {
        name: test_name,
        action: TestAction::Copy,
        bytes_processed: total_bytes_processed,
        elapsed_ms: elapsed,
        thread_id,
        error_count: total_error_count,
    }
}