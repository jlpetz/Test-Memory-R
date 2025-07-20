use std::sync::Arc;
use std::collections::HashMap;
use std::sync::OnceLock;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use windows::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows::Win32::Storage::FileSystem::{CreateFileA, OPEN_EXISTING};
use windows::Win32::System::IO::DeviceIoControl;
use windows::core::PCSTR;
use crate::AllocatedBlock;
use crate::utils::{TableBuilder, Alignment};

static USE_REMAP_ALL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// TMR application version
const APP_VERSION_MAJOR: u32 = 1;
const APP_VERSION_MINOR: u32 = 0;
const APP_VERSION_BUILD: u32 = 0;

/// IOCTL for version query
const IOCTL_TMR_GET_VERSION: u32 = ctl_code(FILE_DEVICE_TMR, 0x7FF, METHOD_BUFFERED, FILE_ANY_ACCESS);
pub const IOCTL_TMR_BATCH_REMAP_MEMORY_TYPE: u32 = ctl_code(FILE_DEVICE_TMR, 0x815, METHOD_BUFFERED, FILE_ANY_ACCESS);
pub const IOCTL_TMR_REMAP_ALL_MEMORY_TYPE: u32 = ctl_code(FILE_DEVICE_TMR, 0x816, METHOD_BUFFERED, FILE_ANY_ACCESS);

pub fn set_use_remap_all(value: bool) {
    USE_REMAP_ALL.store(value, std::sync::atomic::Ordering::Relaxed);
}

pub fn get_use_remap_all() -> bool {
    USE_REMAP_ALL.load(std::sync::atomic::Ordering::Relaxed)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DriverStatType {
    TotalCalls,
    AllocateDma,
    FreeDma,
    GetMemoryStats,
    BatchAllocate,
    GetStatistics,
    SetCpuAffinity,
    GetHardwareInfo,
    ResetAll,
    GetVersion,
    BatchRemapMemoryType,
    RemapAllMemoryType,  // New separate tracking
}

// Stat definition with all metadata
struct StatDefinition {
    stat_type: DriverStatType,
    display_name: &'static str,
    counter: AtomicU64,
}

pub struct AppDriverStats {
    stats: Vec<StatDefinition>,
}

impl AppDriverStats {
    fn new() -> Self {
        // Define all stats in one place with their display names
        let stat_defs = vec![
            (DriverStatType::TotalCalls, "Total Calls"),
            (DriverStatType::AllocateDma, "Allocate DMA"),
            (DriverStatType::FreeDma, "Free DMA"),
            (DriverStatType::GetMemoryStats, "Get Memory Stats"),
            (DriverStatType::BatchAllocate, "Batch Allocate"),
            (DriverStatType::GetStatistics, "Get Statistics"),
            (DriverStatType::SetCpuAffinity, "Set CPU Affinity"),
            (DriverStatType::GetHardwareInfo, "Get Hardware Info"),
            (DriverStatType::ResetAll, "Reset All"),
            (DriverStatType::GetVersion, "Get Version"),
            (DriverStatType::BatchRemapMemoryType, "Batch Remap Memory Type"),
            (DriverStatType::RemapAllMemoryType, "Remap All Memory Type"),
        ];
        
        let stats = stat_defs
            .into_iter()
            .map(|(stat_type, display_name)| StatDefinition {
                stat_type,
                display_name,
                counter: AtomicU64::new(0),
            })
            .collect();
        
        Self { stats }
    }
    
    fn increment(&self, stat_type: DriverStatType) {
        // Find and increment the specific stat
        if let Some(stat) = self.stats.iter().find(|s| s.stat_type == stat_type) {
            stat.counter.fetch_add(1, Ordering::Relaxed);
        }
        // Always increment total calls (except for total calls itself)
        if stat_type != DriverStatType::TotalCalls {
            if let Some(total) = self.stats.iter().find(|s| s.stat_type == DriverStatType::TotalCalls) {
                total.counter.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    
    fn reset(&self) {
        for stat in &self.stats {
            stat.counter.store(0, Ordering::Relaxed);
        }
    }
    
    pub fn get_summary(&self) -> HashMap<String, u64> {
        self.stats
            .iter()
            .map(|stat| (stat.display_name.to_string(), stat.counter.load(Ordering::Relaxed)))
            .collect()
    }
    
    pub fn get_value(&self, stat_type: DriverStatType) -> u64 {
        self.stats
            .iter()
            .find(|s| s.stat_type == stat_type)
            .map(|s| s.counter.load(Ordering::Relaxed))
            .unwrap_or(0)
    }
}

// Global application stats
static APP_DRIVER_STATS: OnceLock<AppDriverStats> = OnceLock::new();

fn get_app_stats() -> &'static AppDriverStats {
    APP_DRIVER_STATS.get_or_init(|| AppDriverStats::new())
}

// Helper macro to track calls
macro_rules! track_driver_call {
    ($stat_type:expr) => {
        {
            let stats = get_app_stats();
            stats.increment($stat_type);
        }
    };
}

/// Input structure for remap all request
#[repr(C)]
#[derive(Copy, Clone)]
pub struct RemapAllInput {
    pub new_memory_type: u32,  // MemoryType as u32
}

/// Output structure for remap all result
#[repr(C)]
#[derive(Copy, Clone)]
pub struct RemapAllOutput {
    pub success: bool,
    pub allocations_remapped: u32,
    pub allocations_failed: u32,
    pub total_time_us: u32,
}

// Make sure these have Default implementations
impl Default for RemapAllInput {
    fn default() -> Self {
        Self {
            new_memory_type: MemoryType::WriteBack as u32,
        }
    }
}

impl Default for RemapAllOutput {
    fn default() -> Self {
        Self {
            success: false,
            allocations_remapped: 0,
            allocations_failed: 0,
            total_time_us: 0,
        }
    }
}


// Global driver handle that gets initialized once
static DRIVER_HANDLE: Mutex<Option<Result<(Arc<DriverHandle>, DriverVersionInfo), String>>> = Mutex::new(None);

// Update get_global_driver_handle
pub fn get_global_driver_handle() -> Result<Arc<DriverHandle>, String> {
    let mut handle_guard = DRIVER_HANDLE.lock().map_err(|_| "Failed to lock driver handle".to_string())?;
    
    if let Some(ref result) = *handle_guard {
        // Return existing handle if available
        result.as_ref()
            .map(|(handle, _)| Arc::clone(handle))
            .map_err(|e| e.clone())
    } else {
        // Initialize new handle
        match DriverHandle::open() {
            Ok(handle) => {
                match handle.check_version_compatibility() {
                    Ok(version) => {
                        let arc_handle = Arc::clone(&handle);
                        *handle_guard = Some(Ok((handle, version)));
                        Ok(arc_handle)
                    }
                    Err(e) => {
                        let error = format!("Driver version incompatible: {}", e);
                        *handle_guard = Some(Err(error.clone()));
                        Err(error)
                    }
                }
            }
            Err(e) => {
                *handle_guard = Some(Err(e.clone()));
                Err(e)
            }
        }
    }
}

pub fn reset_driver_state() {
    if let Ok(mut handle_guard) = DRIVER_HANDLE.lock() {
        // Clear the cached handle - this will drop the Arc and close the handle if no other references
        *handle_guard = None;
    }
}

pub fn is_driver_connected() -> bool {
    DRIVER_HANDLE.lock().ok()
        .and_then(|guard| guard.as_ref().map(|result| result.is_ok()))
        .unwrap_or(false)
}

/// Driver version information
#[repr(C)]
#[derive(Copy, Clone, Debug)]
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
    VersionMismatch { driver_version: String, app_version: String, min_required: String, max_supported: String },
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
            PageSize::Regular => 4 * 1024,
            PageSize::Large => 2 * 1024 * 1024,
            PageSize::Huge => 1024 * 1024 * 1024,
        }
    }
    
    pub fn to_kb(self) -> u32 {
        (self.to_bytes() / 1024) as u32
    }
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
                write!(f, "TMR kernel driver not found")
            }
            DriverVersionError::VersionMismatch { driver_version, app_version, min_required, max_supported } => {
                write!(f, "Driver version mismatch:\n  Driver: {}\n  Application: {}\n  Driver requires app version: {} - {}", 
                       driver_version, app_version, min_required, max_supported)
            }
            DriverVersionError::QueryFailed(msg) => {
                write!(f, "Failed to query driver version: {}", msg)
            }
        }
    }
}

impl std::error::Error for DriverVersionError {}

// IOCTL control codes
const FILE_DEVICE_TMR: u32 = 0x8000;
const METHOD_BUFFERED: u32 = 0;
const FILE_ANY_ACCESS: u32 = 0;

const fn ctl_code(device_type: u32, function: u32, method: u32, access: u32) -> u32 {
    (device_type << 16) | (access << 14) | (function << 2) | method
}

pub const IOCTL_TMR_ALLOCATE_DMA: u32 = ctl_code(FILE_DEVICE_TMR, 0x800, METHOD_BUFFERED, FILE_ANY_ACCESS);
pub const IOCTL_TMR_FREE_DMA: u32 = ctl_code(FILE_DEVICE_TMR, 0x801, METHOD_BUFFERED, FILE_ANY_ACCESS);
pub const IOCTL_TMR_GET_MEMORY_STATS: u32 = ctl_code(FILE_DEVICE_TMR, 0x802, METHOD_BUFFERED, FILE_ANY_ACCESS);
pub const IOCTL_TMR_BATCH_ALLOCATE: u32 = ctl_code(FILE_DEVICE_TMR, 0x810, METHOD_BUFFERED, FILE_ANY_ACCESS);
pub const IOCTL_TMR_GET_STATISTICS: u32 = ctl_code(FILE_DEVICE_TMR, 0x811, METHOD_BUFFERED, FILE_ANY_ACCESS);
pub const IOCTL_TMR_SET_CPU_AFFINITY: u32 = ctl_code(FILE_DEVICE_TMR, 0x812, METHOD_BUFFERED, FILE_ANY_ACCESS);
pub const IOCTL_TMR_GET_HARDWARE_INFO: u32 = ctl_code(FILE_DEVICE_TMR, 0x813, METHOD_BUFFERED, FILE_ANY_ACCESS);
pub const IOCTL_TMR_RESET_ALL: u32 = ctl_code(FILE_DEVICE_TMR, 0x814, METHOD_BUFFERED, FILE_ANY_ACCESS);

/// Input structure for allocation requests matching driver
#[repr(C)]
#[derive(Copy, Clone)]
struct AllocateDmaInput {
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
struct AllocateDmaOutput {
    pub user_address: u64,
    pub physical_address: u64,
    pub size: usize,
    pub allocation_id: u64,
    pub numa_node: u32,
    pub page_size_kb: u32,
}

/// Input structure for free requests
#[repr(C)]
#[derive(Copy, Clone)]
struct FreeDmaInput {
    pub user_address: u64,
}

/// Memory statistics structure
#[repr(C)]
#[derive(Copy, Clone, Default)]
pub struct MemoryStats {
    pub total_physical_memory: u64,
    pub available_physical_memory: u64,
    pub total_large_pages: u64,
    pub available_large_pages: u64,
    pub total_huge_pages: u64,
    pub available_huge_pages: u64,
    pub numa_node_count: u32,
}

/// CPU affinity setting structure
#[repr(C)]
#[derive(Copy, Clone)]
pub struct SetCpuAffinityInput {
    pub cpu_number: u32,
}

/// Hardware info output structure
#[repr(C)]
#[derive(Copy, Clone, Default)]
pub struct HardwareInfoOutput {
    pub numa_node_count: u32,
    pub cpu_count: u32,
    pub l1_cache_size: u32,
    pub l2_cache_size: u32,
    pub l3_cache_size: u32,
    pub memory_channels: u32,
    pub memory_speed_mhz: u32,
}

/// Runtime statistics structure
#[repr(C)]
#[derive(Copy, Clone, Default)]
pub struct RuntimeStatistics {
    pub uptime_seconds: u64,
    pub total_allocations: u64,
    pub successful_allocations: u64,
    pub failed_allocations: u64,
    pub total_bytes_allocated: u64,
    pub current_bytes_allocated: u64,
    pub peak_bytes_allocated: u64,
    pub huge_pages_allocated: u64,
    pub large_pages_allocated: u64,
    pub regular_pages_allocated: u64,
    pub numa_local_allocations: u64,
    pub numa_remote_allocations: u64,
    pub zero_free_allocations: u64,
    pub average_allocation_time_us: u64,
}

/// Batch allocation request for a single thread
#[repr(C)]
#[derive(Copy, Clone)]
pub struct ThreadAllocationRequest {
    pub thread_id: u32,
    pub cpu_id: u32,
    pub size_bytes: usize,
    pub block_count: u32,
    pub minimum_page_size: PageSize,
    pub maximum_page_size: PageSize,
    pub memory_type: MemoryType,
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
            strict_numa: false,
            zero_memory: false,
            contiguous: true,
            timeout_ms: 10000,
            retry_interval_ms: 10,
            max_retries: 100,
        }
    }
}

/// Result for a single allocation in the batch
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
    pub numa_node: u32,
    pub allocation_time_us: u32,
}

/// Batch allocation input structure
#[repr(C)]
pub struct BatchAllocateInput {
    pub request_count: u32,
    pub abort_on_failure: bool,
    pub requests: [ThreadAllocationRequest; 32],
}

impl Default for BatchAllocateInput {
    fn default() -> Self {
        Self {
            request_count: 0,
            abort_on_failure: false,
            requests: [ThreadAllocationRequest::default(); 32],
        }
    }
}

/// Batch allocation output structure
#[repr(C)]
pub struct BatchAllocateOutput {
    pub total_allocations: u32,
    pub successful_allocations: u32,
    pub failed_allocations: u32,
    pub total_time_us: u32,
    pub results: [AllocationResult; 128],
}

/// Driver statistics for reporting
pub struct DriverStatistics {
    pub uptime_seconds: u64,
    pub total_allocations: u64,
    pub successful_allocations: u64,
    pub failed_allocations: u64,
    pub total_memory_allocated_gb: f64,
    pub current_memory_allocated_gb: f64,
    pub peak_memory_allocated_gb: f64,
    pub huge_pages_count: u64,
    pub large_pages_count: u64,
    pub zero_free_allocations: u64,
    pub average_allocation_time_us: u64,
    pub numa_efficiency: f64,
}

pub struct DriverHandle {
    handle: HANDLE,
}


unsafe impl Send for DriverHandle {}
unsafe impl Sync for DriverHandle {}
impl DriverHandle {
    pub fn open() -> Result<Arc<Self>, String> {
        unsafe {
            let device_path = PCSTR::from_raw(b"\\\\.\\TmrMemory\0".as_ptr());
            
			let handle = CreateFileA(
				device_path,
				0x80000000 | 0x40000000, // GENERIC_READ | GENERIC_WRITE
				windows::Win32::Storage::FileSystem::FILE_SHARE_READ | 
				windows::Win32::Storage::FileSystem::FILE_SHARE_WRITE,
				None,
				OPEN_EXISTING,
				windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL,
				None,
			)
			.map_err(|e| format!("Failed to open TMR driver device: {:?}", e))?;

            if handle == INVALID_HANDLE_VALUE {
                return Err("Failed to open TMR driver device".to_string());
            }

            Ok(Arc::new(Self { handle }))
        }
    }

	pub fn reset_all(&self) -> Result<(), String> {
		track_driver_call!(DriverStatType::ResetAll);
		
		unsafe {
			let mut bytes_returned = 0u32;
			
			let result = DeviceIoControl(
				self.handle,
				IOCTL_TMR_RESET_ALL,
				None,
				0,
				None,
				0,
				Some(&mut bytes_returned),
				None,
			);
			
			if result.is_err() {
				Err("Failed to reset driver allocations".to_string())
			} else {
				Ok(())
			}
		}
	}

    /// Check driver version compatibility
    pub fn check_version_compatibility(&self) -> Result<DriverVersionInfo, DriverVersionError> {
		track_driver_call!(DriverStatType::GetVersion);
        
        let mut version_info = DriverVersionInfo {
            driver_version_major: 0,
            driver_version_minor: 0,
            driver_version_build: 0,
            driver_version_revision: 0,
            min_app_version_major: 0,
            min_app_version_minor: 0,
            max_app_version_major: 0,
            max_app_version_minor: 0,
            compatibility_flags: 0,
            driver_build_date: [0; 32],
            driver_build_time: [0; 32],
        };

        unsafe {
            let mut bytes_returned = 0u32;
            
            let result = DeviceIoControl(
                self.handle,
                IOCTL_TMR_GET_VERSION,
                None,
                0,
                Some(&mut version_info as *mut _ as *mut std::ffi::c_void),
                std::mem::size_of::<DriverVersionInfo>() as u32,
                Some(&mut bytes_returned),
                None,
            );

            if result.is_err() {
                return Err(DriverVersionError::QueryFailed("DeviceIoControl failed".to_string()));
            }
        }

        // Check version compatibility
        let driver_version = format!("{}.{}.{}.{}", 
            version_info.driver_version_major,
            version_info.driver_version_minor,
            version_info.driver_version_build,
            version_info.driver_version_revision
        );
        
        let app_version = format!("{}.{}.{}", APP_VERSION_MAJOR, APP_VERSION_MINOR, APP_VERSION_BUILD);
        
        // Check if app version is within driver's supported range
        if APP_VERSION_MAJOR < version_info.min_app_version_major ||
           (APP_VERSION_MAJOR == version_info.min_app_version_major && 
            APP_VERSION_MINOR < version_info.min_app_version_minor) {
            return Err(DriverVersionError::VersionMismatch {
                driver_version,
                app_version,
                min_required: format!("{}.{}", version_info.min_app_version_major, version_info.min_app_version_minor),
                max_supported: format!("{}.{}", version_info.max_app_version_major, version_info.max_app_version_minor),
            });
        }
        
        if APP_VERSION_MAJOR > version_info.max_app_version_major ||
           (APP_VERSION_MAJOR == version_info.max_app_version_major && 
            APP_VERSION_MINOR > version_info.max_app_version_minor) {
            return Err(DriverVersionError::VersionMismatch {
                driver_version,
                app_version,
                min_required: format!("{}.{}", version_info.min_app_version_major, version_info.min_app_version_minor),
                max_supported: format!("{}.{}", version_info.max_app_version_major, version_info.max_app_version_minor),
            });
        }
        Ok(version_info)
    }
	

    /// Open with version check
	pub fn open_with_version_check() -> Result<Arc<Self>, String> {
		let driver_handle = Self::open()?;
		
		// Check version compatibility but don't log here
		match driver_handle.check_version_compatibility() {
			Ok(_) => Ok(driver_handle),
			Err(e) => Err(format!("Driver version incompatible: {}", e))
		}
	}
	
    pub fn remap_all_memory_type(&self, new_memory_type: MemoryType) -> Result<u32, String> {
		track_driver_call!(DriverStatType::RemapAllMemoryType);
        
        let input = RemapAllInput {
            new_memory_type: new_memory_type as u32,
        };
        
        let mut output = RemapAllOutput {
            success: false,
            allocations_remapped: 0,
            allocations_failed: 0,
            total_time_us: 0,
        };
        
        unsafe {
            let mut bytes_returned = 0u32;
            
            let result = DeviceIoControl(
                self.handle,
                IOCTL_TMR_REMAP_ALL_MEMORY_TYPE,
                Some(&input as *const _ as *const std::ffi::c_void),
                std::mem::size_of::<RemapAllInput>() as u32,
                Some(&mut output as *mut _ as *mut std::ffi::c_void),
                std::mem::size_of::<RemapAllOutput>() as u32,
                Some(&mut bytes_returned),
                None,
            );
            
            if result.is_err() {
                return Err("Failed to execute remap all IOCTL".to_string());
            }
            
            if !output.success {
                return Err(format!(
                    "Driver failed to remap all allocations: {} succeeded, {} failed", 
                    output.allocations_remapped, 
                    output.allocations_failed
                ));
            }
            
            log::debug!("Driver remapped {} allocations to {:?} in {}μs", 
                      output.allocations_remapped, new_memory_type, output.total_time_us);
            
            Ok(output.allocations_remapped)
        }
    }
	
}

impl Drop for DriverHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

/// Information about allocated segments
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct SegmentInfo {
    pub virtual_address: u64,
    pub physical_address: u64,
    pub size: usize,
    pub page_size_kb: u32,
    pub numa_node: u32,
}

/// DMA buffer configuration
#[derive(Clone, Debug)]
pub struct DmaConfig {
    pub minimum_page_size: PageSize,
    pub maximum_page_size: PageSize,
    pub prefer_numa_node: Option<u32>,
    pub zero_memory: bool,
    pub memory_type: MemoryType,
    pub contiguous: bool,
    pub timeout_ms: u32,
    pub retry_interval_ms: u32,
    pub max_retries: u32,
	pub strict_numa: bool,
}

impl Default for DmaConfig {
    fn default() -> Self {
        Self {
            minimum_page_size: PageSize::Regular,
            maximum_page_size: PageSize::Huge,
            prefer_numa_node: None,
            zero_memory: true,
            memory_type: MemoryType::WriteBack,
            contiguous: true,
            timeout_ms: 10000,
            retry_interval_ms: 10,
            max_retries: 100,
			strict_numa: false,
        }
    }
}

impl DmaConfig {
    pub fn for_testing() -> Self {
        Self {
            minimum_page_size: PageSize::Regular,
            maximum_page_size: PageSize::Huge,
            prefer_numa_node: None,
            zero_memory: false,
            memory_type: MemoryType::WriteBack,
            contiguous: true,
            timeout_ms: 10000,
            retry_interval_ms: 10,
            max_retries: 100,
            strict_numa: false,
        }
    }
    
    pub fn for_bandwidth_test() -> Self {
        Self {
            minimum_page_size: PageSize::Regular,
            maximum_page_size: PageSize::Huge,
            prefer_numa_node: None,
            zero_memory: false,
            memory_type: MemoryType::WriteCombining,
            contiguous: true,
            timeout_ms: 10000,
            retry_interval_ms: 10,
            max_retries: 100,
            strict_numa: false,
        }
    }
    
    pub fn for_latency_test() -> Self {
        Self {
            minimum_page_size: PageSize::Regular,
            maximum_page_size: PageSize::Regular, // Force regular pages only
            prefer_numa_node: None,
            zero_memory: false,
            memory_type: MemoryType::Uncached,
            contiguous: true,
            timeout_ms: 10000,
            retry_interval_ms: 10,
            max_retries: 100,
            strict_numa: false,
        }
    }
}

/// Batch allocation for TMR multi-threaded setup
pub struct BatchAllocationRequest {
    pub thread_configs: Vec<ThreadConfig>,
}

#[derive(Clone)]
pub struct ThreadConfig {
    pub thread_id: u32,
    pub cpu_id: u32,
    pub size_bytes: usize,
    pub config: DmaConfig,
}

/// Basic DMA buffer
pub struct DmaBuffer {
    driver_handle: Arc<DriverHandle>,
    ptr: *mut u8,
    size: usize,
    physical_address: u64,
}

impl DmaBuffer {
    pub fn new(size_bytes: usize, _use_large_pages: bool) -> Result<Self, String> {
        let config = DmaConfig::for_testing();
        Self::new_with_config(size_bytes, config)
    }

    pub fn new_with_config(size_bytes: usize, config: DmaConfig) -> Result<Self, String> {
        let driver = get_global_driver_handle()?;
        
		track_driver_call!(DriverStatType::AllocateDma);
        
		let input = AllocateDmaInput {
			size: size_bytes,
			numa_node: config.prefer_numa_node.unwrap_or(0xFFFFFFFF),
			memory_type: config.memory_type as u32,
			minimum_page_size: config.minimum_page_size as u32,
			maximum_page_size: config.maximum_page_size as u32,
			strict_numa: config.strict_numa,
			zero_memory: config.zero_memory,
			contiguous: config.contiguous,
			timeout_ms: config.timeout_ms,
			retry_interval_ms: config.retry_interval_ms,
			max_retries: config.max_retries,
		};

        let mut output = AllocateDmaOutput {
            user_address: 0,
            physical_address: 0,
            size: 0,
            allocation_id: 0,
            numa_node: 0,
            page_size_kb: 0,
        };

        unsafe {
            let mut bytes_returned = 0u32;
            
            let result = DeviceIoControl(
                driver.handle,
                IOCTL_TMR_ALLOCATE_DMA,
                Some(&input as *const _ as *const std::ffi::c_void),
                std::mem::size_of::<AllocateDmaInput>() as u32,
                Some(&mut output as *mut _ as *mut std::ffi::c_void),
                std::mem::size_of::<AllocateDmaOutput>() as u32,
                Some(&mut bytes_returned),
                None,
            );

            if result.is_err() {
                return Err("Failed to allocate DMA memory".to_string());
            }

            if output.user_address == 0 {
                return Err("Driver returned null address".to_string());
            }

            Ok(Self {
                driver_handle: driver,
                ptr: output.user_address as *mut u8,
                size: output.size,
                physical_address: output.physical_address,
            })
        }
    }

    pub fn as_mut_ptr(&self) -> *mut u8 {
        self.ptr
    }

    pub fn size(&self) -> usize {
        self.size
    }

    pub fn physical_address(&self) -> u64 {
        self.physical_address
    }

    pub fn is_driver_available() -> bool {
        get_global_driver_handle().is_ok()
    }
    
    pub fn is_driver_available_and_compatible() -> bool {
        get_global_driver_handle().is_ok()
    }

    pub fn get_memory_stats() -> Result<MemoryStats, String> {
        let driver = get_global_driver_handle()?;
        let mut stats = MemoryStats::default();
        
		track_driver_call!(DriverStatType::GetMemoryStats);
        
        unsafe {
            let mut bytes_returned = 0u32;
            
            let result = DeviceIoControl(
                driver.handle,
                IOCTL_TMR_GET_MEMORY_STATS,
                None,
                0,
                Some(&mut stats as *mut _ as *mut std::ffi::c_void),
                std::mem::size_of::<MemoryStats>() as u32,
                Some(&mut bytes_returned),
                None,
            );
            
            if result.is_err() {
                return Err("Failed to get memory stats".to_string());
            }
        }
        
        Ok(stats)
    }

    pub fn get_driver_statistics() -> Result<DriverStatistics, String> {
        let driver = get_global_driver_handle()?;
        
        let mut stats = RuntimeStatistics::default();
        
        track_driver_call!(DriverStatType::GetStatistics);
        
        unsafe {
            let mut bytes_returned = 0u32;
            
            let result = DeviceIoControl(
                driver.handle,
                IOCTL_TMR_GET_STATISTICS,
                None,
                0,
                Some(&mut stats as *mut _ as *mut std::ffi::c_void),
                std::mem::size_of::<RuntimeStatistics>() as u32,
                Some(&mut bytes_returned),
                None,
            );
            
            if result.is_err() {
                return Err("Failed to get driver statistics".to_string());
            }
        }
        
        Ok(DriverStatistics {
            uptime_seconds: stats.uptime_seconds,
            total_allocations: stats.total_allocations,
            successful_allocations: stats.successful_allocations,
            failed_allocations: stats.failed_allocations,
            total_memory_allocated_gb: stats.total_bytes_allocated as f64 / (1024.0 * 1024.0 * 1024.0),
            current_memory_allocated_gb: stats.current_bytes_allocated as f64 / (1024.0 * 1024.0 * 1024.0),
            peak_memory_allocated_gb: stats.peak_bytes_allocated as f64 / (1024.0 * 1024.0 * 1024.0),
            huge_pages_count: stats.huge_pages_allocated,
            large_pages_count: stats.large_pages_allocated,
            zero_free_allocations: stats.zero_free_allocations,
            average_allocation_time_us: stats.average_allocation_time_us,
            numa_efficiency: if stats.numa_local_allocations + stats.numa_remote_allocations > 0 {
                (stats.numa_local_allocations as f64 / 
                 (stats.numa_local_allocations + stats.numa_remote_allocations) as f64) * 100.0
            } else {
                0.0
            },
        })
    }
    
    pub fn batch_allocate(requests: &BatchAllocationRequest) -> Result<Vec<(u32, Vec<DmaBuffer>)>, String> {
        let driver = get_global_driver_handle()?;
        
        track_driver_call!(DriverStatType::BatchAllocate);
        
        // Prepare batch request
        let mut batch_input = BatchAllocateInput::default();
        batch_input.request_count = requests.thread_configs.len().min(32) as u32;
        
        for (i, thread_config) in requests.thread_configs.iter().enumerate() {
            if i >= 32 { break; }
            
			batch_input.requests[i] = ThreadAllocationRequest {
				thread_id: thread_config.thread_id,
				cpu_id: thread_config.cpu_id,
				size_bytes: thread_config.size_bytes,
				block_count: 1,
				minimum_page_size: thread_config.config.minimum_page_size,
				maximum_page_size: thread_config.config.maximum_page_size,
				memory_type: thread_config.config.memory_type,
				strict_numa: thread_config.config.strict_numa,
				zero_memory: thread_config.config.zero_memory,
				contiguous: thread_config.config.contiguous,
				timeout_ms: thread_config.config.timeout_ms,
				retry_interval_ms: thread_config.config.retry_interval_ms,
				max_retries: thread_config.config.max_retries,
			};
        }
        
        let mut batch_output = BatchAllocateOutput {
            total_allocations: 0,
            successful_allocations: 0,
            failed_allocations: 0,
            total_time_us: 0,
            results: [AllocationResult::default(); 128],
        };
        
        unsafe {
            let mut bytes_returned = 0u32;
            
            let result = DeviceIoControl(
                driver.handle,
                IOCTL_TMR_BATCH_ALLOCATE,
                Some(&batch_input as *const _ as *const std::ffi::c_void),
                std::mem::size_of::<BatchAllocateInput>() as u32,
                Some(&mut batch_output as *mut _ as *mut std::ffi::c_void),
                std::mem::size_of::<BatchAllocateOutput>() as u32,
                Some(&mut bytes_returned),
                None,
            );
            
            if result.is_err() {
                return Err("Batch allocation failed".to_string());
            }
        }
        
        // Log results
        log::info!("Batch allocation completed in {}μs: {}/{} successful",
                  batch_output.total_time_us,
                  batch_output.successful_allocations,
                  batch_output.total_allocations);
        
        // Group results by thread
        let mut thread_buffers: Vec<(u32, Vec<DmaBuffer>)> = Vec::new();
        
        for i in 0..batch_output.total_allocations as usize {
            let result = &batch_output.results[i];
            if result.success {
                let buffer = DmaBuffer {
                    driver_handle: Arc::clone(&driver),
                    ptr: result.virtual_address as *mut u8,
                    size: result.size,
                    physical_address: result.physical_address,
                };
                
                // Find or create thread entry
                let thread_entry = thread_buffers.iter_mut()
                    .find(|(id, _)| *id == result.thread_id);
                
                if let Some((_, buffers)) = thread_entry {
                    buffers.push(buffer);
                } else {
                    thread_buffers.push((result.thread_id, vec![buffer]));
                }
            }
        }
        
        Ok(thread_buffers)
    }

    /// Batch remap multiple allocations to a new memory type
    pub fn batch_remap_memory_type(
        driver: &Arc<DriverHandle>,
        remaps: &[(u64, MemoryType)],  // (user_address, new_type) pairs
    ) -> Result<Vec<RemapResult>, String> {
        if remaps.is_empty() {
            return Ok(Vec::new());
        }
        
        track_driver_call!(DriverStatType::BatchRemapMemoryType);
        
        // Group by memory type for efficiency
        let mut by_type: std::collections::HashMap<MemoryType, Vec<u64>> = std::collections::HashMap::new();
        for (addr, mem_type) in remaps {
            by_type.entry(*mem_type).or_default().push(*addr);
        }
        
        let mut all_results = Vec::new();
        
        // Process each memory type group
        for (mem_type, addresses) in by_type {
            // Process in chunks of 128 (max per IOCTL)
            for chunk in addresses.chunks(128) {
                let mut input = BatchRemapInput::default();
                input.request_count = chunk.len() as u32;
                input.new_memory_type = mem_type as u32;
                
                for (i, addr) in chunk.iter().enumerate() {
                    input.requests[i] = RemapRequest {
                        user_address: *addr,
                        allocation_id: 0,  // If you track IDs
                    };
                }
                
                let mut output = BatchRemapOutput::default();
                
                unsafe {
                    let mut bytes_returned = 0u32;
                    
                    let result = DeviceIoControl(
                        driver.handle,
                        IOCTL_TMR_BATCH_REMAP_MEMORY_TYPE,
                        Some(&input as *const _ as *const std::ffi::c_void),
                        std::mem::size_of::<BatchRemapInput>() as u32,
                        Some(&mut output as *mut _ as *mut std::ffi::c_void),
                        std::mem::size_of::<BatchRemapOutput>() as u32,
                        Some(&mut bytes_returned),
                        None,
                    );
                    
                    if result.is_err() {
                        return Err("Batch remap IOCTL failed".to_string());
                    }
                }
                
                // Collect results
                for i in 0..output.success_count + output.failure_count {
                    all_results.push(output.results[i as usize]);
                }
                
                if output.failure_count > 0 {
                    log::warn!("Batch remap: {} succeeded, {} failed", 
                             output.success_count, output.failure_count);
                }
            }
        }
        
        Ok(all_results)
    }
}

impl Drop for DmaBuffer {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            track_driver_call!(DriverStatType::FreeDma);
            
            let input = FreeDmaInput {
                user_address: self.ptr as u64,
            };

            unsafe {
                let mut bytes_returned = 0u32;
                let _ = DeviceIoControl(
                    self.driver_handle.handle,
                    IOCTL_TMR_FREE_DMA,
                    Some(&input as *const _ as *const std::ffi::c_void),
                    std::mem::size_of::<FreeDmaInput>() as u32,
                    None,
                    0,
                    Some(&mut bytes_returned),
                    None,
                );
            }
        }
    }
}

// Safety implementations
unsafe impl Send for DmaBuffer {}
unsafe impl Sync for DmaBuffer {}

/// Enhanced DMA buffer with segment tracking
pub struct DmaBufferEnhanced {
    driver_handle: Arc<DriverHandle>,
    ptr: *mut u8,
    size: usize,
    segments: Vec<SegmentInfo>,
    config: DmaConfig,
}

impl DmaBufferEnhanced {
    /// Create with specific configuration
    pub fn new_with_config(size_bytes: usize, config: DmaConfig) -> Result<Self, String> {
        let driver = get_global_driver_handle()?;
        
        track_driver_call!(DriverStatType::AllocateDma);
        
        // Log the allocation request
        log::info!("DMA allocation request: {} bytes, min_page_size: {:?}, max_page_size: {:?}, NUMA: {:?}",
                  size_bytes, config.minimum_page_size, config.maximum_page_size, config.prefer_numa_node);
        
        let input = AllocateDmaInput {
            size: size_bytes,
            numa_node: config.prefer_numa_node.unwrap_or(0xFFFFFFFF),
            memory_type: config.memory_type as u32,
            minimum_page_size: config.minimum_page_size as u32,
            maximum_page_size: config.maximum_page_size as u32,
            strict_numa: config.strict_numa,
            zero_memory: config.zero_memory,
            contiguous: config.contiguous,
            timeout_ms: config.timeout_ms,
            retry_interval_ms: config.retry_interval_ms,
            max_retries: config.max_retries,
        };

        let mut output = AllocateDmaOutput {
            user_address: 0,
            physical_address: 0,
            size: 0,
            allocation_id: 0,
            numa_node: 0,
            page_size_kb: 0,
        };

        unsafe {
            let mut bytes_returned = 0u32;
            
            let result = DeviceIoControl(
                driver.handle,
                IOCTL_TMR_ALLOCATE_DMA,
                Some(&input as *const _ as *const std::ffi::c_void),
                std::mem::size_of::<AllocateDmaInput>() as u32,
                Some(&mut output as *mut _ as *mut std::ffi::c_void),
                std::mem::size_of::<AllocateDmaOutput>() as u32,
                Some(&mut bytes_returned),
                None,
            );

            if result.is_err() {
                return Err("Failed to allocate DMA memory".to_string());
            }

            if output.user_address == 0 {
                return Err("Driver returned null address".to_string());
            }

            // Create segment info
            let segments = vec![SegmentInfo {
                virtual_address: output.user_address,
                physical_address: output.physical_address,
                size: output.size,
                page_size_kb: output.page_size_kb,
                numa_node: output.numa_node,
            }];

            // Log allocation result
            Self::log_allocation_result(&segments);

            Ok(Self {
                driver_handle: driver,
                ptr: output.user_address as *mut u8,
                size: output.size,
                segments,
                config,
            })
        }
    }

    /// Create optimal allocation for given size
	pub fn new_optimal(size_bytes: usize) -> Result<Self, String> {
		let config = if size_bytes >= 1024 * 1024 * 1024 {
			// For allocations >= 1GB, allow any page size up to huge
			DmaConfig {
				minimum_page_size: PageSize::Regular,
				maximum_page_size: PageSize::Huge,
				prefer_numa_node: None,
				zero_memory: false,
				memory_type: MemoryType::WriteBack,
				contiguous: true,
				timeout_ms: 10000,
				retry_interval_ms: 10,
				max_retries: 100,
				strict_numa: false,
			}
		} else if size_bytes >= 32 * 1024 * 1024 {
			// For allocations >= 32MB, allow up to large pages
			DmaConfig {
				minimum_page_size: PageSize::Regular,
				maximum_page_size: PageSize::Large,
				prefer_numa_node: None,
				zero_memory: false,
				memory_type: MemoryType::WriteBack,
				contiguous: true,
				timeout_ms: 10000,
				retry_interval_ms: 10,
				max_retries: 100,
				strict_numa: false,
			}
		} else {
			// Small allocations use regular pages only
			DmaConfig {
				minimum_page_size: PageSize::Regular,
				maximum_page_size: PageSize::Regular,
				prefer_numa_node: None,
				zero_memory: false,
				memory_type: MemoryType::WriteBack,
				contiguous: true,
				timeout_ms: 10000,
				retry_interval_ms: 10,
				max_retries: 100,
				strict_numa: false,
			}
		};

		Self::new_with_config(size_bytes, config)
	}

    /// Log allocation result
    fn log_allocation_result(segments: &[SegmentInfo]) {
        let huge_pages: Vec<_> = segments.iter()
            .filter(|s| s.page_size_kb == 1048576)
            .collect();
        let large_pages: Vec<_> = segments.iter()
            .filter(|s| s.page_size_kb == 2048)
            .collect();
        let regular_pages: Vec<_> = segments.iter()
            .filter(|s| s.page_size_kb == 4)
            .collect();

        log::info!("DMA allocation successful:");
        
        if !huge_pages.is_empty() {
            log::info!("  {} x 1GB huge pages", huge_pages.len());
            for (i, seg) in huge_pages.iter().enumerate() {
                log::info!("    Huge page {}: PA {:#X}, NUMA node {}", 
                         i, seg.physical_address, seg.numa_node);
            }
        }
        
        if !large_pages.is_empty() {
            log::info!("  {} x 2MB large pages", large_pages.len());
            if large_pages.len() <= 4 {
                for (i, seg) in large_pages.iter().enumerate() {
                    log::info!("    Large page {}: PA {:#X}, NUMA node {}", 
                             i, seg.physical_address, seg.numa_node);
                }
            }
        }
        
        if !regular_pages.is_empty() {
            let total_regular_size: usize = regular_pages.iter().map(|s| s.size).sum();
            log::info!("  Regular pages: {} KB total", total_regular_size / 1024);
        }
    }

    pub fn as_mut_ptr(&self) -> *mut u8 {
        self.ptr
    }

    pub fn size(&self) -> usize {
        self.size
    }

    /// Get segment information
    pub fn segments(&self) -> &[SegmentInfo] {
        &self.segments
    }

    /// Check if allocation used huge pages
    pub fn has_huge_pages(&self) -> bool {
        self.segments.iter().any(|s| s.page_size_kb == 1048576)
    }

    /// Check if allocation used large pages
    pub fn has_large_pages(&self) -> bool {
        self.segments.iter().any(|s| s.page_size_kb == 2048)
    }

    /// Get NUMA node distribution
    pub fn numa_distribution(&self) -> std::collections::HashMap<u32, usize> {
        let mut distribution = std::collections::HashMap::new();
        for segment in &self.segments {
            *distribution.entry(segment.numa_node).or_insert(0) += segment.size;
        }
        distribution
    }
    
    pub fn get_memory_stats_enhanced() -> Result<MemoryStatsEnhanced, String> {
        // For now, return a mock implementation
        Ok(MemoryStatsEnhanced {
            total_physical_bytes: 16 * 1024 * 1024 * 1024,
            available_physical_bytes: 8 * 1024 * 1024 * 1024,
            huge_pages_supported: true,
            numa_nodes: 1,
            node_stats: vec![NodeMemoryStats {
                node_id: 0,
                total_bytes: 16 * 1024 * 1024 * 1024,
                available_bytes: 8 * 1024 * 1024 * 1024,
            }],
        })
    }
}

impl Drop for DmaBufferEnhanced {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            track_driver_call!(DriverStatType::FreeDma);
            
            let input = FreeDmaInput {
                user_address: self.ptr as u64,
            };

            unsafe {
                let mut bytes_returned = 0u32;
                let _ = DeviceIoControl(
                    self.driver_handle.handle,
                    IOCTL_TMR_FREE_DMA,
                    Some(&input as *const _ as *const std::ffi::c_void),
                    std::mem::size_of::<FreeDmaInput>() as u32,
                    None,
                    0,
                    Some(&mut bytes_returned),
                    None,
                );
            }
        }
    }
}

// Safety implementations
unsafe impl Send for DmaBufferEnhanced {}
unsafe impl Sync for DmaBufferEnhanced {}

// Additional structures for enhanced stats
pub struct MemoryStatsEnhanced {
    pub total_physical_bytes: u64,
    pub available_physical_bytes: u64,
    pub huge_pages_supported: bool,
    pub numa_nodes: u32,
    pub node_stats: Vec<NodeMemoryStats>,
}

pub struct NodeMemoryStats {
    pub node_id: u32,
    pub total_bytes: u64,
    pub available_bytes: u64,
}

// Enhanced test buffer enum
use crate::TestBuffer;

pub enum EnhancedTestBuffer {
    Regular(TestBuffer),
    Dma(DmaBuffer),
    DmaEnhanced(DmaBufferEnhanced),
}

impl EnhancedTestBuffer {
    pub fn new_auto(size_bytes: usize, prefer_dma: bool) -> Option<Self> {
        if prefer_dma && DmaBuffer::is_driver_available_and_compatible() {
            // Try DMA allocation
            if let Ok(dma_buffer) = DmaBuffer::new(size_bytes, true) {
                return Some(Self::Dma(dma_buffer));
            }
        }
        
        // Fall back to regular allocation
        TestBuffer::new_large_pages(size_bytes)
            .or_else(|| TestBuffer::new_aligned(size_bytes))
            .map(Self::Regular)
    }
    
    pub fn new_auto_optimal(size_bytes: usize) -> Option<Self> {
        // Try enhanced DMA allocation with optimal settings
        if DmaBuffer::is_driver_available() {
            log::info!("TMR kernel driver detected - attempting optimal DMA allocation");
            
            match DmaBufferEnhanced::new_optimal(size_bytes) {
                Ok(dma_buffer) => {
                    log::info!("Enhanced DMA allocation successful");
                    return Some(Self::DmaEnhanced(dma_buffer));
                }
                Err(e) => {
                    log::warn!("Enhanced DMA allocation failed: {} - trying basic DMA", e);
                    
                    // Fall back to basic DMA
                    if let Ok(basic_dma) = DmaBuffer::new(size_bytes, true) {
                        return Some(Self::Dma(basic_dma));
                    }
                }
            }
        }

        // Fall back to regular allocation
        TestBuffer::new_large_pages(size_bytes)
            .or_else(|| TestBuffer::new_aligned(size_bytes))
            .map(Self::Regular)
    }
    
    pub fn as_mut_ptr(&self) -> *mut u8 {
        match self {
            Self::Regular(buffer) => buffer.as_mut_ptr(),
            Self::Dma(buffer) => buffer.as_mut_ptr(),
            Self::DmaEnhanced(buffer) => buffer.as_mut_ptr(),
        }
    }
    
    pub fn size(&self) -> usize {
        match self {
            Self::Regular(buffer) => buffer.size(),
            Self::Dma(buffer) => buffer.size(),
            Self::DmaEnhanced(buffer) => buffer.size(),
        }
    }
    
    pub fn uses_large_pages(&self) -> bool {
        match self {
            Self::Regular(buffer) => buffer.uses_large_pages(),
            Self::Dma(_) => true, // DMA may use large pages
            Self::DmaEnhanced(buffer) => buffer.has_large_pages() || buffer.has_huge_pages(),
        }
    }
    
    pub fn is_dma(&self) -> bool {
        matches!(self, Self::Dma(_) | Self::DmaEnhanced(_))
    }
    
    pub fn physical_address(&self) -> Option<u64> {
        match self {
            Self::Regular(_) => None,
            Self::Dma(buffer) => Some(buffer.physical_address()),
            Self::DmaEnhanced(buffer) => buffer.segments().first().map(|s| s.physical_address),
        }
    }

    /// Get the current virtual address for remapping
    pub fn get_virtual_address(&self) -> Option<u64> {
        match self {
            Self::Regular(_) => None,  // Can't remap regular allocations
            Self::Dma(buffer) => Some(buffer.ptr as u64),
            Self::DmaEnhanced(buffer) => Some(buffer.ptr as u64),
        }
    }
    
    /// Update address after remap (if it changed)
    pub fn update_address_after_remap(&mut self, new_address: u64) {
        match self {
            Self::Regular(_) => {},  // Nothing to do
            Self::Dma(buffer) => buffer.ptr = new_address as *mut u8,
            Self::DmaEnhanced(buffer) => buffer.ptr = new_address as *mut u8,
        }
    }	
	
}

/// Reset all driver allocations
pub fn reset_driver() {
	if let Ok(driver) = DriverHandle::open() {
		if let Err(e) = driver.reset_all() {
			log::warn!("Failed to reset driver allocations: {}", e);
		} else {
			log::info!("TMR Driver: Reset all allocations");
		}
	}
}

pub fn remap_all_allocations_to_type(
    allocated_blocks: &mut HashMap<usize, Vec<AllocatedBlock>>,
    new_memory_type: MemoryType,
    driver: &Arc<DriverHandle>,
) -> Result<(), String> {
    if get_use_remap_all() {
        // Optimized approach: Just tell the driver to remap everything
        let start = std::time::Instant::now();
        
        match driver.remap_all_memory_type(new_memory_type) {
            Ok(count) => {
                // Update all our local block tracking to reflect the new memory type
                for (_, blocks) in allocated_blocks.iter_mut() {
                    for block in blocks.iter_mut() {
                        block.memory_type = new_memory_type;
                        // Note: Virtual addresses don't change with memory type remap
                        // The physical mapping changes but virtual addresses remain stable
                    }
                }
                
                let elapsed = start.elapsed();
                log::info!("Remapped {} allocations to {:?} in {:?} (remap all mode)", 
                          count, new_memory_type, elapsed);
                Ok(())
            }
            Err(e) => {
                log::error!("Failed to remap all memory types: {}", e);
                Err(e)
            }
        }
    } else {
        // Original batch approach - check each block and build remap list
        let mut remap_requests = Vec::new();
        let mut address_to_buffer: HashMap<u64, (*mut AllocatedBlock, usize, usize)> = HashMap::new();
        
        // Collect all addresses that need remapping
        for (thread_id, blocks) in allocated_blocks.iter_mut() {
            for (block_idx, block) in blocks.iter_mut().enumerate() {
                // Only remap if the memory type is different
                if block.memory_type != new_memory_type {
                    if let Some(addr) = block.buffer.get_virtual_address() {
                        remap_requests.push((addr, new_memory_type));
                        address_to_buffer.insert(addr, (block as *mut _, *thread_id, block_idx));
                    }
                }
            }
        }
        
        if remap_requests.is_empty() {
            log::debug!("No blocks need remapping - all already at {:?}", new_memory_type);
            return Ok(()); // Nothing to remap
        }
        
        log::info!("Remapping {} allocations to {:?} (batch mode)", remap_requests.len(), new_memory_type);
        let start = std::time::Instant::now();
        
        // Execute batch remap
        let results = DmaBuffer::batch_remap_memory_type(driver, &remap_requests)?;
        
        // Update buffers with results
        let mut success_count = 0;
        let mut failure_count = 0;
        
        for result in results {
            if result.success {
                if let Some((block_ptr, _, _)) = address_to_buffer.get(&result.user_address) {
                    unsafe {
                        let block = &mut **block_ptr;
                        // Update address if it changed (shouldn't happen for memory type remap)
                        if result.new_address != result.user_address {
                            block.buffer.update_address_after_remap(result.new_address);
                            log::warn!("Address changed during remap: {:#x} -> {:#x}", 
                                     result.user_address, result.new_address);
                        }
                        // Update the memory type
                        block.memory_type = new_memory_type;
                        success_count += 1;
                    }
                }
            } else {
                failure_count += 1;
                log::error!("Failed to remap address {:#x}, error code: {}", 
                           result.user_address, result.error_code);
            }
        }
        
        let elapsed = start.elapsed();
        
        if failure_count > 0 {
            return Err(format!("Batch remap completed with {} successes and {} failures in {:?}", 
                             success_count, failure_count, elapsed));
        }
        
        log::info!("Successfully remapped {} allocations in {:?} (batch mode)", 
                  success_count, elapsed);
        Ok(())
    }
}

// Update get_cached_driver_version to properly clone the data
pub fn get_cached_driver_version() -> Option<DriverVersionInfo> {
    DRIVER_HANDLE.lock().ok()
        .and_then(|guard| {
            guard.as_ref()
                .and_then(|result| result.as_ref().ok())
                .map(|(_, version)| version.clone())
        })
}

// Add a convenience function for GUI refresh
pub fn refresh_driver_status() -> DriverStatus {
    reset_driver_state();
    check_and_display_driver_status()
}

// Update check_and_display_driver_status to handle re-initialization
pub fn check_and_display_driver_status() -> DriverStatus {
    // Try to get cached info first
    let cached_result = DRIVER_HANDLE.lock().ok()
        .and_then(|guard| guard.as_ref().cloned());
    
    if let Some(result) = cached_result {
        match result {
            Ok((_, version)) => return DriverStatus::Available(version),
            Err(e) => {
                if e.contains("version incompatible") {
                    // Try to get detailed version mismatch info
                    if let Ok(driver) = DriverHandle::open() {
                        if let Err(DriverVersionError::VersionMismatch { driver_version, app_version, min_required, max_supported }) = driver.check_version_compatibility() {
                            return DriverStatus::VersionMismatch { driver_version, app_version, min_required, max_supported };
                        }
                    }
                }
                return DriverStatus::Error(e);
            }
        }
    }
    
    // Not cached, try to initialize
    match get_global_driver_handle() {
        Ok(_) => {
            // Successfully initialized, get the version
            if let Some(version) = get_cached_driver_version() {
                DriverStatus::Available(version)
            } else {
                DriverStatus::Error("Failed to get version after initialization".to_string())
            }
        }
        Err(e) => {
            if e.contains("version incompatible") {
                // Try to get detailed version mismatch info
                if let Ok(driver) = DriverHandle::open() {
                    if let Err(DriverVersionError::VersionMismatch { driver_version, app_version, min_required, max_supported }) = driver.check_version_compatibility() {
                        return DriverStatus::VersionMismatch { driver_version, app_version, min_required, max_supported };
                    }
                }
            } else if e.contains("Failed to open TMR driver device") {
                return DriverStatus::NotFound;
            }
            DriverStatus::Error(e)
        }
    }
}

pub fn display_driver_stats() -> Result<(), String> {
    let stats = DmaBuffer::get_driver_statistics()?;
    
    println!("  DMA Driver Statistics:");
    println!("    Uptime: {}s", stats.uptime_seconds);
    println!("    Allocations: {} total, {} successful", 
             stats.total_allocations, stats.successful_allocations);
    println!("    Memory: {:.2} GB current / {:.2} GB peak",
             stats.current_memory_allocated_gb, stats.peak_memory_allocated_gb);
    println!("    Pages: {} huge, {} large", 
             stats.huge_pages_count, stats.large_pages_count);
    println!("    Performance: {}μs avg allocation time", 
             stats.average_allocation_time_us);
    if stats.zero_free_allocations > 0 {
        println!("    Zero-free: {} allocations",
                 stats.zero_free_allocations);
    }
    println!("    NUMA efficiency: {:.1}%", stats.numa_efficiency);
    
    Ok(())
}

pub fn display_driver_info() {
    let status = check_and_display_driver_status();
    
    match status {
        DriverStatus::Available(version) => {
            println!("  DMA Driver: ✅ Available (TMR kernel driver loaded)");
            println!("    Version: {}.{}.{}.{} (built {} {})",
                version.driver_version_major,
                version.driver_version_minor,
                version.driver_version_build,
                version.driver_version_revision,
                String::from_utf8_lossy(&version.driver_build_date).trim_end_matches('\0'),
                String::from_utf8_lossy(&version.driver_build_time).trim_end_matches('\0')
            );
            
            // Show all features in one place
            print!("    Features:");
            let flags = version.compatibility_flags;
            if flags & CompatibilityFlags::SupportsHugePages as u32 != 0 {
                print!(" ✓ 1GB SuperPages");
            }
            if flags & CompatibilityFlags::SupportsNuma as u32 != 0 {
                print!(" ✓ NUMA awareness");
            }
            if flags & CompatibilityFlags::SupportsMixedPages as u32 != 0 {
                print!(" ✓ Mixed allocation");
            }
            if flags & CompatibilityFlags::SupportsZeroFree as u32 != 0 {
                print!(" ✓ Zero-free allocation");
            }
            if flags & CompatibilityFlags::SupportsBatchAllocation as u32 != 0 {
                print!(" ✓ Batch API");
            }
            if flags & CompatibilityFlags::SupportsETW as u32 != 0 {
                print!(" ✓ ETW tracing");
            }
            if flags & CompatibilityFlags::SupportsMemoryTypes as u32 != 0 {
                print!(" ✓ Memory type control");
            }
            println!();
            
            // Get and show driver stats if available
            if let Ok(stats) = DmaBuffer::get_driver_statistics() {
                println!("    Runtime: {}s, Memory: {:.2} GB current / {:.2} GB peak",
                    stats.uptime_seconds,
                    stats.current_memory_allocated_gb,
                    stats.peak_memory_allocated_gb
                );
            }
        }
        DriverStatus::VersionMismatch { driver_version, app_version, min_required, max_supported } => {
            println!("  DMA Driver: ❌ Version mismatch");
            println!("    Driver version: {}", driver_version);
            println!("    TMR version: {}", app_version);
            println!("    Required: {} - {}", min_required, max_supported);
            println!("    Action: Update TMR or reinstall matching driver version");
        }
        DriverStatus::NotFound => {
            println!("  DMA Driver: ⚠️  Not available - using standard allocation");
            println!("    To enable: Install TMR kernel driver (requires Administrator)");
            println!("    Benefits: Physical address access, better memory controller testing");
        }
        DriverStatus::Error(e) => {
            println!("  DMA Driver: ❌ Error - {}", e);
        }
    }
}

// New functions for displaying app driver stats
pub fn display_app_driver_stats() {
    let app_stats = get_app_stats();
    
    println!("\n📊 Application-side Driver Call Statistics:");
    
    // Display total calls first
    println!("  Total Calls: {}", app_stats.get_value(DriverStatType::TotalCalls));
    println!("  Breakdown by Type:");
    
    // Loop through all stats except TotalCalls
    for stat in &app_stats.stats {
        if stat.stat_type != DriverStatType::TotalCalls {
            let count = stat.counter.load(Ordering::Relaxed);
            if count > 0 {
                println!("    {}: {}", stat.display_name, count);
            }
        }
    }
}

pub fn display_app_driver_stats_table() {
    let app_stats = get_app_stats();
    
    let mut table = TableBuilder::new()
        .add_header("Operation", Alignment::Left)
        .add_header("Count", Alignment::Right)
        .min_column_width(35); // Match your original spacing
    
    for stat in &app_stats.stats {
        let count = stat.counter.load(Ordering::Relaxed);
        if count > 0 || stat.stat_type == DriverStatType::TotalCalls {
            table = table.add_row(vec![
                stat.display_name.to_string(),
                count.to_string(),
            ]);
        }
    }
    
    println!("\n📊 Application Driver Call Statistics:");
    table.print();
}

pub fn compare_app_vs_driver_stats() -> Result<(), String> {
    let app_stats = get_app_stats();
    let driver_stats = DmaBuffer::get_driver_statistics()?;
    
    println!("\n📊 Application vs Driver Statistics Comparison:");
    println!("  Application-side:");
    println!("    Total calls: {}", app_stats.get_value(DriverStatType::TotalCalls));
    println!("    Call breakdown:");
    
    // Compact loop for displaying app stats
    for stat in &app_stats.stats {
        if stat.stat_type != DriverStatType::TotalCalls {
            let count = stat.counter.load(Ordering::Relaxed);
            if count > 0 {
                println!("      {}: {}", stat.display_name, count);
            }
        }
    }
    
    println!("\n  Driver-side:");
    println!("    Total allocations: {}", driver_stats.total_allocations);
    println!("    Successful allocations: {}", driver_stats.successful_allocations);
    println!("    Failed allocations: {}", driver_stats.failed_allocations);
    
    // Calculate discrepancies for specific operations
    let app_allocs = app_stats.get_value(DriverStatType::AllocateDma);
    let app_batch_allocs = app_stats.get_value(DriverStatType::BatchAllocate);
    let total_app_allocs = app_allocs + app_batch_allocs;
    
    if total_app_allocs != driver_stats.total_allocations {
        println!("\n  ⚠️  Discrepancy: App sent {} allocation requests ({}+{} batch), driver reports {} total",
            total_app_allocs, app_allocs, app_batch_allocs, driver_stats.total_allocations);
    }
    
    // Check remap operations
    let app_remaps = app_stats.get_value(DriverStatType::BatchRemapMemoryType) + 
                     app_stats.get_value(DriverStatType::RemapAllMemoryType);
    if app_remaps > 0 {
        println!("\n  Memory Remap Operations:");
        println!("    Batch remaps: {}", app_stats.get_value(DriverStatType::BatchRemapMemoryType));
        println!("    Remap all calls: {}", app_stats.get_value(DriverStatType::RemapAllMemoryType));
    }
    
    Ok(())
}

pub fn reset_app_driver_stats() {
    get_app_stats().reset();
    log::info!("App driver stats reset to zero");
}

impl DmaBuffer {
    pub fn print_driver_info() {
        if let Ok(driver) = DriverHandle::open_with_version_check() {
            if let Ok(version) = driver.check_version_compatibility() {
                println!("  DMA Driver: ✅ Available and compatible");
                println!("    Version: {}.{}.{}.{}", 
                         version.driver_version_major,
                         version.driver_version_minor,
                         version.driver_version_build,
                         version.driver_version_revision);
                println!("    Features:");
                
                let flags = version.compatibility_flags;
                if flags & CompatibilityFlags::SupportsHugePages as u32 != 0 {
                    print!(" ✓ 1GB SuperPages");
                }
                if flags & CompatibilityFlags::SupportsNuma as u32 != 0 {
                    print!(" ✓ NUMA awareness");
                }
                if flags & CompatibilityFlags::SupportsMixedPages as u32 != 0 {
                    print!(" ✓ Mixed allocation");
                }
                if flags & CompatibilityFlags::SupportsZeroFree as u32 != 0 {
                    print!(" ✓ Zero-free allocation");
                }
                if flags & CompatibilityFlags::SupportsBatchAllocation as u32 != 0 {
                    print!(" ✓ Batch API");
                }
                if flags & CompatibilityFlags::SupportsETW as u32 != 0 {
                    print!(" ✓ ETW tracing");
                }
                if flags & CompatibilityFlags::SupportsMemoryTypes as u32 != 0 {
                    print!(" ✓ Memory type control");
                }
                println!();
                
                // Show statistics if available
                if let Ok(stats) = DmaBuffer::get_driver_statistics() {
                    println!("    Runtime Statistics:");
                    println!("      Uptime: {}s", stats.uptime_seconds);
                    println!("      Allocations: {} total, {} successful", 
                             stats.total_allocations, stats.successful_allocations);
                    println!("      Memory: {:.2} GB current / {:.2} GB peak",
                             stats.current_memory_allocated_gb, stats.peak_memory_allocated_gb);
                    println!("      Pages: {} huge, {} large", 
                             stats.huge_pages_count, stats.large_pages_count);
                    println!("      Performance: {}μs avg allocation time", 
                             stats.average_allocation_time_us);
                    if stats.zero_free_allocations > 0 {
                        println!("      Zero-free: {} allocations ({:.1}% faster)",
                                 stats.zero_free_allocations,
                                 30.0); // Approximate speedup
                    }
                    println!("      NUMA efficiency: {:.1}%", stats.numa_efficiency);
                }
            }
        }
    }
}