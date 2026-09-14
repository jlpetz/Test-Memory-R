// Every `unsafe` block in this file crosses a Win32 FFI boundary, where the borrow checker is
// switched off precisely where the invariants get subtle (TODO #66). The lint below makes a missing
// `// SAFETY:` a warning *here* rather than relying on a periodic audit — it is deliberately not
// crate-wide, because the SIMD test kernels' `unsafe` is a different, repetitive story already
// covered by `test_fn_safety.md`, and a blanket rule there would produce boilerplate that trains
// you to skip reading these.
#![warn(clippy::undocumented_unsafe_blocks)]

use crate::memory::buffer::{BufferInfo, PageType};
use crate::memory::allocator::AllocationConfig;
use crate::memory::privileges::setup_large_pages_automatically;
use crate::constants::{HUGE_PAGE_SIZE_USIZE, LARGE_PAGE_SIZE_USIZE, PAGE_SIZE_4KB, MB};

// Windows API imports for VirtualAlloc2 (moved from buffer.rs)
use windows::Win32::Foundation::GetLastError;
use windows::Win32::System::Memory::{
    VirtualAlloc2, MEM_COMMIT, MEM_LARGE_PAGES, MEM_RESERVE, 
    PAGE_READWRITE, MEM_ADDRESS_REQUIREMENTS, MEM_EXTENDED_PARAMETER,
    MemExtendedParameterAttributeFlags, MemExtendedParameterNumaNode, 
    MemExtendedParameterAddressRequirements,
};
use windows::Win32::System::SystemServices::{
    MEM_EXTENDED_PARAMETER_NONPAGED_HUGE, MEM_EXTENDED_PARAMETER_NONPAGED_LARGE,
    MEM_EXTENDED_PARAMETER_TYPE_BITS,
};

pub trait Backend: Send + Sync + std::fmt::Debug {
    fn allocate(&self, config: &AllocationConfig) -> Result<BackendAllocation, String>;
    fn free(&self, allocation: BackendAllocation) -> Result<(), String>;
    fn name(&self) -> &'static str;
}

#[derive(Debug)]
pub struct BackendAllocation {
    pub ptr: *mut u8,
    pub size: usize,
    pub info: BufferInfo,
}

/// Which `Backend` impl to allocate through.
///
/// Kept as an enum rather than collapsed to a bool: a ring-0 backend would return here
/// as one more variant, which is the whole reason the `Backend` trait boundary survived
/// the driver-client purge (TODO #4/5).
#[derive(Debug, Clone)]
pub enum BackendType {
    Auto,                                    // Let system decide
    Windows { large_pages: bool },          // Windows VirtualAlloc
}

/// Windows VirtualAlloc backend
#[derive(Debug)]
pub struct WindowsBackend {
    use_large_pages: bool,
}

impl WindowsBackend {
    pub fn new(use_large_pages: bool) -> Self {
        Self { use_large_pages }
    }
}

impl Backend for WindowsBackend {
    fn allocate(&self, config: &AllocationConfig) -> Result<BackendAllocation, String> {
        use crate::memory::allocator::PageSizePreference as AllocPageSizePref;
        
        // Override page preference if backend doesn't support large pages
        let final_config = if !self.use_large_pages {
            let mut modified_config = config.clone();
            // Force regular pages if backend doesn't support large pages
            modified_config.page_size = match &config.page_size {
                AllocPageSizePref::Any => AllocPageSizePref::Prefer(PageType::Regular(config.size)),
                AllocPageSizePref::Prefer(_) => AllocPageSizePref::Prefer(PageType::Regular(config.size)),
                AllocPageSizePref::Require(page_type) => {
                    match page_type {
                        PageType::Regular(_) => config.page_size.clone(),
                        _ => return Err("Backend doesn't support large/huge pages but they are required".to_string()),
                    }
                },
                AllocPageSizePref::Range { min, max: _ } => {
                    if matches!(min, PageType::Large(_) | PageType::Huge(_)) {
                        return Err("Backend doesn't support large/huge pages but minimum requires them".to_string());
                    }
                    AllocPageSizePref::Prefer(PageType::Regular(config.size))
                }
            };
            modified_config
        } else {
            config.clone()
        };
        
        // VirtualAlloc2 implementation (moved from TestBuffer)
        let (ptr, size, uses_large_pages, uses_huge_pages) = self.allocate_with_virtualalloc2(&final_config)?;
        
        let page_type = if uses_huge_pages {
            PageType::Huge(size)
        } else if uses_large_pages {
            PageType::Large(size)
        } else {
            PageType::Regular(size)
        };
        
        let info = BufferInfo {
            physical_address: None,
            numa_node: config.numa_node.unwrap_or(0),
            page_type,
            segments: Vec::new(),
        };
        
        Ok(BackendAllocation {
            ptr,
            size,
            info,
        })
    }
    
    fn free(&self, allocation: BackendAllocation) -> Result<(), String> {
        use windows::Win32::System::Memory::{VirtualFree, MEM_RELEASE};

        // SAFETY: `allocation` is taken by value, so this is the unique owner of the region and the
        // release happens exactly once. `ptr` came from a successful `VirtualAlloc2` in this same
        // backend, which is the base address `MEM_RELEASE` requires.
        unsafe {
            // VirtualFree with MEM_RELEASE must pass size = 0
            // See: https://docs.microsoft.com/en-us/windows/win32/api/memoryapi/nf-memoryapi-virtualfree
            if !VirtualFree(allocation.ptr as *mut _, 0, MEM_RELEASE).is_ok() {
                let err = GetLastError();
                return Err(format!("VirtualFree failed for ptr {:?}, size {}: error code {}",
                    allocation.ptr, allocation.size, err.0));
            }
        }

        log::trace!("Freed {} bytes at {:?}", allocation.size, allocation.ptr);
        Ok(())
    }
    
    
    fn name(&self) -> &'static str {
        if self.use_large_pages {
            "Windows Large Pages"
        } else {
            "Windows Regular"
        }
    }
}

impl WindowsBackend {
    /// Helper function to create a MEM_EXTENDED_PARAMETER with the correct bitfield packing
    fn create_extended_param(param_type: windows::Win32::System::Memory::MEM_EXTENDED_PARAMETER_TYPE, value: u64) -> MEM_EXTENDED_PARAMETER {
        let mut param = MEM_EXTENDED_PARAMETER::default();
        
        // Pack the Type into the low TYPE_BITS bits of _bitfield
        let type_bits = MEM_EXTENDED_PARAMETER_TYPE_BITS;
        param.Anonymous1._bitfield = (param_type.0 as u64) & ((1_u64 << type_bits) - 1);
        
        // Set the union value
        param.Anonymous2.ULong64 = value;
        
        param
    }

    /// VirtualAlloc2 implementation (moved from TestBuffer)
    fn allocate_with_virtualalloc2(&self, config: &AllocationConfig) -> Result<(*mut u8, usize, bool, bool), String> {
        use crate::memory::allocator::PageSizePreference as AllocPageSizePref;
        
        let size_bytes = config.size;
        
        // Determine what type of pages we want based on PageSizePreference
        let (needs_large_pages, target_page_type) = match &config.page_size {
            AllocPageSizePref::Any => (true, None), // Try best available
            AllocPageSizePref::Prefer(page_type) => {
                match page_type {
                    PageType::Regular(_) => (false, Some((false, false))),
                    PageType::Large(_) => (true, Some((true, false))),
                    PageType::Huge(_) => (true, Some((true, true))),
                    PageType::Mixed(_) => (true, None), // Let auto-select
                }
            },
            AllocPageSizePref::Require(page_type) => {
                match page_type {
                    PageType::Regular(_) => (false, Some((false, false))),
                    PageType::Large(_) => (true, Some((true, false))),
                    PageType::Huge(_) => (true, Some((true, true))),
                    PageType::Mixed(_) => return Err("Can't require mixed page types with VirtualAlloc2".to_string()),
                }
            },
            AllocPageSizePref::Range { min, max } => {
                // Determine based on range
                let needs_large = !matches!(max, PageType::Regular(_));
                let prefer_huge = matches!(min, PageType::Huge(_));
                let prefer_large = matches!(min, PageType::Large(_)) || matches!(max, PageType::Large(_));
                
                if prefer_huge {
                    (true, Some((true, true)))
                } else if prefer_large {
                    (true, Some((true, false)))
                } else {
                    (needs_large, None)
                }
            }
        };
        
        if needs_large_pages
            && setup_large_pages_automatically().is_err() {
                log::debug!("Large/huge page error - missing privilege");
                // Fall back for non-strict modes
                match &config.page_size {
                    AllocPageSizePref::Require(_) => return Err("Large/huge pages required but privilege missing".to_string()),
                    _ => {
                        // Try with regular pages
                        let mut fallback_config = config.clone();
                        fallback_config.page_size = AllocPageSizePref::Require(PageType::Regular(size_bytes));
                        return self.allocate_with_virtualalloc2(&fallback_config);
                    }
                }
            }

        // Determine page size and alignment based on target or auto-select
        let (aligned_size, allocation_type, actual_page_type) = match target_page_type {
            Some((false, false)) => {
                // Regular pages
                let page_size = PAGE_SIZE_4KB;
                let aligned = (size_bytes + page_size - 1) & !(page_size - 1);
                (aligned, MEM_RESERVE | MEM_COMMIT, (false, false))
            },
            Some((true, false)) => {
                // Large pages (2MB)
                let page_size = LARGE_PAGE_SIZE_USIZE;
                let aligned = (size_bytes + page_size - 1) & !(page_size - 1);
                (aligned, MEM_RESERVE | MEM_COMMIT | MEM_LARGE_PAGES, (true, false))
            },
            Some((true, true)) => {
                // Huge pages (1GB)
                let page_size = HUGE_PAGE_SIZE_USIZE;
                let aligned = (size_bytes + page_size - 1) & !(page_size - 1);
                (aligned, MEM_RESERVE | MEM_COMMIT | MEM_LARGE_PAGES, (true, true))
            },
            Some((false, true)) => {
                // This combination is impossible (huge pages require large pages flag too)
                // Treat as regular pages
                let page_size = PAGE_SIZE_4KB;
                let aligned = (size_bytes + page_size - 1) & !(page_size - 1);
                (aligned, MEM_RESERVE | MEM_COMMIT, (false, false))
            },
            None => {
                // Auto-select based on size
                if size_bytes >= HUGE_PAGE_SIZE_USIZE {
                    // >= 1GB, try huge pages
                    let page_size = HUGE_PAGE_SIZE_USIZE;
                    let aligned = (size_bytes + page_size - 1) & !(page_size - 1);
                    (aligned, MEM_RESERVE | MEM_COMMIT | MEM_LARGE_PAGES, (true, true))
                } else if size_bytes >= 4 * MB {
                    // >= 4MB, use large pages  
                    let page_size = LARGE_PAGE_SIZE_USIZE;
                    let aligned = (size_bytes + page_size - 1) & !(page_size - 1);
                    (aligned, MEM_RESERVE | MEM_COMMIT | MEM_LARGE_PAGES, (true, false))
                } else {
                    // Small allocation, use regular pages
                    let page_size = PAGE_SIZE_4KB;
                    let aligned = (size_bytes + page_size - 1) & !(page_size - 1);
                    (aligned, MEM_RESERVE | MEM_COMMIT, (false, false))
                }
            }
        };

        // Build extended parameters
        let mut extended_params = Vec::new();
        
        // Add page attribute based on type
        if actual_page_type.1 { // is_huge (1GB)
            extended_params.push(Self::create_extended_param(
                MemExtendedParameterAttributeFlags,
                MEM_EXTENDED_PARAMETER_NONPAGED_HUGE as u64
            ));
        } else if actual_page_type.0 { // is_large (2MB)
            extended_params.push(Self::create_extended_param(
                MemExtendedParameterAttributeFlags,
                MEM_EXTENDED_PARAMETER_NONPAGED_LARGE as u64
            ));
        }
        // No extended parameter needed for regular 4KB pages

        // Add NUMA node parameter if specified
        if let Some(node) = config.numa_node {
            extended_params.push(Self::create_extended_param(
                MemExtendedParameterNumaNode,
                node as u64
            ));
            
            let is_strict = matches!(&config.page_size, AllocPageSizePref::Require(_));
            log::debug!("Requesting {} pages on NUMA node {} (strict: {})", 
                if actual_page_type.1 { "1GB huge" }
                else if actual_page_type.0 { "2MB large" }
                else { "4KB regular" },
                node, is_strict
            );
        }

        // Add alignment / base-address requirements if specified.
        //
        // LIFETIME: `addr_req` MUST be declared at function scope, not inside the `if` below.
        // `MEM_EXTENDED_PARAMETER` stores it as a *raw pointer*, which the kernel dereferences
        // inside the `VirtualAlloc2` call further down. A block-scoped `addr_req` is dead by
        // then (LLVM emits `lifetime.end` at scope exit and may reuse the stack slot), so the
        // kernel would read whatever landed there — UB that the borrow checker cannot catch
        // because the raw cast erases the borrow. Keep this binding alive until after the call.
        let mut addr_req = MEM_ADDRESS_REQUIREMENTS {
            LowestStartingAddress: std::ptr::null_mut(),
            HighestEndingAddress: std::ptr::null_mut(),
            Alignment: 0,
        };

        if config.alignment.is_some() || config.base_address.is_some() {
            // Both fields are optional and independent: a null LowestStartingAddress means
            // "anywhere", Alignment 0 means "no specific alignment". Filling them from the two
            // Options covers all three previously-separate cases identically.
            addr_req.LowestStartingAddress =
                config.base_address.unwrap_or(std::ptr::null_mut()) as *mut std::ffi::c_void;
            addr_req.Alignment = config.alignment.unwrap_or(0);

            let mut param = Self::create_extended_param(
                MemExtendedParameterAddressRequirements,
                0 // Will be overridden by Pointer field
            );
            param.Anonymous2.Pointer = &mut addr_req as *mut _ as *mut std::ffi::c_void;
            extended_params.push(param);
        }

        // Note: VirtualAlloc2 always zeros memory for security, config.zero_memory is ignored
        if !config.zero_memory {
            log::trace!("Note: VirtualAlloc2 always zeros memory, zero_memory=false is ignored");
        }

        // SAFETY: `extended_params` may hold a raw pointer to `addr_req` (see the LIFETIME note
        // where it is built). `addr_req` is function-scoped, so it is still live here — the
        // kernel dereferences that pointer during this call. Do not move `addr_req` into a
        // narrower scope.
        let ptr = unsafe {
            VirtualAlloc2(
                None,  // Current process
                None,  // Let address requirements handle base address
                aligned_size,
                allocation_type,
                PAGE_READWRITE.0,
                if extended_params.is_empty() { None } else { Some(&mut extended_params) },
            )
        };
        // Keep `addr_req` provably alive across the call above (defensive against a future
        // refactor reordering or shrinking its scope; compiles to nothing).
        let _ = &addr_req;

        if ptr.is_null() {
            // SAFETY: `GetLastError` reads this thread's error slot and takes no arguments. It is
            // read immediately after the failed call, before anything else can overwrite it.
            let err = unsafe { GetLastError() };
            
            // Handle fallback scenarios
            let is_strict = matches!(&config.page_size, AllocPageSizePref::Require(_));
            
            if config.numa_node.is_some() && !is_strict {
                log::debug!(
                    "VirtualAlloc2 failed on NUMA node {:?} (code {}), trying without NUMA preference",
                    config.numa_node, err.0
                );
                let mut fallback_config = config.clone();
                fallback_config.numa_node = None;
                return self.allocate_with_virtualalloc2(&fallback_config);
            }
            
            // For non-strict page preferences, try fallback
            match &config.page_size {
                AllocPageSizePref::Any | AllocPageSizePref::Prefer(_) => {
                    if actual_page_type.1 {
                        // Failed with huge pages, try large pages
                        log::debug!("VirtualAlloc2 with huge pages failed (code {}), trying large pages", err.0);
                        let mut fallback_config = config.clone();
                        fallback_config.page_size = AllocPageSizePref::Prefer(PageType::Large(size_bytes));
                        return self.allocate_with_virtualalloc2(&fallback_config);
                    } else if actual_page_type.0 {
                        // Failed with large pages, try regular pages
                        log::debug!("VirtualAlloc2 with large pages failed (code {}), trying regular pages", err.0);
                        let mut fallback_config = config.clone();
                        fallback_config.page_size = AllocPageSizePref::Prefer(PageType::Regular(size_bytes));
                        return self.allocate_with_virtualalloc2(&fallback_config);
                    }
                },
                AllocPageSizePref::Range { min: _, max } => {
                    // Try next smaller size in range
                    if actual_page_type.1 && !matches!(max, PageType::Huge(_)) {
                        log::debug!("VirtualAlloc2 with huge pages failed (code {}), trying large pages in range", err.0);
                        let mut fallback_config = config.clone();
                        fallback_config.page_size = AllocPageSizePref::Prefer(PageType::Large(size_bytes));
                        return self.allocate_with_virtualalloc2(&fallback_config);
                    } else if actual_page_type.0 && !matches!(max, PageType::Large(_)) {
                        log::debug!("VirtualAlloc2 with large pages failed (code {}), trying regular pages in range", err.0);
                        let mut fallback_config = config.clone();
                        fallback_config.page_size = AllocPageSizePref::Prefer(PageType::Regular(size_bytes));
                        return self.allocate_with_virtualalloc2(&fallback_config);
                    }
                },
                _ => {} // No fallback for Require
            }
            
            Err(format!("VirtualAlloc2 failed: code = {}", err.0))
        } else {
            log::debug!("Successfully allocated {} bytes using {} pages{}", 
                aligned_size,
                if actual_page_type.1 { "1GB huge" } 
                else if actual_page_type.0 { "2MB large" } 
                else { "4KB regular" },
                if let Some(node) = config.numa_node { format!(" on NUMA node {}", node) } else { String::new() }
            );
            
            Ok((ptr as *mut u8, aligned_size, actual_page_type.0, actual_page_type.1))
        }
    }
}
