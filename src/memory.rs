use windows::Win32::System::Memory::{
    VirtualAlloc, VirtualFree, MEM_COMMIT, MEM_RESERVE, MEM_RELEASE, PAGE_READWRITE, MEM_LARGE_PAGES,
};
use windows::Win32::System::SystemInformation::{GetPhysicallyInstalledSystemMemory};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::Win32::Security::{LookupPrivilegeValueW, AdjustTokenPrivileges, TOKEN_ADJUST_PRIVILEGES, TOKEN_QUERY, LUID_AND_ATTRIBUTES, SE_PRIVILEGE_ENABLED};
use windows::Win32::Foundation::{HANDLE, LUID};
use std::ptr::null_mut;
use std::ffi::c_void;

pub struct TestBuffer {
    ptr: *mut u8,
    size: usize,
    uses_large_pages: bool,
}

impl TestBuffer {
    pub fn new_large_pages(size_bytes: usize) -> Option<Self> {
        // Try to enable large page privilege
        if enable_large_page_privilege().is_err() {
            return None;
        }

        // Large pages are typically 2MB on x64
        let large_page_size = 2 * 1024 * 1024;
        let aligned_size = (size_bytes + large_page_size - 1) & !(large_page_size - 1);

        let ptr = unsafe {
            VirtualAlloc(
                Some(null_mut()),
                aligned_size,
                MEM_RESERVE | MEM_COMMIT | MEM_LARGE_PAGES,
                PAGE_READWRITE,
            )
        };

        if ptr.is_null() {
            None
        } else {
            // Don't print allocation message here - will be summarized later
            Some(Self {
                ptr: ptr as *mut u8,
                size: aligned_size,
                uses_large_pages: true,
            })
        }
    }

    pub fn new_aligned(size_bytes: usize) -> Option<Self> {
        // Align size to page boundary (typically 4KB)
        let page_size = 4096;
        let aligned_size = (size_bytes + page_size - 1) & !(page_size - 1);

        let ptr = unsafe {
            VirtualAlloc(
                Some(null_mut()),
                aligned_size,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_READWRITE,
            )
        };

        if ptr.is_null() {
            None
        } else {
            Some(Self {
                ptr: ptr as *mut u8,
                size: aligned_size,
                uses_large_pages: false,
            })
        }
    }

    pub fn as_mut_ptr(&self) -> *mut u8 {
        self.ptr
    }

    pub fn size(&self) -> usize {
        self.size
    }
    
    pub fn uses_large_pages(&self) -> bool {
        self.uses_large_pages
    }
}

impl Drop for TestBuffer {
    fn drop(&mut self) {
        unsafe {
            let _ = VirtualFree(self.ptr as *mut c_void, 0, MEM_RELEASE);
        }
    }
}

// Send and Sync are safe for TestBuffer since it manages its own memory
unsafe impl Send for TestBuffer {}
unsafe impl Sync for TestBuffer {}

pub fn get_total_system_memory() -> usize {
    unsafe {
        let mut total_memory_kb: u64 = 0;
        match GetPhysicallyInstalledSystemMemory(&mut total_memory_kb) {
            Ok(_) => (total_memory_kb * 1024) as usize,
            Err(_) => {
                // Fallback: assume 8GB if we can't detect
                log::warn!("Could not detect system memory, assuming 8 GiB");
                8 * 1024 * 1024 * 1024
            }
        }
    }
}

fn enable_large_page_privilege() -> Result<(), &'static str> {
    unsafe {
        let process = GetCurrentProcess();
        let mut token: HANDLE = HANDLE::default();
        
        if OpenProcessToken(process, TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY, &mut token).is_err() {
            return Err("Failed to open process token");
        }

        let mut luid = LUID::default();
        let privilege_name = windows::core::w!("SeLockMemoryPrivilege");
        
        if LookupPrivilegeValueW(None, privilege_name, &mut luid).is_err() {
            return Err("Failed to lookup privilege value");
        }

        let mut privileges = windows::Win32::Security::TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES {
                Luid: luid,
                Attributes: SE_PRIVILEGE_ENABLED,
            }],
        };

        if AdjustTokenPrivileges(
            token,
            false,
            Some(&mut privileges),
            0,
            None,
            None,
        ).is_err() {
            return Err("Failed to adjust token privileges");
        }

        Ok(())
    }
}

pub fn check_large_page_privilege() -> Result<(), &'static str> {
    enable_large_page_privilege()
}