use crate::memory::backend::{AllocError, Backend, BackendType, WindowsBackend};
use crate::memory::buffer::{MemoryBuffer, PageType};
use crate::memory::buffer::MemoryType as BufferMemoryType;
use crate::constants::{HUGE_PAGE_SIZE_USIZE, BYTES_PER_MIB_USIZE, MB_16};
use std::sync::Arc;

/// The smallest `largefloor`, and its default.
pub(crate) const MIN_LARGE_FLOOR: usize = MB_16;

pub struct MemoryAllocator {
    backend: Arc<dyn Backend>,
    stats: AllocationStats,
}

#[derive(Debug, Clone)]
pub struct AllocationConfig {
    pub size: usize,
    pub numa_node: Option<u32>,
    pub page_size: PageSizePreference,
    #[expect(dead_code, reason = "revival seam (TODO #4/5): VirtualAlloc2 can only give WriteBack, so it never reads this")]
    pub memory_type: BufferMemoryType,
    pub zero_memory: bool,
    pub alignment: Option<usize>,       // Custom alignment requirement (must be power of 2)
    /// `numa_node` is required, not preferred (`NUMA_NODE_MANDATORY`): refuse rather than take
    /// another node's pages. Only 1 GiB and 2 MiB pages take it; 4 KiB requests stay preferred.
    pub numa_strict: bool,
}

#[derive(Debug, Clone)]
pub enum PageSizePreference {
    Any,                                    // Let backend decide
    Prefer(PageType),                       // Prefer but fall back
    Require(PageType),                      // Must have or fail
}

#[derive(Debug, Default)]
pub struct AllocationStats {
    pub total_allocations: usize,
    pub total_bytes_allocated: usize,
    pub large_page_allocations: usize,
    pub huge_page_allocations: usize,
}

/// `largechunk` when it is not set. Stitched commits merge into one run per page size, so a
/// small first request costs only calls, and it spreads a node's 2 MiB shortfall over all its
/// threads instead of the last few: on a 2-node box, 3.02 GiB remote over 32 threads at 128 MiB,
/// against 0.8-0.9 GiB on each of 3 at 1 GiB, for 3.9 s of allocation instead of 2.3 (TODO 75,
/// follow-up 2). To be re-checked now that the tests take the span (TODO 83).
pub(crate) const DEFAULT_LARGE_CHUNK: usize = 128 * BYTES_PER_MIB_USIZE;

/// The block-size tunables, checked: `hugechunk`, `largechunk`, `largefloor`.
/// Built by `MemoryAllocationConfig::block_sizing`; the stitched allocator takes all three.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockSizing {
    /// The first size of each 1 GiB-page request, halved on every refusal down to 1 GiB. A power
    /// of two, at least 1 GiB.
    pub huge_chunk: usize,
    /// The first size of each 2 MiB-page request, halved on every refusal down to `large_floor`.
    /// A power of two, at least `large_floor`.
    pub large_chunk: usize,
    /// The smallest 2 MiB-page request: once the node refuses one this small, it is out of 2 MiB
    /// pages. A power of two, 16 MiB ..= 1 GiB.
    pub large_floor: usize,
}

impl BlockSizing {
    pub fn new(huge_chunk: usize, large_chunk: usize, large_floor: usize) -> Result<Self, String> {
        let mib = |b: usize| b / BYTES_PER_MIB_USIZE;
        if !huge_chunk.is_power_of_two() || huge_chunk < HUGE_PAGE_SIZE_USIZE {
            return Err(format!("hugechunk={} MiB must be a power of two of at least 1 GiB", mib(huge_chunk)));
        }
        if !large_floor.is_power_of_two() || !(MIN_LARGE_FLOOR..=HUGE_PAGE_SIZE_USIZE).contains(&large_floor) {
            return Err(format!("largefloor={} MiB must be a power of two from 16 MiB to 1 GiB", mib(large_floor)));
        }
        if !large_chunk.is_power_of_two() || large_chunk < large_floor {
            return Err(format!("largechunk={} MiB must be a power of two of at least largefloor ({} MiB)",
                               mib(large_chunk), mib(large_floor)));
        }
        Ok(Self { huge_chunk, large_chunk, large_floor })
    }

    /// The largest large-page request the allocator makes.
    pub fn largest_request(&self) -> usize {
        self.huge_chunk.max(self.large_chunk)
    }
}

/// Page size level for constraint checking (ordered: Regular < Large < Huge)
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PageSizeLevel {
    Regular = 0,
    Large = 1,
    Huge = 2,
}

impl PageSizeLevel {
    /// Parse page size level from string (matches config format)
    pub fn from_config_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "huge" | "1gb" => PageSizeLevel::Huge,
            "large" | "2mb" => PageSizeLevel::Large,
            _ => PageSizeLevel::Regular, // "regular", "4kb", or default
        }
    }
}

impl MemoryAllocator {
    pub fn new(backend_type: BackendType) -> Result<Self, String> {
        let backend: Arc<dyn Backend> = match backend_type {
            BackendType::Auto => Self::auto_detect_backend()?,
            BackendType::Windows { large_pages } => {
                Arc::new(WindowsBackend::new(large_pages))
            }
        };

        Ok(Self {
            backend,
            stats: AllocationStats::default(),
        })
    }

    fn auto_detect_backend() -> Result<Arc<dyn Backend>, String> {
        // Large pages if we hold SeLockMemoryPrivilege, otherwise regular 4KB.
        if crate::memory::check_large_page_privilege().is_ok() {
            log::info!("Auto-detected Windows large pages backend");
            Ok(Arc::new(WindowsBackend::new(true)))
        } else {
            log::info!("Auto-detected Windows regular backend");
            Ok(Arc::new(WindowsBackend::new(false)))
        }
    }

    pub fn allocate(&mut self, config: &AllocationConfig) -> Result<MemoryBuffer, AllocError> {
        let allocation = self.backend.allocate(config)?;

        // Update statistics
        self.stats.total_allocations += 1;
        self.stats.total_bytes_allocated += allocation.size;

        if allocation.info.uses_large_pages() {
            self.stats.large_page_allocations += 1;
        }
        if allocation.info.uses_huge_pages() {
            self.stats.huge_page_allocations += 1;
        }

        Ok(MemoryBuffer::new(allocation, self.backend.clone()))
    }
}

impl Default for AllocationConfig {
    fn default() -> Self {
        Self {
            size: 1024 * 1024, // 1MB default
            numa_node: None,
            page_size: PageSizePreference::Any,
            memory_type: BufferMemoryType::WriteBack,
            zero_memory: false,
            alignment: None,
            numa_strict: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: usize = BYTES_PER_MIB_USIZE;
    const GIB: usize = HUGE_PAGE_SIZE_USIZE;

    #[test]
    fn block_sizing_and_share_rounding_are_checked() {
        assert!(BlockSizing::new(GIB, GIB, 16 * MIB).is_ok());
        assert!(BlockSizing::new(8 * GIB, 64 * MIB, 64 * MIB).is_ok());
        assert!(BlockSizing::new(512 * MIB, GIB, 16 * MIB).is_err());
        assert!(BlockSizing::new(3 * GIB, GIB, 16 * MIB).is_err());
        assert!(BlockSizing::new(GIB, GIB, 8 * MIB).is_err());
        assert!(BlockSizing::new(GIB, GIB, 2 * GIB).is_err());
        assert!(BlockSizing::new(GIB, 32 * MIB, 64 * MIB).is_err());

        let rounding = |step: &str, hugechunk: &str, largefloor: &str| {
            let config = crate::config::MemoryAllocationConfig {
                share_round_step: step.to_string(),
                huge_chunk: hugechunk.to_string(),
                large_floor: largefloor.to_string(),
                ..Default::default()
            };
            config.share_rounding().map(|r| r.step_bytes as usize)
        };
        assert_eq!(rounding("1GiB", "1GiB", "16MiB"), Ok(GIB));
        assert_eq!(rounding("256MiB", "1GiB", "16MiB"), Ok(256 * MIB));
        assert!(rounding("8MiB", "1GiB", "16MiB").is_err());
        assert!(rounding("48MiB", "1GiB", "32MiB").is_err());
        // No bigger than the largest request: 1 GiB by default, more with a bigger hugechunk.
        assert!(rounding("2GiB", "1GiB", "16MiB").is_err());
        assert_eq!(rounding("4GiB", "4GiB", "16MiB"), Ok(4 * GIB));
    }

    /// An unset `largechunk` is 128 MiB, and never falls below `largefloor`; a set one is taken
    /// as given.
    #[test]
    fn largechunk_defaults_to_128mib() {
        let large_chunk = |largechunk: Option<&str>, largefloor: &str| {
            let config = crate::config::MemoryAllocationConfig {
                large_chunk: largechunk.map(str::to_string),
                large_floor: largefloor.to_string(),
                ..Default::default()
            };
            config.block_sizing().map(|s| s.large_chunk)
        };
        assert_eq!(large_chunk(None, "16MiB"), Ok(128 * MIB));
        assert_eq!(large_chunk(None, "256MiB"), Ok(256 * MIB));
        assert_eq!(large_chunk(Some("1GiB"), "16MiB"), Ok(GIB));
        assert_eq!(large_chunk(Some("64MiB"), "16MiB"), Ok(64 * MIB));
    }
}
