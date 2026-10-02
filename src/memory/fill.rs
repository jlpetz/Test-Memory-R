//! What the two per-thread allocators share (TODO 75): which page sizes a run may use, each
//! thread's share and NUMA node, the order 1 GiB and 2 MiB pages are handed out in, and the
//! warning when some of it is on another node.
//!
//! plan-pagesize-pref (`allocator.rs`) makes every request its own `VirtualAlloc2` block, so each
//! one is a power of two no bigger than the thread's gap. stitched (`stitched.rs`) commits into one
//! placeholder slice per thread; neighbouring commits of one page size merge into a single run, so
//! a request can be any multiple of the floor, and the run is cut into power-of-two blocks only
//! when it is handed out. That size rule, and how a request is made, are all that differ: both
//! implement [`Fill`] and run the same passes ([`fill_all`]).
//!
//! 1 GiB and 2 MiB requests name their node strictly (`NUMA_NODE_MANDATORY`), so every block's
//! node is the one recorded for it. A preferred node is only a preference: once the node is out,
//! Windows quietly takes the pages from another one, even splitting one request across nodes
//! (`../numa-test/FINDINGS.md`). 4 KiB requests refuse the strict form (87), so they stay
//! preferred, and their node is the one asked for, not one known.
#![warn(clippy::undocumented_unsafe_blocks)]

use std::collections::{BTreeMap, HashMap};

use windows::Win32::System::Threading::GetNumaHighestNodeNumber;

use crate::BlockInfo;
use crate::constants::{BYTES_PER_MIB_USIZE, HUGE_PAGE_SIZE_USIZE, PAGE_SIZE_4KB};
use crate::cpu_topology::get_numa_node_for_cpu;
use crate::memory::allocator::PageSizeLevel;
use crate::memory::backend::describe_error;

// ---------------------------------------------------------------------------------------------
// Inputs
// ---------------------------------------------------------------------------------------------

/// One thread's share of the layout and the node of the CPU it runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ThreadShare {
    pub thread_id: usize,
    pub bytes: usize,
    pub numa_node: u32,
}

/// Each thread's share and node, in thread order. The node comes from the CPU the thread is
/// pinned to (`cpu_list`), not from its index.
pub(crate) fn thread_shares(
    thread_blocks: &HashMap<usize, Vec<BlockInfo>>,
    runtime_config: &crate::RuntimeConfig,
) -> Vec<ThreadShare> {
    let mut shares: Vec<ThreadShare> = thread_blocks
        .iter()
        .map(|(&thread_id, blocks)| {
            let cpu_id = runtime_config
                .cpu_list
                .as_ref()
                .and_then(|list| list.get(thread_id))
                .copied()
                .unwrap_or(thread_id);
            let numa_node = get_numa_node_for_cpu(cpu_id);
            let bytes: usize = blocks.iter().map(|b| b.size_bytes).sum();
            log::debug!(
                "Thread {thread_id} → CPU {cpu_id} → NUMA {numa_node} ({} MiB)",
                bytes / BYTES_PER_MIB_USIZE
            );
            ThreadShare { thread_id, bytes, numa_node }
        })
        .collect();
    shares.sort_by_key(|s| s.thread_id);
    shares
}

/// NUMA nodes on the machine, numbered `0..count`: the nodes remote passes can draw from.
pub(crate) fn numa_node_count() -> u32 {
    let mut highest = 0u32;
    // SAFETY: writes one u32; on failure `highest` stays 0, one node.
    let _ = unsafe { GetNumaHighestNodeNumber(&mut highest) };
    highest + 1
}

/// The page sizes a run may use, smallest and largest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PageSizes {
    pub min: PageSizeLevel,
    pub max: PageSizeLevel,
}

impl PageSizes {
    pub fn allows(self, page: PageSizeLevel) -> bool {
        self.min <= page && page <= self.max
    }
}

/// `minpage` ..= `maxpage`, capped at 4 KiB without SeLockMemoryPrivilege. A `minpage` above
/// 4 KiB without the privilege, or above `maxpage`, is an error.
pub(crate) fn allowed_page_sizes(runtime_config: &crate::RuntimeConfig) -> Result<PageSizes, String> {
    let alloc = &runtime_config.memory_allocation;
    let min = PageSizeLevel::from_config_str(&alloc.min_page_size);
    let mut max = PageSizeLevel::from_config_str(&alloc.max_page_size);
    if min > max {
        return Err(format!(
            "minpage={} is above maxpage={}",
            alloc.min_page_size, alloc.max_page_size
        ));
    }
    if !runtime_config.large_pages_available {
        if min > PageSizeLevel::Regular {
            return Err(format!(
                "minpage={} needs large pages, but SeLockMemoryPrivilege is not available \
                 (tmr.exe --setup-large-pages, elevated)",
                alloc.min_page_size
            ));
        }
        max = PageSizeLevel::Regular;
    }
    Ok(PageSizes { min, max })
}

// ---------------------------------------------------------------------------------------------
// The fill order
// ---------------------------------------------------------------------------------------------

/// A request answered without a hard error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Grant {
    Done,
    /// The thread's pool is out of this page size, or large pages are refused for want of the
    /// privilege (`backend::is_exhaustion`). Carries the Win32 code for the log.
    Refused(u32),
}

/// One allocator's threads, as [`fill_ladder`] sees them. Thread `i` is the `i`th in a fixed
/// order. `numa_node(i)` is the node of the CPU it runs on, its home; a pass's [`Source`] says
/// which node's pages it asks for, and threads drawing on one node share that pool.
pub(crate) trait Fill {
    fn thread_count(&self) -> usize;
    fn thread_id(&self, i: usize) -> usize;
    fn numa_node(&self, i: usize) -> Option<u32>;
    /// Bytes thread `i` holds so far.
    fn filled(&self, i: usize) -> usize;
    /// Bytes thread `i` holds on `page` pages.
    fn bytes_at(&self, i: usize, page: PageSizeLevel) -> usize;
    /// Bytes thread `i` still wants.
    fn remaining(&self, i: usize) -> usize;
    /// What thread `i` asks for with the ladder at `rung`: at most `rung` and `remaining(i)`, at
    /// least `floor`. Only asked of threads with `remaining(i) >= floor`.
    fn request_len(&self, i: usize, rung: usize, floor: usize) -> Result<usize, String>;
    /// Ask for `len` bytes of `page` pages from `node` for thread `i`: strictly that node for
    /// 1 GiB and 2 MiB pages, preferred for 4 KiB (`None`: any node).
    fn request(&mut self, i: usize, len: usize, page: PageSizeLevel, node: Option<u32>) -> Result<Grant, String>;
    /// Whether thread `i` can take `page` pages at all now. Stitched cannot once a smaller page
    /// size sits below the next address: 1 GiB pages need a 1 GiB-aligned one.
    fn can_take(&self, _i: usize, _page: PageSizeLevel) -> bool {
        true
    }
    /// Bytes from thread `i`'s next address up to the next `page` boundary: what it must take in
    /// smaller pages before `page` pages fit again. 0 where requests are separate blocks.
    fn gap_to_boundary(&self, _i: usize, _page: PageSizeLevel) -> usize {
        0
    }
}

/// Which node's pages a pass asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Source {
    /// Each thread's own node.
    Local,
    /// The node `step` after each thread's own, of `nodes`. Threads with no node sit it out.
    Remote { step: u32, nodes: u32 },
}

impl Source {
    /// The node a thread on `home` draws from in this pass; `None` leaves it out of the pass.
    fn pool(self, home: Option<u32>) -> Option<Option<u32>> {
        match (self, home) {
            (Source::Local, home) => Some(home),
            (Source::Remote { step, nodes }, Some(home)) => Some(Some((home + step) % nodes)),
            (Source::Remote { .. }, None) => None,
        }
    }

    /// The pool, for the trace: "NUMA 1", or "NUMA 1 for NUMA 0" when remote.
    fn label(self, pool: Option<u32>) -> String {
        match (self, pool) {
            (Source::Remote { step, nodes }, Some(pool)) => {
                format!("NUMA {pool} for NUMA {}", (pool + nodes - step % nodes) % nodes)
            }
            _ => node_label(pool),
        }
    }
}

/// Every pass, in order: 1 GiB then 2 MiB pages from each thread's own node, then the same from
/// each other node in turn, then 4 KiB pages (preferred home node) for whatever is left. Page
/// size gives way before locality: a thread takes its own node's 2 MiB pages before another
/// node's 1 GiB ones, and no thread goes remote until every node has filled its own threads as
/// far as it can. A stitched thread whose local 2 MiB pages end off a 1 GiB boundary is padded up
/// to it before each remote 1 GiB pass (`pad_to_huge_boundary`). Returns the first 4 KiB refusal,
/// as `fill_regular` does.
pub(crate) fn fill_all(
    fill: &mut impl Fill,
    pages: PageSizes,
    sizing: &crate::memory::allocator::BlockSizing,
    nodes: u32,
    who: &str,
) -> Result<Option<(usize, u32)>, String> {
    let sources = std::iter::once(Source::Local).chain((1..nodes).map(|step| Source::Remote { step, nodes }));
    for source in sources {
        if pages.allows(PageSizeLevel::Huge) {
            if source != Source::Local && pages.allows(PageSizeLevel::Large) {
                pad_to_huge_boundary(fill, source, sizing.large_floor, who)?;
            }
            fill_ladder(fill, PageSizeLevel::Huge, sizing.huge_chunk, HUGE_PAGE_SIZE_USIZE, source, who)?;
        }
        if pages.allows(PageSizeLevel::Large) {
            fill_ladder(fill, PageSizeLevel::Large, sizing.large_chunk, sizing.large_floor, source, who)?;
        }
    }
    if pages.allows(PageSizeLevel::Regular) {
        return fill_regular(fill, who);
    }
    Ok(None)
}

/// Before a remote 1 GiB pass: a thread whose next address is off a 1 GiB boundary takes 2 MiB
/// pages from that pass's node up to the boundary, as long as a 1 GiB page still fits after them,
/// so the pass can give it 1 GiB pages too. (A stitched thread lands there when its own node's
/// last 2 MiB pages came in odd-sized pieces; without the pad it would get no remote 1 GiB pages
/// at all.) Nothing is freed: the pad is part of what the thread needs anyway, and where it
/// lands is fixed, the thread's own next addresses. A refused pad leaves the thread on 2 MiB pages.
fn pad_to_huge_boundary(fill: &mut impl Fill, source: Source, floor: usize, who: &str) -> Result<(), String> {
    let label = page_label(PageSizeLevel::Large);
    for i in 0..fill.thread_count() {
        let gap = fill.gap_to_boundary(i, PageSizeLevel::Huge);
        let Some(pool) = source.pool(fill.numa_node(i)) else {
            continue;
        };
        if gap == 0 || fill.remaining(i) < gap + HUGE_PAGE_SIZE_USIZE {
            continue;
        }
        // In as many requests as the thread's free slots take (`request_len`).
        let mut padded = 0;
        while padded < gap {
            let len = fill.request_len(i, gap - padded, floor)?;
            if let Grant::Refused(code) = fill.request(i, len, PageSizeLevel::Large, pool)? {
                log::info!(
                    "❌ {who}: {}: {} ({label}) for thread {}, up to its next 1 GiB boundary, refused: {}; it stays on 2 MiB pages",
                    source.label(pool),
                    size_label(len),
                    fill.thread_id(i),
                    describe_error(code)
                );
                break;
            }
            padded += len;
        }
        if padded == gap {
            log::info!(
                "✅ {who}: {}: {} ({label}) for thread {}, up to its next 1 GiB boundary",
                source.label(pool),
                size_label(gap),
                fill.thread_id(i)
            );
        }
    }
    Ok(())
}

/// Hand out `page` pages one request at a time, starting at `top` bytes per request, to whichever
/// thread holds the fewest bytes so far. Threads a node shorted on 1 GiB pages are then topped up
/// with 2 MiB ones before anyone gets more. A refusal drops that pool to the largest power of two
/// below the refused size, and the same thread retries. Once a `floor` request is refused, the
/// pool is out of that page size. Pools are per node: `source` says which node each thread asks.
///
/// Ties between equally filled threads go to the one with less on bigger pages, then to the one
/// served longest ago. So once a 2 MiB top-up has levelled the threads 1 GiB pages shorted, those
/// threads still lead every round after it. If the 2 MiB pool runs out partway through a round,
/// the threads it skips are ones that got more 1 GiB pages, not the same threads again.
///
/// At a 1 GiB `top` for 1 GiB pages this is plain round-robin, so a node's threads end at most
/// one page apart. A bigger `top` makes fewer requests and lets the threads end about one `top`
/// apart. Nothing granted is ever released to even the split out.
///
/// Every request is traced at info: a ✅ line per size granted, logged whenever the pool's size
/// drops and at the end, and a ❌ line per refusal.
pub(crate) fn fill_ladder(
    fill: &mut impl Fill,
    page: PageSizeLevel,
    top: usize,
    floor: usize,
    source: Source,
    who: &str,
) -> Result<(), String> {
    let label = page_label(page);
    let mut rung: HashMap<Option<u32>, usize> = HashMap::new();
    // Grants not yet logged, per pool: request size -> count.
    let mut tally: HashMap<Option<u32>, BTreeMap<usize, usize>> = HashMap::new();
    let mut dry: Vec<Option<u32>> = Vec::new();
    // When each thread was last granted a request (0 = never).
    let mut served = vec![0usize; fill.thread_count()];
    let mut grants = 0usize;
    loop {
        let pick = (0..fill.thread_count())
            .filter(|&i| {
                fill.remaining(i) >= floor
                    && fill.can_take(i, page)
                    && source.pool(fill.numa_node(i)).is_some_and(|pool| !dry.contains(&pool))
            })
            .min_by_key(|&i| (fill.filled(i), bytes_above(fill, i, page), served[i]));
        let Some(i) = pick else {
            break;
        };
        let Some(pool) = source.pool(fill.numa_node(i)) else {
            break;
        };
        let size = *rung.entry(pool).or_insert(top);
        let len = fill.request_len(i, size, floor)?;
        match fill.request(i, len, page, pool)? {
            Grant::Done => {
                grants += 1;
                served[i] = grants;
                *tally.entry(pool).or_default().entry(len).or_default() += 1;
            }
            Grant::Refused(code) => {
                log_granted(who, &source.label(pool), label, tally.remove(&pool));
                let mut smaller = size;
                while smaller >= len {
                    smaller /= 2;
                }
                let next = if smaller < floor {
                    dry.push(pool);
                    format!("out of {label} pages")
                } else {
                    rung.insert(pool, smaller);
                    format!("retrying at {}", size_label(smaller))
                };
                log::info!(
                    "❌ {who}: {}: {} ({label}) for thread {} refused: {}; {next}",
                    source.label(pool),
                    size_label(len),
                    fill.thread_id(i),
                    describe_error(code)
                );
            }
        }
    }
    let mut left: Vec<_> = tally.into_iter().collect();
    left.sort_unstable_by_key(|&(pool, _)| pool);
    for (pool, sizes) in left {
        log_granted(who, &source.label(pool), label, Some(sizes));
    }
    Ok(())
}

/// What thread `i` holds on pages bigger than `page`: the tie-break in `fill_ladder`.
fn bytes_above(fill: &impl Fill, i: usize, page: PageSizeLevel) -> usize {
    match page {
        PageSizeLevel::Huge => 0,
        PageSizeLevel::Large => fill.bytes_at(i, PageSizeLevel::Huge),
        PageSizeLevel::Regular => {
            fill.bytes_at(i, PageSizeLevel::Huge) + fill.bytes_at(i, PageSizeLevel::Large)
        }
    }
}

/// One ✅ line per request size granted, largest first.
fn log_granted(who: &str, pool: &str, label: &str, sizes: Option<BTreeMap<usize, usize>>) {
    for (len, count) in sizes.into_iter().flatten().rev() {
        log::info!("✅ {who}: {pool}: {count} × {} granted ({label})", size_label(len));
    }
}

/// Fill what each thread still needs with 4 KiB pages, thread by thread: there is no shared pool
/// to split fairly. The home node is preferred, not required (4 KiB pages refuse the strict
/// form). Returns the first refusal, as (thread index, code); a 4 KiB refusal is the commit
/// limit, and what to do about it is the caller's call. Traced like `fill_ladder`.
pub(crate) fn fill_regular(fill: &mut impl Fill, who: &str) -> Result<Option<(usize, u32)>, String> {
    let label = page_label(PageSizeLevel::Regular);
    let mut tally: BTreeMap<Option<u32>, BTreeMap<usize, usize>> = BTreeMap::new();
    let mut refused = None;
    'threads: for i in 0..fill.thread_count() {
        let home = fill.numa_node(i);
        while fill.remaining(i) > 0 {
            let len = fill.request_len(i, usize::MAX, PAGE_SIZE_4KB)?;
            match fill.request(i, len, PageSizeLevel::Regular, home)? {
                Grant::Done => *tally.entry(home).or_default().entry(len).or_default() += 1,
                Grant::Refused(code) => {
                    log::info!(
                        "❌ {who}: {}: {} ({label}) for thread {} refused: {}",
                        node_label(home),
                        size_label(len),
                        fill.thread_id(i),
                        describe_error(code)
                    );
                    refused = Some((i, code));
                    break 'threads;
                }
            }
        }
    }
    for (node, sizes) in tally {
        log_granted(who, &format!("{} (preferred)", node_label(node)), label, Some(sizes));
    }
    Ok(refused)
}

/// Appended to a hard refusal: the one setting that gets past a 1 GiB-page request the machine
/// cannot serve at all.
pub(crate) fn hard_error_hint(page: PageSizeLevel) -> &'static str {
    match page {
        PageSizeLevel::Huge => "; if this machine has no 1 GiB pages, run with maxpage=large",
        _ => "",
    }
}

/// The largest power of two at most `len` (`len > 0`).
pub(crate) fn prev_power_of_two(len: usize) -> usize {
    1 << (usize::BITS - 1 - len.leading_zeros())
}

pub(crate) fn page_label(page: PageSizeLevel) -> &'static str {
    match page {
        PageSizeLevel::Huge => "1GB huge",
        PageSizeLevel::Large => "2MB large",
        PageSizeLevel::Regular => "4KB regular",
    }
}

pub(crate) fn node_label(node: Option<u32>) -> String {
    node.map_or_else(|| "any node".to_string(), |n| format!("NUMA {n}"))
}

/// A request size for the log: whole GiB when it is one, else MiB.
pub(crate) fn size_label(bytes: usize) -> String {
    if bytes.is_multiple_of(HUGE_PAGE_SIZE_USIZE) {
        format!("{} GiB", bytes / HUGE_PAGE_SIZE_USIZE)
    } else {
        format!("{} MiB", bytes / BYTES_PER_MIB_USIZE)
    }
}

// ---------------------------------------------------------------------------------------------
// Remote memory
// ---------------------------------------------------------------------------------------------

/// Large-page bytes threads got from a node other than their own: (home, source) -> (bytes,
/// threads). Built by each allocator from its records, which the strict requests made exact.
pub(crate) type RemoteMemory = BTreeMap<(u32, u32), (usize, usize)>;

/// One warning when any thread's 1 GiB or 2 MiB pages came from another node: how much, between
/// which nodes, and what to change if it was not intended. Remote memory is still tested, but
/// over the socket link, so those threads run slower. Testing across nodes can be the point, so
/// this warns rather than refuses.
pub(crate) fn warn_remote(who: &str, remote: &RemoteMemory) {
    if remote.is_empty() {
        return;
    }
    let total: usize = remote.values().map(|&(bytes, _)| bytes).sum();
    let pairs: Vec<String> = remote
        .iter()
        .map(|(&(home, source), &(bytes, threads))| {
            format!(
                "{:.2} GiB for {threads} NUMA {home} thread(s) from NUMA {source}",
                bytes as f64 / HUGE_PAGE_SIZE_USIZE as f64
            )
        })
        .collect();
    log::warn!(
        "⚠️ {who}: {:.2} GiB of large-page memory is on another node ({}): those threads' own \
         nodes ran short, and they reach it over the socket link, slower. To keep it local, \
         spread the threads (cpu-stride=even) or lower memory=",
        total as f64 / HUGE_PAGE_SIZE_USIZE as f64,
        pairs.join("; ")
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: usize = HUGE_PAGE_SIZE_USIZE;

    /// Threads with equal targets drawing on per-node pools of 1 GiB pages; requests are
    /// recorded in order as (thread index, bytes, granted).
    struct Pools {
        nodes: Vec<Option<u32>>,
        filled: Vec<usize>,
        target: usize,
        pool: HashMap<Option<u32>, usize>,
        requests: Vec<(usize, usize, bool)>,
        /// The node each request named.
        asked: Vec<Option<u32>>,
    }

    impl Fill for Pools {
        fn thread_count(&self) -> usize {
            self.nodes.len()
        }
        fn thread_id(&self, i: usize) -> usize {
            i
        }
        fn bytes_at(&self, i: usize, page: PageSizeLevel) -> usize {
            if page == PageSizeLevel::Huge { self.filled[i] } else { 0 }
        }
        fn numa_node(&self, i: usize) -> Option<u32> {
            self.nodes[i]
        }
        fn filled(&self, i: usize) -> usize {
            self.filled[i]
        }
        fn remaining(&self, i: usize) -> usize {
            self.target - self.filled[i]
        }
        fn request_len(&self, i: usize, rung: usize, _floor: usize) -> Result<usize, String> {
            Ok(prev_power_of_two(rung.min(self.remaining(i))))
        }
        fn request(&mut self, i: usize, len: usize, _page: PageSizeLevel, node: Option<u32>) -> Result<Grant, String> {
            self.asked.push(node);
            let left = self.pool.get_mut(&node).expect("every node has a pool");
            let granted = *left >= len;
            self.requests.push((i, len, granted));
            if !granted {
                return Ok(Grant::Refused(crate::memory::backend::ERROR_NO_SYSTEM_RESOURCES));
            }
            *left -= len;
            self.filled[i] += len;
            Ok(Grant::Done)
        }
    }

    fn pools(nodes: &[Option<u32>], target: usize, pages: &[(Option<u32>, usize)]) -> Pools {
        Pools {
            nodes: nodes.to_vec(),
            filled: vec![0; nodes.len()],
            target,
            pool: pages.iter().map(|&(node, n)| (node, n * GIB)).collect(),
            requests: Vec::new(),
            asked: Vec::new(),
        }
    }

    /// Two nodes, two threads each, one shared order: each node splits its own pool, one page at
    /// a time, and running dry on one node does not stop the other.
    #[test]
    fn nodes_split_their_own_pools_one_page_apart() {
        let nodes = [Some(0), Some(1), Some(0), Some(1)];
        let mut fill = pools(&nodes, 8 * GIB, &[(Some(0), 3), (Some(1), 9)]);
        fill_ladder(&mut fill, PageSizeLevel::Huge, GIB, GIB, Source::Local, "test").unwrap();
        assert_eq!(fill.filled, vec![2 * GIB, 5 * GIB, GIB, 4 * GIB]);
        // Each node's single refusal ended it; nothing else was refused.
        let refused: Vec<usize> = fill.requests.iter().filter(|r| !r.2).map(|r| r.0).collect();
        assert_eq!(refused, vec![2, 3]);
    }

    /// A refused rung halves for that node and the same thread asks again. The rung only drops
    /// on a refusal, so the empty pool costs two more: 2 GiB, then 1 GiB, the floor.
    #[test]
    fn a_refusal_halves_the_rung_and_the_same_thread_retries() {
        let mut fill = pools(&[Some(0); 2], 8 * GIB, &[(Some(0), 6)]);
        fill_ladder(&mut fill, PageSizeLevel::Huge, 4 * GIB, GIB, Source::Local, "test").unwrap();
        let asked: Vec<(usize, usize, bool)> =
            fill.requests.iter().map(|&(i, len, ok)| (i, len / GIB, ok)).collect();
        assert_eq!(
            asked,
            vec![(0, 4, true), (1, 4, false), (1, 2, true), (1, 2, false), (1, 1, false)]
        );
        assert_eq!(fill.filled, vec![4 * GIB, 2 * GIB]);
    }

    /// Node 0 cannot cover its threads; node 1 has pages to spare. Local first: every node fills
    /// its own threads, and only then do node 0's threads ask node 1, by name.
    #[test]
    fn remote_passes_come_after_every_local_one() {
        let nodes = [Some(0), Some(0), Some(1), Some(1)];
        let mut fill = pools(&nodes, 3 * GIB, &[(Some(0), 4), (Some(1), 9)]);
        let remote = Source::Remote { step: 1, nodes: 2 };
        for source in [Source::Local, remote] {
            fill_ladder(&mut fill, PageSizeLevel::Huge, GIB, GIB, source, "test").unwrap();
        }
        assert_eq!(fill.filled, vec![3 * GIB; 4]);
        // Node 0's last two pages came from node 1, asked for by name after the local passes.
        let from_node_1: Vec<usize> = fill.requests.iter().zip(&fill.asked)
            .filter(|&(&(i, _, ok), &node)| ok && node == Some(1) && fill.nodes[i] == Some(0))
            .map(|(&(i, _, _), _)| i)
            .collect();
        assert_eq!(from_node_1, vec![0, 1]);
        let first_remote = fill.asked.iter().zip(&fill.requests)
            .position(|(&node, &(i, _, _))| node != fill.nodes[i])
            .unwrap();
        assert!(fill.requests[..first_remote].iter().all(|&(i, _, _)| fill.asked[i].is_some()));
        assert!(fill.asked[..first_remote].iter().zip(&fill.requests).all(|(&node, &(i, _, _))| node == fill.nodes[i]));
        assert_eq!(remote.label(Some(1)), "NUMA 1 for NUMA 0");
    }

    #[test]
    fn prev_power_of_two_rounds_down() {
        assert_eq!(prev_power_of_two(1), 1);
        assert_eq!(prev_power_of_two(GIB), GIB);
        assert_eq!(prev_power_of_two(GIB + 1), GIB);
        assert_eq!(prev_power_of_two(3 * GIB), 2 * GIB);
    }
}
