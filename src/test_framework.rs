use crate::tests::{TestStats, TestMemoryConfig, TestTiming};
use crate::{ErrorMode, AllocationBlock};
use std::time::Instant;

/// Common test loop pattern that eliminates duplication across all test functions
pub struct TestLoop<'a> {
    timing: &'a TestTiming,
    start_time: Instant,
    current_cycle: u32,
}

impl<'a> TestLoop<'a> {
    pub fn new(timing: &'a TestTiming) -> Self {
        Self {
            timing,
            start_time: Instant::now(),
            current_cycle: 0,
        }
    }
    
    pub fn should_continue(&mut self) -> bool {
        let elapsed_secs = self.start_time.elapsed().as_secs() as u32;
        let should_continue = self.timing.should_continue(self.current_cycle, elapsed_secs);
        if should_continue {
            self.current_cycle += 1;
        }
        should_continue
    }
    
    pub fn cycle(&self) -> u32 { 
        self.current_cycle 
    }
    
    pub fn elapsed(&self) -> std::time::Duration {
        self.start_time.elapsed()
    }
}

/// Error accumulation helper that handles different error modes consistently
pub struct ErrorAccumulator {
    total_errors: u64,
    error_mode: ErrorMode,
}

impl ErrorAccumulator {
    pub fn new(error_mode: ErrorMode) -> Self {
        Self {
            total_errors: 0,
            error_mode,
        }
    }
    
    pub fn add_errors(&mut self, errors: u64) -> Result<(), String> {
        self.total_errors += errors;
        
        match self.error_mode {
            ErrorMode::Log => Ok(()),
            ErrorMode::Halt if errors > 0 => Err(format!("Test halted due to {} errors", errors)),
            ErrorMode::Halt => Ok(()),
            ErrorMode::Panic if errors > 0 => panic!("Test failed with {} errors", errors),
            ErrorMode::Panic => Ok(()),
        }
    }
    
    pub fn total_errors(&self) -> u64 {
        self.total_errors
    }
}

/// Common pattern for stream-based testing
pub struct StreamedTest {
    pub streams: u32,
    pub base_stride: usize,
}

impl StreamedTest {
    pub fn new(streams: u32) -> Self {
        Self {
            streams,
            base_stride: 64, // Cache line size
        }
    }
    
    pub fn from_config(config: &TestMemoryConfig) -> Self {
        Self {
            streams: config.streams.max(1),
            base_stride: if config.requires_locality { 64 } else { 4096 },
        }
    }
    
    pub fn calculate_stream_offset(&self, stream_id: u32, base_size: usize) -> usize {
        (stream_id as usize * base_size) / self.streams as usize
    }
    
    pub fn calculate_stream_size(&self, stream_id: u32, total_size: usize) -> usize {
        let base_size = total_size / self.streams as usize;
        if stream_id == self.streams - 1 {
            // Last stream gets any remainder
            total_size - (stream_id as usize * base_size)
        } else {
            base_size
        }
    }
    
    pub fn get_stride_for_access_pattern(&self, requires_locality: bool) -> usize {
        if requires_locality {
            self.base_stride // Sequential, cache-friendly
        } else {
            self.base_stride.max(4096) // Cache-busting
        }
    }
}

/// Memory pattern interface for implementing different test patterns
pub trait TestPattern {
    /// Initialize memory with the test pattern
    /// # Safety
    /// Caller must ensure `ptr` is valid for writes of `size` bytes.
    unsafe fn initialize_memory(&self, ptr: *mut u8, size: usize, thread_id: usize);
    
    /// Verify memory contains the expected pattern
    /// # Safety
    /// Caller must ensure `ptr` is valid for reads of `size` bytes.
    unsafe fn verify_memory(&self, ptr: *mut u8, size: usize, thread_id: usize) -> u64;
    
    /// Get a descriptive name for this pattern
    fn name(&self) -> &'static str;
}

/// Simple alternating pattern (0x55, 0xAA)
pub struct AlternatingPattern;

impl TestPattern for AlternatingPattern {
    unsafe fn initialize_memory(&self, ptr: *mut u8, size: usize, _thread_id: usize) {
        for i in 0..size {
            *ptr.add(i) = if i % 2 == 0 { 0x55 } else { 0xAA };
        }
    }
    
    unsafe fn verify_memory(&self, ptr: *mut u8, size: usize, _thread_id: usize) -> u64 {
        let mut errors = 0u64;
        for i in 0..size {
            let expected = if i % 2 == 0 { 0x55 } else { 0xAA };
            let actual = *ptr.add(i);
            if actual != expected {
                errors += 1;
            }
        }
        errors
    }
    
    fn name(&self) -> &'static str {
        "Alternating (0x55/0xAA)"
    }
}

/// Walking ones pattern (single bit walks through each position)
pub struct WalkingOnesPattern {
    bit_position: u8,
}

impl WalkingOnesPattern {
    pub fn new(bit_position: u8) -> Self {
        Self {
            bit_position: bit_position % 8,
        }
    }
}

impl TestPattern for WalkingOnesPattern {
    unsafe fn initialize_memory(&self, ptr: *mut u8, size: usize, _thread_id: usize) {
        let pattern = 1u8 << self.bit_position;
        for i in 0..size {
            *ptr.add(i) = pattern;
        }
    }
    
    unsafe fn verify_memory(&self, ptr: *mut u8, size: usize, _thread_id: usize) -> u64 {
        let expected = 1u8 << self.bit_position;
        let mut errors = 0u64;
        for i in 0..size {
            let actual = *ptr.add(i);
            if actual != expected {
                errors += 1;
            }
        }
        errors
    }
    
    fn name(&self) -> &'static str {
        "Walking Ones"
    }
}

/// Thread-specific pattern (each thread uses different pattern)
pub struct ThreadSpecificPattern;

impl TestPattern for ThreadSpecificPattern {
    unsafe fn initialize_memory(&self, ptr: *mut u8, size: usize, thread_id: usize) {
        let pattern = (thread_id as u8).wrapping_mul(0x11);
        for i in 0..size {
            *ptr.add(i) = pattern;
        }
    }
    
    unsafe fn verify_memory(&self, ptr: *mut u8, size: usize, thread_id: usize) -> u64 {
        let expected = (thread_id as u8).wrapping_mul(0x11);
        let mut errors = 0u64;
        for i in 0..size {
            let actual = *ptr.add(i);
            if actual != expected {
                errors += 1;
            }
        }
        errors
    }
    
    fn name(&self) -> &'static str {
        "Thread-Specific"
    }
}

/// Helper function to run a test with a given pattern
pub fn run_pattern_test<P: TestPattern>(
    pattern: &P,
    allocated_block: &AllocationBlock,
    config: &TestMemoryConfig,
    timing: &TestTiming,
    thread_id: usize,
    error_mode: ErrorMode,
) -> Result<TestStats, String> {
    let ptr = allocated_block.buffer.as_mut_ptr();
    let size = allocated_block.buffer.size();
    
    // Calculate working size based on window configuration
    let working_size = config.calculate_window_size(pattern.name(), size);
    let working_ptr = ptr;
    
    // Use streams if configured
    let streams = config.streams.max(1);
    let stream_size = working_size / streams as usize;
    
    let mut test_loop = TestLoop::new(timing);
    let mut error_accumulator = ErrorAccumulator::new(error_mode);
    let mut total_bytes = 0u64;
    
    while test_loop.should_continue() {
        // Process each stream if multi-stream test
        for stream_id in 0..streams {
            let stream_offset = stream_id as usize * stream_size;
            let current_stream_size = if stream_id == streams - 1 {
                // Last stream gets remainder
                working_size - stream_offset
            } else {
                stream_size
            };
            
            if stream_offset + current_stream_size > working_size {
                break;
            }
            
            let stream_ptr = unsafe { working_ptr.add(stream_offset) };
            
            // Initialize memory with pattern for this stream
            unsafe {
                pattern.initialize_memory(stream_ptr, current_stream_size, thread_id);
            }
            
            // Apply memory access pattern based on configuration
            unsafe {
                if config.requires_locality {
                    // Use cache-friendly sequential access for locality-sensitive tests
                    CacheAwareAccess::sequential_aligned(stream_ptr, current_stream_size, 64);
                } else {
                    // Use strided access for cache-busting tests
                    let stride = if current_stream_size > 4096 { 4096 } else { 64 };
                    CacheAwareAccess::strided_access(stream_ptr, current_stream_size, stride);
                }
            }
            
            // Verify the pattern for this stream
            let errors = unsafe { pattern.verify_memory(stream_ptr, current_stream_size, thread_id) };
            error_accumulator.add_errors(errors)?;
            
            total_bytes += current_stream_size as u64;
        }
    }
    
    Ok(TestStats {
        name: pattern.name(),
        action: crate::tests::TestAction::WriteVerify,
        bytes_processed: total_bytes as usize,
        elapsed_ms: test_loop.elapsed().as_millis(),
        thread_id,
        error_count: error_accumulator.total_errors(),
        total_operations: test_loop.cycle() as u64,  // Total cycles completed
    })
}

/// Cache-friendly memory access patterns
pub struct CacheAwareAccess;

impl CacheAwareAccess {
    /// Sequential access with cache line alignment
    /// # Safety
    /// Caller must ensure `ptr` is valid for writes of `size` bytes.
    pub unsafe fn sequential_aligned(ptr: *mut u8, size: usize, cache_line_size: usize) {
        let mut offset = 0;
        while offset < size {
            let end = (offset + cache_line_size).min(size);
            // Touch the cache line
            for i in (offset..end).step_by(8) {
                std::ptr::write_volatile(ptr.add(i) as *mut u64, 0xDEADBEEFCAFEBABE);
            }
            offset += cache_line_size;
        }
    }
    
    /// Random access with controlled stride
    /// # Safety
    /// Caller must ensure `ptr` is valid for writes of `size` bytes.
    pub unsafe fn strided_access(ptr: *mut u8, size: usize, stride: usize) {
        let mut offset = 0;
        while offset < size {
            std::ptr::write_volatile(ptr.add(offset) as *mut u64, offset as u64);
            offset += stride;
            if offset >= size && stride > 1 {
                // Start over with smaller stride
                offset = stride / 2;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::TestTiming;
    
    #[test]
    fn test_alternating_pattern() {
        let pattern = AlternatingPattern;
        let size = 1024;
        let mut buffer = vec![0u8; size];
        let ptr = buffer.as_mut_ptr();
        
        unsafe {
            pattern.initialize_memory(ptr, size, 0);
        }
        let errors = unsafe { pattern.verify_memory(ptr, size, 0) };
        
        assert_eq!(errors, 0);
        assert_eq!(buffer[0], 0x55);
        assert_eq!(buffer[1], 0xAA);
        assert_eq!(buffer[2], 0x55);
    }
    
    #[test]
    fn test_walking_ones_pattern() {
        let pattern = WalkingOnesPattern::new(3);
        let size = 1024;
        let mut buffer = vec![0u8; size];
        let ptr = buffer.as_mut_ptr();
        
        unsafe {
            pattern.initialize_memory(ptr, size, 0);
        }
        let errors = unsafe { pattern.verify_memory(ptr, size, 0) };
        
        assert_eq!(errors, 0);
        assert_eq!(buffer[0], 0x08); // bit 3 set
        assert_eq!(buffer[100], 0x08);
    }
    
    #[test]
    fn test_test_loop() {
        let timing = TestTiming::cycles_only(3);
        let mut test_loop = TestLoop::new(&timing);
        
        let mut iterations = 0;
        while test_loop.should_continue() {
            iterations += 1;
        }
        
        assert_eq!(iterations, 3);
        assert_eq!(test_loop.cycle(), 3);
    }
}