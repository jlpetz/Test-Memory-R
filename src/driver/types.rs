use crate::constants::{REGULAR_PAGE_SIZE_USIZE, LARGE_PAGE_SIZE_USIZE, HUGE_PAGE_SIZE_USIZE};

/// Driver version information
#[repr(C)]
#[derive(Debug, Clone)]
pub struct DriverVersionInfo {
    pub driver_version_major: u32,
    pub driver_version_minor: u32,
    pub driver_version_build: u32,
    pub driver_version_revision: u32,
    pub min_app_version_major: u32,
    pub min_app_version_minor: u32,
    pub max_app_version_major: u32,
    pub max_app_version_minor: u32,
    pub compatibility_flags: u32,
    pub driver_build_date: [u8; 32],
    pub driver_build_time: [u8; 32],
}

/// Driver compatibility flags
#[repr(u32)]
#[derive(Debug, Clone, Copy)]
pub enum CompatibilityFlags {
    None = 0,
    SupportsHugePages = 1 << 0,
    SupportsNuma = 1 << 1,
    SupportsMixedPages = 1 << 2,
    SupportsPhysicalAddress = 1 << 3,
    SupportsDmaV2 = 1 << 4,
    SupportsZeroFree = 1 << 5,
    SupportsBatchAllocation = 1 << 6,
    SupportsETW = 1 << 7,
    SupportsMemoryTypes = 1 << 8,
    SupportsNonContiguous = 1 << 9,
    SupportsTimeout = 1 << 10,
    SupportsRetryControl = 1 << 11,
    SupportsAbortOnFailure = 1 << 12,
}

#[derive(Debug, Clone)]
pub enum DriverStatus {
    Available(DriverVersionInfo),
    VersionMismatch { 
        driver_version: String, 
        app_version: String, 
        min_required: String, 
        max_supported: String 
    },
    NotFound,
    Error(String),
}

/// Page size types matching driver
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PageSize {
    Regular = 0,   // 4KB
    Large = 1,     // 2MB
    Huge = 2,      // 1GB
}

impl PageSize {
    pub fn to_bytes(self) -> usize {
        match self {
            PageSize::Regular => REGULAR_PAGE_SIZE_USIZE,
            PageSize::Large => LARGE_PAGE_SIZE_USIZE,
            PageSize::Huge => HUGE_PAGE_SIZE_USIZE,
        }
    }
    
    pub fn to_kb(self) -> u32 {
        (self.to_bytes() / 1024) as u32
    }
}

/// Input structure for remap all request
#[repr(C)]
#[derive(Copy, Clone)]
#[derive(Default)]
pub struct RemapAllInput {
    pub new_memory_type: u32,  // MemoryType as u32
}

/// Output structure for remap all result
#[repr(C)]
#[derive(Copy, Clone)]
#[derive(Default)]
pub struct RemapAllOutput {
    pub success: bool,
    pub allocations_remapped: u32,
    pub allocations_failed: u32,
    pub total_time_us: u64,
}




/// Input structure for batch remap request
#[repr(C)]
#[derive(Copy, Clone)]
pub struct BatchRemapInput {
    pub request_count: u32,
    pub new_memory_type: u32,  // Apply same type to all
    pub requests: [RemapRequest; 128],  // Max 128 remaps per call
}

/// Individual remap request
#[repr(C)]
#[derive(Copy, Clone, Default)]
pub struct RemapRequest {
    pub user_address: u64,      // Virtual address returned from allocation
    pub allocation_id: u64,     // Optional: if driver tracks by ID
}

/// Output structure for batch remap
#[repr(C)]
#[derive(Copy, Clone)]
pub struct BatchRemapOutput {
    pub success_count: u32,
    pub failure_count: u32,
    pub results: [RemapResult; 128],
}

/// Individual remap result
#[repr(C)]
#[derive(Copy, Clone, Default)]
pub struct RemapResult {
    pub success: bool,
    pub user_address: u64,      // Original address
    pub new_address: u64,       // New address if changed (might be same)
    pub error_code: u32,        // Windows error code if failed
}

impl Default for BatchRemapInput {
    fn default() -> Self {
        Self {
            request_count: 0,
            new_memory_type: 0,
            requests: [RemapRequest::default(); 128],
        }
    }
}

impl Default for BatchRemapOutput {
    fn default() -> Self {
        Self {
            success_count: 0,
            failure_count: 0,
            results: [RemapResult::default(); 128],
        }
    }
}

/// Memory type for caching behavior matching driver
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MemoryType {
    WriteBack = 0,       // Normal cached (WB)
    WriteThrough = 1,    // Write-through cache (WT)
    Uncached = 2,        // Uncached (UC)
    WriteCombining = 3,  // Write-combining (WC) for GPU/DMA
    WriteProtected = 4,  // Write-protected memory
}

/// Driver version error
#[derive(Debug)]
pub enum DriverVersionError {
    DriverNotFound,
    VersionMismatch {
        driver_version: String,
        app_version: String,
        min_required: String,
        max_supported: String,
    },
    QueryFailed(String),
}

impl std::fmt::Display for DriverVersionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DriverVersionError::DriverNotFound => {
                write!(f, "TMR kernel driver not found or not accessible")
            }
            DriverVersionError::VersionMismatch { driver_version, app_version, min_required, max_supported } => {
                write!(f, "Driver version mismatch: driver={}, app={}, required={}, max={}", 
                       driver_version, app_version, min_required, max_supported)
            }
            DriverVersionError::QueryFailed(error) => {
                write!(f, "Failed to query driver version: {}", error)
            }
        }
    }
}

impl std::error::Error for DriverVersionError {}

/// Input structure for allocation requests matching driver
#[repr(C)]
#[derive(Copy, Clone)]
pub struct AllocateDmaInput {
    pub size: usize,
    pub numa_node: u32,
    pub memory_type: u32,
    pub minimum_page_size: u32,
    pub maximum_page_size: u32,
    pub strict_numa: bool,
    pub zero_memory: bool,
    pub contiguous: bool,
    pub timeout_ms: u32,
    pub retry_interval_ms: u32,
    pub max_retries: u32,
}

impl Default for AllocateDmaInput {
    fn default() -> Self {
        Self {
            size: 0,
            numa_node: 0xFFFFFFFF,
            memory_type: 0,
            minimum_page_size: 0,
            maximum_page_size: 2,
            strict_numa: false,
            zero_memory: false,
            contiguous: true,
            timeout_ms: 10000,
            retry_interval_ms: 10,
            max_retries: 100,
        }
    }
}

/// Output structure for allocation results
#[repr(C)]
#[derive(Copy, Clone)]
#[derive(Default)]
pub struct AllocateDmaOutput {
    pub user_address: u64,
    pub physical_address: u64,
    pub size: usize,
    pub allocation_id: u64,
    pub numa_node: u32,
    pub page_composition: PageCompositionInfo,  // NEW: Detailed breakdown
    pub numa_breakdown: NumaBreakdownInfo,      // NEW: NUMA locality stats
}

/// Page composition for IOCTL (matches driver structure)
#[repr(C)]
#[derive(Copy, Clone, Default)]
pub struct PageCompositionInfo {
    pub huge_pages_count: u32,      // Actual 1GB pages allocated
    pub large_pages_count: u32,     // Actual 2MB pages allocated
    pub regular_pages_count: u32,   // Actual 4KB pages allocated
    pub huge_pages_bytes: u64,      // Total bytes in huge pages
    pub large_pages_bytes: u64,     // Total bytes in large pages
    pub regular_pages_bytes: u64,   // Total bytes in regular pages
}

/// NUMA breakdown for IOCTL - simplified to local vs remote
#[repr(C)]
#[derive(Copy, Clone)]
pub struct NumaBreakdownInfo {
    pub thread_local_node: u32,         // The thread's local NUMA node
    pub local_bytes: u64,               // Bytes allocated on local node
    pub remote_bytes: u64,              // Bytes allocated on remote nodes
    pub local_allocation_percent: f32,   // Percentage on local node
    pub locality_info: [LocalityInfo; 2], // [0]=local, [1]=remote
}

/// Locality info - tracks local vs remote allocations
#[repr(C)]
#[derive(Copy, Clone, Default)]
pub struct LocalityInfo {
    pub allocation_count: u32,   // Number of allocations
    pub bytes_allocated: u64,    // Total bytes
    pub huge_pages: u32,         // Number of huge pages
    pub large_pages: u32,        // Number of large pages
    pub regular_pages: u32,      // Number of regular pages
}

impl Default for NumaBreakdownInfo {
    fn default() -> Self {
        Self {
            thread_local_node: 0,
            local_bytes: 0,
            remote_bytes: 0,
            local_allocation_percent: 0.0,
            locality_info: [LocalityInfo::default(); 2],
        }
    }
}


/// Input structure for free requests
#[repr(C)]
#[derive(Copy, Clone)]
pub struct FreeDmaInput {
    pub user_address: u64,
}

/// Driver runtime statistics from IOCTL_TMR_GET_STATISTICS
#[repr(C)]
#[derive(Copy, Clone, Default)]
pub struct DriverStatistics {
    pub total_calls: u64,
    pub allocate_dma_calls: u64,
    pub free_dma_calls: u64,
    pub batch_allocate_calls: u64,
    pub get_statistics_calls: u64,
    pub set_cpu_affinity_calls: u64,
    pub get_hardware_info_calls: u64,
    pub reset_all_calls: u64,
    pub get_version_calls: u64,
    pub batch_remap_calls: u64,
    pub remap_all_calls: u64,
    pub active_allocations: u64,
    pub total_allocated_bytes: u64,
    pub peak_allocated_bytes: u64,
    pub allocation_failures: u64,
    pub last_error_code: u32,
    pub driver_uptime_ms: u64,
}

/// Batch allocation structures (matching driver)
#[repr(C)]
pub struct BatchAllocateInput {
    pub request_count: u32,
    pub abort_on_failure: bool,
    pub total_memory_target: usize,
    pub distribute_huge_pages_evenly: bool,
    pub requests: [ThreadAllocationRequest; 32],
}

#[repr(C)]
pub struct BatchAllocateOutput {
    pub total_allocations: u32,
    pub successful_allocations: u32,
    pub failed_allocations: u32,
    pub total_time_us: u32,
    pub results: [AllocationResult; 128],
}

/// Enhanced batch allocation structures
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ThreadAllocationRequest {
    pub thread_id: u32,
    pub cpu_id: u32,
    pub size_bytes: usize,
    pub block_count: u32,
    pub minimum_page_size: PageSize,
    pub maximum_page_size: PageSize,
    pub memory_type: MemoryType,
    pub numa_node: u32,
    pub strict_numa: bool,
    pub zero_memory: bool,
    pub contiguous: bool,
    pub timeout_ms: u32,
    pub retry_interval_ms: u32,
    pub max_retries: u32,
}

impl Default for ThreadAllocationRequest {
    fn default() -> Self {
        Self {
            thread_id: 0,
            cpu_id: 0,
            size_bytes: 0,
            block_count: 1,
            minimum_page_size: PageSize::Regular,
            maximum_page_size: PageSize::Huge,
            memory_type: MemoryType::WriteBack,
            numa_node: 0,
            strict_numa: false,
            zero_memory: false,
            contiguous: true,
            timeout_ms: 10000,
            retry_interval_ms: 10,
            max_retries: 100,
        }
    }
}

#[repr(C)]
#[derive(Copy, Clone, Default)]
pub struct AllocationResult {
    pub success: bool,
    pub thread_id: u32,
    pub block_index: u32,
    pub virtual_address: u64,
    pub physical_address: u64,
    pub size: usize,
    pub page_size_kb: u32,
    pub numa_node: u32,              // Requested NUMA node
    pub actual_numa_node: u32,       // Where it was actually allocated
    pub is_numa_local: bool,         // true if actual == requested
    pub guaranteed_page_type: u32,   // Guaranteed page type from driver
    pub allocation_time_us: u32,
}

#[repr(C)]
pub struct EnhancedBatchAllocateInput {
    pub request_count: u32,
    pub abort_on_failure: bool,
    pub total_memory_target: usize,
    pub distribute_huge_pages_evenly: bool,
    pub requests: [ThreadAllocationRequest; 32],
}

impl Default for EnhancedBatchAllocateInput {
    fn default() -> Self {
        Self {
            request_count: 0,
            abort_on_failure: false,
            total_memory_target: 0,
            distribute_huge_pages_evenly: true,
            requests: [ThreadAllocationRequest::default(); 32],
        }
    }
}

#[repr(C)]
pub struct EnhancedBatchAllocateOutput {
    pub total_allocations: u32,
    pub successful_allocations: u32,
    pub failed_allocations: u32,
    pub total_time_us: u32,
    pub results: [AllocationResult; 128],
}