// Driver interface functionality - migrated from dma_memory.rs

use crate::driver::types::{DriverVersionInfo, DriverVersionError, DriverStatistics, MemoryType};
use crate::driver::statistics::{track_call, DriverStatType};
use std::sync::{Arc, Mutex};
use windows::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows::Win32::Storage::FileSystem::{CreateFileA, OPEN_EXISTING};
use windows::Win32::System::IO::DeviceIoControl;
use windows::core::PCSTR;

/// TMR application version
const APP_VERSION_MAJOR: u32 = 1;
const APP_VERSION_MINOR: u32 = 0;
const APP_VERSION_BUILD: u32 = 0;

// Windows device type constants
const FILE_DEVICE_TMR: u32 = 0x8000;
const METHOD_BUFFERED: u32 = 0;
const FILE_ANY_ACCESS: u32 = 0;

const fn ctl_code(device_type: u32, function: u32, method: u32, access: u32) -> u32 {
    (device_type << 16) | (access << 14) | (function << 2) | method
}

// All IOCTL codes sorted alphabetically with sequential numbering (synced with driver)
// Note: Unused IOCTLs prefixed with _ to suppress warnings while keeping them for future use
pub const IOCTL_TMR_ALLOCATE: u32 = ctl_code(FILE_DEVICE_TMR, 0x800, METHOD_BUFFERED, FILE_ANY_ACCESS);
pub const IOCTL_TMR_BATCH_ALLOCATE: u32 = ctl_code(FILE_DEVICE_TMR, 0x801, METHOD_BUFFERED, FILE_ANY_ACCESS);
pub const IOCTL_TMR_FREE: u32 = ctl_code(FILE_DEVICE_TMR, 0x803, METHOD_BUFFERED, FILE_ANY_ACCESS);
pub const IOCTL_TMR_BATCH_FREE: u32 = ctl_code(FILE_DEVICE_TMR, 0x804, METHOD_BUFFERED, FILE_ANY_ACCESS);
pub const IOCTL_TMR_FREE_ALL: u32 = ctl_code(FILE_DEVICE_TMR, 0x805, METHOD_BUFFERED, FILE_ANY_ACCESS);
const IOCTL_TMR_GET_STATISTICS: u32 = ctl_code(FILE_DEVICE_TMR, 0x806, METHOD_BUFFERED, FILE_ANY_ACCESS);
const IOCTL_TMR_GET_VERSION: u32 = ctl_code(FILE_DEVICE_TMR, 0x807, METHOD_BUFFERED, FILE_ANY_ACCESS);
const _IOCTL_TMR_MAP_USER_SPACE: u32 = ctl_code(FILE_DEVICE_TMR, 0x808, METHOD_BUFFERED, FILE_ANY_ACCESS);
const IOCTL_TMR_RESET_ALL: u32 = ctl_code(FILE_DEVICE_TMR, 0x80A, METHOD_BUFFERED, FILE_ANY_ACCESS);
const _IOCTL_TMR_SET_CPU_AFFINITY: u32 = ctl_code(FILE_DEVICE_TMR, 0x80B, METHOD_BUFFERED, FILE_ANY_ACCESS);
const _IOCTL_TMR_UNMAP_USER_SPACE: u32 = ctl_code(FILE_DEVICE_TMR, 0x80C, METHOD_BUFFERED, FILE_ANY_ACCESS);

#[derive(Debug)]
pub struct DriverHandle {
    handle: HANDLE,
}

// SAFETY: the only field is a Win32 `HANDLE` from `CreateFileW`, which is process-wide (NOT
// thread-affine) — any thread may pass it to `DeviceIoControl`, and the kernel serialises
// concurrent IOCTLs on the same file object. Nothing here is mutated after construction, so
// `&DriverHandle` is safe to share; each IOCTL wrapper builds its own input/output structs as
// locals and passes them directly as call arguments (never storing a raw pointer that could
// outlive them — see TODO #66 and the LIFETIME note in `memory/backend.rs`). `Drop` calls
// `CloseHandle` exactly once, and the handle is wrapped in an `Arc` by
// `get_global_driver_handle`, so the close happens after the last user is gone.
unsafe impl Send for DriverHandle {}
unsafe impl Sync for DriverHandle {}

impl DriverHandle {
    /// Get the raw Windows handle for DeviceIoControl calls
    pub fn handle(&self) -> HANDLE {
        self.handle
    }
    pub fn open() -> Result<Arc<Self>, String> {
        unsafe {
            let device_path = PCSTR::from_raw(c"\\\\.\\TmrMemory".as_ptr().cast());
            
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
        track_call(DriverStatType::ResetAll);
        
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
        track_call(DriverStatType::GetVersion);
        
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
                return Err(DriverVersionError::QueryFailed("Failed to query driver version".to_string()));
            }
        }

        // Check version compatibility
        if version_info.driver_version_major == 0 && version_info.driver_version_minor == 0 {
            return Err(DriverVersionError::QueryFailed("Driver returned zero version".to_string()));
        }

        if APP_VERSION_MAJOR < version_info.min_app_version_major || 
           APP_VERSION_MAJOR > version_info.max_app_version_major {
            return Err(DriverVersionError::VersionMismatch {
                app_version: format!("{}.{}.{}", APP_VERSION_MAJOR, APP_VERSION_MINOR, APP_VERSION_BUILD),
                driver_version: format!("{}.{}.{}.{}", 
                    version_info.driver_version_major,
                    version_info.driver_version_minor,
                    version_info.driver_version_build,
                    version_info.driver_version_revision),
                min_required: format!("{}.{}", version_info.min_app_version_major, version_info.min_app_version_minor),
                max_supported: format!("{}.{}", version_info.max_app_version_major, version_info.max_app_version_minor),
            });
        }

        if APP_VERSION_MAJOR == version_info.min_app_version_major && 
           APP_VERSION_MINOR < version_info.min_app_version_minor {
            return Err(DriverVersionError::VersionMismatch {
                app_version: format!("{}.{}.{}", APP_VERSION_MAJOR, APP_VERSION_MINOR, APP_VERSION_BUILD),
                driver_version: format!("{}.{}.{}.{}", 
                    version_info.driver_version_major,
                    version_info.driver_version_minor,
                    version_info.driver_version_build,
                    version_info.driver_version_revision),
                min_required: format!("{}.{}", version_info.min_app_version_major, version_info.min_app_version_minor),
                max_supported: format!("{}.{}", version_info.max_app_version_major, version_info.max_app_version_minor),
            });
        }

        Ok(version_info)
    }
    
    
    
    pub fn get_driver_statistics(&self) -> Result<DriverStatistics, String> {
        track_call(DriverStatType::GetStatistics);
        
        let mut stats = DriverStatistics::default();
        
        unsafe {
            let mut bytes_returned = 0u32;
            
            let result = DeviceIoControl(
                self.handle,
                IOCTL_TMR_GET_STATISTICS,
                None,
                0,
                Some(&mut stats as *mut _ as *mut std::ffi::c_void),
                std::mem::size_of::<DriverStatistics>() as u32,
                Some(&mut bytes_returned),
                None,
            );
            
            if result.is_err() {
                return Err(format!("Failed to get driver statistics: {:?}", windows::core::Error::from_thread()));
            }
        }
        
        Ok(stats)
    }
    
    /// Enhanced batch allocation with intelligent huge page distribution
    pub fn enhanced_batch_allocate(
        &self, 
        total_memory_target: usize,
        thread_requests: &[crate::driver::types::ThreadAllocationRequest]
    ) -> Result<crate::driver::types::BatchAllocateOutput, String> {
        use crate::driver::types::{BatchAllocateInput, BatchAllocateOutput};
        track_call(DriverStatType::BatchAllocate);
        
        let request_count = core::cmp::min(thread_requests.len(), 32);
        let mut input = BatchAllocateInput {
            request_count: request_count as u32,
            abort_on_failure: false,
            total_memory_target,
            distribute_huge_pages_evenly: true,
            requests: [crate::driver::types::ThreadAllocationRequest::default(); 32],
        };
        
        // Copy requests into the input array
        for (i, request) in thread_requests.iter().take(request_count).enumerate() {
            input.requests[i] = *request;
        }
        
        let mut output = BatchAllocateOutput {
            total_allocations: 0,
            successful_allocations: 0,
            failed_allocations: 0,
            total_time_us: 0,
            results: [crate::driver::types::AllocationResult::default(); 128],
        };
        
        unsafe {
            let mut bytes_returned = 0u32;
            
            let result = DeviceIoControl(
                self.handle,
                IOCTL_TMR_BATCH_ALLOCATE,
                Some(&input as *const _ as *const std::ffi::c_void),
                std::mem::size_of::<BatchAllocateInput>() as u32,
                Some(&mut output as *mut _ as *mut std::ffi::c_void),
                std::mem::size_of::<BatchAllocateOutput>() as u32,
                Some(&mut bytes_returned),
                None,
            );
            
            if result.is_err() {
                return Err(format!("Enhanced batch allocation failed: {:?}", windows::core::Error::from_thread()));
            }
        }
        
        Ok(output)
    }

    /// Free all allocations in the driver
    pub fn free_all(&self) -> Result<(), String> {
        track_call(DriverStatType::FreeAll);
        
        unsafe {
            let mut bytes_returned = 0u32;
            
            let result = DeviceIoControl(
                self.handle,
                IOCTL_TMR_FREE_ALL,
                None,
                0,
                None,
                0,
                Some(&mut bytes_returned),
                None,
            );
            
            if result.is_err() {
                return Err(format!("Failed to free all allocations: {:?}", windows::core::Error::from_thread()));
            }
        }
        
        Ok(())
    }
    
    /// Remap all memory to a new type by freeing and reallocating
    /// This replaces the old remap functionality since cache types cannot be changed after allocation
    pub fn remap_all_memory_type(&self, new_memory_type: MemoryType, allocations: &[crate::driver::types::ThreadAllocationRequest]) -> Result<Vec<crate::driver::types::AllocationResult>, String> {
        use crate::driver::types::{BatchAllocateInput, BatchAllocateOutput};
        
        // Step 1: Free all existing allocations
        self.free_all()?;
        
        // Step 2: Reallocate with new memory type
        let mut input = BatchAllocateInput {
            request_count: allocations.len().min(32) as u32,
            abort_on_failure: false,
            total_memory_target: 0, // Not using even distribution
            distribute_huge_pages_evenly: false,
            requests: [crate::driver::types::ThreadAllocationRequest::default(); 32],
        };
        
        // Copy requests and update memory type
        for (i, req) in allocations.iter().take(32).enumerate() {
            let mut new_req = *req;
            new_req.memory_type = new_memory_type;
            input.requests[i] = new_req;
        }
        
        let mut output = BatchAllocateOutput {
            total_allocations: 0,
            successful_allocations: 0,
            failed_allocations: 0,
            total_time_us: 0,
            results: [crate::driver::types::AllocationResult::default(); 128],
        };
        
        unsafe {
            let mut bytes_returned = 0u32;
            
            let result = DeviceIoControl(
                self.handle,
                IOCTL_TMR_BATCH_ALLOCATE,
                Some(&input as *const _ as *const std::ffi::c_void),
                std::mem::size_of::<BatchAllocateInput>() as u32,
                Some(&mut output as *mut _ as *mut std::ffi::c_void),
                std::mem::size_of::<BatchAllocateOutput>() as u32,
                Some(&mut bytes_returned),
                None,
            );
            
            if result.is_err() {
                return Err(format!("Failed to reallocate memory: {:?}", windows::core::Error::from_thread()));
            }
        }
        
        // Convert results to Vec
        let mut results = Vec::new();
        for i in 0..output.total_allocations as usize {
            results.push(output.results[i]);
        }
        
        Ok(results)
    }
}

impl Drop for DriverHandle {
    fn drop(&mut self) {
        if self.handle != INVALID_HANDLE_VALUE {
            unsafe {
                let _ = CloseHandle(self.handle);
            }
        }
    }
}

/// Cached result of initializing the driver: the shared handle plus its version info,
/// or the error string from a failed init attempt.
type DriverInitResult = Result<(Arc<DriverHandle>, DriverVersionInfo), String>;

// Global driver handle that gets initialized once
static DRIVER_HANDLE: Mutex<Option<DriverInitResult>> = Mutex::new(None);

pub fn get_global_driver_handle() -> Result<Arc<DriverHandle>, String> {
    let mut handle_guard = DRIVER_HANDLE.lock().map_err(|_| "Failed to lock driver handle".to_string())?;
    
    if let Some(ref result) = *handle_guard {
        // Return existing handle if available
        log::debug!("get_global_driver_handle() using cached handle, is_ok: {}", result.is_ok());
        result.as_ref()
            .map(|(handle, _)| Arc::clone(handle))
            .map_err(|e| e.clone())
    } else {
        // Initialize new handle
        log::debug!("get_global_driver_handle() initializing new handle");
        match DriverHandle::open() {
            Ok(handle) => {
                match handle.check_version_compatibility() {
                    Ok(version) => {
                        let arc_handle = Arc::clone(&handle);
                        *handle_guard = Some(Ok((handle, version)));
                        Ok(arc_handle)
                    }
                    Err(e) => {
                        let error_msg = format!("Driver version incompatible: {}", e);
                        *handle_guard = Some(Err(error_msg.clone()));
                        Err(error_msg)
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

pub fn is_driver_connected() -> bool {
    match DRIVER_HANDLE.lock() {
        Ok(guard) => {
            match guard.as_ref() {
                Some(result) => {
                    let connected = result.is_ok();
                    log::debug!("is_driver_connected() - have cached result, is_ok: {}", connected);
                    connected
                }
                None => {
                    log::debug!("is_driver_connected() - no cached handle");
                    false
                }
            }
        }
        Err(_) => {
            log::debug!("is_driver_connected() - failed to lock mutex");
            false
        }
    }
}

pub fn reset_driver_state() {
    if let Ok(mut handle_guard) = DRIVER_HANDLE.lock() {
        // Clear the cached handle - this will drop the Arc and close the handle if no other references
        *handle_guard = None;
    }
}