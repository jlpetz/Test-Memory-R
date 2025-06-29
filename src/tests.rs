use crate::ErrorMode;
use crate::cache::CacheInfo;
use std::arch::x86_64::*;
use std::sync::atomic::Ordering;
use std::time::Instant;
use std::sync::OnceLock;

const CACHE_BUSTING_STRIDE: usize = 4096;

// Global cache info that gets detected once at runtime
static CACHE_INFO: OnceLock<CacheInfo> = OnceLock::new();

pub fn get_cache_info() -> &'static CacheInfo {
    CACHE_INFO.get_or_init(|| {
        let cache_info = CacheInfo::detect();
        cache_info.print_info();
        cache_info
    })
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
        let cache_info = get_cache_info();
        
        match test_name {
            // Cache-focused tests use smaller windows
            "CacheBusting" => cache_info.get_optimal_window_size("CacheBusting").min(allocated_size / 4),
            "RandomTorture" => cache_info.get_optimal_window_size("RandomTorture").min(allocated_size / 2),
            
            // Memory bandwidth tests use larger windows
            "BandwidthSat" => cache_info.get_optimal_window_size("BandwidthSat").min(allocated_size / 2),
            "MirrorMove128NonTemporal" | "MirrorMove256NonTemporal" | "MirrorMove512NonTemporal" => {
                cache_info.get_optimal_window_size(test_name).min(allocated_size / 3)
            }
            
            // General tests use moderate windows
            "SimpleTest" | "RefreshStable" => {
                let min_size = 64 * 1024 * 1024; // At least 64MB
                (allocated_size / 4).max(min_size).max(cache_info.l3_cache)
            },
            "StrideAccess" => {
                let min_size = 32 * 1024 * 1024; // At least 32MB
                (allocated_size / 8).max(min_size).max(cache_info.l3_cache / 2)
            },
            
            _ => if self.window_size_bytes > 0 {
                self.window_size_bytes
            } else {
                (allocated_size / 4).max(cache_info.l3_cache) // Default to 25% of allocation or L3 size
            }
        }
    }

    // Calculate optimal block size with alignment
    pub fn calculate_optimal_block_size(&self, test_name: &str, window_size: usize) -> (usize, bool) {
        let cache_info = get_cache_info();
        
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
                align_to_boundary(base_size, cache_info.cache_line_size) // Detected cache line alignment
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
                    align_to_boundary(base_size, cache_info.cache_line_size) // Detected cache line alignment
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

// SIMD-optimized memory testing implementations
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