use crate::memory::backend::{AllocError, Backend, BackendType, WindowsBackend, is_exhaustion};
use crate::memory::buffer::{MemoryBuffer, PageType};
use crate::memory::buffer::MemoryType as BufferMemoryType;
use crate::memory::fill::{self, Fill, Grant, PageSizes, ThreadShare};
use crate::{BlockInfo, AllocationBlock};
use crate::constants::{HUGE_PAGE_SIZE_USIZE, LARGE_PAGE_SIZE_USIZE, BYTES_PER_MIB_USIZE, KB, MB_16, bytes_to_gib_f64};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

/// The smallest `largefloor`, and its default.
pub(crate) const MIN_LARGE_FLOOR: usize = MB_16;

/// `VirtualAlloc` granularity: the alignment for a block with no large pages.
const ALLOCATION_GRANULARITY: usize = 64 * KB;

/// Log prefix for plan-pagesize-pref.
const WHO: &str = "plan-pagesize-pref";

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

/// Which allocator fills the threads' shares (`allocator=`)
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AllocationStrategy {
    /// Every block its own `VirtualAlloc2` request: 1 GiB pages, then 2 MiB, then 4 KiB, each
    /// request sized to fit one thread's gap (the default)
    PlanPageSizePref,
    /// One placeholder reservation carved into one contiguous VA span per thread, filled
    /// 1 GiB → 2 MiB → 4 KiB (`memory::stitched`)
    Stitched,
}

impl AllocationStrategy {
    /// `largechunk` when it is not set. stitched's commits merge into one run per page size, so
    /// a small first request costs only calls, and it spreads a node's 2 MiB shortfall over all
    /// its threads instead of the last few: on a 2-node box, 3.02 GiB remote over 32 threads at
    /// 128 MiB, against 0.8-0.9 GiB on each of 3 at 1 GiB, for 3.9 s of allocation instead of 2.3
    /// (TODO 75, follow-up 2). plan-pagesize-pref's requests are its blocks, so it keeps 1 GiB; it
    /// already splits each gap into power-of-two pieces. Both to be re-checked once the tests are
    /// reworked (TODO 83).
    pub fn default_large_chunk(self) -> usize {
        match self {
            AllocationStrategy::PlanPageSizePref => HUGE_PAGE_SIZE_USIZE,
            AllocationStrategy::Stitched => 128 * BYTES_PER_MIB_USIZE,
        }
    }
}

impl std::str::FromStr for AllocationStrategy {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "plan-pagesize-pref" | "planpagesizepref" => Ok(AllocationStrategy::PlanPageSizePref),
            "stitched" => Ok(AllocationStrategy::Stitched),
            _ => Err(format!("Invalid allocation strategy: '{}'. Valid options: plan-pagesize-pref, stitched", s)),
        }
    }
}

impl std::fmt::Display for AllocationStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AllocationStrategy::PlanPageSizePref => write!(f, "plan-pagesize-pref"),
            AllocationStrategy::Stitched => write!(f, "stitched"),
        }
    }
}

/// The block-size tunables, checked: `hugechunk`, `largechunk`, `largefloor`.
/// Built by `MemoryAllocationConfig::block_sizing`; both allocators take all three.
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

    /// The largest large-page request either allocator makes.
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

    /// Allocate every thread's share with the chosen allocator, as power-of-two blocks.
    pub fn chunk_allocate_planned(
        &mut self,
        thread_blocks: &HashMap<usize, Vec<BlockInfo>>,
        runtime_config: &crate::RuntimeConfig,
        strategy: AllocationStrategy,
    ) -> Result<HashMap<usize, Vec<AllocationBlock>>, String> {
        if strategy == AllocationStrategy::Stitched {
            return crate::memory::stitched::chunk_allocate_stitched(thread_blocks, runtime_config);
        }
        let sizing = runtime_config.memory_allocation.block_sizing()?;
        let pages = fill::allowed_page_sizes(runtime_config)?;
        let shares = fill::thread_shares(thread_blocks, runtime_config);
        self.fill_blocks(&shares, &sizing, pages, fill::numa_node_count())
    }

    /// plan-pagesize-pref: fill each thread's share with 1 GiB pages, then 2 MiB, then 4 KiB
    /// (as `minpage`/`maxpage` allow), in the passes of `fill::fill_all`: each thread's own node
    /// first, then the other `nodes`. Every request is one block, sized to the thread's remaining
    /// gap. A thread the allowed page sizes do not cover comes up short, with a warning; one that
    /// gets nothing fails the allocation.
    fn fill_blocks(
        &mut self,
        shares: &[ThreadShare],
        sizing: &BlockSizing,
        pages: PageSizes,
        nodes: u32,
    ) -> Result<HashMap<usize, Vec<AllocationBlock>>, String> {
        let homes: std::collections::BTreeSet<u32> = shares.iter().map(|s| s.numa_node).collect();
        log::info!("{WHO}: {} threads on {} of {} NUMA node(s); 1 GiB pages from {} per request, 2 MiB from {} down to {}; page sizes {:?}..={:?}",
                  shares.len(), homes.len(), nodes,
                  fill::size_label(sizing.huge_chunk), fill::size_label(sizing.large_chunk),
                  fill::size_label(sizing.large_floor), pages.min, pages.max);

        let mut fill = BlockFill {
            allocator: self,
            threads: shares.iter()
                .map(|&share| ThreadFill { share, filled: 0, blocks: Vec::new() })
                .collect(),
        };
        // A refused 4 KiB request is traced there; the threads it leaves short are warned about below.
        fill::fill_all(&mut fill, pages, sizing, nodes, WHO)?;

        // Each thread against its own target, with its page-size shares
        for thread in &fill.threads {
            let (thread_id, target, got) = (thread.share.thread_id, thread.share.bytes, thread.filled);
            let remote = thread.remote_bytes();
            let shares = format!("1GB {:.2}GB, 2MB {:.2}GB, 4KB {:.2}GB{}",
                                 bytes_to_gib_f64(thread.bytes_at(PageSizeLevel::Huge) as u64),
                                 bytes_to_gib_f64(thread.bytes_at(PageSizeLevel::Large) as u64),
                                 bytes_to_gib_f64(thread.bytes_at(PageSizeLevel::Regular) as u64),
                                 if remote > 0 { format!("; {:.2}GB remote", bytes_to_gib_f64(remote as u64)) } else { String::new() });
            if got < target {
                log::warn!("{WHO}: Thread {}: {:.2}GB of {:.2}GB target ({}) — ⚠️ short by {}MB: page sizes {:?}..={:?} ran out",
                         thread_id, bytes_to_gib_f64(got as u64), bytes_to_gib_f64(target as u64),
                         shares, (target - got) / BYTES_PER_MIB_USIZE, pages.min, pages.max);
            } else {
                log::info!("{WHO}: Thread {}: {:.2}GB of {:.2}GB target ({})",
                         thread_id, bytes_to_gib_f64(got as u64), bytes_to_gib_f64(target as u64), shares);
            }
        }

        let mut remote = fill::RemoteMemory::new();
        for thread in &fill.threads {
            let mut from: BTreeMap<u32, usize> = BTreeMap::new();
            for block in thread.blocks.iter().filter(|b| b.uses_large_pages()) {
                if block.info().numa_node != thread.share.numa_node {
                    *from.entry(block.info().numa_node).or_default() += block.size();
                }
            }
            for (source, bytes) in from {
                let entry = remote.entry((thread.share.numa_node, source)).or_default();
                entry.0 += bytes;
                entry.1 += 1;
            }
        }
        fill::warn_remote(WHO, &remote);

        let empty: Vec<usize> = fill.threads.iter()
            .filter(|t| t.blocks.is_empty())
            .map(|t| t.share.thread_id)
            .collect();
        if !empty.is_empty() {
            return Err(format!("{WHO}: thread(s) {:?} got no memory with page sizes {:?}..={:?}. \
                                Large pages may be exhausted or fragmented: restart, or allow 4 KiB pages with minpage=regular",
                               empty, pages.min, pages.max));
        }

        Ok(fill.threads.into_iter()
            .map(|thread| {
                let thread_id = thread.share.thread_id;
                let blocks = thread.blocks.into_iter()
                    .map(|buffer| AllocationBlock {
                        block_info: BlockInfo { size_bytes: buffer.size(), thread_id },
                        buffer,
                        joins_next: false, // each block is its own VA range
                    })
                    .collect();
                (thread_id, blocks)
            })
            .collect())
    }
}

/// One thread's blocks so far under plan-pagesize-pref.
#[derive(Debug)]
struct ThreadFill {
    share: ThreadShare,
    /// Bytes in `blocks`. No request is bigger than `share.bytes - filled`.
    filled: usize,
    blocks: Vec<MemoryBuffer>,
}

impl ThreadFill {
    /// Bytes on 1 GiB or 2 MiB pages from a node other than the thread's own. Those nodes are
    /// known: the requests named them strictly. 4 KiB pages only prefer the home node.
    fn remote_bytes(&self) -> usize {
        self.blocks.iter()
            .filter(|b| b.uses_large_pages() && b.info().numa_node != self.share.numa_node)
            .map(MemoryBuffer::size)
            .sum()
    }

    fn bytes_at(&self, page: PageSizeLevel) -> usize {
        self.blocks.iter()
            .filter(|b| match page {
                PageSizeLevel::Huge => b.uses_huge_pages(),
                PageSizeLevel::Large => b.uses_large_pages() && !b.uses_huge_pages(),
                PageSizeLevel::Regular => !b.uses_large_pages(),
            })
            .map(MemoryBuffer::size)
            .sum()
    }
}

/// plan-pagesize-pref's side of `fill::Fill`. Every request is its own `VirtualAlloc2` block, a
/// power of two (the bandwidth and latency tests still split windows on that,
/// `prepare_blocks_for_extent`; the correctness tests don't need it, TODO 76) and never bigger
/// than the gap left in the thread it is for: nothing is over-allocated, so
/// nothing is freed. (The pooled phases this replaced asked against a node's whole deficit, then
/// freed blocks no thread had room for: TODO 75 A.)
struct BlockFill<'a> {
    allocator: &'a mut MemoryAllocator,
    threads: Vec<ThreadFill>,
}

impl Fill for BlockFill<'_> {
    fn thread_count(&self) -> usize {
        self.threads.len()
    }

    fn thread_id(&self, i: usize) -> usize {
        self.threads[i].share.thread_id
    }

    fn numa_node(&self, i: usize) -> Option<u32> {
        Some(self.threads[i].share.numa_node)
    }

    fn filled(&self, i: usize) -> usize {
        self.threads[i].filled
    }

    fn bytes_at(&self, i: usize, page: PageSizeLevel) -> usize {
        self.threads[i].bytes_at(page)
    }

    fn remaining(&self, i: usize) -> usize {
        self.threads[i].share.bytes - self.threads[i].filled
    }

    fn request_len(&self, i: usize, rung: usize, _floor: usize) -> Result<usize, String> {
        Ok(fill::prev_power_of_two(rung.min(self.remaining(i))))
    }

    fn request(&mut self, i: usize, len: usize, page: PageSizeLevel, node: Option<u32>) -> Result<Grant, String> {
        let thread = &mut self.threads[i];
        // The alignment is what gets the page size (TMR-APP CLAUDE.md): never drop it.
        let (page_size, alignment) = match page {
            PageSizeLevel::Huge => (PageSizePreference::Require(PageType::Huge(len)), HUGE_PAGE_SIZE_USIZE),
            PageSizeLevel::Large => (PageSizePreference::Require(PageType::Large(len)), LARGE_PAGE_SIZE_USIZE),
            PageSizeLevel::Regular => (PageSizePreference::Prefer(PageType::Regular(len)), ALLOCATION_GRANULARITY),
        };
        let config = AllocationConfig {
            size: len,
            numa_node: node,
            page_size,
            memory_type: BufferMemoryType::WriteBack,
            zero_memory: true,
            alignment: Some(alignment),
            numa_strict: page != PageSizeLevel::Regular,
        };
        match self.allocator.allocate(&config) {
            Ok(buffer) => {
                log::debug!("{WHO}: Thread {}: {} block allocated ({}, {})",
                          thread.share.thread_id, fill::size_label(len), fill::page_label(page), fill::node_label(node));
                thread.filled += len;
                thread.blocks.push(buffer);
                Ok(Grant::Done)
            }
            Err(AllocError { code: Some(code), .. }) if is_exhaustion(code) => Ok(Grant::Refused(code)),
            Err(e) => Err(format!("{WHO}: {} of {} pages for thread {} failed: {e}{}",
                                  fill::size_label(len), fill::page_label(page), thread.share.thread_id,
                                  fill::hard_error_hint(page))),
        }
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
    use crate::memory::backend::{BackendAllocation, ERROR_INVALID_PARAMETER, ERROR_NO_SYSTEM_RESOURCES};
    use crate::memory::buffer::BufferInfo;
    use std::sync::Mutex;

    const MIB: usize = BYTES_PER_MIB_USIZE;
    const GIB: usize = HUGE_PAGE_SIZE_USIZE;

    /// One node's page pools: `huge_pages` 1 GiB pages and `large_bytes` of 2 MiB pages, refused
    /// once short; 4 KiB pages are unlimited.
    #[derive(Debug, Default)]
    struct NodePool {
        huge_pages: usize,
        large_bytes: usize,
    }

    /// Per-node page pools, indexed by node. A 1 GiB or 2 MiB request must name its node strictly,
    /// as Windows would otherwise serve it from another one. Addresses are fake and never
    /// dereferenced.
    #[derive(Debug, Default)]
    struct Pools {
        nodes: Vec<NodePool>,
        next_addr: usize,
        /// Every request in order: page size, bytes, granted, node asked.
        requests: Vec<(PageSizeLevel, usize, bool, u32)>,
        frees: usize,
        /// Answer every 1 GiB-page request with this code instead.
        huge_error: Option<u32>,
    }

    #[derive(Debug, Default)]
    struct PoolBackend(Mutex<Pools>);

    impl PoolBackend {
        fn with(huge_pages: usize, large_bytes: usize) -> Arc<Self> {
            Self::with_nodes(&[(huge_pages, large_bytes)])
        }

        fn with_nodes(nodes: &[(usize, usize)]) -> Arc<Self> {
            let nodes = nodes.iter().map(|&(huge_pages, large_bytes)| NodePool { huge_pages, large_bytes }).collect();
            Arc::new(Self(Mutex::new(Pools { nodes, ..Default::default() })))
        }

        fn pools(&self) -> std::sync::MutexGuard<'_, Pools> {
            self.0.lock().unwrap()
        }

        fn refused(&self, page: PageSizeLevel) -> Vec<usize> {
            self.pools().requests.iter()
                .filter(|&&(p, _, granted, _)| p == page && !granted)
                .map(|&(_, size, _, _)| size)
                .collect()
        }
    }

    impl Backend for PoolBackend {
        fn allocate(&self, config: &AllocationConfig) -> Result<BackendAllocation, AllocError> {
            let mut s = self.pools();
            let size = config.size;
            let (page, page_type) = match &config.page_size {
                PageSizePreference::Require(PageType::Huge(_)) => (PageSizeLevel::Huge, PageType::Huge(size)),
                PageSizePreference::Require(PageType::Large(_)) => (PageSizeLevel::Large, PageType::Large(size)),
                _ => (PageSizeLevel::Regular, PageType::Regular(size)),
            };
            if page == PageSizeLevel::Huge && let Some(code) = s.huge_error {
                return Err(AllocError { code: Some(code), message: format!("error {code}") });
            }
            assert_eq!(config.numa_strict, page != PageSizeLevel::Regular, "large pages name their node strictly");
            let node = config.numa_node.unwrap_or(0);
            let Some(pool) = s.nodes.get_mut(node as usize) else {
                return Err(AllocError { code: Some(ERROR_INVALID_PARAMETER), message: "no such node".to_string() });
            };
            let granted = match page {
                PageSizeLevel::Huge if pool.huge_pages * GIB >= size => {
                    pool.huge_pages -= size / GIB;
                    true
                }
                PageSizeLevel::Large if pool.large_bytes >= size => {
                    pool.large_bytes -= size;
                    true
                }
                PageSizeLevel::Regular => true,
                _ => false,
            };
            s.requests.push((page, size, granted, node));
            if !granted {
                return Err(AllocError {
                    code: Some(ERROR_NO_SYSTEM_RESOURCES),
                    message: "error 1450 (no contiguous physical memory)".to_string(),
                });
            }
            let addr = s.next_addr.max(1 << 40).next_multiple_of(config.alignment.unwrap_or(ALLOCATION_GRANULARITY));
            s.next_addr = addr + size;
            Ok(BackendAllocation {
                ptr: addr as *mut u8,
                size,
                info: BufferInfo { numa_node: config.numa_node.unwrap_or(0), page_type },
            })
        }

        fn free(&self, _allocation: BackendAllocation) -> Result<(), String> {
            self.pools().frees += 1;
            Ok(())
        }

        fn name(&self) -> &'static str {
            "pool fake"
        }
    }

    fn sizing(huge_chunk: usize) -> BlockSizing {
        BlockSizing::new(huge_chunk, GIB, MIN_LARGE_FLOOR).unwrap()
    }

    fn pages(min_page: &str) -> PageSizes {
        PageSizes { min: PageSizeLevel::from_config_str(min_page), max: PageSizeLevel::Huge }
    }

    /// `targets` as one node's thread shares, in thread order.
    fn shares(targets: &[usize]) -> Vec<ThreadShare> {
        targets.iter().enumerate()
            .map(|(thread_id, &bytes)| ThreadShare { thread_id, bytes, numa_node: 0 })
            .collect()
    }

    fn fill_blocks(backend: &Arc<PoolBackend>, targets: &[usize], huge_chunk: usize, min_page: &str)
        -> Result<HashMap<usize, Vec<AllocationBlock>>, String> {
        let mut allocator = MemoryAllocator { backend: backend.clone(), stats: AllocationStats::default() };
        allocator.fill_blocks(&shares(targets), &sizing(huge_chunk), pages(min_page), 1)
    }

    /// plan-pagesize-pref on one node: each thread's (1 GiB-page bytes, 2 MiB, 4 KiB), in
    /// thread order, after checking every thread got exactly its target in power-of-two blocks.
    fn fill(backend: &Arc<PoolBackend>, targets: &[usize], huge_chunk: usize, min_page: &str) -> Vec<(usize, usize, usize)> {
        let blocks = fill_blocks(backend, targets, huge_chunk, min_page).unwrap();
        assert_eq!(backend.pools().frees, 0, "nothing is freed while the blocks are held");
        let shares = targets.iter().enumerate().map(|(thread_id, &target)| {
            let mut share = (0, 0, 0);
            for block in &blocks[&thread_id] {
                let size = block.buffer.size();
                assert!(size.is_power_of_two() && size >= MIN_LARGE_FLOOR, "thread {thread_id}: {size:#x}");
                assert_eq!(block.block_info.size_bytes, size);
                assert_eq!(block.block_info.thread_id, thread_id);
                if block.buffer.uses_huge_pages() {
                    share.0 += size;
                } else if block.buffer.uses_large_pages() {
                    share.1 += size;
                } else {
                    share.2 += size;
                }
            }
            assert_eq!(share.0 + share.1 + share.2, target, "thread {thread_id} is not exact");
            share
        }).collect();
        let held: usize = blocks.values().map(Vec::len).sum();
        drop(blocks);
        assert_eq!(backend.pools().frees, held);
        shares
    }

    fn huge_pages(shares: &[(usize, usize, usize)]) -> Vec<usize> {
        shares.iter().map(|s| s.0 / GIB).collect()
    }

    /// The 2026-09-28 run (TODO 75 A): four 12.47 GiB threads, 33 free 1 GiB pages. The pooled
    /// phases took a 1 GiB page no thread had room for and freed it, and handed the huge pages
    /// out consecutively. Sized to each thread's gap, the threads end exact and 9/8/8/8 on 1 GiB
    /// pages, and the one refusal is the request that found the pool empty.
    #[test]
    fn fill_ends_each_thread_exact_and_spreads_huge_pages() {
        let backend = PoolBackend::with(33, usize::MAX);
        let target = 12 * GIB + 480 * MIB;
        let shares = fill(&backend, &[target; 4], GIB, "large");
        assert_eq!(huge_pages(&shares), vec![9, 8, 8, 8]);
        assert!(shares.iter().all(|s| s.2 == 0));
        assert_eq!(backend.pools().nodes[0].huge_pages, 0);
        assert_eq!(backend.refused(PageSizeLevel::Huge), vec![GIB]);
        assert!(backend.refused(PageSizeLevel::Large).is_empty());
    }

    /// The user's 2026-10-02 run: four 14 GiB threads, 23 free 1 GiB pages. 1 GiB requests split
    /// them 6/6/6/5; starting at 4 GiB, as the allocator used to, the first threads drain the
    /// pool first and it ends 8/6/5/4.
    #[test]
    fn hugechunk_trades_the_split_for_fewer_requests() {
        let backend = PoolBackend::with(23, usize::MAX);
        assert_eq!(huge_pages(&fill(&backend, &[14 * GIB; 4], GIB, "large")), vec![6, 6, 6, 5]);
        let backend = PoolBackend::with(23, usize::MAX);
        assert_eq!(huge_pages(&fill(&backend, &[14 * GIB; 4], 4 * GIB, "large")), vec![8, 6, 5, 4]);
    }

    /// The user's case (2026-10-02 review): 1 GiB pages run out with thread 3 one short, then
    /// 2 MiB pages run out partway through a round. Thread 3 is topped up first and keeps the lead
    /// in the rounds after (ties go to less on 1 GiB pages), so the 4 KiB pages land on threads
    /// that got more 1 GiB pages. Ties to the thread served longest ago gave (2,1,1), (2,1,1),
    /// (2,0,2), (1,1,2): thread 3 short on both.
    #[test]
    fn the_thread_short_of_huge_pages_leads_the_large_rounds() {
        let backend = PoolBackend::with(7, 3 * GIB);
        let shares: Vec<(usize, usize, usize)> = fill(&backend, &[4 * GIB; 4], GIB, "regular")
            .into_iter()
            .map(|(huge, large, regular)| (huge / GIB, large / GIB, regular / GIB))
            .collect();
        assert_eq!(shares, vec![(2, 1, 1), (2, 0, 2), (2, 0, 2), (1, 2, 1)]);
    }

    /// No 1 GiB pages and 1.25 GiB of 2 MiB ones for two 3 GiB threads: the least-filled thread
    /// asks next, and 4 KiB pages finish both.
    #[test]
    fn fill_falls_back_to_regular_per_gap() {
        let backend = PoolBackend::with(0, GIB + 256 * MIB);
        let shares = fill(&backend, &[3 * GIB; 2], GIB, "regular");
        assert_eq!(shares, vec![(0, GIB, 2 * GIB), (0, 256 * MIB, 2 * GIB + 768 * MIB)]);
        assert_eq!(backend.pools().nodes[0].large_bytes, 0);
    }

    /// With 4 KiB pages ruled out, a thread the large pages do not cover comes up short: logged,
    /// not over-allocated.
    #[test]
    fn fill_leaves_threads_short_without_regular() {
        let backend = PoolBackend::with(1, 512 * MIB);
        let blocks = fill_blocks(&backend, &[2 * GIB; 2], GIB, "large").unwrap();
        let got = |t: usize| blocks[&t].iter().map(|b| b.buffer.size()).sum::<usize>();
        assert_eq!((got(0), got(1)), (GIB, 512 * MIB));
        assert!(backend.pools().requests.iter().all(|&(p, _, _, _)| p != PageSizeLevel::Regular));
    }

    /// Two nodes, two threads each, 3 GiB per thread. Node 0 has 2 × 1 GiB and 1 GiB of 2 MiB
    /// pages, not enough for its threads; node 1 has plenty. Node 0's threads take their own
    /// node's 2 MiB pages before node 1's 1 GiB ones, and only go remote once node 0 is out of
    /// both, asking node 1 by name, so each block's node is the one recorded.
    #[test]
    fn short_node_threads_go_remote_last_and_by_name() {
        let backend = PoolBackend::with_nodes(&[(2, GIB), (12, usize::MAX)]);
        let mut allocator = MemoryAllocator { backend: backend.clone(), stats: AllocationStats::default() };
        let shares: Vec<ThreadShare> = [0, 0, 1, 1].iter().enumerate()
            .map(|(thread_id, &numa_node)| ThreadShare { thread_id, bytes: 3 * GIB, numa_node })
            .collect();
        let blocks = allocator.fill_blocks(&shares, &sizing(GIB), pages("large"), 2).unwrap();
        // (local, remote) GiB per thread, from each block's recorded node.
        let split: Vec<(usize, usize)> = shares.iter().map(|share| {
            let mut split = (0, 0);
            for block in &blocks[&share.thread_id] {
                if block.buffer.info().numa_node == share.numa_node {
                    split.0 += block.buffer.size() / GIB;
                } else {
                    split.1 += block.buffer.size() / GIB;
                }
            }
            split
        }).collect();
        assert_eq!(split, vec![(2, 1), (1, 2), (3, 0), (3, 0)]);
        // Local passes first: node 0's last request is its 2 MiB floor refusal, so nothing asked
        // node 0 again once node 0's threads had gone to node 1.
        let requests = backend.pools().requests.clone();
        let last_node0 = requests.iter().rposition(|&(_, _, _, node)| node == 0).unwrap();
        let node0_large_refused = requests.iter().position(|&(page, size, granted, node)|
            page == PageSizeLevel::Large && size == MIN_LARGE_FLOOR && !granted && node == 0).unwrap();
        assert_eq!(last_node0, node0_large_refused, "node 0's last request is its 2 MiB floor refusal");
    }

    /// A refusal that is not exhaustion (87: a bad request) fails the allocation instead of
    /// stepping down, and nothing granted is kept.
    #[test]
    fn a_bad_request_is_an_error_not_a_step_down() {
        let backend = PoolBackend::with(8, usize::MAX);
        backend.pools().huge_error = Some(ERROR_INVALID_PARAMETER);
        let err = fill_blocks(&backend, &[2 * GIB; 2], GIB, "large").unwrap_err();
        assert!(err.contains("error 87") && err.contains("maxpage=large"), "{err}");
    }

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

    /// An unset `largechunk` follows the allocator, and never falls below `largefloor`; a set one
    /// is taken as given.
    #[test]
    fn largechunk_defaults_per_allocator() {
        let large_chunk = |allocator: &str, largechunk: Option<&str>, largefloor: &str| {
            let config = crate::config::MemoryAllocationConfig {
                allocation_strategy: allocator.to_string(),
                large_chunk: largechunk.map(str::to_string),
                large_floor: largefloor.to_string(),
                ..Default::default()
            };
            config.block_sizing().map(|s| s.large_chunk)
        };
        assert_eq!(large_chunk("plan-pagesize-pref", None, "16MiB"), Ok(GIB));
        assert_eq!(large_chunk("stitched", None, "16MiB"), Ok(128 * MIB));
        assert_eq!(large_chunk("stitched", None, "256MiB"), Ok(256 * MIB));
        assert_eq!(large_chunk("stitched", Some("1GiB"), "16MiB"), Ok(GIB));
        assert_eq!(large_chunk("plan-pagesize-pref", Some("64MiB"), "16MiB"), Ok(64 * MIB));
    }
}
