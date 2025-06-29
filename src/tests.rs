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
}

impl TestMemoryConfig {
    pub fn new(window_mode: WindowMode, block_mode: BlockMode, allow_misaligned: bool, requires_locality: bool) -> Self {
        Self {
            window_mode,
            block_mode,
            allow_misaligned,
            requires_locality,
            timing: TestTiming::default(),
        }
    }
    
    pub fn with_timing(mut self, timing: TestTiming) -> Self {
        self.timing = timing;
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
pub unsafe fn stuck_bit_test(ptr: *mut u8, size: usize, thread_id: usize, error_mode: ErrorMode) -> TestStats {
    let start = Instant::now();
    let base = ptr as *mut u64;
    let len = size / std::mem::size_of::<u64>();
    let mut error_count = 0;

    log::info!("[Thread {}] Running stuck bit test on {:.2} MB of memory", 
              thread_id, size as f64 / (1024.0 * 1024.0));

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
            error_count += 1;
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
            error_count += 1;
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
            error_count += 1;
            match error_mode {
                ErrorMode::Panic => panic!("stuck_bit_test: Phase 3 error at index {} - expected {:#x}, got {:#x}", i, pattern1, v),
                ErrorMode::Halt => break,
                ErrorMode::Log => {
                    log::error!("stuck_bit_test: Phase 3 error at index {} - expected {:#x}, got {:#x}", i, pattern1, v);
                }
            }
        }
    }

    let elapsed = start.elapsed().as_millis();
    TestStats {
        name: "StuckBitTest",
        action: TestAction::StuckBitTest,
        bytes_processed: size * 6, // 3 writes + 3 reads
        elapsed_ms: elapsed,
        thread_id,
        error_count,
    }
}

// === EXISTING TEST FUNCTIONS (updated for timing support) ===

pub unsafe fn mirror_move_128_non_temporal(ptr: *mut u8, size: usize, thread_id: usize, _error_mode: ErrorMode) -> TestStats {
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

pub unsafe fn mirror_move_256_non_temporal(ptr: *mut u8, size: usize, thread_id: usize, _error_mode: ErrorMode) -> TestStats {
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

pub unsafe fn mirror_move_512_non_temporal(ptr: *mut u8, size: usize, thread_id: usize, _error_mode: ErrorMode) -> TestStats {
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

pub unsafe fn simple_test(ptr: *mut u8, size: usize, thread_id: usize, error_mode: ErrorMode) -> TestStats {
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

pub unsafe fn refresh_stable(ptr: *mut u8, size: usize, thread_id: usize, error_mode: ErrorMode) -> TestStats {
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

pub unsafe fn cache_busting_write_test(ptr: *mut u8, size: usize, thread_id: usize, error_mode: ErrorMode) -> TestStats {
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

pub unsafe fn random_access_torture_test(ptr: *mut u8, size: usize, thread_id: usize, error_mode: ErrorMode) -> TestStats {
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

pub unsafe fn stride_access_test(ptr: *mut u8, size: usize, thread_id: usize, error_mode: ErrorMode) -> TestStats {
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

pub unsafe fn bandwidth_saturation_test(ptr: *mut u8, size: usize, thread_id: usize, _error_mode: ErrorMode) -> TestStats {
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
