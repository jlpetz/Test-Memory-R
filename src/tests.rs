use crate::ErrorMode;
use crate::cache::{CacheInfo, SystemInfo};
use crate::driver::MemoryType;
use crate::runner::SHUTDOWN_REQUESTED;
use crate::constants::{MB, MB_16, MB_32, MB_64, MB_F64, KB, PAGE_SIZE_4KB};
use std::simd::*; // Docs here https://doc.rust-lang.org/std/simd/index.html
use std::simd::cmp::SimdPartialEq;
use std::sync::atomic::Ordering;
use std::time::Instant;
use std::sync::OnceLock;

const CACHE_BUSTING_STRIDE: usize = PAGE_SIZE_4KB;

// Test memory configuration enums (moved from layout.rs - these are test concerns, not allocation concerns)
#[derive(Debug, Clone)]
pub enum WindowMode {
    FullAllocation,                    // Use entire allocation per thread (default for most tests)
    FixedSize { size_mb: u32 },       // Fixed window size for quick tests or TM5 compatibility
    FixedBytes { size_bytes: usize },  // Fixed window size in bytes for sub-MB precision (latency tests)
    CacheRelative { multiplier: f64 }, // Relative to total cache size for cache-sensitive tests
    CacheLevel { target: CacheTarget }, // Target specific cache level with smart sizing
}

/// Cache level targeting for latency tests
/// Sizes are calculated at runtime based on detected cache and thread count
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CacheTarget {
    /// Target L1 data cache (per-core)
    /// divisor: power of 2 (1, 2, 4, 8) - divides per-core L1 size
    L1 { divisor: u8 },
    /// Target L2 cache (per-core)
    /// divisor: power of 2 (1, 2, 4, 8) - divides per-core L2 size
    L2 { divisor: u8 },
    /// Target L3 cache (shared, auto-divided by thread count)
    /// divisor: power of 2 (1, 2, 4, 8) - divides per-thread L3 share
    L3 { divisor: u8 },
    /// Target DRAM with high TLB hit rate (typical latency)
    /// Uses per-thread L3 share * multiplier - smaller working set per thread
    DRAM { multiplier: u8 },
    /// Target DRAM with TLB stress (worst-case latency)
    /// Uses total L3 * multiplier - larger working set exceeds TLB coverage
    DRAMFull { multiplier: u8 },
}

impl CacheTarget {
    /// L1 with default divisor of 2 (50% of per-core L1)
    pub const L1_DEFAULT: Self = Self::L1 { divisor: 2 };
    /// L2 with default divisor of 2 (50% of per-core L2)
    pub const L2_DEFAULT: Self = Self::L2 { divisor: 2 };
    /// L3 with default divisor of 2 (50% of L3 for single-thread, ignored for multi-thread)
    pub const L3_DEFAULT: Self = Self::L3 { divisor: 2 };
    /// DRAM with default multiplier of 4 - per-thread window = (L3 / threads) * 4
    pub const DRAM_DEFAULT: Self = Self::DRAM { multiplier: 4 };
    /// DRAM-Full uses entire thread allocation (multiplier ignored)
    pub const DRAM_FULL_DEFAULT: Self = Self::DRAMFull { multiplier: 0 };

    /// Calculate actual window size in bytes based on cache info and thread count
    pub fn calculate_window_size(&self, cache_info: &CacheInfo, thread_count: usize) -> usize {
        match self {
            CacheTarget::L1 { divisor } => {
                cache_info.per_core_l1d / (*divisor as usize)
            }
            CacheTarget::L2 { divisor } => {
                cache_info.per_core_l2 / (*divisor as usize)
            }
            CacheTarget::L3 { divisor } => {
                // For VMs: apply extra /2 factor because reported L3 is shared with other VMs
                // Physical: L3/2 (default), VM: L3/4 (effective)
                let vm_factor = if cache_info.is_virtual_machine { 2 } else { 1 };
                let effective_divisor = (*divisor as usize) * vm_factor;

                if thread_count == 1 {
                    // Single-thread: apply divisor (default /2 = 50% of L3, or /4 for VMs)
                    // Leaves headroom and ensures we're testing L3, not thrashing it
                    let l3_target = cache_info.l3_cache / effective_divisor;
                    // Minimum: must exceed L2 to avoid L2 hits
                    let min_l3_size = cache_info.per_core_l2 + (64 * 1024);
                    l3_target.max(min_l3_size)
                } else {
                    // Multi-thread: use full per-thread share with VM adjustment
                    // Total working set = L3/vm_factor, ensuring all threads fit in available L3 together
                    let effective_l3 = cache_info.l3_cache / vm_factor;
                    let per_thread = effective_l3 / thread_count;
                    // Minimum: must exceed L2 to avoid L2 hits
                    let min_l3_size = cache_info.per_core_l2 + (64 * 1024);
                    per_thread.max(min_l3_size)
                }
            }
            CacheTarget::DRAM { multiplier } => {
                // DRAM test - each thread gets (L3 / threads) * multiplier
                // Combined working set = L3 * multiplier, ensuring all threads exceed L3 together
                // Example: 6 threads, 16MB L3, multiplier=4 → 10.67MB each, 64MB total
                let per_thread_l3 = cache_info.l3_cache / thread_count.max(1);
                per_thread_l3 * (*multiplier as usize)
            }
            CacheTarget::DRAMFull { multiplier: _ } => {
                // Full allocation test - returns sentinel to signal "use entire allocated block"
                // Caller (TestMemoryConfig::calculate_window_size) handles this specially
                // This tests the full memory assigned to each thread, regardless of L3 size
                usize::MAX
            }
        }
    }

    /// Get a human-readable name for this target
    pub fn name(&self) -> String {
        match self {
            CacheTarget::L1 { divisor } => format!("L1/{}", divisor),
            CacheTarget::L2 { divisor } => format!("L2/{}", divisor),
            CacheTarget::L3 { divisor } => format!("L3/{}", divisor),
            CacheTarget::DRAM { multiplier } => format!("DRAM*{}", multiplier),
            CacheTarget::DRAMFull { multiplier: _ } => "DRAM-Full".to_string(),
        }
    }

    /// Get a human-readable name that reflects actual calculation with thread count
    pub fn name_with_threads(&self, thread_count: usize) -> String {
        self.name_with_context(thread_count, false)
    }

    /// Get a human-readable name that reflects actual calculation with thread count and VM status
    pub fn name_with_context(&self, thread_count: usize, is_vm: bool) -> String {
        match self {
            CacheTarget::L1 { divisor } => format!("L1/{}", divisor),
            CacheTarget::L2 { divisor } => format!("L2/{}", divisor),
            CacheTarget::L3 { divisor } => {
                // For VMs, effective divisor is doubled
                let vm_factor = if is_vm { 2 } else { 1 };
                if thread_count == 1 {
                    let effective = (*divisor as usize) * vm_factor;
                    format!("L3/{}", effective)
                } else {
                    // Multi-thread uses thread count, but VM still halves the base
                    let effective = thread_count * vm_factor;
                    format!("L3/{}", effective)
                }
            }
            CacheTarget::DRAM { multiplier } => format!("DRAM*{}", multiplier),
            CacheTarget::DRAMFull { multiplier: _ } => "DRAM-Full".to_string(),
        }
    }

    /// Get the base level name (without divisor/multiplier)
    pub fn level_name(&self) -> &'static str {
        match self {
            CacheTarget::L1 { .. } => "L1",
            CacheTarget::L2 { .. } => "L2",
            CacheTarget::L3 { .. } => "L3",
            CacheTarget::DRAM { .. } => "DRAM",
            CacheTarget::DRAMFull { .. } => "DRAM-Full",
        }
    }

    /// Parse from string like "L1", "L1/2", "L2/4", "DRAM", "DRAM*8", "DRAM-FULL*2"
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim().to_uppercase();

        if let Some(rest) = s.strip_prefix("L1") {
            let divisor = Self::parse_divisor(rest).unwrap_or(2);
            Some(CacheTarget::L1 { divisor })
        } else if let Some(rest) = s.strip_prefix("L2") {
            let divisor = Self::parse_divisor(rest).unwrap_or(2);
            Some(CacheTarget::L2 { divisor })
        } else if let Some(rest) = s.strip_prefix("L3") {
            let divisor = Self::parse_divisor(rest).unwrap_or(4);
            Some(CacheTarget::L3 { divisor })
        } else if let Some(rest) = s.strip_prefix("DRAM-FULL") {
            // Must check DRAM-FULL before DRAM (longer prefix first)
            let multiplier = Self::parse_multiplier(rest).unwrap_or(2);
            Some(CacheTarget::DRAMFull { multiplier })
        } else if let Some(rest) = s.strip_prefix("DRAMFULL") {
            // Alternative without hyphen
            let multiplier = Self::parse_multiplier(rest).unwrap_or(2);
            Some(CacheTarget::DRAMFull { multiplier })
        } else if let Some(rest) = s.strip_prefix("DRAM") {
            let multiplier = Self::parse_multiplier(rest).unwrap_or(4);
            Some(CacheTarget::DRAM { multiplier })
        } else if s == "RAM" {
            // Alias for DRAM
            Some(CacheTarget::DRAM_DEFAULT)
        } else if s == "RAM-FULL" || s == "RAMFULL" {
            // Alias for DRAM-FULL
            Some(CacheTarget::DRAM_FULL_DEFAULT)
        } else {
            None
        }
    }

    fn parse_divisor(s: &str) -> Option<u8> {
        if s.is_empty() {
            return None;
        }
        let s = s.trim_start_matches('/');
        s.parse::<u8>().ok().filter(|&d| d.is_power_of_two() && d >= 1)
    }

    fn parse_multiplier(s: &str) -> Option<u8> {
        if s.is_empty() {
            return None;
        }
        let s = s.trim_start_matches('*');
        s.parse::<u8>().ok().filter(|&m| m >= 1)
    }
}

#[derive(Debug, Clone)]
pub enum ChunkMode {
    AutoOptimal,                       // Auto-calculate optimal chunk size per test
    FixedSize { size_mb: u32 },       // Fixed chunk size for consistent behavior
    WindowFraction { fraction: f64 },  // Fraction of window size for proportional chunking
}

/// Controls error checking frequency within tests using power-of-2 intervals
/// for optimal hot loop performance with bitwise operations
#[derive(Debug, Clone, Copy)]
pub struct ErrorCheckInterval {
    /// Power-of-2 shift for check interval (0 = every op, 9 = every 512 ops, etc.)
    /// Special value: u32::MAX = check only at chunk boundaries
    pub power_of_two_shift: u32,
}

impl ErrorCheckInterval {
    /// Check every element (shift = 0, check every 2^0 = 1 operation)
    pub const EVERY_ELEMENT: Self = Self { power_of_two_shift: 0 };
    
    /// Check only at chunk boundaries (shift = MAX, effectively never within chunk)
    pub const PER_CHUNK: Self = Self { power_of_two_shift: u32::MAX };
    
    /// Create from TM5 Parameter value, rounding to nearest power-of-2
    pub fn from_parameter(param: u32) -> Self {
        match param {
            0 => Self::PER_CHUNK,  // TM5 Parameter=0 means full chunk
            1 => Self::EVERY_ELEMENT,  // TM5 Parameter=1 means every element
            n => {
                // Round up to nearest power of 2 and get shift
                let power_of_2 = n.next_power_of_two();
                let shift = power_of_2.trailing_zeros();
                
                if n != power_of_2 {
                    log::debug!("Parameter {} rounded to {} (2^{}) for performance", 
                              n, power_of_2, shift);
                }
                
                Self { power_of_two_shift: shift }
            }
        }
    }
    
    /// Get the mask for bitwise AND checking in hot loops
    /// Returns None for PER_CHUNK mode (no checking within chunk)
    #[inline]
    pub fn get_check_mask(&self) -> Option<u32> {
        if self.power_of_two_shift >= 31 {
            None  // No checking within chunk
        } else {
            Some((1u32 << self.power_of_two_shift) - 1)
        }
    }
}

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
#[derive(Debug, Clone, Copy)]
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
    Latency,
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
            TestAction::Latency => "Latency Measurement",
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone)]
pub struct TestStats {
    pub name: &'static str,
    pub action: TestAction,
    pub bytes_processed: usize,
    pub elapsed_ms: u128,
    pub thread_id: usize,
    pub error_count: u64,
    pub total_operations: u64,  // Total operation count (lightweight tracking)
    pub cycles_completed: u32,  // Actual cycles completed
    pub cycles_planned: Option<u32>, // Planned cycles (None = unlimited)
    pub stopped_by_time_limit: bool, // True if stopped due to time, false if stopped due to cycle limit
}

/// Optional progress reporting structure for tests
/// Allows tests to report progress back to the coordinator at checkpoints
#[derive(Debug)]
pub struct TestProgress {
    pub cycles_completed: std::sync::atomic::AtomicU32,
    pub bytes_processed: std::sync::atomic::AtomicU64,
    pub errors_found: std::sync::atomic::AtomicU64,
    pub last_update_ms: std::sync::atomic::AtomicU64,  // Timestamp of last update
}

impl TestProgress {
    pub fn new() -> Self {
        use std::sync::atomic::AtomicU32;
        use std::sync::atomic::AtomicU64;

        Self {
            cycles_completed: AtomicU32::new(0),
            bytes_processed: AtomicU64::new(0),
            errors_found: AtomicU64::new(0),
            last_update_ms: AtomicU64::new(0),
        }
    }
}

/// Detailed operation breakdown calculated post-test from total_operations
#[derive(Debug, Clone)]
pub struct DetailedOperationCount {
    pub total_reads: u64,
    pub total_writes: u64,
    pub total_verifies: u64,
    pub total_simd_ops: u64,
    pub total_fence_ops: u64,
    pub total_cache_ops: u64,
    pub simd_type: SIMDType,
    pub access_pattern: AccessPattern,
}

#[derive(Debug, Clone)]
pub enum SIMDType {
    None,
    SSE2_128,
    AVX2_256,
    AVX512_512,
}

#[derive(Debug, Clone)]
pub enum AccessPattern {
    Sequential,
    Strided(usize),
    Random,
    Mirror,
    BlockCopy,
    CacheBusting,
}

/// Operation metadata for each test - defines what constitutes one "operation"
#[derive(Debug, Clone)]
pub struct OperationMetadata {
    // Operations per single "operation unit" (typically per loop iteration)
    pub reads_per_op: u64,
    pub writes_per_op: u64,
    pub verifies_per_op: u64,
    pub simd_ops_per_op: u64,
    pub fence_ops_per_op: u64,
    pub cache_ops_per_op: u64,
    
    // Test characteristics
    pub simd_type: SIMDType,
    pub access_pattern: AccessPattern,
    pub memory_coverage: f64,  // Fraction of allocated memory touched per operation
    pub streams: u32,
    pub locality_sensitive: bool,
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
        if let Some(min_secs) = self.min_duration_secs
            && elapsed_secs < min_secs {
                return true;
            }
        
        // Check maximum duration
        if let Some(max_secs) = self.duration_secs
            && elapsed_secs >= max_secs {
                return false;
            }
        
        // Check cycle limit
        if let Some(max_cycles) = self.cycles
            && current_cycle >= max_cycles {
                return false;
            }
        
        // If no limits set, run forever (or until externally stopped)
        true
    }
}

// Three-stage memory configuration with corrected window logic
#[derive(Debug, Clone)]
pub struct TestMemoryConfig {
    pub window_mode: WindowMode,
    pub chunk_mode: ChunkMode,
    pub allow_misaligned: bool,
    pub requires_locality: bool,    // True if test needs temporal locality (small window)
    pub timing: TestTiming,
    pub streams: u32,               // Number of access streams (equivalent to TM5 jump/parameter)
    pub pattern_mode: Option<u32>,  // TM5 pattern mode
    pub pattern_param0: Option<u64>, // TM5 pattern parameter 0
    pub pattern_param1: Option<u64>, // TM5 pattern parameter 1
    pub memory_type: Option<MemoryType>,
    pub error_check_interval: ErrorCheckInterval,  // Controls error checking frequency
    pub tsc_frequency_ghz: f64,     // TSC frequency detected at startup (for latency tests)
    pub thread_count: usize,        // Total thread count for cache-aware window calculations
}

impl TestMemoryConfig {
    pub fn new(window_mode: WindowMode, chunk_mode: ChunkMode, allow_misaligned: bool, requires_locality: bool) -> Self {
        Self {
            window_mode,
            chunk_mode,
            allow_misaligned,
            requires_locality,
            timing: TestTiming::default(),
            streams: 1, // Default to single stream (equivalent to TM5 jump=1)
            pattern_mode: None,
            pattern_param0: None,
            pattern_param1: None,
            memory_type: None,
            error_check_interval: ErrorCheckInterval::PER_CHUNK,  // Default: check at chunk boundaries
            tsc_frequency_ghz: 0.0,  // MUST be set from detected CacheInfo before running latency tests
            thread_count: 1,  // Default to 1, should be set by runner for accurate cache calculations
        }
    }
    
    /// Get operation metadata for a specific test
    pub fn get_operation_metadata(&self, test_name: &str) -> OperationMetadata {
        match test_name {
            "Mem-StuckBit" => OperationMetadata {
                reads_per_op: 3,  // 3 verification reads per cycle per element
                writes_per_op: 3,  // 3 pattern writes per cycle per element
                verifies_per_op: 3,  // Same as reads for this test
                simd_ops_per_op: 0,
                fence_ops_per_op: 1,  // Per cycle, not per element
                cache_ops_per_op: 0,
                simd_type: SIMDType::None,
                access_pattern: AccessPattern::Sequential,
                memory_coverage: 1.0,
                streams: 1,
                locality_sensitive: false,
            },
            "Mem-StuckBit128" => OperationMetadata {
                reads_per_op: 3,  // 3 verification reads per cycle per element
                writes_per_op: 3,  // 3 pattern writes per cycle per element
                verifies_per_op: 3,  // Same as reads for this test
                simd_ops_per_op: 6,  // 3 loads + 3 stores per cycle per u64x2
                fence_ops_per_op: 1,  // Per cycle, not per element
                cache_ops_per_op: 0,
                simd_type: SIMDType::SSE2_128,
                access_pattern: AccessPattern::Sequential,
                memory_coverage: 1.0,
                streams: 1,
                locality_sensitive: false,
            },
            "Mem-StuckBit256" => OperationMetadata {
                reads_per_op: 3,  // 3 verification reads per cycle per element
                writes_per_op: 3,  // 3 pattern writes per cycle per element
                verifies_per_op: 3,  // Same as reads for this test
                simd_ops_per_op: 6,  // 3 loads + 3 stores per cycle per u64x4
                fence_ops_per_op: 1,  // Per cycle, not per element
                cache_ops_per_op: 0,
                simd_type: SIMDType::AVX2_256,
                access_pattern: AccessPattern::Sequential,
                memory_coverage: 1.0,
                streams: 1,
                locality_sensitive: false,
            },
            "Mem-StuckBit512" => OperationMetadata {
                reads_per_op: 3,  // 3 verification reads per cycle per element
                writes_per_op: 3,  // 3 pattern writes per cycle per element
                verifies_per_op: 3,  // Same as reads for this test
                simd_ops_per_op: 6,  // 3 loads + 3 stores per cycle per u64x8
                fence_ops_per_op: 1,  // Per cycle, not per element
                cache_ops_per_op: 0,
                simd_type: SIMDType::AVX512_512,
                access_pattern: AccessPattern::Sequential,
                memory_coverage: 1.0,
                streams: 1,
                locality_sensitive: false,
            },
            "Mem-Mirror" => OperationMetadata {
                reads_per_op: 2,  // Load + restore read per element
                writes_per_op: 2,  // Stream + restore write per element
                verifies_per_op: 1,  // Individual element verification (detailed error reporting)
                simd_ops_per_op: 0,  // Scalar implementation
                fence_ops_per_op: 1,  // Per cycle
                cache_ops_per_op: 2,  // Pattern with intentional cache behavior
                simd_type: SIMDType::None,
                access_pattern: AccessPattern::Mirror,
                memory_coverage: 1.0,
                streams: self.streams,
                locality_sensitive: false,
            },
            "Mem-Mirror128" => OperationMetadata {
                reads_per_op: 2,  // Load + restore read per element
                writes_per_op: 2,  // Stream + restore write per element
                verifies_per_op: 1,  // XOR verification per element
                simd_ops_per_op: 6,  // load + stream + verify + load + stream + fence
                fence_ops_per_op: 1,  // Per cycle
                cache_ops_per_op: 2,  // stream operations per element
                simd_type: SIMDType::SSE2_128,
                access_pattern: AccessPattern::Mirror,
                memory_coverage: 1.0,
                streams: self.streams,
                locality_sensitive: false,
            },
            "Mem-Mirror256" => OperationMetadata {
                reads_per_op: 2,
                writes_per_op: 2,
                verifies_per_op: 1,
                simd_ops_per_op: 6,
                fence_ops_per_op: 1,
                cache_ops_per_op: 2,
                simd_type: SIMDType::AVX2_256,
                access_pattern: AccessPattern::Mirror,
                memory_coverage: 1.0,
                streams: self.streams,
                locality_sensitive: false,
            },
            "Mem-Mirror512" => OperationMetadata {
                reads_per_op: 2,
                writes_per_op: 2,
                verifies_per_op: 1,
                simd_ops_per_op: 6,
                fence_ops_per_op: 1,
                cache_ops_per_op: 2,
                simd_type: SIMDType::AVX512_512,
                access_pattern: AccessPattern::Mirror,
                memory_coverage: 1.0,
                streams: self.streams,
                locality_sensitive: false,
            },
            "Mem-Simple" => OperationMetadata {
                reads_per_op: 1,  // Verify read per element
                writes_per_op: 1,  // Pattern write per element
                verifies_per_op: 1,  // Same as reads
                simd_ops_per_op: 0,
                fence_ops_per_op: 1,  // Per cycle
                cache_ops_per_op: 0,
                simd_type: SIMDType::None,
                access_pattern: AccessPattern::Sequential,
                memory_coverage: 1.0,
                streams: self.streams,
                locality_sensitive: false,
            },
            "Mem-Refresh" => OperationMetadata {
                reads_per_op: 1,  // Verify read per element
                writes_per_op: 1,  // Pattern write per element
                verifies_per_op: 1,  // Same as reads
                simd_ops_per_op: 0,
                fence_ops_per_op: 1,  // Per cycle
                cache_ops_per_op: 0,
                simd_type: SIMDType::None,
                access_pattern: AccessPattern::Sequential,
                memory_coverage: 1.0,
                streams: 1,
                locality_sensitive: true,
            },
            "Mem-Refresh128" => OperationMetadata {
                reads_per_op: 1,  // Verify read per element
                writes_per_op: 1,  // Pattern write per element
                verifies_per_op: 1,  // Same as reads
                simd_ops_per_op: 2,  // 1 load + 1 store per u64x2
                fence_ops_per_op: 1,  // Per cycle
                cache_ops_per_op: 0,
                simd_type: SIMDType::SSE2_128,
                access_pattern: AccessPattern::Sequential,
                memory_coverage: 1.0,
                streams: 1,
                locality_sensitive: true,
            },
            "Mem-Refresh256" => OperationMetadata {
                reads_per_op: 1,  // Verify read per element
                writes_per_op: 1,  // Pattern write per element
                verifies_per_op: 1,  // Same as reads
                simd_ops_per_op: 2,  // 1 load + 1 store per u64x4
                fence_ops_per_op: 1,  // Per cycle
                cache_ops_per_op: 0,
                simd_type: SIMDType::AVX2_256,
                access_pattern: AccessPattern::Sequential,
                memory_coverage: 1.0,
                streams: 1,
                locality_sensitive: true,
            },
            "Mem-Refresh512" => OperationMetadata {
                reads_per_op: 1,  // Verify read per element
                writes_per_op: 1,  // Pattern write per element
                verifies_per_op: 1,  // Same as reads
                simd_ops_per_op: 2,  // 1 load + 1 store per u64x8
                fence_ops_per_op: 1,  // Per cycle
                cache_ops_per_op: 0,
                simd_type: SIMDType::AVX512_512,
                access_pattern: AccessPattern::Sequential,
                memory_coverage: 1.0,
                streams: 1,
                locality_sensitive: true,
            },
            "Mem-CacheBust" => OperationMetadata {
                reads_per_op: 1,
                writes_per_op: 1,
                verifies_per_op: 1,
                simd_ops_per_op: 0,
                fence_ops_per_op: 1,
                cache_ops_per_op: 0,
                simd_type: SIMDType::None,
                access_pattern: AccessPattern::CacheBusting,
                memory_coverage: 1.0,  // All memory touched with stride pattern for cache busting
                streams: self.streams,
                locality_sensitive: false,
            },
            "Mem-Random" => OperationMetadata {
                reads_per_op: 1,  // Random verification read
                writes_per_op: 0,  // No writes in hot loop
                verifies_per_op: 1,  // Same as reads
                simd_ops_per_op: 0,
                fence_ops_per_op: 0,  // No fences in hot loop
                cache_ops_per_op: 0,
                simd_type: SIMDType::None,
                access_pattern: AccessPattern::Random,
                memory_coverage: 0.1,  // Random coverage varies, use conservative estimate
                streams: self.streams,
                locality_sensitive: false,
            },
            "Mem-Stride" => OperationMetadata {
                reads_per_op: 1,
                writes_per_op: 1,
                verifies_per_op: 1,
                simd_ops_per_op: 0,
                fence_ops_per_op: 1,  // Per stride pattern
                cache_ops_per_op: 0,
                simd_type: SIMDType::None,
                access_pattern: AccessPattern::Strided(1024),  // Average stride
                memory_coverage: 0.85,  // Aggregate across all strides
                streams: self.streams,
                locality_sensitive: false,
            },
            "Spd-Saturate" => OperationMetadata {
                reads_per_op: 1,  // Sequential read
                writes_per_op: 1,  // Sequential write
                verifies_per_op: 0,  // No verification
                simd_ops_per_op: 0,
                fence_ops_per_op: 1,  // Per cycle
                cache_ops_per_op: 0,
                simd_type: SIMDType::None,
                access_pattern: AccessPattern::Sequential,
                memory_coverage: 1.0,
                streams: self.streams,
                locality_sensitive: false,
            },
            "Mem-BlockMove" => OperationMetadata {
                reads_per_op: 2,  // Source read + destination verify read
                writes_per_op: 1,  // Destination write
                verifies_per_op: 1,  // Destination verify
                simd_ops_per_op: 0,
                fence_ops_per_op: 1,  // Per cycle
                cache_ops_per_op: 0,
                simd_type: SIMDType::None,
                access_pattern: AccessPattern::BlockCopy,
                memory_coverage: 0.5,  // Uses half of allocation (source to dest)
                streams: self.streams,
                locality_sensitive: false,
            },
            _ => panic!("Unknown test '{}' - add explicit metadata to get_operation_metadata()", test_name),
        }
    }

    pub fn with_memory_type(mut self, memory_type: Option<MemoryType>) -> Self {
        self.memory_type = memory_type;
        self
    }
    
    pub fn with_timing(mut self, timing: TestTiming) -> Self {
        self.timing = timing;
        self
    }

    pub fn with_tsc(mut self, tsc_frequency_ghz: f64) -> Self {
        self.tsc_frequency_ghz = tsc_frequency_ghz;
        self
    }

    pub fn with_streams(mut self, streams: u32) -> Self {
        // Validate streams is power-of-2 for multi-stream tests
        if streams > 1 && !streams.is_power_of_two() {
            panic!(
                "Stream count {} must be power-of-2 (1, 2, 4, 8, 16, etc.) for proper memory interleaving. \
                 This is a configuration error.",
                streams
            );
        }
        self.streams = streams;
        self
    }
    
    pub fn with_error_check_interval(mut self, interval: ErrorCheckInterval) -> Self {
        self.error_check_interval = interval;
        self
    }
    
    pub fn with_pattern_config(mut self, mode: Option<u32>, param0: Option<u64>, param1: Option<u64>) -> Self {
        self.pattern_mode = mode;
        self.pattern_param0 = param0;
        self.pattern_param1 = param1;
        self
    }

    pub fn with_thread_count(mut self, thread_count: usize) -> Self {
        self.thread_count = thread_count.max(1);
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
                let fixed_size = (*size_mb as usize) * MB;
                fixed_size.min(allocated_size)
            }
            WindowMode::FixedBytes { size_bytes } => {
                (*size_bytes).min(allocated_size)
            }
            WindowMode::CacheRelative { multiplier } => {
                let cache_info = get_cache_info();
                let cache_based_size = (cache_info.total_cache as f64 * multiplier) as usize;
                cache_based_size.min(allocated_size)
            }
            WindowMode::CacheLevel { target } => {
                // Calculate window size based on cache target using actual thread count
                let cache_info = get_cache_info();
                let calculated = target.calculate_window_size(cache_info, self.thread_count);
                // DRAMFull returns usize::MAX as sentinel to indicate "use full allocation"
                if calculated == usize::MAX {
                    allocated_size
                } else {
                    calculated.min(allocated_size)
                }
            }
        }
    }
    
    // Calculate locality-specific window for tests that need it
    fn calculate_locality_window_size(&self, test_name: &str, allocated_size: usize) -> usize {
        let cache_info = get_cache_info();

        let optimal_size = match test_name {
            "Mem-CacheBust" => (cache_info.l3_cache / 2).max(cache_info.l2_cache * 4),
            "Mem-Refresh" => cache_info.l2_cache * 2, // Small window for refresh testing
            _ => cache_info.total_cache * 2,
        };

        optimal_size.min(allocated_size)
    }

    // Calculate optimal chunk size with alignment  
    pub fn calculate_chunk_size(&self, test_name: &str, window_size: usize) -> usize {
        let cache_info = get_cache_info();
        
        let raw_chunk_size = match &self.chunk_mode {
            ChunkMode::FixedSize { size_mb } => {
                let fixed_size = (*size_mb as usize) * MB;
                if self.allow_misaligned {
                    fixed_size
                } else {
                    align_to_boundary(fixed_size, cache_info.cache_line_size)
                }
            }
            ChunkMode::WindowFraction { fraction } => {
                let fraction_size = (window_size as f64 * fraction) as usize;
                if self.allow_misaligned {
                    fraction_size
                } else {
                    align_to_boundary(fraction_size, cache_info.cache_line_size)
                }
            }
            ChunkMode::AutoOptimal => {
                self.calculate_optimal_block_for_test(test_name, window_size, cache_info)
            }
        };

        // Calculate minimum chunk size considering SIMD operations and stream requirements
        let minimum_chunk_size = self.calculate_minimum_chunk_size(test_name, self.streams);
        
        // Cap at window size first
        let window_capped_size = raw_chunk_size.min(window_size);
        
        // Apply minimum size requirements
        let final_chunk_size = window_capped_size.max(minimum_chunk_size);
        
        // Log corrections for user awareness
        if final_chunk_size != raw_chunk_size {
            let raw_mb = raw_chunk_size as f64 / MB as f64;
            let final_mb = final_chunk_size as f64 / MB as f64;
            
            if final_chunk_size > raw_chunk_size {
                log::debug!("🔧 Chunk size corrected for {}: {:.2}MB → {:.2}MB (minimum required for {} streams + SIMD alignment)", 
                           test_name, raw_mb, final_mb, self.streams);
            } else {
                log::debug!("🔧 Chunk size capped for {}: {:.2}MB → {:.2}MB (limited by window size)", 
                           test_name, raw_mb, final_mb);
            }
        }
        
        final_chunk_size
    }
    
    fn calculate_minimum_chunk_size(&self, test_name: &str, streams: u32) -> usize {
        // SIMD operation size requirements - explicit for each test to catch missing implementations
        let simd_requirement = match test_name {
            "Mem-Mirror" => 8,       // Basic u64 operations (scalar)
            "Mem-Mirror128" => 16,   // 128-bit operations
            "Mem-Mirror256" => 32,   // 256-bit operations
            "Mem-Mirror512" => 64,   // 512-bit operations
            "Mem-StuckBit" => 8,                // Basic u64 operations
            "Mem-StuckBit128" => 16,            // 128-bit SIMD operations
            "Mem-StuckBit256" => 32,            // 256-bit SIMD operations
            "Mem-StuckBit512" => 64,            // 512-bit SIMD operations
            "Mem-Simple" => 8,                 // Basic u64 operations
            "Mem-Refresh" => 8,              // Basic u64 operations
            "Mem-Refresh128" => 16,          // 128-bit SIMD operations
            "Mem-Refresh256" => 32,          // 256-bit SIMD operations
            "Mem-Refresh512" => 64,          // 512-bit SIMD operations
            "Mem-CacheBust" => 64,        // Cache line operations
            "Mem-Random" => 8,       // Basic u64 operations
            "Mem-Stride" => 8,               // Basic u64 operations
            "Spd-Saturate" => 128,     // Large block operations
            "Mem-BlockMove" => 64,                  // Block operations
            _ => panic!("Unknown test '{}' - add explicit SIMD requirement to calculate_minimum_chunk_size()", test_name),
        };
        
        // Stream division requirement: each stream needs at least simd_requirement bytes
        let stream_requirement = simd_requirement * streams.max(1) as usize;
        
        // Performance minimum: 64KB for reasonable cache behavior
        let performance_minimum = 64 * 1024; // 64KB
        
        // Ensure result is aligned to u64 boundaries and power-of-2 element count for fast stream operations
        let minimum_bytes = stream_requirement.max(performance_minimum);
        let elements = minimum_bytes / std::mem::size_of::<u64>();
        let power_of_2_elements = elements.next_power_of_two();
        
        power_of_2_elements * std::mem::size_of::<u64>()
    }
    
    fn calculate_optimal_block_for_test(&self, test_name: &str, window_size: usize, cache_info: &CacheInfo) -> usize {
        match test_name {
            // SIMD tests need specific alignments
            "MirrorMove128" => {
                align_to_boundary(MB_16, 128 / 8) // 128-bit alignment
            }
            "MirrorMove256" => {
                align_to_boundary(MB_32, 256 / 8) // 256-bit alignment  
            }
            "MirrorMove512" => {
                align_to_boundary(MB_64, 512 / 8) // 512-bit alignment
            }
            
            // Full memory tests should use large blocks for efficiency
            "StuckBitTest" | "FullMemoryPattern" => {
                align_to_boundary(window_size / 16, cache_info.cache_line_size).max(MB)
            }
            
            // Cache tests use cache-line aligned blocks
            "CacheBusting" => {
                align_to_boundary(MB, cache_info.cache_line_size)
            }
            
            // Small blocks for refresh testing
            "RefreshStable" => {
                align_to_boundary(512 * KB, cache_info.cache_line_size)
            }
            
            _ => {
                if self.allow_misaligned {
                    8 * MB // 8MB default unaligned
                } else {
                    align_to_boundary(8 * MB, cache_info.cache_line_size)
                }
            }
        }
    }

    // Ensure window size is multiple of block size
    pub fn align_window_to_blocks(&self, window_size: usize, block_size: usize) -> usize {
        if block_size == 0 || window_size == 0 {
            return window_size;
        }
        
        let aligned_window = (window_size / block_size) * block_size;
        let _was_adjusted = aligned_window != window_size;
        
        // Ensure we have at least one full block
        
        
        if aligned_window < block_size {
            block_size
        } else {
            aligned_window
        }
    }
}

fn align_to_boundary(size: usize, alignment: usize) -> usize {
    size.div_ceil(alignment) * alignment
}

/// Round down to nearest power of 2 (for optimal bit masking in hot loops)
pub fn round_down_to_power_of_2(size: usize) -> usize {
    if size == 0 { return 0; }
    if size.is_power_of_two() { return size; }
    
    // Find the highest set bit position
    let mut v = size;
    v |= v >> 1;
    v |= v >> 2;
    v |= v >> 4;
    v |= v >> 8;
    v |= v >> 16;
    if std::mem::size_of::<usize>() > 4 {
        v |= v >> 32;
    }
    // v is now the next power of 2 minus 1
    // The power of 2 we want is (v + 1) >> 1
    (v + 1) >> 1
}

/// Calculate ideal chunk size once at test start (power-of-2 elements for fast stream operations)
/// Block sizes are already guaranteed to be powers-of-2, so we just need to ensure chunk elements are power-of-2
pub fn calculate_ideal_chunk_size(config: &TestMemoryConfig, test_name: &str, total_memory_size: usize) -> usize {
    // Get base chunk size from user configuration
    let base_chunk_size = config.calculate_chunk_size(test_name, total_memory_size);
    
    // Convert to elements and ensure power-of-2 for fast stream operations (no remainder calculations needed)
    let base_elements = base_chunk_size / std::mem::size_of::<u64>();
    let power_of_2_elements = base_elements.next_power_of_two();
    
    power_of_2_elements * std::mem::size_of::<u64>()
}

/// Simple clamp chunk size to current block size (blocks are already power-of-2)
#[inline]
pub fn get_safe_chunk_size(ideal_chunk_size: usize, block_size_bytes: usize) -> usize {
    ideal_chunk_size.min(block_size_bytes)
}

/// Represents a block with window-limited test size
/// Used by MultiBlock tests to respect window limits while maintaining power-of-2 alignment
#[derive(Debug)]
pub struct TestBlock<'a> {
    pub block: &'a crate::runner::AllocationBlock,
    pub test_size: usize,  // How much to test (power-of-2, <= block size)
}

/// Prepare blocks for testing with window size limits
/// Returns a list of blocks with their test sizes, ensuring:
/// - Only complete blocks are tested (never split blocks)
/// - Blocks are added until total meets or exceeds window_size
/// - At least one block is tested (even if window < block size)
pub fn prepare_blocks_for_window<'a>(
    blocks: &'a [crate::runner::AllocationBlock],
    window_size: usize,
    test_name: &str,
) -> Vec<TestBlock<'a>> {
    if blocks.is_empty() {
        return Vec::new();
    }

    let mut result = Vec::new();
    let mut accumulated = 0usize;

    // Add complete blocks until we meet or exceed the window size
    // Blocks are pre-sorted by allocator (largest first)
    for block in blocks.iter() {
        let block_size = block.buffer.size();

        // Always test complete blocks (never split)
        result.push(TestBlock {
            block,
            test_size: block_size,
        });
        accumulated += block_size;

        log::debug!(
            "{}: Block {} - size {:.2} MiB, testing complete block",
            test_name,
            result.len() - 1,
            block_size as f64 / MB_F64
        );

        // Stop when we've met or exceeded the window
        if accumulated >= window_size {
            break;
        }
    }

    // Log window behavior - use INFO level so users can see what's happening
    if accumulated > window_size {
        log::info!(
            "{}: Window {:.2} MiB exceeded by {:.2} MiB (testing complete blocks only)",
            test_name,
            window_size as f64 / MB_F64,
            (accumulated - window_size) as f64 / MB_F64
        );
    }

    log::info!(
        "{}: Prepared {} block(s) for testing, total {:.2} MiB (window: {:.2} MiB)",
        test_name,
        result.len(),
        accumulated as f64 / MB_F64,
        window_size as f64 / MB_F64
    );

    result
}

/// Expand total_operations into detailed operation breakdown using test metadata
pub fn expand_operations(
    test_stats: &TestStats,
    metadata: &OperationMetadata,
    memory_size: usize,
) -> DetailedOperationCount {
    // Calculate how many elements were processed based on memory coverage
    let total_elements = memory_size / std::mem::size_of::<u64>();
    let covered_elements = (total_elements as f64 * metadata.memory_coverage) as u64;
    
    // For most tests, total_operations represents total loop iterations
    // Each iteration processes `covered_elements` worth of operations
    let operations_per_element = if covered_elements > 0 {
        test_stats.total_operations / covered_elements.max(1)
    } else {
        test_stats.total_operations
    };
    
    DetailedOperationCount {
        total_reads: operations_per_element * metadata.reads_per_op * covered_elements,
        total_writes: operations_per_element * metadata.writes_per_op * covered_elements,
        total_verifies: operations_per_element * metadata.verifies_per_op * covered_elements,
        total_simd_ops: operations_per_element * metadata.simd_ops_per_op * covered_elements,
        total_fence_ops: operations_per_element * metadata.fence_ops_per_op,  // Per iteration, not per element
        total_cache_ops: operations_per_element * metadata.cache_ops_per_op * covered_elements,
        simd_type: metadata.simd_type.clone(),
        access_pattern: metadata.access_pattern.clone(),
    }
}

/// Aggregate operation counts from multiple threads
pub fn aggregate_operation_counts(counts: &[DetailedOperationCount]) -> DetailedOperationCount {
    if counts.is_empty() {
        return DetailedOperationCount {
            total_reads: 0,
            total_writes: 0,
            total_verifies: 0,
            total_simd_ops: 0,
            total_fence_ops: 0,
            total_cache_ops: 0,
            simd_type: SIMDType::None,
            access_pattern: AccessPattern::Sequential,
        };
    }
    
    let mut aggregated = DetailedOperationCount {
        total_reads: counts.iter().map(|c| c.total_reads).sum(),
        total_writes: counts.iter().map(|c| c.total_writes).sum(),
        total_verifies: counts.iter().map(|c| c.total_verifies).sum(),
        total_simd_ops: counts.iter().map(|c| c.total_simd_ops).sum(),
        total_fence_ops: counts.iter().map(|c| c.total_fence_ops).sum(),
        total_cache_ops: counts.iter().map(|c| c.total_cache_ops).sum(),
        simd_type: counts[0].simd_type.clone(),  // Use first thread's SIMD type
        access_pattern: counts[0].access_pattern.clone(),  // Use first thread's access pattern
    };
    
    // If threads have different SIMD types, use the most advanced one
    for count in counts {
        aggregated.simd_type = match (&aggregated.simd_type, &count.simd_type) {
            (SIMDType::None, other) => other.clone(),
            (_current, SIMDType::AVX512_512) => SIMDType::AVX512_512,
            (SIMDType::SSE2_128, SIMDType::AVX2_256) => SIMDType::AVX2_256,
            (current, _) => current.clone(),
        };
    }
    
    aggregated
}


// === NEW: Full Memory Stuck Bit Test ===
/// # Safety
/// Caller must ensure `ptr` is valid for reads/writes of `size` bytes.

/// # Safety
/// Caller must ensure all blocks are valid for reads/writes.
///
/// StuckBitTest MultiBlock implementation - tests for stuck bits using alternating patterns.
///
/// 3-Phase Pattern per cycle:
/// - Phase 1: Write 0xAAAAAAAAAAAAAAAA (10101010...) → Verify
/// - Phase 2: Write 0x5555555555555555 (01010101...) → Verify
/// - Phase 3: Write 0xAAAAAAAAAAAAAAAA (10101010...) → Verify
///
/// Tests blocks in interleaved fashion with shared timer to fix N×duration bug.
pub unsafe fn stuck_bit_test_multi(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> TestStats {
    let test_name = "Mem-StuckBit";
    let start = Instant::now();

    // Calculate total allocated memory
    let total_allocated: usize = blocks.iter().map(|b| b.buffer.size()).sum();

    // Window preparation - determines which blocks to test
    let window_size = config.calculate_window_size(test_name, total_allocated);
    let test_blocks = prepare_blocks_for_window(blocks, window_size, test_name);

    let total_test_size: usize = test_blocks.iter().map(|b| b.test_size).sum();

    log::info!("[Thread {}] Running {} on {:.2} MB of memory (window: {:.2} MB)",
              thread_id, test_name,
              total_test_size as f64 / MB_F64,
              window_size as f64 / MB_F64);

    let mut cycle = 0u32;
    let test_start = Instant::now();
    let mut total_error_count = 0u64;
    let mut total_bytes_processed = 0usize;

    let mut last_progress_update = Instant::now();

    loop {
        cycle += 1;
        let mut cycle_errors = 0u64;

        // Interleave testing across all blocks with shared timer
        for test_block in test_blocks.iter() {
            let base = test_block.block.buffer.as_mut_ptr() as *mut u64;
            let len = test_block.test_size / std::mem::size_of::<u64>();

            // Recalculate chunk size for THIS block's size
            let ideal_chunk_size = calculate_ideal_chunk_size(config, test_name, test_block.test_size);
            let chunk_size_bytes = get_safe_chunk_size(ideal_chunk_size, test_block.test_size);
            let chunk_size_operations = (chunk_size_bytes / std::mem::size_of::<u64>()).max(1024);

            // Process this block in chunks
            let mut processed = 0;
            while processed < len {
                let chunk_end = (processed + chunk_size_operations).min(len);

                // Phase 1: Write 0xAAAAAAAAAAAAAAAA (10101010...)
                let pattern1 = 0xAAAAAAAAAAAAAAAAu64;
                for i in processed..chunk_end {
                    *base.add(i) = pattern1;
                }

                std::sync::atomic::fence(Ordering::SeqCst);

                // Phase 1 Verify
                for i in processed..chunk_end {
                    let v = *base.add(i);
                    if v != pattern1 {
                        cycle_errors += 1;
                        log::error!("{}: Phase 1 memory error at index {} - expected {:#x}, got {:#x}",
                                   test_name, i, pattern1, v);
                    }
                }

                // Phase 2: Write 0x5555555555555555 (01010101...)
                let pattern2 = 0x5555555555555555u64;
                for i in processed..chunk_end {
                    *base.add(i) = pattern2;
                }

                std::sync::atomic::fence(Ordering::SeqCst);

                // Phase 2 Verify
                for i in processed..chunk_end {
                    let v = *base.add(i);
                    if v != pattern2 {
                        cycle_errors += 1;
                        log::error!("{}: Phase 2 memory error at index {} - expected {:#x}, got {:#x}",
                                   test_name, i, pattern2, v);
                    }
                }

                // Phase 3: Write back to 0xAAAAAAAAAAAAAAAA
                for i in processed..chunk_end {
                    *base.add(i) = pattern1;
                }

                std::sync::atomic::fence(Ordering::SeqCst);

                // Phase 3 Verify
                for i in processed..chunk_end {
                    let v = *base.add(i);
                    if v != pattern1 {
                        cycle_errors += 1;
                        log::error!("{}: Phase 3 memory error at index {} - expected {:#x}, got {:#x}",
                                   test_name, i, pattern1, v);
                    }
                }

                // Handle errors if found
                if cycle_errors > 0 {
                    match error_mode {
                        ErrorMode::Panic => {
                            panic!("{}: panicking due to {} memory errors in cycle {}",
                                  test_name, cycle_errors, cycle);
                        }
                        ErrorMode::Halt => {
                            let elapsed = start.elapsed().as_millis();
                            let total_operations = (total_bytes_processed / std::mem::size_of::<u64>()) as u64;

                            return TestStats {
                                name: test_name,
                                action: TestAction::StuckBitTest,
                                bytes_processed: total_bytes_processed,
                                elapsed_ms: elapsed,
                                thread_id,
                                error_count: total_error_count + cycle_errors,
                                total_operations,
                                cycles_completed: cycle,
                                cycles_planned: timing.cycles,
                                stopped_by_time_limit: false,
                            };
                        }
                        ErrorMode::Log => {
                            // Continue testing - errors already logged
                        }
                    }
                }

                processed = chunk_end;

                // Check for shutdown request
                if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
                    let elapsed = start.elapsed().as_millis();
                    let total_operations = (total_bytes_processed / std::mem::size_of::<u64>()) as u64;

                    return TestStats {
                        name: test_name,
                        action: TestAction::StuckBitTest,
                        bytes_processed: total_bytes_processed,
                        elapsed_ms: elapsed,
                        thread_id,
                        error_count: total_error_count + cycle_errors,
                        total_operations,
                        cycles_completed: cycle,
                        cycles_planned: timing.cycles,
                        stopped_by_time_limit: false,
                    };
                }
            }

            // Update bytes processed for this block (3 writes + 3 reads)
            total_bytes_processed += test_block.test_size * 6;
        }

        total_error_count += cycle_errors;

        // Update progress tracker every 250ms
        if let Some(progress) = progress {
            let now = Instant::now();
            if now.duration_since(last_progress_update).as_millis() >= 250 {
                progress.cycles_completed.store(cycle, Ordering::Relaxed);
                progress.bytes_processed.store(total_bytes_processed as u64, Ordering::Relaxed);
                progress.errors_found.store(total_error_count, Ordering::Relaxed);
                progress.last_update_ms.store(start.elapsed().as_millis() as u64, Ordering::Relaxed);
                last_progress_update = now;
            }
        }

        // Check if should continue based on timing
        let elapsed_secs = test_start.elapsed().as_secs() as u32;
        if !timing.should_continue(cycle, elapsed_secs) {
            let elapsed = start.elapsed().as_millis();
            let total_operations = (total_bytes_processed / std::mem::size_of::<u64>()) as u64;

            return TestStats {
                name: test_name,
                action: TestAction::StuckBitTest,
                bytes_processed: total_bytes_processed,
                elapsed_ms: elapsed,
                thread_id,
                error_count: total_error_count,
                total_operations,
                cycles_completed: cycle,
                cycles_planned: timing.cycles,
                stopped_by_time_limit: timing.cycles.map_or(true, |limit| cycle < limit),
            };
        }
    }
}

/// # Safety
/// Caller must ensure all blocks are valid for reads/writes.
///
/// StuckBitTest128 MultiBlock implementation (SSE2) - tests for stuck bits using alternating patterns.
/// Preserves EXACT SSE2 algorithm from old version.
/// StuckBitTest128 - SSE2 optimized - MultiBlock pattern
/// Runtime-checked wrapper for SSE2 availability
pub unsafe fn stuck_bit_test_128_multi(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> TestStats {
    let test_name = "Mem-StuckBit128";

    // Runtime CPU feature check
    if !is_x86_feature_detected!("sse2") {
        log::warn!("{}: SSE2 not available, returning zero stats", test_name);
        return TestStats {
            name: test_name,
            action: TestAction::StuckBitTest,
            bytes_processed: 0,
            elapsed_ms: 0,
            thread_id,
            error_count: 0,
            total_operations: 0,
            cycles_completed: 0,
            cycles_planned: timing.cycles,
            stopped_by_time_limit: false,
        };
    }

    // Call the SSE2 optimized implementation
    stuck_bit_test_128_impl(blocks, thread_id, error_mode, timing, config, progress)
}

/// StuckBitTest128 - SSE2 optimized implementation
/// This function is annotated with #[target_feature] to enable full compiler optimization
#[inline]
#[target_feature(enable = "sse2")]
unsafe fn stuck_bit_test_128_impl(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> TestStats {

    let test_name = "Mem-StuckBit128";

    let start = Instant::now();

    // Calculate total allocated memory
    let total_allocated: usize = blocks.iter().map(|b| b.buffer.size()).sum();

    // Window preparation
    let window_size = config.calculate_window_size(test_name, total_allocated);
    let test_blocks = prepare_blocks_for_window(blocks, window_size, test_name);

    let total_test_size: usize = test_blocks.iter().map(|b| b.test_size).sum();

    log::info!("[Thread {}] Running {} on {:.2} MB of memory (window: {:.2} MB)",
              thread_id, test_name,
              total_test_size as f64 / MB_F64,
              window_size as f64 / MB_F64);

    let mut cycle = 0u32;
    let test_start = Instant::now();
    let mut total_error_count = 0u64;
    let mut total_bytes_processed = 0usize;
    let mut last_progress_update = Instant::now();

    // Initialize patterns ONCE outside loop for performance
    let pattern1 = u64x2::splat(0xAAAAAAAAAAAAAAAAu64);
    let pattern2 = u64x2::splat(0x5555555555555555u64);

    loop {
        cycle += 1;
        let mut cycle_errors = 0u64;

        // Interleave testing across all blocks with shared timer
        for test_block in test_blocks.iter() {
            let base = test_block.block.buffer.as_mut_ptr() as *mut u64x2;
            let len = test_block.test_size / std::mem::size_of::<u64x2>();

            // Recalculate chunk size for THIS block's size
            let ideal_chunk_size = calculate_ideal_chunk_size(config, test_name, test_block.test_size);
            let chunk_size_bytes = get_safe_chunk_size(ideal_chunk_size, test_block.test_size);
            let chunk_size_operations_raw = chunk_size_bytes / std::mem::size_of::<u64x2>();
            let chunk_size_operations = chunk_size_operations_raw.max(1024);

            let error_check_mode = match config.error_check_interval.get_check_mask() {
                Some(mask) => format!("Intermediate (mask={})", mask),
                None => "PER_CHUNK".to_string(),
            };
            log::debug!("{}: Thread {} - chunk_size_bytes={}, ops_raw={}, ops_after_max={} (actual bytes={}), error_check_mode={}",
                       test_name, thread_id, chunk_size_bytes,
                       chunk_size_operations_raw, chunk_size_operations,
                       chunk_size_operations * std::mem::size_of::<u64x2>(),
                       error_check_mode);

            // Process this block in chunks
            let mut processed = 0;
            while processed < len {
                let chunk_end = (processed + chunk_size_operations).min(len);

                // Phase 1: Write 0xAAAA pattern using SSE2
                for i in processed..chunk_end {
                    *base.add(i) = pattern1;
                }

                std::sync::atomic::fence(Ordering::SeqCst);

                // Phase 1 Verify - accumulator pattern with configurable error checking
                let mut error_accumulator = u64x2::splat(0);
                let mut element_count = 0usize;

                // Pre-compute check mask for hot loop optimization
                match config.error_check_interval.get_check_mask() {
                    Some(check_mask) => {
                        for i in processed..chunk_end {
                            let value = *base.add(i);
                            let diff = value ^ pattern1 ;
                            error_accumulator = error_accumulator | diff ;

                            element_count += 1;

                            // Check errors at configured intervals (zero-branch hot loop optimization)
                            if (element_count as u32 & check_mask) == 0 {
                                if error_accumulator.simd_ne(u64x2::splat(0)).any() {
                                    cycle_errors += 1;
                                    log::error!("{}: memory error detected in phase 1 element {} (thread {})",
                                               test_name, element_count, thread_id);
                                    error_accumulator = u64x2::splat(0);
                                }
                            }
                        }
                    }
                    None => {
                        // PER_CHUNK mode - no intermediate checks, maximum performance
                        for i in processed..chunk_end {
                            let value = *base.add(i);
                            let diff = value ^ pattern1 ;
                            error_accumulator = error_accumulator | diff ;
                        }
                    }
                }

                // Final error check for phase 1 (always performed regardless of mode)
                if error_accumulator.simd_ne(u64x2::splat(0)).any() {
                    cycle_errors += 1;
                    log::error!("{}: memory error detected in phase 1 chunk (thread {})", test_name, thread_id);
                }

                // Phase 2: Write 0x5555 pattern using SSE2
                for i in processed..chunk_end {
                    *base.add(i) = pattern2;
                }

                std::sync::atomic::fence(Ordering::SeqCst);

                // Phase 2 Verify - accumulator pattern with configurable error checking
                let mut error_accumulator = u64x2::splat(0);
                let mut element_count = 0usize;

                // Pre-compute check mask for hot loop optimization
                match config.error_check_interval.get_check_mask() {
                    Some(check_mask) => {
                        for i in processed..chunk_end {
                            let value = *base.add(i);
                            let diff = value ^ pattern2 ;
                            error_accumulator = error_accumulator | diff ;

                            element_count += 1;

                            // Check errors at configured intervals (zero-branch hot loop optimization)
                            if (element_count as u32 & check_mask) == 0 {
                                if error_accumulator.simd_ne(u64x2::splat(0)).any() {
                                    cycle_errors += 1;
                                    log::error!("{}: memory error detected in phase 2 element {} (thread {})",
                                               test_name, element_count, thread_id);
                                    error_accumulator = u64x2::splat(0);
                                }
                            }
                        }
                    }
                    None => {
                        // PER_CHUNK mode - no intermediate checks, maximum performance
                        for i in processed..chunk_end {
                            let value = *base.add(i);
                            let diff = value ^ pattern2 ;
                            error_accumulator = error_accumulator | diff ;
                        }
                    }
                }

                // Final error check for phase 2 (always performed regardless of mode)
                if error_accumulator.simd_ne(u64x2::splat(0)).any() {
                    cycle_errors += 1;
                    log::error!("{}: memory error detected in phase 2 chunk (thread {})", test_name, thread_id);
                }

                // Phase 3: Write back to 0xAAAA pattern
                for i in processed..chunk_end {
                    *base.add(i) = pattern1;
                }

                std::sync::atomic::fence(Ordering::SeqCst);

                // Phase 3 Verify - accumulator pattern with configurable error checking
                let mut error_accumulator = u64x2::splat(0);
                let mut element_count = 0usize;

                // Pre-compute check mask for hot loop optimization
                match config.error_check_interval.get_check_mask() {
                    Some(check_mask) => {
                        for i in processed..chunk_end {
                            let value = *base.add(i);
                            let diff = value ^ pattern1 ;
                            error_accumulator = error_accumulator | diff ;

                            element_count += 1;

                            // Check errors at configured intervals (zero-branch hot loop optimization)
                            if (element_count as u32 & check_mask) == 0 {
                                if error_accumulator.simd_ne(u64x2::splat(0)).any() {
                                    cycle_errors += 1;
                                    log::error!("{}: memory error detected in phase 3 element {} (thread {})",
                                               test_name, element_count, thread_id);
                                    error_accumulator = u64x2::splat(0);
                                }
                            }
                        }
                    }
                    None => {
                        // PER_CHUNK mode - no intermediate checks, maximum performance
                        for i in processed..chunk_end {
                            let value = *base.add(i);
                            let diff = value ^ pattern1 ;
                            error_accumulator = error_accumulator | diff ;
                        }
                    }
                }

                // Final error check for phase 3 (always performed regardless of mode)
                if error_accumulator.simd_ne(u64x2::splat(0)).any() {
                    cycle_errors += 1;
                    log::error!("{}: memory error detected in phase 3 chunk (thread {})", test_name, thread_id);
                }

                processed = chunk_end;

                // Optimized error handling - check ONCE at end of chunk (not between phases)
                if cycle_errors > 0 {
                    match error_mode {
                        ErrorMode::Panic => panic!("{}: {} memory errors detected (see logs above)", test_name, cycle_errors),
                        ErrorMode::Halt => {
                            let elapsed = start.elapsed().as_millis();
                            let total_operations = (total_bytes_processed / std::mem::size_of::<u64x2>()) as u64;
                            return TestStats {
                                name: test_name, action: TestAction::StuckBitTest,
                                bytes_processed: total_bytes_processed, elapsed_ms: elapsed, thread_id,
                                error_count: total_error_count + cycle_errors, total_operations,
                                cycles_completed: cycle, cycles_planned: timing.cycles, stopped_by_time_limit: false,
                            };
                        }
                        ErrorMode::Log => { /* Continue - already logged above */ }
                    }
                }

                // Check for shutdown request after processing each chunk
                if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
                    // Calculate partial bytes processed for early exit: 3 writes + 3 reads per chunk
                    let partial_cycle_bytes = processed * std::mem::size_of::<u64x2>() * 6;
                    total_bytes_processed += partial_cycle_bytes;
                    total_error_count += cycle_errors;

                    let elapsed = start.elapsed().as_millis();
                    let total_operations: u64 = ((cycle - 1) as u64 * len as u64) + processed as u64;

                    return TestStats {
                        name: test_name, action: TestAction::StuckBitTest,
                        bytes_processed: total_bytes_processed, elapsed_ms: elapsed, thread_id,
                        error_count: total_error_count, total_operations,
                        cycles_completed: cycle, cycles_planned: timing.cycles, stopped_by_time_limit: false,
                    };
                }
            }

            // Update bytes processed for this block (3 writes + 3 reads per cycle)
            total_bytes_processed += test_block.test_size * 6;
        }

        // Handle errors according to error mode
        if cycle_errors > 0 {
            match error_mode {
                ErrorMode::Panic => {
                    panic!("{}: memory error detected in cycle {} (thread {})", test_name, cycle, thread_id);
                }
                ErrorMode::Halt => {
                    let elapsed = test_start.elapsed().as_millis();
                    total_error_count += cycle_errors;
                    let total_operations = (total_bytes_processed / std::mem::size_of::<u64x2>()) as u64;
                    return TestStats {
                        name: test_name, action: TestAction::StuckBitTest,
                        bytes_processed: total_bytes_processed, elapsed_ms: elapsed, thread_id,
                        error_count: total_error_count, total_operations,
                        cycles_completed: cycle, cycles_planned: timing.cycles,
                        stopped_by_time_limit: false,
                    };
                }
                ErrorMode::Log => {
                    // Errors already logged, continue testing
                }
            }
        }

        total_error_count += cycle_errors;

        // Update progress tracker every 250ms
        if let Some(progress) = progress {
            let now = Instant::now();
            if now.duration_since(last_progress_update).as_millis() >= 250 {
                progress.cycles_completed.store(cycle, Ordering::Relaxed);
                progress.bytes_processed.store(total_bytes_processed as u64, Ordering::Relaxed);
                progress.errors_found.store(total_error_count, Ordering::Relaxed);
                progress.last_update_ms.store(start.elapsed().as_millis() as u64, Ordering::Relaxed);
                last_progress_update = now;
            }
        }

        // Check timing/cycles
        let elapsed_secs = test_start.elapsed().as_secs() as u32;
        if !timing.should_continue(cycle, elapsed_secs) {
            let elapsed = start.elapsed().as_millis();

            // Calculate total operations (total elements processed across all blocks)
            let total_operations = (total_bytes_processed / std::mem::size_of::<u64x2>()) as u64;

            return TestStats {
                name: test_name, action: TestAction::StuckBitTest,
                bytes_processed: total_bytes_processed, elapsed_ms: elapsed, thread_id,
                error_count: total_error_count, total_operations,
                cycles_completed: cycle, cycles_planned: timing.cycles,
                stopped_by_time_limit: timing.cycles.map_or(true, |limit| cycle < limit),
            };
        }
    }
}

/// StuckBitTest256 - AVX2 optimized - MultiBlock pattern
/// Runtime-checked wrapper for AVX2 availability
#[allow(clippy::missing_safety_doc)]
pub unsafe fn stuck_bit_test_256_multi(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> TestStats {
    let test_name = "Mem-StuckBit256";

    // Runtime CPU feature check
    if !is_x86_feature_detected!("avx2") {
        log::warn!("{}: AVX2 not available, returning zero stats", test_name);
        return TestStats {
            name: test_name, action: TestAction::StuckBitTest,
            bytes_processed: 0, elapsed_ms: 0, thread_id,
            error_count: 0, total_operations: 0,
            cycles_completed: 0, cycles_planned: timing.cycles,
            stopped_by_time_limit: false,
        };
    }

    // Call the AVX2 optimized implementation
    stuck_bit_test_256_impl(blocks, thread_id, error_mode, timing, config, progress)
}

/// StuckBitTest256 - AVX2 optimized implementation
/// This function is annotated with #[target_feature] to enable full compiler optimization
#[inline]
#[target_feature(enable = "avx2")]
unsafe fn stuck_bit_test_256_impl(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> TestStats {
    use std::sync::atomic::Ordering;
    use std::time::Instant;

    let test_name = "Mem-StuckBit256";

    let total_allocated: usize = blocks.iter().map(|b| b.buffer.size()).sum();
    let window_size = config.calculate_window_size(test_name, total_allocated);
    let test_blocks = prepare_blocks_for_window(blocks, window_size, test_name);

    let start = Instant::now();
    let test_start = Instant::now();
    let mut total_bytes_processed = 0usize;
    let mut total_error_count = 0u64;
    let mut last_progress_update = Instant::now();

    let pattern1 = u64x4::splat(0xAAAAAAAAAAAAAAAAu64);
    let pattern2 = u64x4::splat(0x5555555555555555u64);

    let mut cycle = 0u32;
    loop {
        cycle += 1;  // BUG FIX #2: Use 1-indexed cycles like StuckBitTest128

        if crate::runner::SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
            break;
        }

        let mut cycle_errors = 0u64;

        for test_block in test_blocks.iter() {
            let base = test_block.block.buffer.as_mut_ptr() as *mut u64x4;
            let len = test_block.test_size / std::mem::size_of::<u64x4>();
            let ideal_chunk_size = calculate_ideal_chunk_size(config, test_name, test_block.test_size);
            let chunk_size_bytes = get_safe_chunk_size(ideal_chunk_size, test_block.test_size);
            let chunk_size_operations_raw = chunk_size_bytes / std::mem::size_of::<u64x4>();
            let chunk_size_operations = chunk_size_operations_raw.max(1024);

            let error_check_mode = match config.error_check_interval.get_check_mask() {
                Some(mask) => format!("Intermediate (mask={})", mask),
                None => "PER_CHUNK".to_string(),
            };
            log::debug!("{}: Thread {} - chunk_size_bytes={}, ops_raw={}, ops_after_max={} (actual bytes={}), error_check_mode={}",
                       test_name, thread_id, chunk_size_bytes,
                       chunk_size_operations_raw, chunk_size_operations,
                       chunk_size_operations * std::mem::size_of::<u64x4>(),
                       error_check_mode);

            let mut processed = 0;
            while processed < len {
                let chunk_end = (processed + chunk_size_operations).min(len);

                // Phase 1: Write 0xAAAA
                for i in processed..chunk_end {
                    *base.add(i) = pattern1;
                }
                std::sync::atomic::fence(Ordering::SeqCst); // BUG FIX: Added missing fence

                // Phase 1 Verify
                let mut error_accumulator = u64x4::splat(0);
                let mut element_count = 0usize;
                match config.error_check_interval.get_check_mask() {
                    Some(check_mask) => {
                        for i in processed..chunk_end {
                            let value = *base.add(i);
                            let diff = value ^ pattern1 ;
                            error_accumulator = error_accumulator | diff ;
                            element_count += 1;
                            if (element_count as u32 & check_mask) == 0 {
                                if error_accumulator.simd_ne(u64x4::splat(0)).any() {
                                    cycle_errors += 1;
                                    error_accumulator = u64x4::splat(0);
                                }
                            }
                        }
                    }
                    None => {
                        for i in processed..chunk_end {
                            let value = *base.add(i);
                            let diff = value ^ pattern1 ;
                            error_accumulator = error_accumulator | diff ;
                        }
                    }
                }
                if error_accumulator.simd_ne(u64x4::splat(0)).any() {
                    cycle_errors += 1;
                }

                // Phase 2: Write 0x5555
                for i in processed..chunk_end {
                    *base.add(i) = pattern2;
                }
                std::sync::atomic::fence(Ordering::SeqCst); // BUG FIX: Added missing fence

                // Phase 2 Verify
                error_accumulator = u64x4::splat(0);
                element_count = 0;
                match config.error_check_interval.get_check_mask() {
                    Some(check_mask) => {
                        for i in processed..chunk_end {
                            let value = *base.add(i);
                            let diff = value ^ pattern2 ;
                            error_accumulator = error_accumulator | diff ;
                            element_count += 1;
                            if (element_count as u32 & check_mask) == 0 {
                                if error_accumulator.simd_ne(u64x4::splat(0)).any() {
                                    cycle_errors += 1;
                                    error_accumulator = u64x4::splat(0);
                                }
                            }
                        }
                    }
                    None => {
                        for i in processed..chunk_end {
                            let value = *base.add(i);
                            let diff = value ^ pattern2 ;
                            error_accumulator = error_accumulator | diff ;
                        }
                    }
                }
                if error_accumulator.simd_ne(u64x4::splat(0)).any() {
                    cycle_errors += 1;
                }

                // Phase 3: Write back to 0xAAAA
                for i in processed..chunk_end {
                    *base.add(i) = pattern1;
                }
                std::sync::atomic::fence(Ordering::SeqCst);

                // Phase 3 Verify
                error_accumulator = u64x4::splat(0);
                element_count = 0;
                match config.error_check_interval.get_check_mask() {
                    Some(check_mask) => {
                        for i in processed..chunk_end {
                            let value = *base.add(i);
                            let diff = value ^ pattern1 ;
                            error_accumulator = error_accumulator | diff ;
                            element_count += 1;
                            if (element_count as u32 & check_mask) == 0 {
                                if error_accumulator.simd_ne(u64x4::splat(0)).any() {
                                    cycle_errors += 1;
                                    error_accumulator = u64x4::splat(0);
                                }
                            }
                        }
                    }
                    None => {
                        for i in processed..chunk_end {
                            let value = *base.add(i);
                            let diff = value ^ pattern1 ;
                            error_accumulator = error_accumulator | diff ;
                        }
                    }
                }
                if error_accumulator.simd_ne(u64x4::splat(0)).any() {
                    cycle_errors += 1;
                }

                processed = chunk_end;
            }

            total_bytes_processed += test_block.test_size * 6;
        }

        // Handle errors according to error mode
        if cycle_errors > 0 {
            match error_mode {
                ErrorMode::Panic => {
                    panic!("{}: memory error detected in cycle {} (thread {})", test_name, cycle, thread_id);
                }
                ErrorMode::Halt => {
                    let elapsed = test_start.elapsed().as_millis();
                    total_error_count += cycle_errors;
                    let total_operations = (total_bytes_processed / std::mem::size_of::<u64x4>()) as u64;
                    return TestStats {
                        name: test_name, action: TestAction::StuckBitTest,
                        bytes_processed: total_bytes_processed, elapsed_ms: elapsed, thread_id,
                        error_count: total_error_count, total_operations,
                        cycles_completed: cycle, cycles_planned: timing.cycles,
                        stopped_by_time_limit: false,
                    };
                }
                ErrorMode::Log => {
                    // Errors already logged, continue testing
                }
            }
        }

        total_error_count += cycle_errors;

        if let Some(progress) = progress {
            let now = Instant::now();
            if now.duration_since(last_progress_update).as_millis() >= 250 {
                progress.cycles_completed.store(cycle, Ordering::Relaxed);
                progress.bytes_processed.store(total_bytes_processed as u64, Ordering::Relaxed);
                progress.errors_found.store(total_error_count, Ordering::Relaxed);
                progress.last_update_ms.store(start.elapsed().as_millis() as u64, Ordering::Relaxed);
                last_progress_update = now;
            }
        }

        let elapsed_secs = test_start.elapsed().as_secs() as u32;
        if !timing.should_continue(cycle, elapsed_secs) {
            let elapsed = start.elapsed().as_millis();
            let total_operations = (total_bytes_processed / std::mem::size_of::<u64x4>()) as u64;
            return TestStats {
                name: test_name, action: TestAction::StuckBitTest,
                bytes_processed: total_bytes_processed, elapsed_ms: elapsed, thread_id,
                error_count: total_error_count, total_operations,
                cycles_completed: cycle, cycles_planned: timing.cycles,
                stopped_by_time_limit: timing.cycles.map_or(true, |limit| cycle < limit),
            };
        }
    }

    let elapsed = start.elapsed().as_millis();
    let total_operations = (total_bytes_processed / std::mem::size_of::<u64x4>()) as u64;
    TestStats {
        name: test_name, action: TestAction::StuckBitTest,
        bytes_processed: total_bytes_processed, elapsed_ms: elapsed, thread_id,
        error_count: total_error_count, total_operations,
        cycles_completed: cycle, cycles_planned: timing.cycles,  // BUG FIX: Report actual cycles completed
        stopped_by_time_limit: false,
    }
}

/// StuckBitTest512 - AVX-512 optimized - MultiBlock pattern
/// Runtime-checked wrapper for AVX-512 availability
#[allow(clippy::missing_safety_doc)]
pub unsafe fn stuck_bit_test_512_multi(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> TestStats {
    let test_name = "Mem-StuckBit512";

    // Runtime CPU feature check
    if !is_x86_feature_detected!("avx512f") {
        log::warn!("{}: AVX-512 not available, returning zero stats", test_name);
        return TestStats {
            name: test_name, action: TestAction::StuckBitTest,
            bytes_processed: 0, elapsed_ms: 0, thread_id,
            error_count: 0, total_operations: 0,
            cycles_completed: 0, cycles_planned: timing.cycles,
            stopped_by_time_limit: false,
        };
    }

    // Call the AVX-512 optimized implementation
    stuck_bit_test_512_impl(blocks, thread_id, error_mode, timing, config, progress)
}

/// StuckBitTest512 - AVX-512 optimized implementation
/// This function is annotated with #[target_feature] to enable full compiler optimization
#[inline]
#[target_feature(enable = "avx512f")]
unsafe fn stuck_bit_test_512_impl(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> TestStats {
    use std::sync::atomic::Ordering;
    use std::time::Instant;

    let test_name = "Mem-StuckBit512";

    let total_allocated: usize = blocks.iter().map(|b| b.buffer.size()).sum();
    let window_size = config.calculate_window_size(test_name, total_allocated);
    let test_blocks = prepare_blocks_for_window(blocks, window_size, test_name);

    let start = Instant::now();
    let test_start = Instant::now();
    let mut total_bytes_processed = 0usize;
    let mut total_error_count = 0u64;
    let mut last_progress_update = Instant::now();

    let pattern1 = u64x8::splat(0xAAAAAAAAAAAAAAAAu64);
    let pattern2 = u64x8::splat(0x5555555555555555u64);

    let mut cycle = 0u32;
    loop {
        cycle += 1;  // BUG FIX #2: Use 1-indexed cycles like StuckBitTest128

        if crate::runner::SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
            break;
        }

        let mut cycle_errors = 0u64;

        for test_block in test_blocks.iter() {
            let base = test_block.block.buffer.as_mut_ptr() as *mut u64x8;
            let len = test_block.test_size / std::mem::size_of::<u64x8>();
            let ideal_chunk_size = calculate_ideal_chunk_size(config, test_name, test_block.test_size);
            let chunk_size_bytes = get_safe_chunk_size(ideal_chunk_size, test_block.test_size);
            let chunk_size_operations_raw = chunk_size_bytes / std::mem::size_of::<u64x8>();
            let chunk_size_operations = chunk_size_operations_raw.max(1024);

            let error_check_mode = match config.error_check_interval.get_check_mask() {
                Some(mask) => format!("Intermediate (mask={})", mask),
                None => "PER_CHUNK".to_string(),
            };
            log::debug!("{}: Thread {} - chunk_size_bytes={}, ops_raw={}, ops_after_max={} (actual bytes={}), error_check_mode={}",
                       test_name, thread_id, chunk_size_bytes,
                       chunk_size_operations_raw, chunk_size_operations,
                       chunk_size_operations * std::mem::size_of::<u64x8>(),
                       error_check_mode);

            let mut processed = 0;
            while processed < len {
                let chunk_end = (processed + chunk_size_operations).min(len);

                // Phase 1: Write 0xAAAA
                for i in processed..chunk_end {
                    *base.add(i) = pattern1;
                }
                std::sync::atomic::fence(Ordering::SeqCst); // BUG FIX: Added missing fence

                // Phase 1 Verify
                let mut error_accumulator = u64x8::splat(0);
                let mut element_count = 0usize;
                match config.error_check_interval.get_check_mask() {
                    Some(check_mask) => {
                        for i in processed..chunk_end {
                            let value = *base.add(i);
                            let diff = value ^ pattern1 ;
                            error_accumulator = error_accumulator | diff ;
                            element_count += 1;
                            if (element_count as u32 & check_mask) == 0 {
                                if !error_accumulator.simd_eq(u64x8::splat(0)).all() { cycle_errors += 1; }
                                error_accumulator = u64x8::splat(0);
                            }
                        }
                    }
                    None => {
                        for i in processed..chunk_end {
                            let value = *base.add(i);
                            let diff = value ^ pattern1 ;
                            error_accumulator = error_accumulator | diff ;
                        }
                    }
                }
                if !error_accumulator.simd_eq(u64x8::splat(0)).all() { cycle_errors += 1; }

                // Phase 2: Write 0x5555
                for i in processed..chunk_end {
                    *base.add(i) = pattern2;
                }
                std::sync::atomic::fence(Ordering::SeqCst); // BUG FIX: Added missing fence

                // Phase 2 Verify
                error_accumulator = u64x8::splat(0);
                element_count = 0;
                match config.error_check_interval.get_check_mask() {
                    Some(check_mask) => {
                        for i in processed..chunk_end {
                            let value = *base.add(i);
                            let diff = value ^ pattern2 ;
                            error_accumulator = error_accumulator | diff ;
                            element_count += 1;
                            if (element_count as u32 & check_mask) == 0 {
                                if !error_accumulator.simd_eq(u64x8::splat(0)).all() { cycle_errors += 1; }
                                error_accumulator = u64x8::splat(0);
                            }
                        }
                    }
                    None => {
                        for i in processed..chunk_end {
                            let value = *base.add(i);
                            let diff = value ^ pattern2 ;
                            error_accumulator = error_accumulator | diff ;
                        }
                    }
                }
                if !error_accumulator.simd_eq(u64x8::splat(0)).all() { cycle_errors += 1; }

                // Phase 3: Write back to 0xAAAA
                for i in processed..chunk_end {
                    *base.add(i) = pattern1;
                }
                std::sync::atomic::fence(Ordering::SeqCst); // BUG FIX: Added missing fence

                // Phase 3 Verify
                error_accumulator = u64x8::splat(0);
                element_count = 0;
                match config.error_check_interval.get_check_mask() {
                    Some(check_mask) => {
                        for i in processed..chunk_end {
                            let value = *base.add(i);
                            let diff = value ^ pattern1 ;
                            error_accumulator = error_accumulator | diff ;
                            element_count += 1;
                            if (element_count as u32 & check_mask) == 0 {
                                if !error_accumulator.simd_eq(u64x8::splat(0)).all() { cycle_errors += 1; }
                                error_accumulator = u64x8::splat(0);
                            }
                        }
                    }
                    None => {
                        for i in processed..chunk_end {
                            let value = *base.add(i);
                            let diff = value ^ pattern1 ;
                            error_accumulator = error_accumulator | diff ;
                        }
                    }
                }
                if !error_accumulator.simd_eq(u64x8::splat(0)).all() { cycle_errors += 1; }

                processed = chunk_end;
            }

            total_bytes_processed += test_block.test_size * 6;
        }

        // Handle errors according to error mode
        if cycle_errors > 0 {
            match error_mode {
                ErrorMode::Panic => {
                    panic!("{}: memory error detected in cycle {} (thread {})", test_name, cycle, thread_id);
                }
                ErrorMode::Halt => {
                    let elapsed = test_start.elapsed().as_millis();
                    total_error_count += cycle_errors;
                    let total_operations = (total_bytes_processed / std::mem::size_of::<u64x8>()) as u64;
                    return TestStats {
                        name: test_name, action: TestAction::StuckBitTest,
                        bytes_processed: total_bytes_processed, elapsed_ms: elapsed, thread_id,
                        error_count: total_error_count, total_operations,
                        cycles_completed: cycle, cycles_planned: timing.cycles,
                        stopped_by_time_limit: false,
                    };
                }
                ErrorMode::Log => {
                    // Errors already logged, continue testing
                }
            }
        }

        total_error_count += cycle_errors;

        if let Some(progress) = progress {
            let now = Instant::now();
            if now.duration_since(last_progress_update).as_millis() >= 250 {
                progress.cycles_completed.store(cycle, Ordering::Relaxed);
                progress.bytes_processed.store(total_bytes_processed as u64, Ordering::Relaxed);
                progress.errors_found.store(total_error_count, Ordering::Relaxed);
                progress.last_update_ms.store(start.elapsed().as_millis() as u64, Ordering::Relaxed);
                last_progress_update = now;
            }
        }

        let elapsed_secs = test_start.elapsed().as_secs() as u32;
        if !timing.should_continue(cycle, elapsed_secs) {
            let elapsed = start.elapsed().as_millis();
            let total_operations = (total_bytes_processed / std::mem::size_of::<u64x8>()) as u64;
            return TestStats {
                name: test_name, action: TestAction::StuckBitTest,
                bytes_processed: total_bytes_processed, elapsed_ms: elapsed, thread_id,
                error_count: total_error_count, total_operations,
                cycles_completed: cycle, cycles_planned: timing.cycles,
                stopped_by_time_limit: timing.cycles.map_or(true, |limit| cycle < limit),
            };
        }
    }

    let elapsed = start.elapsed().as_millis();
    let total_operations = (total_bytes_processed / std::mem::size_of::<u64x8>()) as u64;
    TestStats {
        name: test_name, action: TestAction::StuckBitTest,
        bytes_processed: total_bytes_processed, elapsed_ms: elapsed, thread_id,
        error_count: total_error_count, total_operations,
        cycles_completed: cycle, cycles_planned: timing.cycles,  // BUG FIX: Report actual cycles completed
        stopped_by_time_limit: false,
    }
}

// === STUCK BIT TEST SIMD VARIANTS ===
/// # Safety
/// Caller must ensure `ptr` is valid for reads/writes of `size` bytes.

/// # Safety
/// Caller must ensure `ptr` is valid for reads/writes of `size` bytes.

/// # Safety
/// Caller must ensure `ptr` is valid for reads/writes of `size` bytes.

/// StuckBitTestAuto - MultiBlock dispatcher
/// Auto-selects best SIMD implementation: AVX-512 > AVX2 > SSE2 > scalar
///
/// # Safety
/// Caller must ensure blocks contain valid, aligned memory
#[allow(clippy::missing_safety_doc)]
pub unsafe fn stuck_bit_test_auto_multi(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> TestStats {
    // Check CPU capabilities and dispatch to the best available MultiBlock implementation
    if is_x86_feature_detected!("avx512f") {
        stuck_bit_test_512_multi(blocks, thread_id, error_mode, timing, config, progress)
    } else if is_x86_feature_detected!("avx2") {
        stuck_bit_test_256_multi(blocks, thread_id, error_mode, timing, config, progress)
    } else if is_x86_feature_detected!("sse2") {
        stuck_bit_test_128_multi(blocks, thread_id, error_mode, timing, config, progress)
    } else {
        // Fallback to scalar MultiBlock implementation
        stuck_bit_test_multi(blocks, thread_id, error_mode, timing, config, progress)
    }
}

/// Auto-dispatch wrapper that selects the best SIMD implementation based on CPU capabilities
/// # Safety
/// Caller must ensure `ptr` is valid for reads/writes of `size` bytes.

// ================================================================================================
// RefreshStable MultiBlock Implementations
// ================================================================================================

/// RefreshStable MultiBlock implementation - tests DRAM refresh stability.
///
/// Pattern per cycle:
/// - Write 0xA5A5A5A5A5A5A5A5 pattern
/// - Sleep 64ms (DRAM refresh cycle timing)
/// - Verify pattern unchanged
///
/// Tests blocks in interleaved fashion with shared timer to fix N×duration bug.
pub unsafe fn refresh_stable_multi(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> TestStats {
    let test_name = "Mem-Refresh";
    let start = Instant::now();

    // Calculate total allocated memory
    let total_allocated: usize = blocks.iter().map(|b| b.buffer.size()).sum();

    // Window preparation - determines which blocks to test
    let window_size = config.calculate_window_size(test_name, total_allocated);
    let test_blocks = prepare_blocks_for_window(blocks, window_size, test_name);

    let total_test_size: usize = test_blocks.iter().map(|b| b.test_size).sum();

    log::info!("[Thread {}] Running {} on {:.2} MB of memory (window: {:.2} MB)",
              thread_id, test_name,
              total_test_size as f64 / MB_F64,
              window_size as f64 / MB_F64);

    let mut cycle = 0u32;
    let test_start = Instant::now();
    let mut total_error_count = 0u64;
    let mut total_bytes_processed = 0usize;

    let mut last_progress_update = Instant::now();

    loop {
        cycle += 1;
        let mut cycle_errors = 0u64;

        // Interleave testing across all blocks with shared timer
        for test_block in test_blocks.iter() {
            let base = test_block.block.buffer.as_mut_ptr() as *mut u64;
            let len = test_block.test_size / std::mem::size_of::<u64>();

            // Recalculate chunk size for THIS block's size
            let ideal_chunk_size = calculate_ideal_chunk_size(config, test_name, test_block.test_size);
            let chunk_size_bytes = get_safe_chunk_size(ideal_chunk_size, test_block.test_size);
            let chunk_size_operations = (chunk_size_bytes / std::mem::size_of::<u64>()).max(1024);

            // Process this block in chunks
            let mut processed = 0;
            while processed < len {
                let chunk_end = (processed + chunk_size_operations).min(len);

                // Write phase: 0xA5A5A5A5A5A5A5A5 pattern
                let pattern = 0xA5A5A5A5A5A5A5A5u64;
                for i in processed..chunk_end {
                    *base.add(i) = pattern;
                }

                std::sync::atomic::fence(Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(64)); // DRAM refresh cycle timing

                // Verify phase
                for i in processed..chunk_end {
                    let v = *base.add(i);
                    if v != pattern {
                        cycle_errors += 1;
                        log::error!("{}: memory error at index {} - expected {:#x}, got {:#x}",
                                   test_name, i, pattern, v);
                    }
                }

                // Handle errors if found
                if cycle_errors > 0 {
                    match error_mode {
                        ErrorMode::Panic => {
                            panic!("{}: panicking due to {} memory errors in cycle {}",
                                  test_name, cycle_errors, cycle);
                        }
                        ErrorMode::Halt => {
                            let elapsed = start.elapsed().as_millis();
                            let total_operations = (total_bytes_processed / std::mem::size_of::<u64>()) as u64;

                            return TestStats {
                                name: test_name,
                                action: TestAction::WriteWaitVerify,
                                bytes_processed: total_bytes_processed,
                                elapsed_ms: elapsed,
                                thread_id,
                                error_count: total_error_count + cycle_errors,
                                total_operations,
                                cycles_completed: cycle,
                                cycles_planned: timing.cycles,
                                stopped_by_time_limit: false,
                            };
                        }
                        ErrorMode::Log => {
                            // Continue testing - errors already logged
                        }
                    }
                }

                processed = chunk_end;

                // Check for shutdown request
                if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
                    let elapsed = start.elapsed().as_millis();
                    let total_operations = (total_bytes_processed / std::mem::size_of::<u64>()) as u64;

                    return TestStats {
                        name: test_name,
                        action: TestAction::WriteWaitVerify,
                        bytes_processed: total_bytes_processed,
                        elapsed_ms: elapsed,
                        thread_id,
                        error_count: total_error_count + cycle_errors,
                        total_operations,
                        cycles_completed: cycle,
                        cycles_planned: timing.cycles,
                        stopped_by_time_limit: false,
                    };
                }
            }

            // Update bytes processed for this block (1 write + 1 read)
            total_bytes_processed += test_block.test_size * 2;
        }

        total_error_count += cycle_errors;

        // Update progress tracker every 250ms
        if let Some(progress) = progress {
            let now = Instant::now();
            if now.duration_since(last_progress_update).as_millis() >= 250 {
                progress.cycles_completed.store(cycle, Ordering::Relaxed);
                progress.bytes_processed.store(total_bytes_processed as u64, Ordering::Relaxed);
                progress.errors_found.store(total_error_count, Ordering::Relaxed);
                progress.last_update_ms.store(start.elapsed().as_millis() as u64, Ordering::Relaxed);
                last_progress_update = now;
            }
        }

        // Check if should continue based on timing
        let elapsed_secs = test_start.elapsed().as_secs() as u32;
        if !timing.should_continue(cycle, elapsed_secs) {
            let elapsed = start.elapsed().as_millis();
            let total_operations = (total_bytes_processed / std::mem::size_of::<u64>()) as u64;

            return TestStats {
                name: test_name,
                action: TestAction::WriteWaitVerify,
                bytes_processed: total_bytes_processed,
                elapsed_ms: elapsed,
                thread_id,
                error_count: total_error_count,
                total_operations,
                cycles_completed: cycle,
                cycles_planned: timing.cycles,
                stopped_by_time_limit: timing.cycles.map_or(true, |limit| cycle < limit),
            };
        }
    }
}

/// RefreshStable128 MultiBlock implementation (SSE2) - tests DRAM refresh stability.
///
/// # Safety
/// Caller must ensure blocks contain valid, aligned memory
#[allow(clippy::missing_safety_doc)]
/// RefreshStable128 - SSE2 optimized - MultiBlock pattern
/// Runtime-checked wrapper for SSE2 availability
pub unsafe fn refresh_stable_128_multi(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> TestStats {
    let test_name = "Mem-Refresh128";

    // Runtime CPU feature check
    if !is_x86_feature_detected!("sse2") {
        log::warn!("{}: SSE2 not available, returning zero stats", test_name);
        return TestStats {
            name: test_name,
            action: TestAction::WriteWaitVerify,
            bytes_processed: 0,
            elapsed_ms: 0,
            thread_id,
            error_count: 0,
            total_operations: 0,
            cycles_completed: 0,
            cycles_planned: timing.cycles,
            stopped_by_time_limit: false,
        };
    }

    // Call the SSE2 optimized implementation
    refresh_stable_128_impl(blocks, thread_id, error_mode, timing, config, progress)
}

/// RefreshStable128 - SSE2 optimized implementation
/// This function is annotated with #[target_feature] to enable full compiler optimization
#[inline]
#[target_feature(enable = "sse2")]
unsafe fn refresh_stable_128_impl(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> TestStats {
    let test_name = "Mem-Refresh128";

    let start = Instant::now();

    // Calculate total allocated memory
    let total_allocated: usize = blocks.iter().map(|b| b.buffer.size()).sum();

    // Window preparation
    let window_size = config.calculate_window_size(test_name, total_allocated);
    let test_blocks = prepare_blocks_for_window(blocks, window_size, test_name);

    let total_test_size: usize = test_blocks.iter().map(|b| b.test_size).sum();

    log::info!("[Thread {}] Running {} on {:.2} MB of memory (window: {:.2} MB)",
              thread_id, test_name,
              total_test_size as f64 / MB_F64,
              window_size as f64 / MB_F64);

    let pattern = u64x2::splat(0xA5A5A5A5A5A5A5A5u64);

    let mut cycle = 0u32;
    let test_start = Instant::now();
    let mut total_error_count = 0u64;
    let mut total_bytes_processed = 0usize;

    let mut last_progress_update = Instant::now();

    loop {
        cycle += 1;
        let mut cycle_errors = 0u64;

        // Interleave testing across all blocks
        for test_block in test_blocks.iter() {
            let base = test_block.block.buffer.as_mut_ptr() as *mut u64x2;
            let len = test_block.test_size / std::mem::size_of::<u64x2>();

            let ideal_chunk_size = calculate_ideal_chunk_size(config, test_name, test_block.test_size);
            let chunk_size_bytes = get_safe_chunk_size(ideal_chunk_size, test_block.test_size);
            let chunk_size_vectors = (chunk_size_bytes / std::mem::size_of::<u64x2>()).max(256);

            let mut processed = 0;
            while processed < len {
                let chunk_end = (processed + chunk_size_vectors).min(len);

                // Write phase
                for i in processed..chunk_end {
                    *base.add(i) = pattern;
                }

                std::sync::atomic::fence(Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(64));

                // Verify phase - accumulator pattern with configurable error checking
                let mut error_accumulator = u64x2::splat(0);
                let mut element_count = 0usize;

                // Pre-compute check mask for hot loop optimization
                match config.error_check_interval.get_check_mask() {
                    Some(check_mask) => {
                        for i in processed..chunk_end {
                            let value = *base.add(i);
                            let diff = value ^ pattern ;
                            error_accumulator = error_accumulator | diff ;

                            element_count += 1;

                            // Check errors at configured intervals (zero-branch hot loop optimization)
                            if (element_count as u32 & check_mask) == 0 {
                                if error_accumulator.simd_ne(u64x2::splat(0)).any() {
                                    cycle_errors += 1;
                                    log::error!("{}: memory error detected element {} (thread {})",
                                               test_name, element_count, thread_id);
                                    error_accumulator = u64x2::splat(0);
                                }
                            }
                        }
                    }
                    None => {
                        // PER_CHUNK mode - no intermediate checks, maximum performance
                        for i in processed..chunk_end {
                            let value = *base.add(i);
                            let diff = value ^ pattern ;
                            error_accumulator = error_accumulator | diff ;
                        }
                    }
                }

                // Final error check (always performed regardless of mode)
                if error_accumulator.simd_ne(u64x2::splat(0)).any() {
                    cycle_errors += 1;
                    log::error!("{}: memory error detected in chunk (thread {})", test_name, thread_id);
                }

                // Handle errors
                if cycle_errors > 0 {
                    match error_mode {
                        ErrorMode::Panic => {
                            panic!("{}: panicking due to {} memory errors in cycle {}",
                                  test_name, cycle_errors, cycle);
                        }
                        ErrorMode::Halt => {
                            let elapsed = start.elapsed().as_millis();
                            return TestStats {
                                name: test_name,
                                action: TestAction::WriteWaitVerify,
                                bytes_processed: total_bytes_processed,
                                elapsed_ms: elapsed,
                                thread_id,
                                error_count: total_error_count + cycle_errors,
                                total_operations: (total_bytes_processed / std::mem::size_of::<u64x2>()) as u64,
                                cycles_completed: cycle,
                                cycles_planned: timing.cycles,
                                stopped_by_time_limit: false,
                            };
                        }
                        ErrorMode::Log => {}
                    }
                }

                processed = chunk_end;

                // Check for shutdown
                if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
                    let elapsed = start.elapsed().as_millis();
                    return TestStats {
                        name: test_name,
                        action: TestAction::WriteWaitVerify,
                        bytes_processed: total_bytes_processed,
                        elapsed_ms: elapsed,
                        thread_id,
                        error_count: total_error_count + cycle_errors,
                        total_operations: (total_bytes_processed / std::mem::size_of::<u64x2>()) as u64,
                        cycles_completed: cycle,
                        cycles_planned: timing.cycles,
                        stopped_by_time_limit: false,
                    };
                }
            }

            total_bytes_processed += test_block.test_size * 2;
        }

        total_error_count += cycle_errors;

        // Update progress
        if let Some(progress) = progress {
            let now = Instant::now();
            if now.duration_since(last_progress_update).as_millis() >= 250 {
                progress.cycles_completed.store(cycle, Ordering::Relaxed);
                progress.bytes_processed.store(total_bytes_processed as u64, Ordering::Relaxed);
                progress.errors_found.store(total_error_count, Ordering::Relaxed);
                progress.last_update_ms.store(start.elapsed().as_millis() as u64, Ordering::Relaxed);
                last_progress_update = now;
            }
        }

        // Check timing
        let elapsed_secs = test_start.elapsed().as_secs() as u32;
        if !timing.should_continue(cycle, elapsed_secs) {
            let elapsed = start.elapsed().as_millis();
            return TestStats {
                name: test_name,
                action: TestAction::WriteWaitVerify,
                bytes_processed: total_bytes_processed,
                elapsed_ms: elapsed,
                thread_id,
                error_count: total_error_count,
                total_operations: (total_bytes_processed / std::mem::size_of::<u64x2>()) as u64,
                cycles_completed: cycle,
                cycles_planned: timing.cycles,
                stopped_by_time_limit: timing.cycles.map_or(true, |limit| cycle < limit),
            };
        }
    }
}

/// RefreshStable256 - AVX2 optimized - MultiBlock pattern
/// Runtime-checked wrapper for AVX2 availability
///
/// # Safety
/// Caller must ensure blocks contain valid, aligned memory
#[allow(clippy::missing_safety_doc)]
pub unsafe fn refresh_stable_256_multi(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> TestStats {
    let test_name = "Mem-Refresh256";

    // Runtime CPU feature check
    if !is_x86_feature_detected!("avx2") {
        log::warn!("{}: AVX2 not available, returning zero stats", test_name);
        return TestStats {
            name: test_name,
            action: TestAction::WriteWaitVerify,
            bytes_processed: 0,
            elapsed_ms: 0,
            thread_id,
            error_count: 0,
            total_operations: 0,
            cycles_completed: 0,
            cycles_planned: timing.cycles,
            stopped_by_time_limit: false,
        };
    }

    // Call the AVX2 optimized implementation
    refresh_stable_256_impl(blocks, thread_id, error_mode, timing, config, progress)
}

/// RefreshStable256 - AVX2 optimized implementation
/// This function is annotated with #[target_feature] to enable full compiler optimization
#[inline]
#[target_feature(enable = "avx2")]
unsafe fn refresh_stable_256_impl(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> TestStats {
    let test_name = "Mem-Refresh256";

    let start = Instant::now();

    // Calculate total allocated memory
    let total_allocated: usize = blocks.iter().map(|b| b.buffer.size()).sum();

    // Window preparation
    let window_size = config.calculate_window_size(test_name, total_allocated);
    let test_blocks = prepare_blocks_for_window(blocks, window_size, test_name);

    let total_test_size: usize = test_blocks.iter().map(|b| b.test_size).sum();

    log::info!("[Thread {}] Running {} on {:.2} MB of memory (window: {:.2} MB)",
              thread_id, test_name,
              total_test_size as f64 / MB_F64,
              window_size as f64 / MB_F64);

    let pattern = u64x4::splat(0xA5A5A5A5A5A5A5A5u64);

    let mut cycle = 0u32;
    let test_start = Instant::now();
    let mut total_error_count = 0u64;
    let mut total_bytes_processed = 0usize;

    let mut last_progress_update = Instant::now();

    loop {
        cycle += 1;
        let mut cycle_errors = 0u64;

        // Interleave testing across all blocks
        for test_block in test_blocks.iter() {
            let base = test_block.block.buffer.as_mut_ptr() as *mut u64x4;
            let len = test_block.test_size / std::mem::size_of::<u64x4>();

            let ideal_chunk_size = calculate_ideal_chunk_size(config, test_name, test_block.test_size);
            let chunk_size_bytes = get_safe_chunk_size(ideal_chunk_size, test_block.test_size);
            let chunk_size_vectors = (chunk_size_bytes / std::mem::size_of::<u64x4>()).max(128);

            let mut processed = 0;
            while processed < len {
                let chunk_end = (processed + chunk_size_vectors).min(len);

                // Write phase
                for i in processed..chunk_end {
                    *base.add(i) = pattern;
                }

                std::sync::atomic::fence(Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(64));

                // Verify phase - accumulator pattern with configurable error checking
                let mut error_accumulator = u64x4::splat(0);
                let mut element_count = 0usize;

                // Pre-compute check mask for hot loop optimization
                match config.error_check_interval.get_check_mask() {
                    Some(check_mask) => {
                        for i in processed..chunk_end {
                            let value = *base.add(i);
                            let diff = value ^ pattern ;
                            error_accumulator = error_accumulator | diff ;

                            element_count += 1;

                            // Check errors at configured intervals (zero-branch hot loop optimization)
                            if (element_count as u32 & check_mask) == 0 {
                                if error_accumulator.simd_ne(u64x4::splat(0)).any() {
                                    cycle_errors += 1;
                                    log::error!("{}: memory error detected element {} (thread {})",
                                               test_name, element_count, thread_id);
                                    error_accumulator = u64x4::splat(0);
                                }
                            }
                        }
                    }
                    None => {
                        // PER_CHUNK mode - no intermediate checks, maximum performance
                        for i in processed..chunk_end {
                            let value = *base.add(i);
                            let diff = value ^ pattern ;
                            error_accumulator = error_accumulator | diff ;
                        }
                    }
                }

                // Final error check (always performed regardless of mode)
                if error_accumulator.simd_ne(u64x4::splat(0)).any() {
                    cycle_errors += 1;
                    log::error!("{}: memory error detected in chunk (thread {})", test_name, thread_id);
                }

                // Handle errors
                if cycle_errors > 0 {
                    match error_mode {
                        ErrorMode::Panic => {
                            panic!("{}: panicking due to {} memory errors in cycle {}",
                                  test_name, cycle_errors, cycle);
                        }
                        ErrorMode::Halt => {
                            let elapsed = start.elapsed().as_millis();
                            return TestStats {
                                name: test_name,
                                action: TestAction::WriteWaitVerify,
                                bytes_processed: total_bytes_processed,
                                elapsed_ms: elapsed,
                                thread_id,
                                error_count: total_error_count + cycle_errors,
                                total_operations: (total_bytes_processed / std::mem::size_of::<u64x4>()) as u64,
                                cycles_completed: cycle,
                                cycles_planned: timing.cycles,
                                stopped_by_time_limit: false,
                            };
                        }
                        ErrorMode::Log => {}
                    }
                }

                processed = chunk_end;

                // Check for shutdown
                if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
                    let elapsed = start.elapsed().as_millis();
                    return TestStats {
                        name: test_name,
                        action: TestAction::WriteWaitVerify,
                        bytes_processed: total_bytes_processed,
                        elapsed_ms: elapsed,
                        thread_id,
                        error_count: total_error_count + cycle_errors,
                        total_operations: (total_bytes_processed / std::mem::size_of::<u64x4>()) as u64,
                        cycles_completed: cycle,
                        cycles_planned: timing.cycles,
                        stopped_by_time_limit: false,
                    };
                }
            }

            total_bytes_processed += test_block.test_size * 2;
        }

        total_error_count += cycle_errors;

        // Update progress
        if let Some(progress) = progress {
            let now = Instant::now();
            if now.duration_since(last_progress_update).as_millis() >= 250 {
                progress.cycles_completed.store(cycle, Ordering::Relaxed);
                progress.bytes_processed.store(total_bytes_processed as u64, Ordering::Relaxed);
                progress.errors_found.store(total_error_count, Ordering::Relaxed);
                progress.last_update_ms.store(start.elapsed().as_millis() as u64, Ordering::Relaxed);
                last_progress_update = now;
            }
        }

        // Check timing
        let elapsed_secs = test_start.elapsed().as_secs() as u32;
        if !timing.should_continue(cycle, elapsed_secs) {
            let elapsed = start.elapsed().as_millis();
            return TestStats {
                name: test_name,
                action: TestAction::WriteWaitVerify,
                bytes_processed: total_bytes_processed,
                elapsed_ms: elapsed,
                thread_id,
                error_count: total_error_count,
                total_operations: (total_bytes_processed / std::mem::size_of::<u64x4>()) as u64,
                cycles_completed: cycle,
                cycles_planned: timing.cycles,
                stopped_by_time_limit: timing.cycles.map_or(true, |limit| cycle < limit),
            };
        }
    }
}

/// RefreshStable512 - AVX-512 optimized - MultiBlock pattern
/// Runtime-checked wrapper for AVX-512 availability
///
/// # Safety
/// Caller must ensure blocks contain valid, aligned memory
#[allow(clippy::missing_safety_doc)]
pub unsafe fn refresh_stable_512_multi(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> TestStats {
    let test_name = "Mem-Refresh512";

    // Runtime CPU feature check
    if !is_x86_feature_detected!("avx512f") {
        log::warn!("{}: AVX-512 not available, returning zero stats", test_name);
        return TestStats {
            name: test_name,
            action: TestAction::WriteWaitVerify,
            bytes_processed: 0,
            elapsed_ms: 0,
            thread_id,
            error_count: 0,
            total_operations: 0,
            cycles_completed: 0,
            cycles_planned: timing.cycles,
            stopped_by_time_limit: false,
        };
    }

    // Call the AVX-512 optimized implementation
    refresh_stable_512_impl(blocks, thread_id, error_mode, timing, config, progress)
}

/// RefreshStable512 - AVX-512 optimized implementation
/// This function is annotated with #[target_feature] to enable full compiler optimization
#[inline]
#[target_feature(enable = "avx512f")]
unsafe fn refresh_stable_512_impl(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> TestStats {
    let test_name = "Mem-Refresh512";

    let start = Instant::now();

    // Calculate total allocated memory
    let total_allocated: usize = blocks.iter().map(|b| b.buffer.size()).sum();

    // Window preparation
    let window_size = config.calculate_window_size(test_name, total_allocated);
    let test_blocks = prepare_blocks_for_window(blocks, window_size, test_name);

    let total_test_size: usize = test_blocks.iter().map(|b| b.test_size).sum();

    log::info!("[Thread {}] Running {} on {:.2} MB of memory (window: {:.2} MB)",
              thread_id, test_name,
              total_test_size as f64 / MB_F64,
              window_size as f64 / MB_F64);

    let pattern = u64x8::splat(0xA5A5A5A5A5A5A5A5u64);

    let mut cycle = 0u32;
    let test_start = Instant::now();
    let mut total_error_count = 0u64;
    let mut total_bytes_processed = 0usize;

    let mut last_progress_update = Instant::now();

    loop {
        cycle += 1;
        let mut cycle_errors = 0u64;

        // Interleave testing across all blocks
        for test_block in test_blocks.iter() {
            let base = test_block.block.buffer.as_mut_ptr() as *mut u64x8;
            let len = test_block.test_size / std::mem::size_of::<u64x8>();

            let ideal_chunk_size = calculate_ideal_chunk_size(config, test_name, test_block.test_size);
            let chunk_size_bytes = get_safe_chunk_size(ideal_chunk_size, test_block.test_size);
            let chunk_size_vectors = (chunk_size_bytes / std::mem::size_of::<u64x8>()).max(64);

            let mut processed = 0;
            while processed < len {
                let chunk_end = (processed + chunk_size_vectors).min(len);

                // Write phase
                for i in processed..chunk_end {
                    *base.add(i) = pattern;
                }

                std::sync::atomic::fence(Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(64));

                // Verify phase - accumulator pattern with configurable error checking
                let mut error_accumulator = u64x8::splat(0);
                let mut element_count = 0usize;

                // Pre-compute check mask for hot loop optimization
                match config.error_check_interval.get_check_mask() {
                    Some(check_mask) => {
                        for i in processed..chunk_end {
                            let value = *base.add(i);
                            let diff = value ^ pattern ;
                            error_accumulator = error_accumulator | diff ;

                            element_count += 1;

                            // Check errors at configured intervals (zero-branch hot loop optimization)
                            if (element_count as u32 & check_mask) == 0 {
                                if !error_accumulator.simd_eq(u64x8::splat(0)).all() {
                                    cycle_errors += 1;
                                    log::error!("{}: memory error detected element {} (thread {})",
                                               test_name, element_count, thread_id);
                                    error_accumulator = u64x8::splat(0);
                                }
                            }
                        }
                    }
                    None => {
                        // PER_CHUNK mode - no intermediate checks, maximum performance
                        for i in processed..chunk_end {
                            let value = *base.add(i);
                            let diff = value ^ pattern ;
                            error_accumulator = error_accumulator | diff ;
                        }
                    }
                }

                // Final error check (always performed regardless of mode)
                if !error_accumulator.simd_eq(u64x8::splat(0)).all() {
                    cycle_errors += 1;
                    log::error!("{}: memory error detected in chunk (thread {})", test_name, thread_id);
                }

                // Handle errors
                if cycle_errors > 0 {
                    match error_mode {
                        ErrorMode::Panic => {
                            panic!("{}: panicking due to {} memory errors in cycle {}",
                                  test_name, cycle_errors, cycle);
                        }
                        ErrorMode::Halt => {
                            let elapsed = start.elapsed().as_millis();
                            return TestStats {
                                name: test_name,
                                action: TestAction::WriteWaitVerify,
                                bytes_processed: total_bytes_processed,
                                elapsed_ms: elapsed,
                                thread_id,
                                error_count: total_error_count + cycle_errors,
                                total_operations: (total_bytes_processed / std::mem::size_of::<u64x8>()) as u64,
                                cycles_completed: cycle,
                                cycles_planned: timing.cycles,
                                stopped_by_time_limit: false,
                            };
                        }
                        ErrorMode::Log => {}
                    }
                }

                processed = chunk_end;

                // Check for shutdown
                if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
                    let elapsed = start.elapsed().as_millis();
                    return TestStats {
                        name: test_name,
                        action: TestAction::WriteWaitVerify,
                        bytes_processed: total_bytes_processed,
                        elapsed_ms: elapsed,
                        thread_id,
                        error_count: total_error_count + cycle_errors,
                        total_operations: (total_bytes_processed / std::mem::size_of::<u64x8>()) as u64,
                        cycles_completed: cycle,
                        cycles_planned: timing.cycles,
                        stopped_by_time_limit: false,
                    };
                }
            }

            total_bytes_processed += test_block.test_size * 2;
        }

        total_error_count += cycle_errors;

        // Update progress
        if let Some(progress) = progress {
            let now = Instant::now();
            if now.duration_since(last_progress_update).as_millis() >= 250 {
                progress.cycles_completed.store(cycle, Ordering::Relaxed);
                progress.bytes_processed.store(total_bytes_processed as u64, Ordering::Relaxed);
                progress.errors_found.store(total_error_count, Ordering::Relaxed);
                progress.last_update_ms.store(start.elapsed().as_millis() as u64, Ordering::Relaxed);
                last_progress_update = now;
            }
        }

        // Check timing
        let elapsed_secs = test_start.elapsed().as_secs() as u32;
        if !timing.should_continue(cycle, elapsed_secs) {
            let elapsed = start.elapsed().as_millis();
            return TestStats {
                name: test_name,
                action: TestAction::WriteWaitVerify,
                bytes_processed: total_bytes_processed,
                elapsed_ms: elapsed,
                thread_id,
                error_count: total_error_count,
                total_operations: (total_bytes_processed / std::mem::size_of::<u64x8>()) as u64,
                cycles_completed: cycle,
                cycles_planned: timing.cycles,
                stopped_by_time_limit: timing.cycles.map_or(true, |limit| cycle < limit),
            };
        }
    }
}

/// RefreshStableAuto - MultiBlock dispatcher
/// Auto-selects best SIMD implementation: AVX-512 > AVX2 > SSE2 > scalar
///
/// # Safety
/// Caller must ensure blocks contain valid, aligned memory
#[allow(clippy::missing_safety_doc)]
pub unsafe fn refresh_stable_auto_multi(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> TestStats {
    // Check CPU capabilities and dispatch to the best available MultiBlock implementation
    if is_x86_feature_detected!("avx512f") {
        refresh_stable_512_multi(blocks, thread_id, error_mode, timing, config, progress)
    } else if is_x86_feature_detected!("avx2") {
        refresh_stable_256_multi(blocks, thread_id, error_mode, timing, config, progress)
    } else if is_x86_feature_detected!("sse2") {
        refresh_stable_128_multi(blocks, thread_id, error_mode, timing, config, progress)
    } else {
        // Fallback to scalar MultiBlock implementation
        refresh_stable_multi(blocks, thread_id, error_mode, timing, config, progress)
    }
}

// ================================================================================================
// CacheBusting MultiBlock Implementation
// ================================================================================================

/// CacheBusting MultiBlock implementation - tests memory with stride-based cache-busting patterns.
///
/// Pattern per cycle:
/// - Writes with large strides (CACHE_BUSTING_STRIDE) to evict from cache
/// - Verifies the pattern
/// - Supports single stream and multi-stream modes
///
/// Tests blocks in interleaved fashion with shared timer to fix N×duration bug.
pub unsafe fn cache_busting_multi(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> TestStats {
    let test_name = "Mem-CacheBust";
    let start = Instant::now();

    // Calculate total allocated memory
    let total_allocated: usize = blocks.iter().map(|b| b.buffer.size()).sum();

    // Window preparation - determines which blocks to test
    let window_size = config.calculate_window_size(test_name, total_allocated);
    let test_blocks = prepare_blocks_for_window(blocks, window_size, test_name);

    let total_test_size: usize = test_blocks.iter().map(|b| b.test_size).sum();

    log::info!("[Thread {}] Running {} on {:.2} MB of memory (window: {:.2} MB)",
              thread_id, test_name,
              total_test_size as f64 / MB_F64,
              window_size as f64 / MB_F64);

    let streams = config.streams.max(1) as usize;

    // Pre-calculate stride constants outside all loops
    let base_stride = CACHE_BUSTING_STRIDE / std::mem::size_of::<u64>();
    let pattern_base = 0x0123456789ABCDEFu64.wrapping_add(thread_id as u64);

    let mut cycle = 0u32;
    let test_start = Instant::now();
    let mut total_error_count = 0u64;
    let mut total_bytes_processed = 0usize;

    let mut last_progress_update = Instant::now();

    loop {
        cycle += 1;
        let mut cycle_errors = 0u64;

        // Interleave testing across all blocks with shared timer
        for test_block in test_blocks.iter() {
            let base = test_block.block.buffer.as_mut_ptr() as *mut u64;
            let len = test_block.test_size / std::mem::size_of::<u64>();

            // Recalculate chunk size for THIS block's size
            let ideal_chunk_size = calculate_ideal_chunk_size(config, test_name, test_block.test_size);
            let chunk_size_bytes = get_safe_chunk_size(ideal_chunk_size, test_block.test_size);
            let chunk_size_operations = (chunk_size_bytes / std::mem::size_of::<u64>()).max(1024);

            // Process this block in chunks
            let mut processed = 0;
            while processed < len {
                let chunk_end = (processed + chunk_size_operations).min(len);

                // Apply stream-based access patterns within chunk
                match config.streams {
                    1 => {
                        // Single stream with large strides to bust cache
                        for offset in 0..base_stride.min(chunk_end - processed) {
                            let mut i = processed + offset;
                            while i < chunk_end {
                                *base.add(i) = pattern_base.wrapping_add(i as u64);
                                i += base_stride;
                                if i >= chunk_end { break; }
                            }
                        }

                        std::sync::atomic::fence(Ordering::SeqCst);

                        // Verify with same stride pattern within chunk
                        for offset in 0..base_stride.min(chunk_end - processed) {
                            let mut i = processed + offset;
                            while i < chunk_end {
                                let v = *base.add(i);
                                let expected = pattern_base.wrapping_add(i as u64);
                                if v != expected {
                                    cycle_errors += 1;
                                    log::error!("{}: memory error at index {} - expected {:#x}, got {:#x}",
                                               test_name, i, expected, v);
                                }
                                i += base_stride;
                                if i >= chunk_end { break; }
                            }
                        }
                    }
                    _ => {
                        // Multiple streams with different patterns - divide offsets among streams
                        // Each stream handles a subset of offsets, but ALL offsets are covered
                        for offset in 0..base_stride.min(chunk_end - processed) {
                            // Assign this offset to a stream using round-robin
                            let stream = (offset % streams as usize) as u64;
                            let pattern = pattern_base.wrapping_add(stream * 0x1111111111111111u64);

                            let mut i = processed + offset;
                            while i < chunk_end {
                                *base.add(i) = pattern.wrapping_add(i as u64);
                                i += base_stride;
                                if i >= chunk_end { break; }
                            }
                        }

                        std::sync::atomic::fence(Ordering::SeqCst);

                        // Verify all offsets with their respective stream patterns
                        for offset in 0..base_stride.min(chunk_end - processed) {
                            let stream = (offset % streams as usize) as u64;
                            let pattern = pattern_base.wrapping_add(stream * 0x1111111111111111u64);

                            let mut i = processed + offset;
                            while i < chunk_end {
                                let v = *base.add(i);
                                let expected = pattern.wrapping_add(i as u64);
                                if v != expected {
                                    cycle_errors += 1;
                                    log::error!("{}: memory error at index {} - expected {:#x}, got {:#x}",
                                               test_name, i, expected, v);
                                }
                                i += base_stride;
                                if i >= chunk_end { break; }
                            }
                        }
                    }
                }

                // Handle errors if found
                if cycle_errors > 0 {
                    match error_mode {
                        ErrorMode::Panic => {
                            panic!("{}: panicking due to {} memory errors in cycle {}",
                                  test_name, cycle_errors, cycle);
                        }
                        ErrorMode::Halt => {
                            let elapsed = start.elapsed().as_millis();
                            let total_operations = (total_bytes_processed / std::mem::size_of::<u64>()) as u64;

                            return TestStats {
                                name: test_name,
                                action: TestAction::CacheBusting,
                                bytes_processed: total_bytes_processed,
                                elapsed_ms: elapsed,
                                thread_id,
                                error_count: total_error_count + cycle_errors,
                                total_operations,
                                cycles_completed: cycle,
                                cycles_planned: timing.cycles,
                                stopped_by_time_limit: false,
                            };
                        }
                        ErrorMode::Log => {
                            // Continue testing - errors already logged
                        }
                    }
                }

                processed = chunk_end;

                // Check for shutdown request
                if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
                    let elapsed = start.elapsed().as_millis();
                    let total_operations = (total_bytes_processed / std::mem::size_of::<u64>()) as u64;

                    return TestStats {
                        name: test_name,
                        action: TestAction::CacheBusting,
                        bytes_processed: total_bytes_processed,
                        elapsed_ms: elapsed,
                        thread_id,
                        error_count: total_error_count + cycle_errors,
                        total_operations,
                        cycles_completed: cycle,
                        cycles_planned: timing.cycles,
                        stopped_by_time_limit: false,
                    };
                }
            }

            // Update bytes processed for this block (write + verify = 2×)
            total_bytes_processed += test_block.test_size * 2;
        }

        total_error_count += cycle_errors;

        // Update progress tracker every 250ms
        if let Some(progress) = progress {
            let now = Instant::now();
            if now.duration_since(last_progress_update).as_millis() >= 250 {
                progress.cycles_completed.store(cycle, Ordering::Relaxed);
                progress.bytes_processed.store(total_bytes_processed as u64, Ordering::Relaxed);
                progress.errors_found.store(total_error_count, Ordering::Relaxed);
                progress.last_update_ms.store(start.elapsed().as_millis() as u64, Ordering::Relaxed);
                last_progress_update = now;
            }
        }

        // Check if should continue based on timing
        let elapsed_secs = test_start.elapsed().as_secs() as u32;
        if !timing.should_continue(cycle, elapsed_secs) {
            let elapsed = start.elapsed().as_millis();
            let total_operations = (total_bytes_processed / std::mem::size_of::<u64>()) as u64;

            return TestStats {
                name: test_name,
                action: TestAction::CacheBusting,
                bytes_processed: total_bytes_processed,
                elapsed_ms: elapsed,
                thread_id,
                error_count: total_error_count,
                total_operations,
                cycles_completed: cycle,
                cycles_planned: timing.cycles,
                stopped_by_time_limit: timing.cycles.map_or(true, |limit| cycle < limit),
            };
        }
    }
}

// ================================================================================================
// RandomTorture MultiBlock Implementation
// ================================================================================================

/// RandomTorture MultiBlock implementation - random access pattern testing.
///
/// Pattern per cycle:
/// - Initializes memory with sequential pattern (index as value)
/// - Performs random reads using XORshift RNG
/// - Verifies each random read matches expected value
/// - Supports multiple streams with different RNG seeds
///
/// Tests blocks in interleaved fashion with shared timer to fix N×duration bug.
pub unsafe fn random_torture_multi(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> TestStats {
    let test_name = "Mem-Random";
    let start = Instant::now();

    // Calculate total allocated memory
    let total_allocated: usize = blocks.iter().map(|b| b.buffer.size()).sum();

    // Window preparation - determines which blocks to test
    let window_size = config.calculate_window_size(test_name, total_allocated);
    let test_blocks = prepare_blocks_for_window(blocks, window_size, test_name);

    let total_test_size: usize = test_blocks.iter().map(|b| b.test_size).sum();

    log::info!("[Thread {}] Running {} on {:.2} MB of memory (window: {:.2} MB)",
              thread_id, test_name,
              total_test_size as f64 / MB_F64,
              window_size as f64 / MB_F64);

    let mut cycle = 0u32;
    let test_start = Instant::now();
    let mut total_error_count = 0u64;
    let mut total_bytes_processed = 0usize;

    let mut last_progress_update = Instant::now();

    // Initialize all blocks with known pattern once
    for test_block in test_blocks.iter() {
        let base = test_block.block.buffer.as_mut_ptr() as *mut u64;
        let len = test_block.test_size / std::mem::size_of::<u64>();

        for i in 0..len {
            *base.add(i) = i as u64;
        }
        std::sync::atomic::fence(Ordering::SeqCst);
        total_bytes_processed += test_block.test_size;
    }

    loop {
        cycle += 1;
        let mut cycle_errors = 0u64;

        // Interleave testing across all blocks with shared timer
        for test_block in test_blocks.iter() {
            let base = test_block.block.buffer.as_mut_ptr() as *mut u64;
            let len = test_block.test_size / std::mem::size_of::<u64>();

            // Assert power-of-2 size for optimal performance
            if !len.is_power_of_two() {
                panic!("{}: Window size {} is not power-of-2! This is a bug in the alignment code.",
                       test_name, len);
            }
            let mask = len - 1;

            // Calculate chunk size for responsive shutdown
            let ideal_chunk_size = calculate_ideal_chunk_size(config, test_name, test_block.test_size);
            let chunk_size_bytes = get_safe_chunk_size(ideal_chunk_size, test_block.test_size);
            let chunk_size_operations = chunk_size_bytes / std::mem::size_of::<u64>();

            // Random access torture with configurable streams
            let base_iterations = (len / 1000).clamp(5000, 50000);
            let streams_max = config.streams.max(1) as usize;
            let stream_shift = streams_max.trailing_zeros();
            let iterations_per_stream = (base_iterations >> stream_shift).max(1);

            // Process streams in chunks for responsive shutdown
            for stream in 0..config.streams {
                let mut rng_state = 0x123456789ABCDEFu64
                    .wrapping_add(thread_id as u64)
                    .wrapping_add(cycle as u64)
                    .wrapping_add((stream as u64).wrapping_mul(0x8765432187654321u64));

                // Random read verification for this stream - chunked for responsive shutdown
                for chunk_start in (0..iterations_per_stream).step_by(chunk_size_operations) {
                    let chunk_end = (chunk_start + chunk_size_operations).min(iterations_per_stream);

                    for _iteration in chunk_start..chunk_end {
                        rng_state ^= rng_state << 13;
                        rng_state ^= rng_state >> 17;
                        rng_state ^= rng_state << 5;

                        let idx = (rng_state as usize) & mask;
                        let expected = idx as u64;
                        let actual = *base.add(idx);

                        if actual != expected {
                            cycle_errors += 1;
                            log::error!(
                                "{}: memory error at index {}, iteration {}, stream {}, expected {}, actual {}",
                                test_name,
                                idx,
                                _iteration,
                                stream,
                                expected,
                                actual
                            );
                        }
                    }

                    // Handle errors if found
                    if cycle_errors > 0 {
                        match error_mode {
                            ErrorMode::Panic => {
                                panic!("{}: {} memory errors detected in stream {} (see logs above)",
                                      test_name, cycle_errors, stream);
                            }
                            ErrorMode::Halt => {
                                let elapsed = start.elapsed().as_millis();
                                let total_operations = cycle as u64 * (chunk_end - chunk_start) as u64;

                                return TestStats {
                                    name: test_name,
                                    action: TestAction::RandomAccess,
                                    bytes_processed: total_bytes_processed,
                                    elapsed_ms: elapsed,
                                    thread_id,
                                    error_count: total_error_count + cycle_errors,
                                    total_operations,
                                    cycles_completed: cycle,
                                    cycles_planned: timing.cycles,
                                    stopped_by_time_limit: false,
                                };
                            }
                            ErrorMode::Log => {
                                // Continue - errors already logged
                            }
                        }
                    }

                    // Check for shutdown request
                    if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
                        let elapsed = start.elapsed().as_millis();
                        let total_operations = cycle as u64 * (chunk_end - chunk_start) as u64;

                        return TestStats {
                            name: test_name,
                            action: TestAction::RandomAccess,
                            bytes_processed: total_bytes_processed,
                            elapsed_ms: elapsed,
                            thread_id,
                            error_count: total_error_count + cycle_errors,
                            total_operations,
                            cycles_completed: cycle,
                            cycles_planned: timing.cycles,
                            stopped_by_time_limit: false,
                        };
                    }
                }
            }

            // Update bytes processed for this block
            let bytes_this_cycle = iterations_per_stream
                .saturating_mul(config.streams as usize)
                .saturating_mul(std::mem::size_of::<u64>());
            total_bytes_processed = total_bytes_processed.saturating_add(bytes_this_cycle);
        }

        total_error_count += cycle_errors;

        // Update progress tracker every 250ms
        if let Some(progress) = progress {
            let now = Instant::now();
            if now.duration_since(last_progress_update).as_millis() >= 250 {
                progress.cycles_completed.store(cycle, Ordering::Relaxed);
                progress.bytes_processed.store(total_bytes_processed as u64, Ordering::Relaxed);
                progress.errors_found.store(total_error_count, Ordering::Relaxed);
                progress.last_update_ms.store(start.elapsed().as_millis() as u64, Ordering::Relaxed);
                last_progress_update = now;
            }
        }

        // Check if should continue based on timing
        let elapsed_secs = test_start.elapsed().as_secs() as u32;
        if !timing.should_continue(cycle, elapsed_secs) {
            let elapsed = start.elapsed().as_millis();

            // Calculate total operations
            let base_iterations = (total_test_size / std::mem::size_of::<u64>() / 1000).clamp(5000, 50000);
            let streams_max = config.streams.max(1) as usize;
            let iterations_per_stream = (base_iterations >> streams_max.trailing_zeros()).max(1);
            let total_operations: u64 = cycle as u64 * (iterations_per_stream * config.streams as usize) as u64;

            return TestStats {
                name: test_name,
                action: TestAction::RandomAccess,
                bytes_processed: total_bytes_processed,
                elapsed_ms: elapsed,
                thread_id,
                error_count: total_error_count,
                total_operations,
                cycles_completed: cycle,
                cycles_planned: timing.cycles,
                stopped_by_time_limit: timing.cycles.map_or(true, |limit| cycle < limit),
            };
        }
    }
}

// ================================================================================================
// StrideAccess, BandwidthSat, BlockMove MultiBlock Implementations
// ================================================================================================

/// StrideAccess MultiBlock implementation - tests various stride patterns.
pub unsafe fn stride_access_multi(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> TestStats {
    let test_name = "Mem-Stride";
    let start = Instant::now();
    let total_allocated: usize = blocks.iter().map(|b| b.buffer.size()).sum();
    let window_size = config.calculate_window_size(test_name, total_allocated);
    let test_blocks = prepare_blocks_for_window(blocks, window_size, test_name);
    let total_test_size: usize = test_blocks.iter().map(|b| b.test_size).sum();

    log::info!("[Thread {}] Running {} on {:.2} MB of memory (window: {:.2} MB)",
              thread_id, test_name, total_test_size as f64 / MB_F64, window_size as f64 / MB_F64);

    let streams = config.streams.max(1) as usize;
    let stream_shift = streams.trailing_zeros();
    let mut cycle = 0u32;
    let test_start = Instant::now();
    let mut total_error_count = 0u64;
    let mut total_bytes_processed = 0usize;
    let mut last_progress_update = Instant::now();

    loop {
        cycle += 1;
        let mut cycle_errors = 0u64;
        let strides = [1, 16, 64, 256, 1024, 4096];
        let pattern_base = 0xFEDCBA9876543210u64.wrapping_add(thread_id as u64).wrapping_add(cycle as u64);

        for test_block in test_blocks.iter() {
            let base = test_block.block.buffer.as_mut_ptr() as *mut u64;
            let len = test_block.test_size / std::mem::size_of::<u64>();
            let ideal_chunk_size = calculate_ideal_chunk_size(config, test_name, test_block.test_size);
            let chunk_size_bytes = get_safe_chunk_size(ideal_chunk_size, test_block.test_size);
            let chunk_size_elements = chunk_size_bytes / std::mem::size_of::<u64>();

            'stride_loop: for &stride in &strides {
                if stride >= len { continue; }

                for chunk_start in (0..len).step_by(chunk_size_elements) {
                    let chunk_end = (chunk_start + chunk_size_elements).min(len);
                    let chunk_len = chunk_end - chunk_start;
                    let elements_per_stream = chunk_len >> stream_shift;

                    for stream in 0..streams {
                        let pattern = pattern_base.wrapping_add((stride as u64) << 32).wrapping_add((stream as u64) << 48);
                        let stream_start = chunk_start + stream * elements_per_stream;
                        let stream_end = stream_start + elements_per_stream;
                        let mut pos = stream_start;
                        while pos < stream_end {
                            *base.add(pos) = pattern.wrapping_add(pos as u64);
                            pos += stride;
                            if pos >= stream_end { break; }
                        }
                    }

                    std::sync::atomic::fence(Ordering::SeqCst);

                    for stream in 0..streams {
                        let pattern = pattern_base.wrapping_add((stride as u64) << 32).wrapping_add((stream as u64) << 48);
                        let stream_start = chunk_start + stream * elements_per_stream;
                        let stream_end = stream_start + elements_per_stream;
                        let mut pos = stream_start;
                        while pos < stream_end {
                            let expected = pattern.wrapping_add(pos as u64);
                            let actual = *base.add(pos);
                            if actual != expected {
                                cycle_errors += 1;
                                log::error!("{}: memory error at index {} - expected {:#x}, got {:#x}", test_name, pos, expected, actual);
                            }
                            pos += stride;
                            if pos >= stream_end { break; }
                        }
                    }

                    if cycle_errors > 0 {
                        match error_mode {
                            ErrorMode::Panic => panic!("{}: {} memory errors detected (see logs above)", test_name, cycle_errors),
                            ErrorMode::Halt => break 'stride_loop,
                            ErrorMode::Log => {}
                        }
                    }

                    if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
                        total_error_count += cycle_errors;
                        total_bytes_processed += (chunk_end - chunk_start) * std::mem::size_of::<u64>() * 2;
                        let elapsed = start.elapsed().as_millis();
                        return TestStats {
                            name: test_name, action: TestAction::ReadWrite,
                            bytes_processed: total_bytes_processed, elapsed_ms: elapsed, thread_id,
                            error_count: total_error_count,
                            total_operations: cycle as u64 * (chunk_end - chunk_start) as u64,
                            cycles_completed: cycle, cycles_planned: timing.cycles, stopped_by_time_limit: false,
                        };
                    }
                }
            }

            let mut bytes_this_cycle = 0;
            for &stride in &strides {
                if stride < len {
                    bytes_this_cycle += (len / stride) * std::mem::size_of::<u64>() * 2;
                }
            }
            total_bytes_processed = total_bytes_processed.saturating_add(bytes_this_cycle);
        }

        total_error_count += cycle_errors;
        if let Some(progress) = progress {
            let now = Instant::now();
            if now.duration_since(last_progress_update).as_millis() >= 250 {
                progress.cycles_completed.store(cycle, Ordering::Relaxed);
                progress.bytes_processed.store(total_bytes_processed as u64, Ordering::Relaxed);
                progress.errors_found.store(total_error_count, Ordering::Relaxed);
                progress.last_update_ms.store(start.elapsed().as_millis() as u64, Ordering::Relaxed);
                last_progress_update = now;
            }
        }

        let elapsed_secs = test_start.elapsed().as_secs() as u32;
        if !timing.should_continue(cycle, elapsed_secs) {
            let elapsed = start.elapsed().as_millis();
            let strides = [1, 16, 64, 256, 1024, 4096];
            let mut total_elements_per_cycle = 0u64;
            for &stride in &strides {
                if stride < (total_test_size / std::mem::size_of::<u64>()) {
                    total_elements_per_cycle += ((total_test_size / std::mem::size_of::<u64>()) / stride) as u64;
                }
            }
            let total_operations: u64 = cycle as u64 * total_elements_per_cycle;
            return TestStats {
                name: test_name, action: TestAction::ReadWrite,
                bytes_processed: total_bytes_processed, elapsed_ms: elapsed, thread_id,
                error_count: total_error_count, total_operations,
                cycles_completed: cycle, cycles_planned: timing.cycles,
                stopped_by_time_limit: timing.cycles.map_or(true, |limit| cycle < limit),
            };
        }
    }
}

/// BandwidthSat MultiBlock - saturate memory bandwidth.
pub unsafe fn bandwidth_saturation_multi(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    _error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> TestStats {
    let test_name = "Spd-Saturate";
    let start = Instant::now();
    let total_allocated: usize = blocks.iter().map(|b| b.buffer.size()).sum();
    let window_size = config.calculate_window_size(test_name, total_allocated);
    let test_blocks = prepare_blocks_for_window(blocks, window_size, test_name);
    let total_test_size: usize = test_blocks.iter().map(|b| b.test_size).sum();

    log::info!("[Thread {}] Running {} on {:.2} MB of memory (window: {:.2} MB)",
              thread_id, test_name, total_test_size as f64 / MB_F64, window_size as f64 / MB_F64);

    let streams = config.streams.max(1) as usize;
    let stream_shift = streams.trailing_zeros();
    let mut cycle = 0u32;
    let test_start = Instant::now();
    let mut total_bytes_processed = 0usize;
    let mut last_progress_update = Instant::now();

    loop {
        cycle += 1;

        for test_block in test_blocks.iter() {
            let base = test_block.block.buffer.as_mut_ptr() as *mut u64;
            let len = test_block.test_size / std::mem::size_of::<u64>();

            // Calculate chunk size for responsive shutdown
            let ideal_chunk_size = calculate_ideal_chunk_size(config, test_name, test_block.test_size);
            let chunk_size_bytes = get_safe_chunk_size(ideal_chunk_size, test_block.test_size);
            let chunk_size_elements = chunk_size_bytes / std::mem::size_of::<u64>();

            // Process memory in chunks for responsive shutdown
            for chunk_start in (0..len).step_by(chunk_size_elements) {
                let chunk_end = (chunk_start + chunk_size_elements).min(len);

                // Pure memory bandwidth test with configurable streams
                match config.streams {
                    1 => {
                        // Single stream - maximum sequential bandwidth within chunk
                        let pattern = 0x5555AAAA5555AAAAu64.wrapping_add(thread_id as u64).wrapping_add(cycle as u64);

                        // Write phase for chunk
                        for i in chunk_start..chunk_end {
                            *base.add(i) = pattern.wrapping_add(i as u64);
                        }

                        std::sync::atomic::fence(Ordering::SeqCst);

                        // Read phase for chunk
                        let mut checksum = 0u64;
                        for i in chunk_start..chunk_end {
                            checksum = checksum.wrapping_add(*base.add(i));
                        }

                        // Prevent optimization
                        std::ptr::write_volatile(&mut checksum, checksum);
                    }
                    _ => {
                        // Multiple streams - interleaved access for bandwidth within chunk
                        let chunk_len = chunk_end - chunk_start;
                        let stream_size = chunk_len >> stream_shift;

                        // Write phase with multiple streams within chunk
                        for stream in 0..streams {
                            let pattern = 0x5555AAAA5555AAAAu64
                                .wrapping_add(thread_id as u64)
                                .wrapping_add(cycle as u64)
                                .wrapping_add((stream as u64) << 32);

                            let start_idx = chunk_start + stream * stream_size;
                            let end_idx = if stream == config.streams as usize - 1 {
                                chunk_end // Last stream handles remainder
                            } else {
                                chunk_start + (stream + 1) * stream_size
                            };

                            for i in start_idx..end_idx {
                                *base.add(i) = pattern.wrapping_add(i as u64);
                            }
                        }

                        std::sync::atomic::fence(Ordering::SeqCst);

                        // Read phase with multiple streams within chunk
                        let mut checksums = vec![0u64; config.streams as usize];
                        for stream in 0..streams {
                            let start_idx = chunk_start + stream * stream_size;
                            let end_idx = if stream == config.streams as usize - 1 {
                                chunk_end
                            } else {
                                chunk_start + (stream + 1) * stream_size
                            };

                            for i in start_idx..end_idx {
                                checksums[stream] = checksums[stream].wrapping_add(*base.add(i));
                            }
                        }

                        // Prevent optimization
                        for checksum in &mut checksums {
                            std::ptr::write_volatile(checksum, *checksum);
                        }
                    }
                }

                // Check for shutdown after each chunk (responsive shutdown!)
                if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
                    total_bytes_processed += (chunk_end - chunk_start) * std::mem::size_of::<u64>() * 2;
                    let elapsed = start.elapsed().as_millis();
                    return TestStats {
                        name: test_name,
                        action: TestAction::ReadWrite,
                        bytes_processed: total_bytes_processed,
                        elapsed_ms: elapsed,
                        thread_id,
                        error_count: 0,
                        total_operations: cycle as u64 * (chunk_end - chunk_start) as u64,
                        cycles_completed: cycle,
                        cycles_planned: timing.cycles,
                        stopped_by_time_limit: false,
                    };
                }
            }

            total_bytes_processed += test_block.test_size * 2; // Read + Write
        }

        if let Some(progress) = progress {
            let now = Instant::now();
            if now.duration_since(last_progress_update).as_millis() >= 250 {
                progress.cycles_completed.store(cycle, Ordering::Relaxed);
                progress.bytes_processed.store(total_bytes_processed as u64, Ordering::Relaxed);
                progress.last_update_ms.store(start.elapsed().as_millis() as u64, Ordering::Relaxed);
                last_progress_update = now;
            }
        }

        let elapsed_secs = test_start.elapsed().as_secs() as u32;
        if !timing.should_continue(cycle, elapsed_secs) {
            let elapsed = start.elapsed().as_millis();
            let total_operations: u64 = cycle as u64 * (total_test_size / std::mem::size_of::<u64>()) as u64;
            return TestStats {
                name: test_name,
                action: TestAction::ReadWrite,
                bytes_processed: total_bytes_processed,
                elapsed_ms: elapsed,
                thread_id,
                error_count: 0,
                total_operations,
                cycles_completed: cycle,
                cycles_planned: timing.cycles,
                stopped_by_time_limit: timing.cycles.map_or(true, |limit| cycle < limit),
            };
        }
    }
}

/// BlockMove MultiBlock - sequential block memory moves.
pub unsafe fn block_move_multi(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> TestStats {
    let test_name = "Mem-BlockMove";
    let start = Instant::now();
    let total_allocated: usize = blocks.iter().map(|b| b.buffer.size()).sum();
    let window_size = config.calculate_window_size(test_name, total_allocated);
    let test_blocks = prepare_blocks_for_window(blocks, window_size, test_name);
    let total_test_size: usize = test_blocks.iter().map(|b| b.test_size).sum();

    log::info!("[Thread {}] Running {} on {:.2} MB of memory (window: {:.2} MB)",
              thread_id, test_name, total_test_size as f64 / MB_F64, window_size as f64 / MB_F64);

    let mut cycle = 0u32;
    let test_start = Instant::now();
    let mut total_error_count = 0u64;
    let mut total_bytes_processed = 0usize;
    let mut last_progress_update = Instant::now();

    // Initialize source memory in each block
    let pattern_base = 0xDEADBEEFCAFEBABEu64;
    for test_block in test_blocks.iter() {
        // Divide block in half: first half = source, second half = destination
        let half_size = test_block.test_size / 2;
        let src_base = test_block.block.buffer.as_mut_ptr() as *mut u64;
        let len = half_size / std::mem::size_of::<u64>();

        // Initialize source with pattern
        for i in 0..len {
            *src_base.add(i) = pattern_base.wrapping_add(i as u64);
        }
    }
    std::sync::atomic::fence(Ordering::SeqCst);

    loop {
        cycle += 1;
        let mut cycle_errors = 0u64;

        for test_block in test_blocks.iter() {
            // Divide block: source (first half) → destination (second half)
            let half_size = test_block.test_size / 2;
            let src_base = test_block.block.buffer.as_mut_ptr() as *mut u64;
            let dst_base = src_base.add(half_size / std::mem::size_of::<u64>());
            let len = half_size / std::mem::size_of::<u64>();

            // Calculate chunk size for responsive shutdown
            let ideal_chunk_size = calculate_ideal_chunk_size(config, test_name, test_block.test_size);
            let chunk_size_bytes = get_safe_chunk_size(ideal_chunk_size, test_block.test_size);
            let chunk_size_operations = chunk_size_bytes / (2 * std::mem::size_of::<u64>());

            // Process in chunks
            let mut processed = 0;
            while processed < len {
                let chunk_end = (processed + chunk_size_operations).min(len);

                // Copy from source to destination with stream patterns
                match config.streams {
                    1 => {
                        // Single stream: Simple forward copy
                        for i in processed..chunk_end {
                            let val = *src_base.add(i);
                            *dst_base.add(i) = val;
                        }
                    }
                    2 => {
                        // Two streams: Copy forward and backward simultaneously
                        let chunk_size = chunk_end - processed;
                        let mid = processed + chunk_size / 2;

                        // Stream 1: First half forward
                        for i in processed..mid {
                            *dst_base.add(i) = *src_base.add(i);
                        }

                        // Stream 2: Second half backward
                        for i in 0..(chunk_end - mid) {
                            let idx = chunk_end - 1 - i;
                            *dst_base.add(idx) = *src_base.add(idx);
                        }
                    }
                    4 => {
                        // Four streams: Interleaved block copy with different patterns
                        let chunk_size = chunk_end - processed;
                        let block_size = chunk_size / 4;

                        for stream in 0..4 {
                            let start_idx = processed + stream * block_size;
                            let end_idx = (processed + (stream + 1) * block_size).min(chunk_end);

                            if start_idx < end_idx {
                                match stream & 3 {
                                    0 | 3 => {
                                        // Forward copy
                                        for i in start_idx..end_idx {
                                            *dst_base.add(i) = *src_base.add(i);
                                        }
                                    }
                                    1 => {
                                        // Backward copy within block
                                        for i in 0..(end_idx - start_idx) {
                                            let idx = end_idx - 1 - i;
                                            *dst_base.add(idx) = *src_base.add(idx);
                                        }
                                    }
                                    _ => {
                                        // Skip pattern (every other element)
                                        for i in (start_idx..end_idx).step_by(2) {
                                            *dst_base.add(i) = *src_base.add(i);
                                        }
                                        for i in ((start_idx + 1)..end_idx).step_by(2) {
                                            *dst_base.add(i) = *src_base.add(i);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    _ => {
                        // Many streams: Strided copy pattern
                        let streams = config.streams as usize;
                        for stream in 0..streams {
                            for i in (processed + stream..chunk_end).step_by(streams) {
                                *dst_base.add(i) = *src_base.add(i);
                            }
                        }
                    }
                }

                std::sync::atomic::fence(Ordering::SeqCst);

                // Verify copied data
                for i in processed..chunk_end {
                    let expected = pattern_base.wrapping_add(i as u64);
                    let actual = *dst_base.add(i);
                    if actual != expected {
                        cycle_errors += 1;
                        log::error!("{}: error at {} - expected {:#x}, got {:#x}",
                                   test_name, i, expected, actual);
                    }
                }

                // Handle errors
                if cycle_errors > 0 {
                    match error_mode {
                        ErrorMode::Panic => {
                            panic!("{}: {} errors detected in cycle {}", test_name, cycle_errors, cycle);
                        }
                        ErrorMode::Halt => {
                            total_error_count += cycle_errors;
                            let elapsed = start.elapsed().as_millis();
                            return TestStats {
                                name: test_name,
                                action: TestAction::ReadWrite,
                                bytes_processed: total_bytes_processed,
                                elapsed_ms: elapsed,
                                thread_id,
                                error_count: total_error_count,
                                total_operations: cycle as u64 * len as u64,
                                cycles_completed: cycle,
                                cycles_planned: timing.cycles,
                                stopped_by_time_limit: false,
                            };
                        }
                        ErrorMode::Log => {
                            // Continue - errors already logged
                        }
                    }
                }

                // Check for shutdown
                if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
                    total_error_count += cycle_errors;
                    total_bytes_processed += (chunk_end - processed) * std::mem::size_of::<u64>() * 2;
                    let elapsed = start.elapsed().as_millis();
                    return TestStats {
                        name: test_name,
                        action: TestAction::ReadWrite,
                        bytes_processed: total_bytes_processed,
                        elapsed_ms: elapsed,
                        thread_id,
                        error_count: total_error_count,
                        total_operations: cycle as u64 * (chunk_end - processed) as u64,
                        cycles_completed: cycle,
                        cycles_planned: timing.cycles,
                        stopped_by_time_limit: false,
                    };
                }

                processed = chunk_end;
            }

            total_bytes_processed += test_block.test_size * 3 / 2; // Copy (R source + W dest = 1×) + Verify (R dest = 0.5×) = 1.5×
        }

        total_error_count += cycle_errors;

        // Progress updates
        if let Some(progress) = progress {
            let now = Instant::now();
            if now.duration_since(last_progress_update).as_millis() >= 250 {
                progress.cycles_completed.store(cycle, Ordering::Relaxed);
                progress.bytes_processed.store(total_bytes_processed as u64, Ordering::Relaxed);
                progress.errors_found.store(total_error_count, Ordering::Relaxed);
                progress.last_update_ms.store(start.elapsed().as_millis() as u64, Ordering::Relaxed);
                last_progress_update = now;
            }
        }

        // Check timing
        let elapsed_secs = test_start.elapsed().as_secs() as u32;
        if !timing.should_continue(cycle, elapsed_secs) {
            let elapsed = start.elapsed().as_millis();
            let total_operations: u64 = cycle as u64 * (total_test_size / (std::mem::size_of::<u64>() * 2)) as u64;
            return TestStats {
                name: test_name,
                action: TestAction::ReadWrite,
                bytes_processed: total_bytes_processed,
                elapsed_ms: elapsed,
                thread_id,
                error_count: total_error_count,
                total_operations,
                cycles_completed: cycle,
                cycles_planned: timing.cycles,
                stopped_by_time_limit: timing.cycles.map_or(true, |limit| cycle < limit),
            };
        }
    }
}

// ================================================================================================
// Legacy RefreshStable Implementations (Old Single-Block Pattern)
// ================================================================================================

/// # Safety
/// Caller must ensure `ptr` is valid for reads/writes of `size` bytes.
/// # Safety
/// Caller must ensure `ptr` is valid for reads/writes of `size` bytes.

/// # Safety
/// Caller must ensure `ptr` is valid for reads/writes of `size` bytes.


// === UPDATED TEST FUNCTIONS WITH STREAM SUPPORT ===


/// # Safety
/// Caller must ensure all blocks are valid for reads/writes.
///
/// MirrorMoveAuto MultiBlock implementation - auto-dispatches to best available SIMD variant.
/// Detects CPU features at runtime and routes to AVX-512, AVX2, SSE2, or scalar implementation.
pub unsafe fn mirror_move_auto_multi(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> TestStats {
    // Auto-dispatch to best available SIMD implementation
    if is_x86_feature_detected!("avx512f") {
        mirror_move_512_multi(blocks, thread_id, error_mode, timing, config, progress)
    } else if is_x86_feature_detected!("avx2") {
        mirror_move_256_multi(blocks, thread_id, error_mode, timing, config, progress)
    } else if is_x86_feature_detected!("sse2") {
        mirror_move_128_multi(blocks, thread_id, error_mode, timing, config, progress)
    } else {
        // Fallback to scalar implementation
        mirror_move_multi(blocks, thread_id, error_mode, timing, config, progress)
    }
}

/// # Safety
/// Caller must ensure `ptr` is valid for reads/writes of `size` bytes.
/// 
/// SSE2 128-bit SIMD MirrorMove implementation.
/// 
/// Error Reporting Style: Binary chunk-level reporting (matches TM5 SIMD approach)
/// - Uses SIMD error accumulation across entire chunk
/// - Reports 1 error per failed chunk regardless of actual error count within chunk
/// - Optimized for maximum performance in hot loops
/// # Safety
/// Caller must ensure `ptr` is valid for reads/writes of `size` bytes.
/// # Safety
/// Caller must ensure `ptr` is valid for reads/writes of `size` bytes.
/// # Safety
/// Caller must ensure `ptr` is valid for reads/writes of `size` bytes.

/// Validate chunk divisibility for multi-stream tests
/// Ensures chunk sizes are evenly divisible by stream count
/// Note: Stream power-of-2 validation happens at config creation time
fn validate_chunk_divisibility(
    test_name: &str,
    streams: u32,
    chunk_size_elements: usize,
) {
    // Validate chunk is evenly divisible by stream count
    // This should always pass since chunks are power-of-2 and streams are power-of-2,
    // but we check defensively to catch any edge cases
    if streams > 1 && chunk_size_elements % (streams as usize) != 0 {
        panic!(
            "{}: Chunk size {} elements not evenly divisible by {} streams (remainder: {} elements). \
             This should never happen if chunk and stream are both power-of-2. This is a bug.",
            test_name,
            chunk_size_elements,
            streams,
            chunk_size_elements % (streams as usize)
        );
    }
}

/// Multi-block version of simple_test that handles interleaving internally
/// Dispatches to the appropriate stream implementation based on config.streams
/// # Safety
/// Caller must ensure all blocks contain valid memory for testing
pub unsafe fn simple_test_multi(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> TestStats {
    let test_name = "Mem-Simple";

    if blocks.is_empty() {
        return TestStats {
            name: test_name,
            action: TestAction::WriteVerify,
            bytes_processed: 0,
            elapsed_ms: 0,
            thread_id,
            error_count: 0,
            total_operations: 0,
            cycles_completed: 0,
            cycles_planned: timing.cycles,
            stopped_by_time_limit: false,
        };
    }

    // Calculate pattern base once
    let pattern_base = if let (Some(mode), Some(param0), Some(param1)) = (config.pattern_mode, config.pattern_param0, config.pattern_param1) {
        match mode {
            1 => param0 ^ param1,
            2 => (param0 << 32) | (param1 & 0xFFFFFFFF),
            _ => 0xDEADBEEFDEADBEEF,
        }
    } else {
        0xDEADBEEFDEADBEEF
    };

    // Dispatch to appropriate stream implementation
    match config.streams {
        1 => simple_test_stream1_multi(blocks, thread_id, error_mode, timing, config, progress, pattern_base, test_name),
        2 => simple_test_stream2_multi(blocks, thread_id, error_mode, timing, config, progress, pattern_base, test_name),
        4 => simple_test_stream4_multi(blocks, thread_id, error_mode, timing, config, progress, pattern_base, test_name),
        _ => simple_test_stream_n_multi(blocks, thread_id, error_mode, timing, config, progress, pattern_base, test_name),
    }
}

/// SimpleTest Stream1 - MultiBlock version (linear sequential access)
unsafe fn simple_test_stream1_multi(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
    pattern_base: u64,
    test_name: &'static str,
) -> TestStats {
    // Calculate window size and prepare blocks with window limits
    let total_allocated: usize = blocks.iter().map(|b| b.buffer.size()).sum();
    let window_size = config.calculate_window_size(test_name, total_allocated);
    let test_blocks = prepare_blocks_for_window(blocks, window_size, test_name);

    if test_blocks.is_empty() {
        log::warn!("{}: No blocks prepared for testing (window too small?)", test_name);
        return TestStats {
            name: test_name,
            action: TestAction::WriteVerify,
            bytes_processed: 0,
            elapsed_ms: 0,
            thread_id,
            error_count: 0,
            total_operations: 0,
            cycles_completed: 0,
            cycles_planned: timing.cycles,
            stopped_by_time_limit: false,
        };
    }

    // Track totals across all blocks
    let mut total_bytes_processed = 0usize;
    let mut total_error_count = 0u64;
    let mut total_operations = 0u64;

    // Single shared timer for ALL blocks
    let test_start = Instant::now();
    let start = Instant::now();
    let mut cycle = 0u32;

    // Progress reporting setup
    let update_interval_ms = 1000;
    let mut last_progress_update = Instant::now();

    // Main test loop - interleaves across blocks
    loop {
        cycle += 1;

        // Test each block in sequence for this cycle (interleaved: 1,2,3,1,2,3...)
        for test_block in test_blocks.iter() {
            let ptr = test_block.block.buffer.as_mut_ptr() as *mut u64;
            let len = test_block.test_size / std::mem::size_of::<u64>();

            // Calculate chunk size for this block's test_size
            let ideal_chunk_size = calculate_ideal_chunk_size(config, test_name, test_block.test_size);
            let chunk_size_bytes = get_safe_chunk_size(ideal_chunk_size, test_block.test_size);
            let chunk_size_elements = chunk_size_bytes / std::mem::size_of::<u64>();

            let mut block_errors = 0u64;

            // Process block in chunks for responsive shutdown
            for chunk_start in (0..len).step_by(chunk_size_elements) {
                let chunk_end = (chunk_start + chunk_size_elements).min(len);

                // Write phase for this chunk (stream 1: linear sequential)
                for idx in chunk_start..chunk_end {
                    *ptr.add(idx) = idx as u64 ^ pattern_base;
                }

                std::sync::atomic::fence(Ordering::SeqCst);

                // Verify phase for this chunk
                for idx in chunk_start..chunk_end {
                    let v = *ptr.add(idx);
                    let expected = idx as u64 ^ pattern_base;
                    if v != expected {
                        block_errors += 1;
                        log::error!("{}: memory error at index {} - expected {:#x}, got {:#x}",
                                   test_name, idx, expected, v);
                    }
                }

                // Check for shutdown after each chunk
                if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
                    total_error_count += block_errors;
                    total_bytes_processed += (chunk_end - chunk_start) * std::mem::size_of::<u64>() * 2;
                    let elapsed = start.elapsed().as_millis();
                    return TestStats {
                        name: test_name,
                        action: TestAction::WriteVerify,
                        bytes_processed: total_bytes_processed,
                        elapsed_ms: elapsed,
                        thread_id,
                        error_count: total_error_count,
                        total_operations,
                        cycles_completed: cycle,
                        cycles_planned: timing.cycles,
                        stopped_by_time_limit: false,
                    };
                }
            }

            total_error_count += block_errors;
            total_bytes_processed += test_block.test_size * 2; // write + read
            total_operations += len as u64 * 2;

            // Handle errors based on mode
            if block_errors > 0 {
                match error_mode {
                    ErrorMode::Panic => panic!("{}: {} memory errors detected", test_name, block_errors),
                    ErrorMode::Halt => {
                        let elapsed = start.elapsed().as_millis();
                        return TestStats {
                            name: test_name,
                            action: TestAction::WriteVerify,
                            bytes_processed: total_bytes_processed,
                            elapsed_ms: elapsed,
                            thread_id,
                            error_count: total_error_count,
                            total_operations,
                            cycles_completed: cycle,
                            cycles_planned: timing.cycles,
                            stopped_by_time_limit: false,
                        };
                    }
                    ErrorMode::Log => { /* Continue */ }
                }
            }

            // Check for shutdown between blocks
            if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
                let elapsed = start.elapsed().as_millis();
                return TestStats {
                    name: test_name,
                    action: TestAction::WriteVerify,
                    bytes_processed: total_bytes_processed,
                    elapsed_ms: elapsed,
                    thread_id,
                    error_count: total_error_count,
                    total_operations,
                    cycles_completed: cycle,
                    cycles_planned: timing.cycles,
                    stopped_by_time_limit: false,
                };
            }
        }

        // Update progress if provided
        if let Some(progress) = progress {
            let now = Instant::now();
            if now.duration_since(last_progress_update).as_millis() >= update_interval_ms {
                progress.cycles_completed.store(cycle, Ordering::Relaxed);
                progress.bytes_processed.store(total_bytes_processed as u64, Ordering::Relaxed);
                progress.errors_found.store(total_error_count, Ordering::Relaxed);
                progress.last_update_ms.store(now.duration_since(start).as_millis() as u64, Ordering::Relaxed);
                last_progress_update = now;
            }
        }

        // Check timing - shared across ALL blocks
        let elapsed_secs = test_start.elapsed().as_secs() as u32;
        if !timing.should_continue(cycle, elapsed_secs) {
            break;
        }
    }

    TestStats {
        name: test_name,
        action: TestAction::WriteVerify,
        bytes_processed: total_bytes_processed,
        elapsed_ms: start.elapsed().as_millis(),
        thread_id,
        error_count: total_error_count,
        total_operations,
        cycles_completed: cycle,
        cycles_planned: timing.cycles,
        stopped_by_time_limit: timing.cycles.map_or(true, |limit| cycle < limit),
    }
}

/// SimpleTest Stream2 - MultiBlock version (2-way interleaved access)
unsafe fn simple_test_stream2_multi(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
    pattern_base: u64,
    test_name: &'static str,
) -> TestStats {
    // Calculate window size and prepare blocks with window limits
    let total_allocated: usize = blocks.iter().map(|b| b.buffer.size()).sum();
    let window_size = config.calculate_window_size(test_name, total_allocated);
    let test_blocks = prepare_blocks_for_window(blocks, window_size, test_name);

    if test_blocks.is_empty() {
        log::warn!("{}: No blocks prepared for testing (window too small?)", test_name);
        return TestStats {
            name: test_name,
            action: TestAction::WriteVerify,
            bytes_processed: 0,
            elapsed_ms: 0,
            thread_id,
            error_count: 0,
            total_operations: 0,
            cycles_completed: 0,
            cycles_planned: timing.cycles,
            stopped_by_time_limit: false,
        };
    }

    // Track totals across all blocks
    let mut total_bytes_processed = 0usize;
    let mut total_error_count = 0u64;
    let mut total_operations = 0u64;

    // Single shared timer for ALL blocks
    let test_start = Instant::now();
    let start = Instant::now();
    let mut cycle = 0u32;

    // Progress reporting setup
    let update_interval_ms = 1000;
    let mut last_progress_update = Instant::now();

    // Validate chunk divisibility for ALL blocks ONCE before starting (not in hot loop)
    // Different block sizes may have different chunk sizes, so validate each
    for test_block in test_blocks.iter() {
        let ideal_chunk_size = calculate_ideal_chunk_size(config, test_name, test_block.test_size);
        let chunk_size_bytes = get_safe_chunk_size(ideal_chunk_size, test_block.test_size);
        let chunk_size_elements = chunk_size_bytes / std::mem::size_of::<u64>();
        validate_chunk_divisibility(test_name, 2, chunk_size_elements);
    }

    // Main test loop - interleaves across blocks
    loop {
        cycle += 1;

        // Test each block in sequence for this cycle (interleaved: 1,2,3,1,2,3...)
        for test_block in test_blocks.iter() {
            let ptr = test_block.block.buffer.as_mut_ptr() as *mut u64;
            let len = test_block.test_size / std::mem::size_of::<u64>();

            // Calculate chunk size for this block's test_size
            let ideal_chunk_size = calculate_ideal_chunk_size(config, test_name, test_block.test_size);
            let chunk_size_bytes = get_safe_chunk_size(ideal_chunk_size, test_block.test_size);
            let chunk_size_elements = chunk_size_bytes / std::mem::size_of::<u64>();

            let mut block_errors = 0u64;

            // Process block in chunks for responsive shutdown
            for chunk_start in (0..len).step_by(chunk_size_elements) {
                let chunk_end = (chunk_start + chunk_size_elements).min(len);
                let chunk_len = chunk_end - chunk_start;
                let chunk_half = chunk_len / 2;
                let mid_point = chunk_start + chunk_half;

                // Write both halves with different patterns (stream 2: 2-way interleave)
                for idx in chunk_start..mid_point {
                    *ptr.add(idx) = idx as u64 ^ pattern_base;
                    *ptr.add(idx + chunk_half) = (idx + chunk_half) as u64 ^ !pattern_base;
                }

                std::sync::atomic::fence(Ordering::SeqCst);

                // Verify both halves
                for idx in chunk_start..mid_point {
                    let v1 = *ptr.add(idx);
                    let v2 = *ptr.add(idx + chunk_half);
                    let expected1 = idx as u64 ^ pattern_base;
                    let expected2 = (idx + chunk_half) as u64 ^ !pattern_base;

                    if v1 != expected1 {
                        block_errors += 1;
                        log::error!("{}: memory error at index {} - expected {:#x}, got {:#x}",
                                   test_name, idx, expected1, v1);
                    }
                    if v2 != expected2 {
                        block_errors += 1;
                        log::error!("{}: memory error at index {} - expected {:#x}, got {:#x}",
                                   test_name, idx + chunk_half, expected2, v2);
                    }
                }

                // Check for shutdown after each chunk
                if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
                    total_error_count += block_errors;
                    total_bytes_processed += chunk_len * std::mem::size_of::<u64>() * 2;
                    let elapsed = start.elapsed().as_millis();
                    return TestStats {
                        name: test_name,
                        action: TestAction::WriteVerify,
                        bytes_processed: total_bytes_processed,
                        elapsed_ms: elapsed,
                        thread_id,
                        error_count: total_error_count,
                        total_operations,
                        cycles_completed: cycle,
                        cycles_planned: timing.cycles,
                        stopped_by_time_limit: false,
                    };
                }
            }

            total_error_count += block_errors;
            total_bytes_processed += test_block.test_size * 2; // write + read
            total_operations += len as u64 * 2;

            // Handle errors based on mode
            if block_errors > 0 {
                match error_mode {
                    ErrorMode::Panic => panic!("{}: {} memory errors detected", test_name, block_errors),
                    ErrorMode::Halt => {
                        let elapsed = start.elapsed().as_millis();
                        return TestStats {
                            name: test_name,
                            action: TestAction::WriteVerify,
                            bytes_processed: total_bytes_processed,
                            elapsed_ms: elapsed,
                            thread_id,
                            error_count: total_error_count,
                            total_operations,
                            cycles_completed: cycle,
                            cycles_planned: timing.cycles,
                            stopped_by_time_limit: false,
                        };
                    }
                    ErrorMode::Log => { /* Continue */ }
                }
            }

            // Check for shutdown between blocks
            if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
                let elapsed = start.elapsed().as_millis();
                return TestStats {
                    name: test_name,
                    action: TestAction::WriteVerify,
                    bytes_processed: total_bytes_processed,
                    elapsed_ms: elapsed,
                    thread_id,
                    error_count: total_error_count,
                    total_operations,
                    cycles_completed: cycle,
                    cycles_planned: timing.cycles,
                    stopped_by_time_limit: false,
                };
            }
        }

        // Update progress if provided
        if let Some(progress) = progress {
            let now = Instant::now();
            if now.duration_since(last_progress_update).as_millis() >= update_interval_ms {
                progress.cycles_completed.store(cycle, Ordering::Relaxed);
                progress.bytes_processed.store(total_bytes_processed as u64, Ordering::Relaxed);
                progress.errors_found.store(total_error_count, Ordering::Relaxed);
                progress.last_update_ms.store(now.duration_since(start).as_millis() as u64, Ordering::Relaxed);
                last_progress_update = now;
            }
        }

        // Check timing - shared across ALL blocks
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
        total_operations,
        cycles_completed: cycle,
        cycles_planned: timing.cycles,
        stopped_by_time_limit: timing.cycles.map_or(true, |limit| cycle < limit),
    }
}

/// SimpleTest Stream4 - MultiBlock version (4-way interleaved access)
unsafe fn simple_test_stream4_multi(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
    pattern_base: u64,
    test_name: &'static str,
) -> TestStats {
    // Calculate window size and prepare blocks with window limits
    let total_allocated: usize = blocks.iter().map(|b| b.buffer.size()).sum();
    let window_size = config.calculate_window_size(test_name, total_allocated);
    let test_blocks = prepare_blocks_for_window(blocks, window_size, test_name);

    if test_blocks.is_empty() {
        log::warn!("{}: No blocks prepared for testing (window too small?)", test_name);
        return TestStats {
            name: test_name,
            action: TestAction::WriteVerify,
            bytes_processed: 0,
            elapsed_ms: 0,
            thread_id,
            error_count: 0,
            total_operations: 0,
            cycles_completed: 0,
            cycles_planned: timing.cycles,
            stopped_by_time_limit: false,
        };
    }

    // Track totals across all blocks
    let mut total_bytes_processed = 0usize;
    let mut total_error_count = 0u64;
    let mut total_operations = 0u64;

    // Single shared timer for ALL blocks
    let test_start = Instant::now();
    let start = Instant::now();
    let mut cycle = 0u32;

    // Progress reporting setup
    let update_interval_ms = 1000;
    let mut last_progress_update = Instant::now();

    // Validate chunk divisibility for ALL blocks ONCE before starting (not in hot loop)
    // Different block sizes may have different chunk sizes, so validate each
    for test_block in test_blocks.iter() {
        let ideal_chunk_size = calculate_ideal_chunk_size(config, test_name, test_block.test_size);
        let chunk_size_bytes = get_safe_chunk_size(ideal_chunk_size, test_block.test_size);
        let chunk_size_elements = chunk_size_bytes / std::mem::size_of::<u64>();
        validate_chunk_divisibility(test_name, 4, chunk_size_elements);
    }

    // Main test loop - interleaves across blocks
    loop {
        cycle += 1;

        // Test each block in sequence for this cycle (interleaved: 1,2,3,1,2,3...)
        for test_block in test_blocks.iter() {
            let ptr = test_block.block.buffer.as_mut_ptr() as *mut u64;
            let len = test_block.test_size / std::mem::size_of::<u64>();

            // Calculate chunk size for this block's test_size
            let ideal_chunk_size = calculate_ideal_chunk_size(config, test_name, test_block.test_size);
            let chunk_size_bytes = get_safe_chunk_size(ideal_chunk_size, test_block.test_size);
            let chunk_size_elements = chunk_size_bytes / std::mem::size_of::<u64>();

            let mut block_errors = 0u64;

            // Process block in chunks for responsive shutdown
            for chunk_start in (0..len).step_by(chunk_size_elements) {
                let chunk_end = (chunk_start + chunk_size_elements).min(len);
                let chunk_len = chunk_end - chunk_start;
                let quarter = chunk_len / 4;

                // Write four quarters with different rotated patterns (stream 4: 4-way interleave)
                for q_idx in 0..quarter {
                    let idx0 = chunk_start + q_idx;
                    let idx1 = chunk_start + quarter + q_idx;
                    let idx2 = chunk_start + 2 * quarter + q_idx;
                    let idx3 = chunk_start + 3 * quarter + q_idx;

                    *ptr.add(idx0) = idx0 as u64 ^ pattern_base;
                    *ptr.add(idx1) = idx1 as u64 ^ pattern_base.rotate_left(16);
                    *ptr.add(idx2) = idx2 as u64 ^ pattern_base.rotate_left(32);
                    *ptr.add(idx3) = idx3 as u64 ^ pattern_base.rotate_left(48);
                }

                std::sync::atomic::fence(Ordering::SeqCst);

                // Verify four quarters
                for q_idx in 0..quarter {
                    let idx0 = chunk_start + q_idx;
                    let idx1 = chunk_start + quarter + q_idx;
                    let idx2 = chunk_start + 2 * quarter + q_idx;
                    let idx3 = chunk_start + 3 * quarter + q_idx;

                    let v0 = *ptr.add(idx0);
                    let v1 = *ptr.add(idx1);
                    let v2 = *ptr.add(idx2);
                    let v3 = *ptr.add(idx3);

                    let expected0 = idx0 as u64 ^ pattern_base;
                    let expected1 = idx1 as u64 ^ pattern_base.rotate_left(16);
                    let expected2 = idx2 as u64 ^ pattern_base.rotate_left(32);
                    let expected3 = idx3 as u64 ^ pattern_base.rotate_left(48);

                    if v0 != expected0 {
                        block_errors += 1;
                        log::error!("{}: memory error at index {} - expected {:#x}, got {:#x}",
                                   test_name, idx0, expected0, v0);
                    }
                    if v1 != expected1 {
                        block_errors += 1;
                        log::error!("{}: memory error at index {} - expected {:#x}, got {:#x}",
                                   test_name, idx1, expected1, v1);
                    }
                    if v2 != expected2 {
                        block_errors += 1;
                        log::error!("{}: memory error at index {} - expected {:#x}, got {:#x}",
                                   test_name, idx2, expected2, v2);
                    }
                    if v3 != expected3 {
                        block_errors += 1;
                        log::error!("{}: memory error at index {} - expected {:#x}, got {:#x}",
                                   test_name, idx3, expected3, v3);
                    }
                }

                // Check for shutdown after each chunk
                if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
                    total_error_count += block_errors;
                    total_bytes_processed += chunk_len * std::mem::size_of::<u64>() * 2;
                    let elapsed = start.elapsed().as_millis();
                    return TestStats {
                        name: test_name,
                        action: TestAction::WriteVerify,
                        bytes_processed: total_bytes_processed,
                        elapsed_ms: elapsed,
                        thread_id,
                        error_count: total_error_count,
                        total_operations,
                        cycles_completed: cycle,
                        cycles_planned: timing.cycles,
                        stopped_by_time_limit: false,
                    };
                }
            }

            total_error_count += block_errors;
            total_bytes_processed += test_block.test_size * 2; // write + read
            total_operations += len as u64 * 2;

            // Handle errors based on mode
            if block_errors > 0 {
                match error_mode {
                    ErrorMode::Panic => panic!("{}: {} memory errors detected", test_name, block_errors),
                    ErrorMode::Halt => {
                        let elapsed = start.elapsed().as_millis();
                        return TestStats {
                            name: test_name,
                            action: TestAction::WriteVerify,
                            bytes_processed: total_bytes_processed,
                            elapsed_ms: elapsed,
                            thread_id,
                            error_count: total_error_count,
                            total_operations,
                            cycles_completed: cycle,
                            cycles_planned: timing.cycles,
                            stopped_by_time_limit: false,
                        };
                    }
                    ErrorMode::Log => { /* Continue */ }
                }
            }

            // Check for shutdown between blocks
            if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
                let elapsed = start.elapsed().as_millis();
                return TestStats {
                    name: test_name,
                    action: TestAction::WriteVerify,
                    bytes_processed: total_bytes_processed,
                    elapsed_ms: elapsed,
                    thread_id,
                    error_count: total_error_count,
                    total_operations,
                    cycles_completed: cycle,
                    cycles_planned: timing.cycles,
                    stopped_by_time_limit: false,
                };
            }
        }

        // Update progress if provided
        if let Some(progress) = progress {
            let now = Instant::now();
            if now.duration_since(last_progress_update).as_millis() >= update_interval_ms {
                progress.cycles_completed.store(cycle, Ordering::Relaxed);
                progress.bytes_processed.store(total_bytes_processed as u64, Ordering::Relaxed);
                progress.errors_found.store(total_error_count, Ordering::Relaxed);
                progress.last_update_ms.store(now.duration_since(start).as_millis() as u64, Ordering::Relaxed);
                last_progress_update = now;
            }
        }

        // Check timing - shared across ALL blocks
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
        total_operations,
        cycles_completed: cycle,
        cycles_planned: timing.cycles,
        stopped_by_time_limit: timing.cycles.map_or(true, |limit| cycle < limit),
    }
}

/// SimpleTest StreamN - MultiBlock version (N-way interleaved access)
unsafe fn simple_test_stream_n_multi(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
    pattern_base: u64,
    test_name: &'static str,
) -> TestStats {
    // Calculate window size and prepare blocks with window limits
    let total_allocated: usize = blocks.iter().map(|b| b.buffer.size()).sum();
    let window_size = config.calculate_window_size(test_name, total_allocated);
    let test_blocks = prepare_blocks_for_window(blocks, window_size, test_name);

    if test_blocks.is_empty() {
        log::warn!("{}: No blocks prepared for testing (window too small?)", test_name);
        return TestStats {
            name: test_name,
            action: TestAction::WriteVerify,
            bytes_processed: 0,
            elapsed_ms: 0,
            thread_id,
            error_count: 0,
            total_operations: 0,
            cycles_completed: 0,
            cycles_planned: timing.cycles,
            stopped_by_time_limit: false,
        };
    }

    let n = config.streams as usize;

    // Precompute rotation step ONCE (hot loop optimization)
    // Ensures each stream gets unique rotation: N=8 → step=8, N=16 → step=4
    let rotation_step = 64 / n;

    // Track totals across all blocks
    let mut total_bytes_processed = 0usize;
    let mut total_error_count = 0u64;
    let mut total_operations = 0u64;

    // Single shared timer for ALL blocks
    let test_start = Instant::now();
    let start = Instant::now();
    let mut cycle = 0u32;

    // Progress reporting setup
    let update_interval_ms = 1000;
    let mut last_progress_update = Instant::now();

    // Validate chunk divisibility for ALL blocks ONCE before starting (not in hot loop)
    // Different block sizes may have different chunk sizes, so validate each
    for test_block in test_blocks.iter() {
        let ideal_chunk_size = calculate_ideal_chunk_size(config, test_name, test_block.test_size);
        let chunk_size_bytes = get_safe_chunk_size(ideal_chunk_size, test_block.test_size);
        let chunk_size_elements = chunk_size_bytes / std::mem::size_of::<u64>();
        validate_chunk_divisibility(test_name, config.streams, chunk_size_elements);
    }

    // Main test loop - interleaves across blocks
    loop {
        cycle += 1;

        // Test each block in sequence for this cycle (interleaved: 1,2,3,1,2,3...)
        for test_block in test_blocks.iter() {
            let ptr = test_block.block.buffer.as_mut_ptr() as *mut u64;
            let len = test_block.test_size / std::mem::size_of::<u64>();

            // Calculate chunk size for this block's test_size
            let ideal_chunk_size = calculate_ideal_chunk_size(config, test_name, test_block.test_size);
            let chunk_size_bytes = get_safe_chunk_size(ideal_chunk_size, test_block.test_size);
            let chunk_size_elements = chunk_size_bytes / std::mem::size_of::<u64>();

            let mut block_errors = 0u64;

            // Process block in chunks for responsive shutdown
            for chunk_start in (0..len).step_by(chunk_size_elements) {
                let chunk_end = (chunk_start + chunk_size_elements).min(len);
                let chunk_len = chunk_end - chunk_start;
                let segment_size = chunk_len / n;

                // Write N segments with different rotated patterns (stream N: N-way interleave)
                for seg_offset in 0..segment_size {
                    for stream_idx in 0..n {
                        let idx = chunk_start + (stream_idx * segment_size) + seg_offset;
                        if idx < chunk_end {
                            let rotation = (stream_idx * rotation_step) as u32;
                            *ptr.add(idx) = idx as u64 ^ pattern_base.rotate_left(rotation);
                        }
                    }
                }

                std::sync::atomic::fence(Ordering::SeqCst);

                // Verify N segments
                for seg_offset in 0..segment_size {
                    for stream_idx in 0..n {
                        let idx = chunk_start + (stream_idx * segment_size) + seg_offset;
                        if idx < chunk_end {
                            let rotation = (stream_idx * rotation_step) as u32;
                            let v = *ptr.add(idx);
                            let expected = idx as u64 ^ pattern_base.rotate_left(rotation);

                            if v != expected {
                                block_errors += 1;
                                log::error!("{}: memory error at index {} - expected {:#x}, got {:#x}",
                                           test_name, idx, expected, v);
                            }
                        }
                    }
                }

                // Check for shutdown after each chunk
                if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
                    total_error_count += block_errors;
                    total_bytes_processed += chunk_len * std::mem::size_of::<u64>() * 2;
                    let elapsed = start.elapsed().as_millis();
                    return TestStats {
                        name: test_name,
                        action: TestAction::WriteVerify,
                        bytes_processed: total_bytes_processed,
                        elapsed_ms: elapsed,
                        thread_id,
                        error_count: total_error_count,
                        total_operations,
                        cycles_completed: cycle,
                        cycles_planned: timing.cycles,
                        stopped_by_time_limit: false,
                    };
                }
            }

            total_error_count += block_errors;
            total_bytes_processed += test_block.test_size * 2; // write + read
            total_operations += len as u64 * 2;

            // Handle errors based on mode
            if block_errors > 0 {
                match error_mode {
                    ErrorMode::Panic => panic!("{}: {} memory errors detected", test_name, block_errors),
                    ErrorMode::Halt => {
                        let elapsed = start.elapsed().as_millis();
                        return TestStats {
                            name: test_name,
                            action: TestAction::WriteVerify,
                            bytes_processed: total_bytes_processed,
                            elapsed_ms: elapsed,
                            thread_id,
                            error_count: total_error_count,
                            total_operations,
                            cycles_completed: cycle,
                            cycles_planned: timing.cycles,
                            stopped_by_time_limit: false,
                        };
                    }
                    ErrorMode::Log => { /* Continue */ }
                }
            }

            // Check for shutdown between blocks
            if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
                let elapsed = start.elapsed().as_millis();
                return TestStats {
                    name: test_name,
                    action: TestAction::WriteVerify,
                    bytes_processed: total_bytes_processed,
                    elapsed_ms: elapsed,
                    thread_id,
                    error_count: total_error_count,
                    total_operations,
                    cycles_completed: cycle,
                    cycles_planned: timing.cycles,
                    stopped_by_time_limit: false,
                };
            }
        }

        // Update progress if provided
        if let Some(progress) = progress {
            let now = Instant::now();
            if now.duration_since(last_progress_update).as_millis() >= update_interval_ms {
                progress.cycles_completed.store(cycle, Ordering::Relaxed);
                progress.bytes_processed.store(total_bytes_processed as u64, Ordering::Relaxed);
                progress.errors_found.store(total_error_count, Ordering::Relaxed);
                progress.last_update_ms.store(now.duration_since(start).as_millis() as u64, Ordering::Relaxed);
                last_progress_update = now;
            }
        }

        // Check timing - shared across ALL blocks
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
        total_operations,
        cycles_completed: cycle,
        cycles_planned: timing.cycles,
        stopped_by_time_limit: timing.cycles.map_or(true, |limit| cycle < limit),
    }
}

/// MirrorMove test - MultiBlock version
/// Tests memory by mirroring data and verifying the mirror operation
pub unsafe fn mirror_move_multi(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> TestStats {
    let test_name = "Mem-Mirror";

    if blocks.is_empty() {
        return TestStats {
            name: test_name,
            action: TestAction::WriteWaitVerify,
            bytes_processed: 0,
            elapsed_ms: 0,
            thread_id,
            error_count: 0,
            total_operations: 0,
            cycles_completed: 0,
            cycles_planned: timing.cycles,
            stopped_by_time_limit: false,
        };
    }

    // Calculate window size and prepare blocks with window limits
    let total_allocated: usize = blocks.iter().map(|b| b.buffer.size()).sum();
    let window_size = config.calculate_window_size(test_name, total_allocated);
    let test_blocks = prepare_blocks_for_window(blocks, window_size, test_name);

    if test_blocks.is_empty() {
        log::warn!("{}: No blocks prepared for testing (window too small?)", test_name);
        return TestStats {
            name: test_name,
            action: TestAction::WriteWaitVerify,
            bytes_processed: 0,
            elapsed_ms: 0,
            thread_id,
            error_count: 0,
            total_operations: 0,
            cycles_completed: 0,
            cycles_planned: timing.cycles,
            stopped_by_time_limit: false,
        };
    }

    // Pre-compute pattern base outside all loops
    let thread_pattern_base = (thread_id as u64) << 16;

    // Track totals across all blocks
    let mut total_bytes_processed = 0usize;
    let mut total_error_count = 0u64;
    let mut total_operations = 0u64;

    // Single shared timer for ALL blocks
    let test_start = Instant::now();
    let start = Instant::now();
    let mut cycle = 0u32;

    // Progress reporting setup
    let update_interval_ms = 1000;
    let mut last_progress_update = Instant::now();

    // Initialize all test blocks with thread-specific patterns (only the test_size portion)
    for test_block in test_blocks.iter() {
        let ptr = test_block.block.buffer.as_mut_ptr() as *mut u64;
        let len = test_block.test_size / std::mem::size_of::<u64>();

        for i in 0..len {
            let pattern = (i as u64).wrapping_add(thread_pattern_base).wrapping_mul(0x0123456789ABCDEFu64);
            *ptr.add(i) = pattern;
        }
        std::sync::atomic::fence(Ordering::SeqCst);
    }

    // Main test loop - interleaves across blocks
    loop {
        cycle += 1;

        // Test each block in sequence for this cycle (interleaved: 1,2,3,1,2,3...)
        for test_block in test_blocks.iter() {
            let ptr = test_block.block.buffer.as_mut_ptr() as *mut u64;
            let len = test_block.test_size / std::mem::size_of::<u64>();

            // Calculate chunk size for this block's test_size
            let ideal_chunk_size = calculate_ideal_chunk_size(config, test_name, test_block.test_size);
            let chunk_size_bytes = get_safe_chunk_size(ideal_chunk_size, test_block.test_size);
            let chunk_size_elements = chunk_size_bytes / std::mem::size_of::<u64>();

            let mut block_errors = 0u64;

            // Process block in chunks
            for chunk_start in (0..len).step_by(chunk_size_elements) {
                let chunk_end = (chunk_start + chunk_size_elements).min(len);

                // 1. Mirror operation
                let mut idx1 = chunk_start;
                let mut idx2 = chunk_end - 1;
                while idx1 < idx2 {
                    let val1 = *ptr.add(idx1);
                    let val2 = *ptr.add(idx2);
                    *ptr.add(idx2) = val1;
                    *ptr.add(idx1) = val2;
                    idx1 += 1;
                    idx2 -= 1;
                }

                std::sync::atomic::fence(Ordering::SeqCst);

                // 2. Verify mirrored data with configurable error checking
                let mut element_count = 0usize;
                let mut chunk_errors = 0u64;

                match config.error_check_interval.get_check_mask() {
                    Some(check_mask) => {
                        // Intermediate error checking - check at configured intervals
                        for idx in chunk_start..chunk_end {
                            let mirrored_idx = chunk_start + chunk_end - 1 - idx;
                            let expected_pattern = (mirrored_idx as u64).wrapping_add(thread_pattern_base).wrapping_mul(0x0123456789ABCDEFu64);
                            let actual_value = *ptr.add(idx);

                            if actual_value != expected_pattern {
                                chunk_errors += 1;
                                if chunk_errors <= 10 {  // Limit error logging
                                    log::error!("{}: memory error at index {} - expected {:#x}, got {:#x}",
                                               test_name, idx, expected_pattern, actual_value);
                                }
                            }

                            element_count += 1;

                            // Check errors at configured intervals (zero-branch hot loop optimization)
                            if (element_count as u32 & check_mask) == 0 {
                                if chunk_errors > 0 {
                                    block_errors += chunk_errors;
                                    log::error!("{}: {} errors detected at element {} (thread {})",
                                               test_name, chunk_errors, element_count, thread_id);
                                    chunk_errors = 0;  // Reset for next interval
                                }
                            }
                        }

                        // Final check for any remaining errors
                        if chunk_errors > 0 {
                            block_errors += chunk_errors;
                        }
                    }
                    None => {
                        // PER_CHUNK mode - no intermediate checks, maximum performance
                        for idx in chunk_start..chunk_end {
                            let mirrored_idx = chunk_start + chunk_end - 1 - idx;
                            let expected_pattern = (mirrored_idx as u64).wrapping_add(thread_pattern_base).wrapping_mul(0x0123456789ABCDEFu64);
                            let actual_value = *ptr.add(idx);

                            if actual_value != expected_pattern {
                                chunk_errors += 1;
                                if chunk_errors <= 10 {  // Limit error logging
                                    log::error!("{}: memory error at index {} - expected {:#x}, got {:#x}",
                                               test_name, idx, expected_pattern, actual_value);
                                }
                            }
                        }

                        // Check errors only once at end of chunk
                        if chunk_errors > 0 {
                            block_errors += chunk_errors;
                        }
                    }
                }

                // 3. Mirror back to restore original pattern
                idx1 = chunk_start;
                idx2 = chunk_end - 1;
                while idx1 < idx2 {
                    let val1 = *ptr.add(idx1);
                    let val2 = *ptr.add(idx2);
                    *ptr.add(idx2) = val1;
                    *ptr.add(idx1) = val2;
                    idx1 += 1;
                    idx2 -= 1;
                }

                std::sync::atomic::fence(Ordering::SeqCst);

                // Check for shutdown
                if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
                    total_error_count += block_errors;
                    total_bytes_processed += (chunk_end - chunk_start) * std::mem::size_of::<u64>() * 3;
                    let elapsed = start.elapsed().as_millis();
                    return TestStats {
                        name: test_name,
                        action: TestAction::WriteWaitVerify,
                        bytes_processed: total_bytes_processed,
                        elapsed_ms: elapsed,
                        thread_id,
                        error_count: total_error_count,
                        total_operations,
                        cycles_completed: cycle,
                        cycles_planned: timing.cycles,
                        stopped_by_time_limit: false,
                    };
                }
            }

            total_error_count += block_errors;
            total_bytes_processed += test_block.test_size * 5; // mirror (R+W) + verify (R) + restore (R+W) = 3R + 2W
            total_operations += len as u64;

            // Handle errors based on mode
            if block_errors > 0 {
                match error_mode {
                    ErrorMode::Panic => panic!("{}: {} memory errors detected", test_name, block_errors),
                    ErrorMode::Halt => {
                        let elapsed = start.elapsed().as_millis();
                        return TestStats {
                            name: test_name,
                            action: TestAction::WriteWaitVerify,
                            bytes_processed: total_bytes_processed,
                            elapsed_ms: elapsed,
                            thread_id,
                            error_count: total_error_count,
                            total_operations,
                            cycles_completed: cycle,
                            cycles_planned: timing.cycles,
                            stopped_by_time_limit: false,
                        };
                    }
                    ErrorMode::Log => { /* Continue */ }
                }
            }
        }

        // Update progress if provided
        if let Some(progress) = progress {
            let now = Instant::now();
            if now.duration_since(last_progress_update).as_millis() >= update_interval_ms {
                progress.cycles_completed.store(cycle, Ordering::Relaxed);
                progress.bytes_processed.store(total_bytes_processed as u64, Ordering::Relaxed);
                progress.errors_found.store(total_error_count, Ordering::Relaxed);
                progress.last_update_ms.store(now.duration_since(start).as_millis() as u64, Ordering::Relaxed);
                last_progress_update = now;
            }
        }

        // Check timing - shared across ALL blocks
        let elapsed_secs = test_start.elapsed().as_secs() as u32;
        if !timing.should_continue(cycle, elapsed_secs) {
            break;
        }

        // Check for shutdown
        if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
            break;
        }
    }

    let elapsed = start.elapsed().as_millis();

    log::info!("[Thread {}] {} completed: {} cycles, {} errors, {:.2} MB processed in {} ms",
              thread_id, test_name, cycle, total_error_count,
              total_bytes_processed as f64 / MB_F64, elapsed);

    TestStats {
        name: test_name,
        action: TestAction::WriteWaitVerify,
        bytes_processed: total_bytes_processed,
        elapsed_ms: elapsed,
        thread_id,
        error_count: total_error_count,
        total_operations,
        cycles_completed: cycle,
        cycles_planned: timing.cycles,
        stopped_by_time_limit: timing.cycles.map_or(true, |limit| cycle < limit),
    }
}

/// MirrorMove128 Stream1 - MultiBlock version (SSE2 optimized, single stream)
/// MirrorMove128 Stream1 - SSE2 optimized - MultiBlock pattern
/// Runtime-checked wrapper for SSE2 availability
unsafe fn mirror_move_128_stream1_multi(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> TestStats {
    let test_name = "Mem-Mirror128";

    // Runtime CPU feature check
    if !is_x86_feature_detected!("sse4.1") {
        log::warn!("{}: SSE4.1 not supported, test skipped", test_name);
        return TestStats {
            name: test_name,
            action: TestAction::WriteWaitVerify,
            bytes_processed: 0,
            elapsed_ms: 0,
            thread_id,
            error_count: 0,
            total_operations: 0,
            cycles_completed: 0,
            cycles_planned: timing.cycles,
            stopped_by_time_limit: false,
        };
    }

    // Call the SSE4.1 optimized implementation
    mirror_move_128_stream1_impl(blocks, thread_id, error_mode, timing, config, progress)
}

/// MirrorMove128 Stream1 - SSE4.1 optimized implementation
/// This function is annotated with #[target_feature] to enable full compiler optimization
/// Upgraded from SSE2 to SSE4.1 for _mm_mullo_epi32 vector multiply support
#[inline]
#[target_feature(enable = "sse4.1")]
unsafe fn mirror_move_128_stream1_impl(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> TestStats {
    let test_name = "Mem-Mirror128";

    if blocks.is_empty() {
        return TestStats {
            name: test_name,
            action: TestAction::WriteWaitVerify,
            bytes_processed: 0,
            elapsed_ms: 0,
            thread_id,
            error_count: 0,
            total_operations: 0,
            cycles_completed: 0,
            cycles_planned: timing.cycles,
            stopped_by_time_limit: false,
        };
    }

    // Calculate window size and prepare blocks with window limits
    let total_allocated: usize = blocks.iter().map(|b| b.buffer.size()).sum();
    let window_size = config.calculate_window_size(test_name, total_allocated);
    let test_blocks = prepare_blocks_for_window(blocks, window_size, test_name);

    if test_blocks.is_empty() {
        log::warn!("{}: No blocks prepared for testing (window too small?)", test_name);
        return TestStats {
            name: test_name,
            action: TestAction::WriteWaitVerify,
            bytes_processed: 0,
            elapsed_ms: 0,
            thread_id,
            error_count: 0,
            total_operations: 0,
            cycles_completed: 0,
            cycles_planned: timing.cycles,
            stopped_by_time_limit: false,
        };
    }

    // Pre-compute pattern base outside all loops
    let thread_pattern_base = (thread_id as i32) << 16;

    // VECTOR OPTIMIZATION: Pre-compute pattern components for initialization
    let multipliers = i32x4::from_array([4, 3, 2, 1]);
    let base_broadcast = i32x4::splat(thread_pattern_base);
    let pattern_base = base_broadcast * multipliers ;  // base × [1,2,3,4]

    // Track totals across all blocks
    let mut total_bytes_processed = 0usize;
    let mut total_error_count = 0u64;
    let mut total_operations = 0u64;

    // Single shared timer for ALL blocks
    let test_start = Instant::now();
    let start = Instant::now();
    let mut cycle = 0u32;

    // Progress reporting setup
    let update_interval_ms = 1000;
    let mut last_progress_update = Instant::now();

    // Initialize all test blocks with thread-specific patterns (only the test_size portion)
    for test_block in test_blocks.iter() {
        let base = test_block.block.buffer.as_mut_ptr() as *mut i32x4;
        let len = test_block.test_size / std::mem::size_of::<i32x4>();

        for i in 0..len {
            // OPTIMIZED: 1 broadcast + 1 mullo + 1 add (replaces 4 scalar muls + 4 adds)
            let idx_broadcast = i32x4::splat(i as i32);
            let idx_scaled = idx_broadcast * multipliers ;  // i × [1,2,3,4]
            let pattern = idx_scaled + pattern_base ;         // (i×M) + (base×M)
            *base.add(i) = pattern;
        }
    }

    // Main test loop - interleaves across blocks
    loop {
        cycle += 1;

        // Test each block in sequence for this cycle (interleaved: 1,2,3,1,2,3...)
        for test_block in test_blocks.iter() {
            let base = test_block.block.buffer.as_mut_ptr() as *mut i32x4;
            let len = test_block.test_size / std::mem::size_of::<i32x4>();

            // Calculate chunk size for this block's test_size
            let ideal_chunk_size = calculate_ideal_chunk_size(config, test_name, test_block.test_size);
            let chunk_size_bytes = get_safe_chunk_size(ideal_chunk_size, test_block.test_size);
            let chunk_size_elements = chunk_size_bytes / std::mem::size_of::<i32x4>();

            let mut block_errors = 0u64;

            // Process block in chunks
            for chunk_start in (0..len).step_by(chunk_size_elements) {
                let chunk_end = (chunk_start + chunk_size_elements).min(len);
                let chunk_len = chunk_end - chunk_start;

                // 1. Mirror operation
                let mut idx1 = chunk_start;
                let mut idx2 = chunk_start + chunk_len - 1;
                while idx1 < idx2 {
                    let val1 = *base.add(idx1);
                    let val2 = *base.add(idx2);
                    *base.add(idx2) = val1;  // Cache-backed store
                    *base.add(idx1) = val2;  // Cache-backed store
                    idx1 += 1;
                    idx2 -= 1;
                }

                // 2. Verify with configurable error checking frequency
                let mut error_accumulator = i32x4::splat(0);
                let mut i = chunk_start;
                let chunk_sum = chunk_start + chunk_end - 1;
                let mut element_count = 0usize;
                let mut cycle_errors = 0u64;

                // Pre-compute check mask for hot loop optimization
                match config.error_check_interval.get_check_mask() {
                    Some(check_mask) => {
                        while i < chunk_end {
                            let mirrored_idx = chunk_sum - i;

                            // OPTIMIZED: 1 broadcast + 1 mullo + 1 add (replaces 4 scalar muls + 4 adds)
                            let idx_broadcast = i32x4::splat(mirrored_idx as i32);
                            let idx_scaled = idx_broadcast * multipliers;  // idx × [1,2,3,4]
                            let expected = idx_scaled + pattern_base;         // (idx×M) + (base×M)

                            let actual = *base.add(i);
                            let diff = expected ^ actual;
                            error_accumulator = error_accumulator | diff;

                            element_count += 1;
                            i += 1;

                            // Check errors at configured intervals (zero-branch hot loop optimization)
                            if (element_count as u32 & check_mask) == 0 {
                                if error_accumulator.simd_ne(i32x4::splat(0)).any() {
                                    cycle_errors += 1;
                                    log::error!("{}: memory error detected in cycle {} chunk {} element {} (thread {})",
                                               test_name, cycle, chunk_start, element_count, thread_id);
                                    error_accumulator = i32x4::splat(0);
                                }
                            }
                        }
                    }
                    None => {
                        // PER_CHUNK mode - no intermediate checks, maximum performance
                        while i < chunk_end {
                            let mirrored_idx = chunk_sum - i;

                            // OPTIMIZED: 1 broadcast + 1 mullo + 1 add (replaces 4 scalar muls + 4 adds)
                            let idx_broadcast = i32x4::splat(mirrored_idx as i32);
                            let idx_scaled = idx_broadcast * multipliers;  // idx × [1,2,3,4]
                            let expected = idx_scaled + pattern_base;         // (idx×M) + (base×M)

                            let actual = *base.add(i);
                            let diff = expected ^ actual;
                            error_accumulator = error_accumulator | diff;
                            i += 1;
                        }
                    }
                }

                // Final error check (always performed regardless of mode)
                if error_accumulator.simd_ne(i32x4::splat(0)).any() {
                    cycle_errors += 1;
                    log::error!("{}: memory error detected in cycle {} chunk {} (thread {})",
                               test_name, cycle, chunk_start, thread_id);
                }

                block_errors += cycle_errors;

                // 3. Mirror back to restore original pattern
                idx1 = chunk_start;
                idx2 = chunk_start + chunk_len - 1;
                while idx1 < idx2 {
                    let val1 = *base.add(idx1);
                    let val2 = *base.add(idx2);
                    *base.add(idx2) = val1;  // Cache-backed store
                    *base.add(idx1) = val2;  // Cache-backed store
                    idx1 += 1;
                    idx2 -= 1;
                }

                // Check for shutdown
                if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
                    total_error_count += block_errors;
                    total_bytes_processed += (chunk_end - chunk_start) * std::mem::size_of::<u64x2>() * 2;
                    let elapsed = start.elapsed().as_millis();
                    return TestStats {
                        name: test_name,
                        action: TestAction::WriteWaitVerify,
                        bytes_processed: total_bytes_processed,
                        elapsed_ms: elapsed,
                        thread_id,
                        error_count: total_error_count,
                        total_operations,
                        cycles_completed: cycle,
                        cycles_planned: timing.cycles,
                        stopped_by_time_limit: false,
                    };
                }
            }

            total_error_count += block_errors;
            total_bytes_processed += test_block.test_size * 5; // mirror (R+W) + verify (R) + mirror back (R+W) = 3R + 2W
            total_operations += len as u64 * 2;

            // Handle errors based on mode
            if block_errors > 0 {
                match error_mode {
                    ErrorMode::Panic => panic!("{}: {} memory errors detected", test_name, block_errors),
                    ErrorMode::Halt => {
                        let elapsed = start.elapsed().as_millis();
                        return TestStats {
                            name: test_name,
                            action: TestAction::WriteWaitVerify,
                            bytes_processed: total_bytes_processed,
                            elapsed_ms: elapsed,
                            thread_id,
                            error_count: total_error_count,
                            total_operations,
                            cycles_completed: cycle,
                            cycles_planned: timing.cycles,
                            stopped_by_time_limit: false,
                        };
                    }
                    ErrorMode::Log => { /* Continue */ }
                }
            }

            // Check for shutdown between blocks
            if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
                let elapsed = start.elapsed().as_millis();
                return TestStats {
                    name: test_name,
                    action: TestAction::WriteWaitVerify,
                    bytes_processed: total_bytes_processed,
                    elapsed_ms: elapsed,
                    thread_id,
                    error_count: total_error_count,
                    total_operations,
                    cycles_completed: cycle,
                    cycles_planned: timing.cycles,
                    stopped_by_time_limit: false,
                };
            }
        }

        // Progress reporting (per cycle, not per block)
        if let Some(progress) = progress {
            let now = Instant::now();
            if now.duration_since(last_progress_update).as_millis() >= update_interval_ms {
                progress.cycles_completed.store(cycle, Ordering::Relaxed);
                progress.bytes_processed.store(total_bytes_processed as u64, Ordering::Relaxed);
                progress.errors_found.store(total_error_count, Ordering::Relaxed);
                progress.last_update_ms.store(now.duration_since(start).as_millis() as u64, Ordering::Relaxed);
                last_progress_update = now;
            }
        }

        // Check timing - shared across ALL blocks
        let elapsed_secs = test_start.elapsed().as_secs() as u32;
        if !timing.should_continue(cycle, elapsed_secs) {
            break;
        }
    }

    TestStats {
        name: test_name,
        action: TestAction::WriteWaitVerify,
        bytes_processed: total_bytes_processed,
        elapsed_ms: start.elapsed().as_millis(),
        thread_id,
        error_count: total_error_count,
        total_operations,
        cycles_completed: cycle,
        cycles_planned: timing.cycles,
        stopped_by_time_limit: timing.cycles.map_or(true, |limit| cycle < limit),
    }
}

/// MirrorMove128 StreamN - SSE2 optimized - MultiBlock pattern
/// Runtime-checked wrapper for SSE2 availability
unsafe fn mirror_move_128_stream_n_multi(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
    streams: usize,
    test_name: &'static str,
) -> TestStats {
    // Runtime CPU feature check
    if !is_x86_feature_detected!("sse2") {
        log::warn!("{}: SSE2 not supported, test skipped", test_name);
        return TestStats {
            name: test_name,
            action: TestAction::WriteWaitVerify,
            bytes_processed: 0,
            elapsed_ms: 0,
            thread_id,
            error_count: 0,
            total_operations: 0,
            cycles_completed: 0,
            cycles_planned: timing.cycles,
            stopped_by_time_limit: false,
        };
    }

    // Call the SSE4.1 optimized implementation
    mirror_move_128_stream_n_impl(blocks, thread_id, error_mode, timing, config, progress, streams, test_name)
}

/// MirrorMove128 StreamN - SSE4.1 optimized implementation
/// This function is annotated with #[target_feature] to enable full compiler optimization
/// Upgraded from SSE2 to SSE4.1 for _mm_mullo_epi32 vector multiply support
#[inline]
#[target_feature(enable = "sse4.1")]
unsafe fn mirror_move_128_stream_n_impl(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
    streams: usize,
    test_name: &'static str,
) -> TestStats {
    if blocks.is_empty() {
        return TestStats {
            name: test_name,
            action: TestAction::WriteWaitVerify,
            bytes_processed: 0,
            elapsed_ms: 0,
            thread_id,
            error_count: 0,
            total_operations: 0,
            cycles_completed: 0,
            cycles_planned: timing.cycles,
            stopped_by_time_limit: false,
        };
    }

    // Calculate window size and prepare blocks with window limits
    let total_allocated: usize = blocks.iter().map(|b| b.buffer.size()).sum();
    let window_size = config.calculate_window_size(test_name, total_allocated);
    let test_blocks = prepare_blocks_for_window(blocks, window_size, test_name);

    if test_blocks.is_empty() {
        log::warn!("{}: No blocks prepared for testing (window too small?)", test_name);
        return TestStats {
            name: test_name,
            action: TestAction::WriteWaitVerify,
            bytes_processed: 0,
            elapsed_ms: 0,
            thread_id,
            error_count: 0,
            total_operations: 0,
            cycles_completed: 0,
            cycles_planned: timing.cycles,
            stopped_by_time_limit: false,
        };
    }

    // Pre-compute pattern base and stream shift
    let thread_pattern_base = (thread_id as i32) << 16;
    let stream_shift = streams.trailing_zeros();

    // VECTOR OPTIMIZATION: Pre-compute pattern components for initialization
    let multipliers = i32x4::from_array([4, 3, 2, 1]);
    let base_broadcast = i32x4::splat(thread_pattern_base);
    let pattern_base = base_broadcast * multipliers ;  // base × [1,2,3,4]

    // Track totals across all blocks
    let mut total_bytes_processed = 0usize;
    let mut total_error_count = 0u64;
    let mut total_operations = 0u64;

    // Single shared timer for ALL blocks
    let test_start = Instant::now();
    let start = Instant::now();
    let mut cycle = 0u32;

    // Progress reporting setup
    let update_interval_ms = 1000;
    let mut last_progress_update = Instant::now();

    // Initialize all test blocks with thread-specific patterns
    for test_block in test_blocks.iter() {
        let base = test_block.block.buffer.as_mut_ptr() as *mut i32x4;
        let len = test_block.test_size / std::mem::size_of::<i32x4>();

        for i in 0..len {
            // OPTIMIZED: 1 broadcast + 1 mullo + 1 add (replaces 4 scalar muls + 4 adds)
            let idx_broadcast = i32x4::splat(i as i32);
            let idx_scaled = idx_broadcast * multipliers ;  // i × [1,2,3,4]
            let pattern = idx_scaled + pattern_base ;         // (i×M) + (base×M)
            *base.add(i) = pattern;
        }
    }

    // Validate chunk divisibility for ALL blocks ONCE before starting
    for test_block in test_blocks.iter() {
        let ideal_chunk_size = calculate_ideal_chunk_size(config, test_name, test_block.test_size);
        let chunk_size_bytes = get_safe_chunk_size(ideal_chunk_size, test_block.test_size);
        let chunk_size_elements = chunk_size_bytes / std::mem::size_of::<i32x4>();

        // Validate chunk is divisible by stream count
        if streams > 1 && chunk_size_elements % streams != 0 {
            panic!(
                "{}: Chunk size {} elements not evenly divisible by {} streams. \
                 This should never happen if chunk and stream are both power-of-2.",
                test_name, chunk_size_elements, streams
            );
        }
    }

    // Main test loop - interleaves across blocks
    loop {
        cycle += 1;

        // Test each block in sequence for this cycle
        for test_block in test_blocks.iter() {
            let base = test_block.block.buffer.as_mut_ptr() as *mut i32x4;
            let len = test_block.test_size / std::mem::size_of::<i32x4>();

            // Calculate chunk size for this block's test_size
            let ideal_chunk_size = calculate_ideal_chunk_size(config, test_name, test_block.test_size);
            let chunk_size_bytes = get_safe_chunk_size(ideal_chunk_size, test_block.test_size);
            let chunk_size_elements = chunk_size_bytes / std::mem::size_of::<i32x4>();

            let mut block_errors = 0u64;

            // Process block in chunks
            for chunk_start in (0..len).step_by(chunk_size_elements) {
                let chunk_end = (chunk_start + chunk_size_elements).min(len);
                let chunk_len = chunk_end - chunk_start;

                // Pre-compute stream boundaries for this chunk
                let elements_per_stream = chunk_len >> stream_shift;

                // 1. Mirror operation - N-way streams (mirror within each stream)
                for stream_id in 0..streams {
                    let stream_start = chunk_start + (stream_id * elements_per_stream);
                    let stream_end = stream_start + elements_per_stream;

                    let mut idx1 = stream_start;
                    let mut idx2 = stream_end - 1;
                    while idx1 < idx2 {
                        let val1 = *base.add(idx1);
                        let val2 = *base.add(idx2);
                        *base.add(idx2) = val1;  // Cache-backed store
                        *base.add(idx1) = val2;  // Cache-backed store
                        idx1 += 1;
                        idx2 -= 1;
                    }
                }

                // 2. Verify with configurable error checking frequency - multi-stream
                let mut error_accumulator = i32x4::splat(0);
                let mut element_count = 0usize;
                let mut cycle_errors = 0u64;

                match config.error_check_interval.get_check_mask() {
                    Some(check_mask) => {
                        for stream_id in 0..streams {
                            let stream_start = chunk_start + (stream_id * elements_per_stream);
                            let stream_end = stream_start + elements_per_stream;
                            let stream_sum = stream_start + stream_end - 1;

                            let mut i = stream_start;
                            while i < stream_end {
                                let mirrored_idx = stream_sum - i;

                                // OPTIMIZED: 1 broadcast + 1 mullo + 1 add (replaces 4 scalar muls + 4 adds)
                                let idx_broadcast = i32x4::splat(mirrored_idx as i32);
                                let idx_scaled = idx_broadcast * multipliers;  // idx × [1,2,3,4]
                                let expected = idx_scaled + pattern_base;         // (idx×M) + (base×M)

                                let actual = *base.add(i);
                                let diff = expected ^ actual;
                                error_accumulator = error_accumulator | diff;

                                element_count += 1;
                                i += 1;

                                if (element_count as u32 & check_mask) == 0 {
                                    if error_accumulator.simd_ne(i32x4::splat(0)).any() {
                                        cycle_errors += 1;
                                        log::error!("{}: memory error detected in cycle {} chunk {} stream {} element {} (thread {})",
                                                   test_name, cycle, chunk_start, stream_id, element_count, thread_id);
                                        error_accumulator = i32x4::splat(0);
                                    }
                                }
                            }
                        }
                    }
                    None => {
                        // PER_CHUNK mode - no intermediate checks, maximum performance
                        for stream_id in 0..streams {
                            let stream_start = chunk_start + (stream_id * elements_per_stream);
                            let stream_end = stream_start + elements_per_stream;
                            let stream_sum = stream_start + stream_end - 1;

                            let mut i = stream_start;
                            while i < stream_end {
                                let mirrored_idx = stream_sum - i;

                                // OPTIMIZED: 1 broadcast + 1 mullo + 1 add (replaces 4 scalar muls + 4 adds)
                                let idx_broadcast = i32x4::splat(mirrored_idx as i32);
                                let idx_scaled = idx_broadcast * multipliers;  // idx × [1,2,3,4]
                                let expected = idx_scaled + pattern_base;         // (idx×M) + (base×M)

                                let actual = *base.add(i);
                                let diff = expected ^ actual;
                                error_accumulator = error_accumulator | diff;
                                i += 1;
                            }
                        }
                    }
                }

                // Final error check (always performed regardless of mode)
                if error_accumulator.simd_ne(i32x4::splat(0)).any() {
                    cycle_errors += 1;
                    log::error!("{}: memory error detected in cycle {} chunk {} (thread {})",
                               test_name, cycle, chunk_start, thread_id);
                }

                block_errors += cycle_errors;

                // 3. Mirror back to restore original pattern (N-way streams)
                for stream_id in 0..streams {
                    let stream_start = chunk_start + (stream_id * elements_per_stream);
                    let stream_end = stream_start + elements_per_stream;

                    let mut idx1 = stream_start;
                    let mut idx2 = stream_end - 1;
                    while idx1 < idx2 {
                        let val1 = *base.add(idx1);
                        let val2 = *base.add(idx2);
                        *base.add(idx2) = val1;  // Cache-backed store
                        *base.add(idx1) = val2;  // Cache-backed store
                        idx1 += 1;
                        idx2 -= 1;
                    }
                }

                // Check for shutdown
                if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
                    total_error_count += block_errors;
                    total_bytes_processed += (chunk_end - chunk_start) * std::mem::size_of::<i32x4>() * 2;
                    let elapsed = start.elapsed().as_millis();
                    return TestStats {
                        name: test_name,
                        action: TestAction::WriteWaitVerify,
                        bytes_processed: total_bytes_processed,
                        elapsed_ms: elapsed,
                        thread_id,
                        error_count: total_error_count,
                        total_operations,
                        cycles_completed: cycle,
                        cycles_planned: timing.cycles,
                        stopped_by_time_limit: false,
                    };
                }
            }

            total_error_count += block_errors;
            total_bytes_processed += test_block.test_size * 5; // mirror (R+W) + verify (R) + mirror back (R+W) = 3R + 2W
            total_operations += len as u64 * 2;

            // Handle errors based on mode
            if block_errors > 0 {
                match error_mode {
                    ErrorMode::Panic => panic!("{}: {} memory errors detected", test_name, block_errors),
                    ErrorMode::Halt => {
                        let elapsed = start.elapsed().as_millis();
                        return TestStats {
                            name: test_name,
                            action: TestAction::WriteWaitVerify,
                            bytes_processed: total_bytes_processed,
                            elapsed_ms: elapsed,
                            thread_id,
                            error_count: total_error_count,
                            total_operations,
                            cycles_completed: cycle,
                            cycles_planned: timing.cycles,
                            stopped_by_time_limit: false,
                        };
                    }
                    ErrorMode::Log => { /* Continue */ }
                }
            }

            // Check for shutdown between blocks
            if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
                let elapsed = start.elapsed().as_millis();
                return TestStats {
                    name: test_name,
                    action: TestAction::WriteWaitVerify,
                    bytes_processed: total_bytes_processed,
                    elapsed_ms: elapsed,
                    thread_id,
                    error_count: total_error_count,
                    total_operations,
                    cycles_completed: cycle,
                    cycles_planned: timing.cycles,
                    stopped_by_time_limit: false,
                };
            }
        }

        // Progress reporting (per cycle, not per block)
        if let Some(progress) = progress {
            let now = Instant::now();
            if now.duration_since(last_progress_update).as_millis() >= update_interval_ms {
                progress.cycles_completed.store(cycle, Ordering::Relaxed);
                progress.bytes_processed.store(total_bytes_processed as u64, Ordering::Relaxed);
                progress.errors_found.store(total_error_count, Ordering::Relaxed);
                progress.last_update_ms.store(now.duration_since(start).as_millis() as u64, Ordering::Relaxed);
                last_progress_update = now;
            }
        }

        // Check timing - shared across ALL blocks
        let elapsed_secs = test_start.elapsed().as_secs() as u32;
        if !timing.should_continue(cycle, elapsed_secs) {
            break;
        }
    }

    TestStats {
        name: test_name,
        action: TestAction::WriteWaitVerify,
        bytes_processed: total_bytes_processed,
        elapsed_ms: start.elapsed().as_millis(),
        thread_id,
        error_count: total_error_count,
        total_operations,
        cycles_completed: cycle,
        cycles_planned: timing.cycles,
        stopped_by_time_limit: timing.cycles.map_or(true, |limit| cycle < limit),
    }
}

/// MirrorMove128 - MultiBlock dispatcher (SSE2 optimized, routes to stream variants)
pub unsafe fn mirror_move_128_multi(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> TestStats {
    let test_name = "Mem-Mirror128";

    // Dispatch based on stream configuration
    let streams = config.streams.max(1) as usize;

    if streams == 1 {
        mirror_move_128_stream1_multi(blocks, thread_id, error_mode, timing, config, progress)
    } else {
        mirror_move_128_stream_n_multi(blocks, thread_id, error_mode, timing, config, progress, streams, test_name)
    }
}

// ============================================================================
// MirrorMove256 (AVX2) - MultiBlock Implementations
// ============================================================================

/// MirrorMove256 Stream1 - Single stream (linear sequential) - MultiBlock pattern
/// Tests all blocks with shared timer and interleaved execution
///
/// This is the runtime-checked wrapper that calls the optimized implementation
unsafe fn mirror_move_256_stream1_multi(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> TestStats {
    let test_name = "Mem-Mirror256";

    // Runtime CPU feature check
    if !is_x86_feature_detected!("avx2") {
        log::warn!("{}: AVX2 not available, returning zero stats", test_name);
        return TestStats {
            name: test_name,
            action: TestAction::ReadWrite,
            bytes_processed: 0,
            elapsed_ms: 0,
            thread_id,
            error_count: 0,
            total_operations: 0,
            cycles_completed: 0,
            cycles_planned: None,
            stopped_by_time_limit: false,
        };
    }

    // Call the optimized AVX2 implementation
    mirror_move_256_stream1_impl(blocks, thread_id, error_mode, timing, config, progress)
}

/// MirrorMove256 Stream1 - AVX2-optimized implementation
/// CRITICAL: #[target_feature] enables full compiler optimization of SIMD code
#[inline]
#[target_feature(enable = "avx2")]
unsafe fn mirror_move_256_stream1_impl(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> TestStats {

    let test_name = "Mem-Mirror256";
    let test_start = Instant::now();

    // Calculate window and prepare blocks
    let total_allocated: usize = blocks.iter().map(|b| b.buffer.size()).sum();
    let window_size = config.calculate_window_size(test_name, total_allocated);
    let test_blocks = prepare_blocks_for_window(blocks, window_size, test_name);

    if test_blocks.is_empty() {
        log::warn!("{}: No blocks to test", test_name);
        return TestStats {
            name: test_name,
            action: TestAction::ReadWrite,
            bytes_processed: 0,
            elapsed_ms: 0,
            thread_id,
            error_count: 0,
            total_operations: 0,
            cycles_completed: 0,
            cycles_planned: None,
            stopped_by_time_limit: false,
        };
    }

    // Pre-compute pattern base outside all loops
    let thread_pattern_base = (thread_id as i32) << 16;

    // VECTOR OPTIMIZATION: Pre-compute pattern components for initialization
    let multipliers = i32x8::from_array([8, 7, 6, 5, 4, 3, 2, 1]);
    let base_broadcast = i32x8::splat(thread_pattern_base);
    let pattern_base = base_broadcast * multipliers ;  // base × [1,2,3,4,5,6,7,8]

    // Initialize memory with AVX2 patterns for each block
    for test_block in test_blocks.iter() {
        let base = test_block.block.buffer.as_mut_ptr() as *mut i32x8;
        let len = test_block.test_size / std::mem::size_of::<i32x8>();

        for i in 0..len {
            // OPTIMIZED: 1 broadcast + 1 mullo + 1 add (replaces 8 scalar muls + 8 adds)
            let idx_broadcast = i32x8::splat(i as i32);
            let idx_scaled = idx_broadcast * multipliers ;  // i × [1,2,3,4,5,6,7,8]
            let pattern = idx_scaled + pattern_base ;         // (i×M) + (base×M)
            *base.add(i) = pattern;
        }
        // NOTE: _mm_sfence() removed - not needed for temporal stores (cache coherency handles ordering)
        // TODO: If using TMR-MD Uncached/WC memory OR non-temporal stores, may need to restore fence
    }

    // Validate chunk divisibility for ALL blocks ONCE before starting (not in hot loop)
    // Different block sizes may have different chunk sizes, so validate each
    for test_block in test_blocks.iter() {
        let ideal_chunk_size = calculate_ideal_chunk_size(config, test_name, test_block.test_size);
        let chunk_size_bytes = get_safe_chunk_size(ideal_chunk_size, test_block.test_size);
        let chunk_size_elements = chunk_size_bytes / std::mem::size_of::<i32x8>();

        // Stream1 has streams=1, so chunk doesn't need to be divisible by streams
        // But we validate anyway for consistency
        validate_chunk_divisibility(test_name, 1, chunk_size_elements);
    }

    let mut cycle = 0u32;
    let mut total_bytes_processed = 0u64;
    let mut total_error_count = 0u64;
    let mut total_operations = 0u64;

    // Progress tracking
    let update_interval_ms = 250u128; // 4 updates/sec
    let mut last_progress_update = Instant::now();

    // Main cycle loop - shared timer across all blocks
    loop {
        cycle += 1;

        // Test all blocks in interleaved manner (1,2,3,1,2,3...)
        for test_block in test_blocks.iter() {
            let base = test_block.block.buffer.as_mut_ptr() as *mut i32x8;
            let len = test_block.test_size / std::mem::size_of::<i32x8>();

            // Calculate chunk size for this block's test_size
            let ideal_chunk_size = calculate_ideal_chunk_size(config, test_name, test_block.test_size);
            let chunk_size_bytes = get_safe_chunk_size(ideal_chunk_size, test_block.test_size);
            let chunk_size_elements = chunk_size_bytes / std::mem::size_of::<i32x8>();

            let mut cycle_errors = 0u64;

            for chunk_start in (0..len).step_by(chunk_size_elements) {
                let chunk_end = (chunk_start + chunk_size_elements).min(len);
                let chunk_len = chunk_end - chunk_start;

                // 1. Mirror operation - single stream with two-pointer optimization
                let mut idx1 = chunk_start;
                let mut idx2 = chunk_start + chunk_len - 1;
                while idx1 < idx2 {
                    let val1 = *base.add(idx1);
                    let val2 = *base.add(idx2);
                    *base.add(idx2) = val1;  // Cache-backed store
                    *base.add(idx1) = val2;  // Cache-backed store
                    idx1 += 1;
                    idx2 -= 1;
                }

                // 2. Verify with configurable error checking frequency
                let mut error_accumulator = i32x8::splat(0);
                let mut i = chunk_start;
                let chunk_sum = chunk_start + chunk_end - 1;
                let mut element_count = 0usize;

                // VECTOR OPTIMIZATION: Pre-compute pattern components ONCE
                // Formula: (idx + base) × M = (idx × M) + (base × M)
                let multipliers = i32x8::from_array([8, 7, 6, 5, 4, 3, 2, 1]);
                let base_broadcast = i32x8::splat(thread_pattern_base);
                let pattern_base = base_broadcast * multipliers ;  // base × [1,2,3,4,5,6,7,8]

                // Pre-compute check mask for hot loop optimization
                match config.error_check_interval.get_check_mask() {
                    Some(check_mask) => {
                        while i < chunk_end {
                            let mirrored_idx = chunk_sum - i;

                            // OPTIMIZED: 1 broadcast + 1 mullo + 1 add (replaces 8 scalar muls + 8 adds)
                            let idx_broadcast = i32x8::splat(mirrored_idx as i32);
                            let idx_scaled = idx_broadcast * multipliers ;  // idx × [1,2,3,4,5,6,7,8]
                            let expected = idx_scaled + pattern_base ;         // (idx×M) + (base×M)

                            let actual = *base.add(i);
                            let diff = expected ^ actual ;
                            error_accumulator = error_accumulator | diff ;

                            element_count += 1;
                            i += 1;

                            // Check errors at configured intervals (zero-branch hot loop optimization)
                            if (element_count as u32 & check_mask) == 0 {
                                if error_accumulator.simd_ne(i32x8::splat(0)).any() {
                                    cycle_errors += 1;
                                    log::error!("{}: memory error detected in cycle {} chunk {} element {} (thread {})",
                                               test_name, cycle, chunk_start, element_count, thread_id);
                                    error_accumulator = i32x8::splat(0);
                                }
                            }
                        }
                    }
                    None => {
                        // PER_CHUNK mode - no intermediate checks, maximum performance
                        while i < chunk_end {
                            let mirrored_idx = chunk_sum - i;

                            // OPTIMIZED: 1 broadcast + 1 mullo + 1 add (replaces 8 scalar muls + 8 adds)
                            let idx_broadcast = i32x8::splat(mirrored_idx as i32);
                            let idx_scaled = idx_broadcast * multipliers ;  // idx × [1,2,3,4,5,6,7,8]
                            let expected = idx_scaled + pattern_base ;         // (idx×M) + (base×M)

                            let actual = *base.add(i);
                            let diff = expected ^ actual ;
                            error_accumulator = error_accumulator | diff ;
                            i += 1;
                        }
                    }
                }

                // Final error check (always performed regardless of mode)
                if error_accumulator.simd_ne(i32x8::splat(0)).any() {
                    cycle_errors += 1;
                    log::error!("{}: memory error detected in cycle {} chunk {} (thread {})",
                               test_name, cycle, chunk_start, thread_id);
                }

                // 3. Mirror back to restore - same two-pointer optimization
                let mut idx1 = chunk_start;
                let mut idx2 = chunk_start + chunk_len - 1;
                while idx1 < idx2 {
                    let val1 = *base.add(idx1);
                    let val2 = *base.add(idx2);
                    *base.add(idx2) = val1;  // Cache-backed store
                    *base.add(idx1) = val2;  // Cache-backed store
                    idx1 += 1;
                    idx2 -= 1;
                }

                // Optimized error handling - check ONCE at end of chunk
                if cycle_errors > 0 {
                    match error_mode {
                        ErrorMode::Panic => {
                            panic!("{}: memory error detected in cycle {} (thread {})", test_name, cycle, thread_id);
                        }
                        ErrorMode::Halt => {
                            let elapsed = test_start.elapsed().as_millis();
                            total_error_count += cycle_errors;
                            return TestStats {
                                name: test_name,
                                action: TestAction::WriteWaitVerify,
                                bytes_processed: total_bytes_processed as usize,
                                elapsed_ms: elapsed,
                                thread_id,
                                error_count: total_error_count,
                                total_operations,
                                cycles_completed: cycle,
                                cycles_planned: timing.cycles,
                                stopped_by_time_limit: false,
                            };
                        }
                        ErrorMode::Log => {
                            // Already logged, continue
                        }
                    }
                }
            }

            total_error_count += cycle_errors;
            total_bytes_processed += (test_block.test_size * 5) as u64; // Mirror (R+W) + verify (R) + mirror back (R+W) = 3R + 2W
            total_operations += len as u64;
        }

        // Update progress tracker
        if let Some(progress) = progress {
            let now = Instant::now();
            if now.duration_since(last_progress_update).as_millis() >= update_interval_ms {
                progress.cycles_completed.store(cycle, Ordering::Relaxed);
                progress.bytes_processed.store(total_bytes_processed, Ordering::Relaxed);
                progress.errors_found.store(total_error_count, Ordering::Relaxed);
                progress.last_update_ms.store(
                    test_start.elapsed().as_millis() as u64,
                    Ordering::Relaxed
                );
                last_progress_update = now;
            }
        }

        // Check timing limits
        let elapsed_secs = test_start.elapsed().as_secs() as u32;
        if !timing.should_continue(cycle, elapsed_secs) {
            return TestStats {
                name: test_name,
                action: TestAction::WriteWaitVerify,
                bytes_processed: total_bytes_processed as usize,
                elapsed_ms: test_start.elapsed().as_millis(),
                thread_id,
                error_count: total_error_count,
                total_operations,
                cycles_completed: cycle,
                cycles_planned: timing.cycles,
                stopped_by_time_limit: timing.cycles.map_or(true, |limit| cycle < limit),
            };
        }
    }
}

/// MirrorMove256 StreamN - N-way segmented mirroring - MultiBlock pattern
/// Tests all blocks with shared timer and interleaved execution
///
/// This is the runtime-checked wrapper that calls the optimized implementation
unsafe fn mirror_move_256_stream_n_multi(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
    streams: usize,
    test_name: &'static str,
) -> TestStats {
    // Runtime CPU feature check
    if !is_x86_feature_detected!("avx2") {
        log::warn!("{}: AVX2 not available, returning zero stats", test_name);
        return TestStats {
            name: test_name,
            action: TestAction::ReadWrite,
            bytes_processed: 0,
            elapsed_ms: 0,
            thread_id,
            error_count: 0,
            total_operations: 0,
            cycles_completed: 0,
            cycles_planned: None,
            stopped_by_time_limit: false,
        };
    }

    // Call the optimized AVX2 implementation
    mirror_move_256_stream_n_impl(blocks, thread_id, error_mode, timing, config, progress, streams, test_name)
}

/// MirrorMove256 StreamN - AVX2-optimized implementation
/// CRITICAL: #[target_feature] enables full compiler optimization of SIMD code
#[inline]
#[target_feature(enable = "avx2")]
unsafe fn mirror_move_256_stream_n_impl(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
    streams: usize,
    test_name: &'static str,
) -> TestStats {

    let test_start = Instant::now();
    let stream_shift = streams.trailing_zeros();

    // Calculate window and prepare blocks
    let total_allocated: usize = blocks.iter().map(|b| b.buffer.size()).sum();
    let window_size = config.calculate_window_size(test_name, total_allocated);
    let test_blocks = prepare_blocks_for_window(blocks, window_size, test_name);

    if test_blocks.is_empty() {
        log::warn!("{}: No blocks to test", test_name);
        return TestStats {
            name: test_name,
            action: TestAction::ReadWrite,
            bytes_processed: 0,
            elapsed_ms: 0,
            thread_id,
            error_count: 0,
            total_operations: 0,
            cycles_completed: 0,
            cycles_planned: None,
            stopped_by_time_limit: false,
        };
    }

    // Pre-compute pattern base outside all loops
    let thread_pattern_base = (thread_id as i32) << 16;

    // VECTOR OPTIMIZATION: Pre-compute pattern components for initialization
    let multipliers = i32x8::from_array([8, 7, 6, 5, 4, 3, 2, 1]);
    let base_broadcast = i32x8::splat(thread_pattern_base);
    let pattern_base = base_broadcast * multipliers ;  // base × [1,2,3,4,5,6,7,8]

    // Initialize memory with AVX2 patterns for each block
    for test_block in test_blocks.iter() {
        let base = test_block.block.buffer.as_mut_ptr() as *mut i32x8;
        let len = test_block.test_size / std::mem::size_of::<i32x8>();

        for i in 0..len {
            // OPTIMIZED: 1 broadcast + 1 mullo + 1 add (replaces 8 scalar muls + 8 adds)
            let idx_broadcast = i32x8::splat(i as i32);
            let idx_scaled = idx_broadcast * multipliers ;  // i × [1,2,3,4,5,6,7,8]
            let pattern = idx_scaled + pattern_base ;         // (i×M) + (base×M)
            *base.add(i) = pattern;
        }
        // NOTE: _mm_sfence() removed - not needed for temporal stores (cache coherency handles ordering)
        // TODO: If using TMR-MD Uncached/WC memory OR non-temporal stores, may need to restore fence
    }

    // Validate chunk divisibility for ALL blocks ONCE before starting
    for test_block in test_blocks.iter() {
        let ideal_chunk_size = calculate_ideal_chunk_size(config, test_name, test_block.test_size);
        let chunk_size_bytes = get_safe_chunk_size(ideal_chunk_size, test_block.test_size);
        let chunk_size_elements = chunk_size_bytes / std::mem::size_of::<i32x8>();

        validate_chunk_divisibility(test_name, config.streams, chunk_size_elements);
    }

    let mut cycle = 0u32;
    let mut total_bytes_processed = 0u64;
    let mut total_error_count = 0u64;
    let mut total_operations = 0u64;

    // Progress tracking
    let update_interval_ms = 250u128;
    let mut last_progress_update = Instant::now();

    // Main cycle loop - shared timer across all blocks
    loop {
        cycle += 1;

        // Test all blocks in interleaved manner
        for test_block in test_blocks.iter() {
            let base = test_block.block.buffer.as_mut_ptr() as *mut i32x8;
            let len = test_block.test_size / std::mem::size_of::<i32x8>();

            // Calculate chunk size for this block's test_size
            let ideal_chunk_size = calculate_ideal_chunk_size(config, test_name, test_block.test_size);
            let chunk_size_bytes = get_safe_chunk_size(ideal_chunk_size, test_block.test_size);
            let chunk_size_elements = chunk_size_bytes / std::mem::size_of::<i32x8>();

            let mut cycle_errors = 0u64;

            for chunk_start in (0..len).step_by(chunk_size_elements) {
                let chunk_end = (chunk_start + chunk_size_elements).min(len);
                let chunk_len = chunk_end - chunk_start;

                // Pre-compute stream boundaries for this chunk
                let elements_per_stream = chunk_len >> stream_shift;

                // 1. Mirror operation - multi-stream
                for stream_id in 0..streams {
                    let stream_start = chunk_start + (stream_id * elements_per_stream);
                    let stream_end = stream_start + elements_per_stream;

                    let mut idx1 = stream_start;
                    let mut idx2 = stream_end - 1;
                    while idx1 < idx2 {
                        let val1 = *base.add(idx1);
                        let val2 = *base.add(idx2);
                        *base.add(idx2) = val1;  // Cache-backed store
                        *base.add(idx1) = val2;  // Cache-backed store
                        idx1 += 1;
                        idx2 -= 1;
                    }
                }

                // 2. Verify with configurable error checking frequency - multi-stream
                let mut error_accumulator = i32x8::splat(0);
                let mut element_count = 0usize;

                // VECTOR OPTIMIZATION: Pre-compute pattern components ONCE
                // Formula: (idx + base) × M = (idx × M) + (base × M)
                let multipliers = i32x8::from_array([8, 7, 6, 5, 4, 3, 2, 1]);
                let base_broadcast = i32x8::splat(thread_pattern_base);
                let pattern_base = base_broadcast * multipliers ;  // base × [1,2,3,4,5,6,7,8]

                match config.error_check_interval.get_check_mask() {
                    Some(check_mask) => {
                        for stream_id in 0..streams {
                            let stream_start = chunk_start + (stream_id * elements_per_stream);
                            let stream_end = stream_start + elements_per_stream;
                            let stream_sum = stream_start + stream_end - 1;

                            let mut i = stream_start;
                            while i < stream_end {
                                let mirrored_idx = stream_sum - i;

                                // OPTIMIZED: 1 broadcast + 1 mullo + 1 add (replaces 8 scalar muls + 8 adds)
                                let idx_broadcast = i32x8::splat(mirrored_idx as i32);
                                let idx_scaled = idx_broadcast * multipliers ;  // idx × [1,2,3,4,5,6,7,8]
                                let expected = idx_scaled + pattern_base ;         // (idx×M) + (base×M)

                                let actual = *base.add(i);
                                let diff = expected ^ actual ;
                                error_accumulator = error_accumulator | diff ;

                                element_count += 1;
                                i += 1;

                                if (element_count as u32 & check_mask) == 0 {
                                    if error_accumulator.simd_ne(i32x8::splat(0)).any() {
                                        cycle_errors += 1;
                                        log::error!("{}: memory error detected in cycle {} chunk {} stream {} element {} (thread {})",
                                                   test_name, cycle, chunk_start, stream_id, element_count, thread_id);
                                        error_accumulator = i32x8::splat(0);
                                    }
                                }
                            }
                        }
                    }
                    None => {
                        // PER_CHUNK mode
                        for stream_id in 0..streams {
                            let stream_start = chunk_start + (stream_id * elements_per_stream);
                            let stream_end = stream_start + elements_per_stream;
                            let stream_sum = stream_start + stream_end - 1;

                            let mut i = stream_start;
                            while i < stream_end {
                                let mirrored_idx = stream_sum - i;

                                // OPTIMIZED: 1 broadcast + 1 mullo + 1 add (replaces 8 scalar muls + 8 adds)
                                let idx_broadcast = i32x8::splat(mirrored_idx as i32);
                                let idx_scaled = idx_broadcast * multipliers ;  // idx × [1,2,3,4,5,6,7,8]
                                let expected = idx_scaled + pattern_base ;         // (idx×M) + (base×M)

                                let actual = *base.add(i);
                                let diff = expected ^ actual ;
                                error_accumulator = error_accumulator | diff ;
                                i += 1;
                            }
                        }
                    }
                }

                // Final error check
                if error_accumulator.simd_ne(i32x8::splat(0)).any() {
                    cycle_errors += 1;
                    log::error!("{}: memory error detected in cycle {} chunk {} (thread {})",
                               test_name, cycle, chunk_start, thread_id);
                }

                // 3. Mirror back to restore - multi-stream
                for stream_id in 0..streams {
                    let stream_start = chunk_start + (stream_id * elements_per_stream);
                    let stream_end = stream_start + elements_per_stream;

                    let mut idx1 = stream_start;
                    let mut idx2 = stream_end - 1;
                    while idx1 < idx2 {
                        let val1 = *base.add(idx1);
                        let val2 = *base.add(idx2);
                        *base.add(idx2) = val1;  // Cache-backed store
                        *base.add(idx1) = val2;  // Cache-backed store
                        idx1 += 1;
                        idx2 -= 1;
                    }
                }

                // Error handling
                if cycle_errors > 0 {
                    match error_mode {
                        ErrorMode::Panic => {
                            panic!("{}: memory error detected in cycle {} (thread {})", test_name, cycle, thread_id);
                        }
                        ErrorMode::Halt => {
                            let elapsed = test_start.elapsed().as_millis();
                            total_error_count += cycle_errors;
                            return TestStats {
                                name: test_name,
                                action: TestAction::WriteWaitVerify,
                                bytes_processed: total_bytes_processed as usize,
                                elapsed_ms: elapsed,
                                thread_id,
                                error_count: total_error_count,
                                total_operations,
                                cycles_completed: cycle,
                                cycles_planned: timing.cycles,
                                stopped_by_time_limit: false,
                            };
                        }
                        ErrorMode::Log => {
                            // Already logged, continue
                        }
                    }
                }
            }

            total_error_count += cycle_errors;
            total_bytes_processed += (test_block.test_size * 5) as u64; // Mirror (R+W) + verify (R) + mirror back (R+W) = 3R + 2W
            total_operations += len as u64;
        }

        // Update progress tracker
        if let Some(progress) = progress {
            let now = Instant::now();
            if now.duration_since(last_progress_update).as_millis() >= update_interval_ms {
                progress.cycles_completed.store(cycle, Ordering::Relaxed);
                progress.bytes_processed.store(total_bytes_processed, Ordering::Relaxed);
                progress.errors_found.store(total_error_count, Ordering::Relaxed);
                progress.last_update_ms.store(
                    test_start.elapsed().as_millis() as u64,
                    Ordering::Relaxed
                );
                last_progress_update = now;
            }
        }

        // Check timing limits
        let elapsed_secs = test_start.elapsed().as_secs() as u32;
        if !timing.should_continue(cycle, elapsed_secs) {
            return TestStats {
                name: test_name,
                action: TestAction::WriteWaitVerify,
                bytes_processed: total_bytes_processed as usize,
                elapsed_ms: test_start.elapsed().as_millis(),
                thread_id,
                error_count: total_error_count,
                total_operations,
                cycles_completed: cycle,
                cycles_planned: timing.cycles,
                stopped_by_time_limit: timing.cycles.map_or(true, |limit| cycle < limit),
            };
        }
    }
}

/// MirrorMove256 - PUBLIC MultiBlock dispatcher
/// Routes to stream1 or stream_n based on configuration
pub unsafe fn mirror_move_256_multi(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> TestStats {
    let test_name = "Mem-Mirror256";
    let streams = config.streams.max(1) as usize;

    if streams == 1 {
        mirror_move_256_stream1_multi(blocks, thread_id, error_mode, timing, config, progress)
    } else {
        mirror_move_256_stream_n_multi(blocks, thread_id, error_mode, timing, config, progress, streams, test_name)
    }
}

// ============================================================================
// MirrorMove512 (AVX-512) - MultiBlock Implementations
// ============================================================================

/// MirrorMove512 Stream1 - Single stream (linear sequential) - MultiBlock pattern
/// Tests all blocks with shared timer and interleaved execution
/// Runtime-checked wrapper for AVX-512 availability
unsafe fn mirror_move_512_stream1_multi(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> TestStats {
    let test_name = "Mem-Mirror512";

    // Runtime CPU feature check
    if !is_x86_feature_detected!("avx512f") {
        log::warn!("{}: AVX-512 not available, returning zero stats", test_name);
        return TestStats {
            name: test_name,
            action: TestAction::ReadWrite,
            bytes_processed: 0,
            elapsed_ms: 0,
            thread_id,
            error_count: 0,
            total_operations: 0,
            cycles_completed: 0,
            cycles_planned: None,
            stopped_by_time_limit: false,
        };
    }

    // Call the AVX-512 optimized implementation
    mirror_move_512_stream1_impl(blocks, thread_id, error_mode, timing, config, progress)
}

/// MirrorMove512 Stream1 - AVX-512 optimized implementation
/// This function is annotated with #[target_feature] to enable full compiler optimization
#[inline]
#[target_feature(enable = "avx512f")]
unsafe fn mirror_move_512_stream1_impl(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> TestStats {

    let test_name = "Mem-Mirror512";

    let test_start = Instant::now();

    // Calculate window and prepare blocks
    let total_allocated: usize = blocks.iter().map(|b| b.buffer.size()).sum();
    let window_size = config.calculate_window_size(test_name, total_allocated);
    let test_blocks = prepare_blocks_for_window(blocks, window_size, test_name);

    if test_blocks.is_empty() {
        log::warn!("{}: No blocks to test", test_name);
        return TestStats {
            name: test_name,
            action: TestAction::ReadWrite,
            bytes_processed: 0,
            elapsed_ms: 0,
            thread_id,
            error_count: 0,
            total_operations: 0,
            cycles_completed: 0,
            cycles_planned: None,
            stopped_by_time_limit: false,
        };
    }

    // Pre-compute pattern base outside all loops
    let thread_pattern_base = (thread_id as i32) << 16;

    // VECTOR OPTIMIZATION: Pre-compute pattern components for initialization
    let multipliers = i32x16::from_array([16, 15, 14, 13, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1]);
    let base_broadcast = i32x16::splat(thread_pattern_base);
    let pattern_base = base_broadcast * multipliers ;  // base × [1..16]

    // Initialize memory with AVX-512 patterns for each block
    for test_block in test_blocks.iter() {
        let base = test_block.block.buffer.as_mut_ptr() as *mut i32x16;
        let len = test_block.test_size / std::mem::size_of::<i32x16>();

        for i in 0..len {
            // OPTIMIZED: 1 broadcast + 1 mullo + 1 add (replaces 16 scalar muls + 16 adds)
            let idx_broadcast = i32x16::splat(i as i32);
            let idx_scaled = idx_broadcast * multipliers ;  // i × [1..16]
            let pattern = idx_scaled + pattern_base ;         // (i×M) + (base×M)
            *base.add(i) = pattern;
        }
    }

    // Validate chunk divisibility for ALL blocks ONCE before starting (not in hot loop)
    // Different block sizes may have different chunk sizes, so validate each
    for test_block in test_blocks.iter() {
        let ideal_chunk_size = calculate_ideal_chunk_size(config, test_name, test_block.test_size);
        let chunk_size_bytes = get_safe_chunk_size(ideal_chunk_size, test_block.test_size);
        let chunk_size_elements = chunk_size_bytes / std::mem::size_of::<i32x16>();

        // Stream1 has streams=1, so chunk doesn't need to be divisible by streams
        // But we validate anyway for consistency
        validate_chunk_divisibility(test_name, 1, chunk_size_elements);
    }

    let mut cycle = 0u32;
    let mut total_bytes_processed = 0u64;
    let mut total_error_count = 0u64;
    let mut total_operations = 0u64;

    // Progress tracking
    let update_interval_ms = 250u128; // 4 updates/sec
    let mut last_progress_update = Instant::now();

    // Main cycle loop - shared timer across all blocks
    loop {
        cycle += 1;

        // Test all blocks in interleaved manner (1,2,3,1,2,3...)
        for test_block in test_blocks.iter() {
            let base = test_block.block.buffer.as_mut_ptr() as *mut i32x16;
            let len = test_block.test_size / std::mem::size_of::<i32x16>();

            // Calculate chunk size for this block's test_size
            let ideal_chunk_size = calculate_ideal_chunk_size(config, test_name, test_block.test_size);
            let chunk_size_bytes = get_safe_chunk_size(ideal_chunk_size, test_block.test_size);
            let chunk_size_elements = chunk_size_bytes / std::mem::size_of::<i32x16>();

            let mut cycle_errors = 0u64;

            for chunk_start in (0..len).step_by(chunk_size_elements) {
                let chunk_end = (chunk_start + chunk_size_elements).min(len);
                let chunk_len = chunk_end - chunk_start;

                // 1. Mirror operation - single stream with two-pointer optimization
                let mut idx1 = chunk_start;
                let mut idx2 = chunk_start + chunk_len - 1;
                while idx1 < idx2 {
                    let val1 = *base.add(idx1);
                    let val2 = *base.add(idx2);
                    *base.add(idx2) = val1;  // Cache-backed store
                    *base.add(idx1) = val2;  // Cache-backed store
                    idx1 += 1;
                    idx2 -= 1;
                }
                // NOTE: _mm_sfence() removed - not needed for temporal stores (cache coherency handles ordering)
        // TODO: If using TMR-MD Uncached/WC memory OR non-temporal stores, may need to restore fence

                // 2. Verify with configurable error checking frequency
                let mut error_accumulator = i32x16::splat(0);
                let mut i = chunk_start;
                let chunk_sum = chunk_start + chunk_end - 1;
                let mut element_count = 0usize;

                // Pre-compute check mask for hot loop optimization
                match config.error_check_interval.get_check_mask() {
                    Some(check_mask) => {
                        while i < chunk_end {
                            // Pre-compute mirrored index (hot loop optimization)
                            let mirrored_idx = chunk_sum - i;

                            // OPTIMIZED: 1 broadcast + 1 mullo + 1 add (replaces 16 scalar muls + 16 adds)
                            let idx_broadcast = i32x16::splat(mirrored_idx as i32);
                            let idx_scaled = idx_broadcast * multipliers ;  // idx × [1..16]
                            let expected = idx_scaled + pattern_base ;         // (idx×M) + (base×M)

                            let actual = *base.add(i);
                            let diff = expected ^ actual ;
                            error_accumulator = error_accumulator | diff ;

                            element_count += 1;
                            i += 1;

                            // Check errors at configured intervals (zero-branch hot loop optimization)
                            if (element_count as u32 & check_mask) == 0 {
                                if error_accumulator.simd_ne(i32x16::splat(0)).any() {
                                    cycle_errors += 1;
                                    log::error!("{}: memory error detected in cycle {} chunk {} element {} (thread {})",
                                               test_name, cycle, chunk_start, element_count, thread_id);
                                    error_accumulator = i32x16::splat(0);
                                }
                            }
                        }
                    }
                    None => {
                        // PER_CHUNK mode - no intermediate checks, maximum performance
                        while i < chunk_end {
                            // Pre-compute mirrored index (hot loop optimization)
                            let mirrored_idx = chunk_sum - i;

                            // OPTIMIZED: 1 broadcast + 1 mullo + 1 add (replaces 16 scalar muls + 16 adds)
                            let idx_broadcast = i32x16::splat(mirrored_idx as i32);
                            let idx_scaled = idx_broadcast * multipliers ;  // idx × [1..16]
                            let expected = idx_scaled + pattern_base ;         // (idx×M) + (base×M)

                            let actual = *base.add(i);
                            let diff = expected ^ actual ;
                            error_accumulator = error_accumulator | diff ;
                            i += 1;
                        }
                    }
                }

                // Final error check (always performed regardless of mode)
                if error_accumulator.simd_ne(i32x16::splat(0)).any() {
                    cycle_errors += 1;
                    log::error!("{}: memory error detected in cycle {} chunk {} (thread {})",
                               test_name, cycle, chunk_start, thread_id);
                }

                // 3. Mirror back to restore - same two-pointer optimization
                let mut idx1 = chunk_start;
                let mut idx2 = chunk_start + chunk_len - 1;
                while idx1 < idx2 {
                    let val1 = *base.add(idx1);
                    let val2 = *base.add(idx2);
                    *base.add(idx2) = val1;  // Cache-backed store
                    *base.add(idx1) = val2;  // Cache-backed store
                    idx1 += 1;
                    idx2 -= 1;
                }
                // NOTE: _mm_sfence() removed - not needed for temporal stores (cache coherency handles ordering)
        // TODO: If using TMR-MD Uncached/WC memory OR non-temporal stores, may need to restore fence

                // Optimized error handling - check ONCE at end of chunk
                if cycle_errors > 0 {
                    match error_mode {
                        ErrorMode::Panic => {
                            panic!("{}: memory error detected in cycle {} (thread {})", test_name, cycle, thread_id);
                        }
                        ErrorMode::Halt => {
                            let elapsed = test_start.elapsed().as_millis();
                            total_error_count += cycle_errors;
                            return TestStats {
                                name: test_name,
                                action: TestAction::WriteWaitVerify,
                                bytes_processed: total_bytes_processed as usize,
                                elapsed_ms: elapsed,
                                thread_id,
                                error_count: total_error_count,
                                total_operations,
                                cycles_completed: cycle,
                                cycles_planned: timing.cycles,
                                stopped_by_time_limit: false,
                            };
                        }
                        ErrorMode::Log => {
                            // Already logged, continue
                        }
                    }
                }
            }

            total_error_count += cycle_errors;
            total_bytes_processed += (test_block.test_size * 5) as u64; // Mirror (R+W) + verify (R) + mirror back (R+W) = 3R + 2W
            total_operations += len as u64;
        }

        // Update progress tracker
        if let Some(progress) = progress {
            let now = Instant::now();
            if now.duration_since(last_progress_update).as_millis() >= update_interval_ms {
                progress.cycles_completed.store(cycle, Ordering::Relaxed);
                progress.bytes_processed.store(total_bytes_processed, Ordering::Relaxed);
                progress.errors_found.store(total_error_count, Ordering::Relaxed);
                progress.last_update_ms.store(
                    test_start.elapsed().as_millis() as u64,
                    Ordering::Relaxed
                );
                last_progress_update = now;
            }
        }

        // Check timing limits
        let elapsed_secs = test_start.elapsed().as_secs() as u32;
        if !timing.should_continue(cycle, elapsed_secs) {
            return TestStats {
                name: test_name,
                action: TestAction::WriteWaitVerify,
                bytes_processed: total_bytes_processed as usize,
                elapsed_ms: test_start.elapsed().as_millis(),
                thread_id,
                error_count: total_error_count,
                total_operations,
                cycles_completed: cycle,
                cycles_planned: timing.cycles,
                stopped_by_time_limit: timing.cycles.map_or(true, |limit| cycle < limit),
            };
        }
    }
}

/// MirrorMove512 StreamN - N-way segmented mirroring - MultiBlock pattern
/// Tests all blocks with shared timer and interleaved execution
/// Runtime-checked wrapper for AVX-512 availability
unsafe fn mirror_move_512_stream_n_multi(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
    streams: usize,
    test_name: &'static str,
) -> TestStats {
    // Runtime CPU feature check
    if !is_x86_feature_detected!("avx512f") {
        log::warn!("{}: AVX-512 not available, returning zero stats", test_name);
        return TestStats {
            name: test_name,
            action: TestAction::ReadWrite,
            bytes_processed: 0,
            elapsed_ms: 0,
            thread_id,
            error_count: 0,
            total_operations: 0,
            cycles_completed: 0,
            cycles_planned: None,
            stopped_by_time_limit: false,
        };
    }

    // Call the AVX-512 optimized implementation
    mirror_move_512_stream_n_impl(blocks, thread_id, error_mode, timing, config, progress, streams, test_name)
}

/// MirrorMove512 StreamN - AVX-512 optimized implementation
/// This function is annotated with #[target_feature] to enable full compiler optimization
#[inline]
#[target_feature(enable = "avx512f")]
unsafe fn mirror_move_512_stream_n_impl(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
    streams: usize,
    test_name: &'static str,
) -> TestStats {

    let test_start = Instant::now();
    let stream_shift = streams.trailing_zeros();

    // Calculate window and prepare blocks
    let total_allocated: usize = blocks.iter().map(|b| b.buffer.size()).sum();
    let window_size = config.calculate_window_size(test_name, total_allocated);
    let test_blocks = prepare_blocks_for_window(blocks, window_size, test_name);

    if test_blocks.is_empty() {
        log::warn!("{}: No blocks to test", test_name);
        return TestStats {
            name: test_name,
            action: TestAction::ReadWrite,
            bytes_processed: 0,
            elapsed_ms: 0,
            thread_id,
            error_count: 0,
            total_operations: 0,
            cycles_completed: 0,
            cycles_planned: None,
            stopped_by_time_limit: false,
        };
    }

    // Pre-compute pattern base outside all loops
    let thread_pattern_base = (thread_id as i32) << 16;

    // VECTOR OPTIMIZATION: Pre-compute pattern components for initialization
    let multipliers = i32x16::from_array([16, 15, 14, 13, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1]);
    let base_broadcast = i32x16::splat(thread_pattern_base);
    let pattern_base = base_broadcast * multipliers ;  // base × [1..16]

    // Initialize memory with AVX-512 patterns for each block
    for test_block in test_blocks.iter() {
        let base = test_block.block.buffer.as_mut_ptr() as *mut i32x16;
        let len = test_block.test_size / std::mem::size_of::<i32x16>();

        for i in 0..len {
            // OPTIMIZED: 1 broadcast + 1 mullo + 1 add (replaces 16 scalar muls + 16 adds)
            let idx_broadcast = i32x16::splat(i as i32);
            let idx_scaled = idx_broadcast * multipliers ;  // i × [1..16]
            let pattern = idx_scaled + pattern_base ;         // (i×M) + (base×M)
            *base.add(i) = pattern;
        }
    }

    // Validate chunk divisibility for ALL blocks ONCE before starting
    for test_block in test_blocks.iter() {
        let ideal_chunk_size = calculate_ideal_chunk_size(config, test_name, test_block.test_size);
        let chunk_size_bytes = get_safe_chunk_size(ideal_chunk_size, test_block.test_size);
        let chunk_size_elements = chunk_size_bytes / std::mem::size_of::<i32x16>();

        validate_chunk_divisibility(test_name, config.streams, chunk_size_elements);
    }

    let mut cycle = 0u32;
    let mut total_bytes_processed = 0u64;
    let mut total_error_count = 0u64;
    let mut total_operations = 0u64;

    // Progress tracking
    let update_interval_ms = 250u128;
    let mut last_progress_update = Instant::now();

    // Main cycle loop - shared timer across all blocks
    loop {
        cycle += 1;

        // Test all blocks in interleaved manner
        for test_block in test_blocks.iter() {
            let base = test_block.block.buffer.as_mut_ptr() as *mut i32x16;
            let len = test_block.test_size / std::mem::size_of::<i32x16>();

            // Calculate chunk size for this block's test_size
            let ideal_chunk_size = calculate_ideal_chunk_size(config, test_name, test_block.test_size);
            let chunk_size_bytes = get_safe_chunk_size(ideal_chunk_size, test_block.test_size);
            let chunk_size_elements = chunk_size_bytes / std::mem::size_of::<i32x16>();

            let mut cycle_errors = 0u64;

            for chunk_start in (0..len).step_by(chunk_size_elements) {
                let chunk_end = (chunk_start + chunk_size_elements).min(len);
                let chunk_len = chunk_end - chunk_start;

                // Pre-compute stream boundaries for this chunk
                let elements_per_stream = chunk_len >> stream_shift;

                // 1. Mirror operation - multi-stream
                for stream_id in 0..streams {
                    let stream_start = chunk_start + (stream_id * elements_per_stream);
                    let stream_end = stream_start + elements_per_stream;

                    let mut idx1 = stream_start;
                    let mut idx2 = stream_end - 1;
                    while idx1 < idx2 {
                        let val1 = *base.add(idx1);
                        let val2 = *base.add(idx2);
                        *base.add(idx2) = val1;  // Cache-backed store
                        *base.add(idx1) = val2;  // Cache-backed store
                        idx1 += 1;
                        idx2 -= 1;
                    }
                }
                // NOTE: _mm_sfence() removed - not needed for temporal stores (cache coherency handles ordering)
        // TODO: If using TMR-MD Uncached/WC memory OR non-temporal stores, may need to restore fence

                // 2. Verify with configurable error checking frequency - multi-stream
                let mut error_accumulator = i32x16::splat(0);
                let mut element_count = 0usize;

                match config.error_check_interval.get_check_mask() {
                    Some(check_mask) => {
                        for stream_id in 0..streams {
                            let stream_start = chunk_start + (stream_id * elements_per_stream);
                            let stream_end = stream_start + elements_per_stream;
                            let stream_sum = stream_start + stream_end - 1;

                            let mut i = stream_start;
                            while i < stream_end {
                                let mirrored_idx = stream_sum - i;

                                // OPTIMIZED: 1 broadcast + 1 mullo + 1 add (replaces 16 scalar muls + 16 adds)
                                let idx_broadcast = i32x16::splat(mirrored_idx as i32);
                                let idx_scaled = idx_broadcast * multipliers ;  // idx × [1..16]
                                let expected = idx_scaled + pattern_base ;         // (idx×M) + (base×M)

                                let actual = *base.add(i);
                                let diff = expected ^ actual ;
                                error_accumulator = error_accumulator | diff ;

                                element_count += 1;
                                i += 1;

                                if (element_count as u32 & check_mask) == 0 {
                                    if error_accumulator.simd_ne(i32x16::splat(0)).any() {
                                        cycle_errors += 1;
                                        log::error!("{}: memory error detected in cycle {} chunk {} stream {} element {} (thread {})",
                                                   test_name, cycle, chunk_start, stream_id, element_count, thread_id);
                                        error_accumulator = i32x16::splat(0);
                                    }
                                }
                            }
                        }
                    }
                    None => {
                        // PER_CHUNK mode
                        for stream_id in 0..streams {
                            let stream_start = chunk_start + (stream_id * elements_per_stream);
                            let stream_end = stream_start + elements_per_stream;
                            let stream_sum = stream_start + stream_end - 1;

                            let mut i = stream_start;
                            while i < stream_end {
                                let mirrored_idx = stream_sum - i;

                                // OPTIMIZED: 1 broadcast + 1 mullo + 1 add (replaces 16 scalar muls + 16 adds)
                                let idx_broadcast = i32x16::splat(mirrored_idx as i32);
                                let idx_scaled = idx_broadcast * multipliers ;  // idx × [1..16]
                                let expected = idx_scaled + pattern_base ;         // (idx×M) + (base×M)

                                let actual = *base.add(i);
                                let diff = expected ^ actual ;
                                error_accumulator = error_accumulator | diff ;
                                i += 1;
                            }
                        }
                    }
                }

                // Final error check
                if error_accumulator.simd_ne(i32x16::splat(0)).any() {
                    cycle_errors += 1;
                    log::error!("{}: memory error detected in cycle {} chunk {} (thread {})",
                               test_name, cycle, chunk_start, thread_id);
                }

                // 3. Mirror back to restore - multi-stream
                for stream_id in 0..streams {
                    let stream_start = chunk_start + (stream_id * elements_per_stream);
                    let stream_end = stream_start + elements_per_stream;

                    let mut idx1 = stream_start;
                    let mut idx2 = stream_end - 1;
                    while idx1 < idx2 {
                        let val1 = *base.add(idx1);
                        let val2 = *base.add(idx2);
                        *base.add(idx2) = val1;  // Cache-backed store
                        *base.add(idx1) = val2;  // Cache-backed store
                        idx1 += 1;
                        idx2 -= 1;
                    }
                }
                // NOTE: _mm_sfence() removed - not needed for temporal stores (cache coherency handles ordering)
        // TODO: If using TMR-MD Uncached/WC memory OR non-temporal stores, may need to restore fence

                // Error handling
                if cycle_errors > 0 {
                    match error_mode {
                        ErrorMode::Panic => {
                            panic!("{}: memory error detected in cycle {} (thread {})", test_name, cycle, thread_id);
                        }
                        ErrorMode::Halt => {
                            let elapsed = test_start.elapsed().as_millis();
                            total_error_count += cycle_errors;
                            return TestStats {
                                name: test_name,
                                action: TestAction::WriteWaitVerify,
                                bytes_processed: total_bytes_processed as usize,
                                elapsed_ms: elapsed,
                                thread_id,
                                error_count: total_error_count,
                                total_operations,
                                cycles_completed: cycle,
                                cycles_planned: timing.cycles,
                                stopped_by_time_limit: false,
                            };
                        }
                        ErrorMode::Log => {
                            // Already logged, continue
                        }
                    }
                }
            }

            total_error_count += cycle_errors;
            total_bytes_processed += (test_block.test_size * 5) as u64; // Mirror (R+W) + verify (R) + mirror back (R+W) = 3R + 2W
            total_operations += len as u64;
        }

        // Update progress tracker
        if let Some(progress) = progress {
            let now = Instant::now();
            if now.duration_since(last_progress_update).as_millis() >= update_interval_ms {
                progress.cycles_completed.store(cycle, Ordering::Relaxed);
                progress.bytes_processed.store(total_bytes_processed, Ordering::Relaxed);
                progress.errors_found.store(total_error_count, Ordering::Relaxed);
                progress.last_update_ms.store(
                    test_start.elapsed().as_millis() as u64,
                    Ordering::Relaxed
                );
                last_progress_update = now;
            }
        }

        // Check timing limits
        let elapsed_secs = test_start.elapsed().as_secs() as u32;
        if !timing.should_continue(cycle, elapsed_secs) {
            return TestStats {
                name: test_name,
                action: TestAction::WriteWaitVerify,
                bytes_processed: total_bytes_processed as usize,
                elapsed_ms: test_start.elapsed().as_millis(),
                thread_id,
                error_count: total_error_count,
                total_operations,
                cycles_completed: cycle,
                cycles_planned: timing.cycles,
                stopped_by_time_limit: timing.cycles.map_or(true, |limit| cycle < limit),
            };
        }
    }
}

/// MirrorMove512 - PUBLIC MultiBlock dispatcher
/// Routes to stream1 or stream_n based on configuration
pub unsafe fn mirror_move_512_multi(
    blocks: &[crate::runner::AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &TestTiming,
    config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> TestStats {
    let test_name = "Mem-Mirror512";
    let streams = config.streams.max(1) as usize;

    if streams == 1 {
        mirror_move_512_stream1_multi(blocks, thread_id, error_mode, timing, config, progress)
    } else {
        mirror_move_512_stream_n_multi(blocks, thread_id, error_mode, timing, config, progress, streams, test_name)
    }
}

// Stream 1: Linear sequential access pattern

/// # Safety
/// Caller must ensure `ptr` is valid for reads/writes of `size` bytes.

/// # Safety
/// Caller must ensure `ptr` is valid for reads/writes of `size` bytes.
/// Caller must ensure `ptr` is valid for reads/writes of `size` bytes.

// ============================================================================
// Test Registry - Maps config test names to actual function implementations
// ============================================================================

/// Map test name (from config or CLI) to its function implementation
/// This is the single source of truth for test name → function mapping
pub fn get_test_function_by_name(name: &str) -> Option<crate::runner::TestFunction> {

    match name {
        // Stuck Bit Tests

        // Mirror Move Tests

        // Simple Test


        // Performance & Stress Tests

        _ => None,
    }
}