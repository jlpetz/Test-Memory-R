//! What the two per-thread allocators share (TODO 75): which page sizes a run may use, each
//! thread's share and NUMA node, the order 1 GiB and 2 MiB pages are handed out in, and the audit
//! of what backs the memory afterwards.
//!
//! plan-pagesize-pref (`allocator.rs`) makes every request its own `VirtualAlloc2` block, so each
//! one is a power of two no bigger than the thread's gap. stitched (`stitched.rs`) commits into one
//! placeholder slice per thread; neighbouring commits of one page size merge into a single run, so
//! a request can be any multiple of the floor, and the run is cut into power-of-two blocks only
//! when it is handed out. That size rule, and how a request is made, are all that differ: both
//! implement [`Fill`] and run the same [`fill_ladder`].
#![warn(clippy::undocumented_unsafe_blocks)]

use std::collections::{BTreeMap, HashMap};
use std::ffi::c_void;

use windows::Win32::System::ProcessStatus::{PSAPI_WORKING_SET_EX_INFORMATION, QueryWorkingSetEx};
use windows::Win32::System::Threading::GetCurrentProcess;

use crate::BlockInfo;
use crate::constants::{BYTES_PER_MIB_USIZE, HUGE_PAGE_SIZE_USIZE, LARGE_PAGE_SIZE_USIZE, PAGE_SIZE_4KB};
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
/// order; threads on the same node draw from one pool, and fairness is judged within it.
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
    /// Ask for `len` bytes of `page` pages for thread `i`.
    fn request(&mut self, i: usize, len: usize, page: PageSizeLevel) -> Result<Grant, String>;
}

/// Hand out `page` pages one request at a time, starting at `top` bytes per request, to whichever
/// thread holds the fewest bytes so far. Threads a node shorted on 1 GiB pages are then topped up
/// with 2 MiB ones before anyone gets more. A refusal drops that node to the largest power of two
/// below the refused size, and the same thread retries. Once a `floor` request is refused, the node
/// is out of that page size.
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
/// Every request is traced at info: a ✅ line per size granted, logged whenever the node's size
/// drops and at the end, and a ❌ line per refusal.
pub(crate) fn fill_ladder(
    fill: &mut impl Fill,
    page: PageSizeLevel,
    top: usize,
    floor: usize,
    who: &str,
) -> Result<(), String> {
    let label = page_label(page);
    let mut rung: HashMap<Option<u32>, usize> = HashMap::new();
    // Grants not yet logged, per node: request size -> count.
    let mut tally: HashMap<Option<u32>, BTreeMap<usize, usize>> = HashMap::new();
    let mut dry: Vec<Option<u32>> = Vec::new();
    // When each thread was last granted a request (0 = never).
    let mut served = vec![0usize; fill.thread_count()];
    let mut grants = 0usize;
    loop {
        let pick = (0..fill.thread_count())
            .filter(|&i| fill.remaining(i) >= floor && !dry.contains(&fill.numa_node(i)))
            .min_by_key(|&i| (fill.filled(i), bytes_above(fill, i, page), served[i]));
        let Some(i) = pick else {
            break;
        };
        let node = fill.numa_node(i);
        let size = *rung.entry(node).or_insert(top);
        let len = fill.request_len(i, size, floor)?;
        match fill.request(i, len, page)? {
            Grant::Done => {
                grants += 1;
                served[i] = grants;
                *tally.entry(node).or_default().entry(len).or_default() += 1;
            }
            Grant::Refused(code) => {
                log_granted(who, node, label, tally.remove(&node));
                let mut smaller = size;
                while smaller >= len {
                    smaller /= 2;
                }
                let next = if smaller < floor {
                    dry.push(node);
                    format!("out of {label} pages")
                } else {
                    rung.insert(node, smaller);
                    format!("retrying at {}", size_label(smaller))
                };
                log::info!(
                    "❌ {who}: {}: {} ({label}) for thread {} refused: {}; {next}",
                    node_label(node),
                    size_label(len),
                    fill.thread_id(i),
                    describe_error(code)
                );
            }
        }
    }
    let mut left: Vec<_> = tally.into_iter().collect();
    left.sort_unstable_by_key(|&(node, _)| node);
    for (node, sizes) in left {
        log_granted(who, node, label, Some(sizes));
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
fn log_granted(who: &str, node: Option<u32>, label: &str, sizes: Option<BTreeMap<usize, usize>>) {
    for (len, count) in sizes.into_iter().flatten().rev() {
        log::info!("✅ {who}: {}: {count} × {} granted ({label})", node_label(node), size_label(len));
    }
}

/// Fill what each thread still needs with 4 KiB pages, thread by thread: there is no shared pool
/// to split fairly. Returns the first refusal, as (thread index, code); a 4 KiB refusal is the
/// commit limit, and what to do about it is the caller's call. Traced like `fill_ladder`.
pub(crate) fn fill_regular(fill: &mut impl Fill, who: &str) -> Result<Option<(usize, u32)>, String> {
    let label = page_label(PageSizeLevel::Regular);
    let mut tally: BTreeMap<Option<u32>, BTreeMap<usize, usize>> = BTreeMap::new();
    let mut refused = None;
    'threads: for i in 0..fill.thread_count() {
        while fill.remaining(i) > 0 {
            let len = fill.request_len(i, usize::MAX, PAGE_SIZE_4KB)?;
            match fill.request(i, len, PageSizeLevel::Regular)? {
                Grant::Done => *tally.entry(fill.numa_node(i)).or_default().entry(len).or_default() += 1,
                Grant::Refused(code) => {
                    log::info!(
                        "❌ {who}: {}: {} ({label}) for thread {} refused: {}",
                        node_label(fill.numa_node(i)),
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
        log_granted(who, node, label, Some(sizes));
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
// Page audit
// ---------------------------------------------------------------------------------------------

/// A stretch on 1 GiB or 2 MiB pages to audit, for `thread_id`, wanted on `numa_node`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct AuditRun {
    pub addr: usize,
    pub len: usize,
    pub numa_node: Option<u32>,
    pub thread_id: usize,
}

/// What the working set says backs the 1 GiB- and 2 MiB-page memory.
#[derive(Debug, Clone, Default)]
pub(crate) struct PageAudit {
    /// Addresses checked: one per 2 MiB across every run.
    pub sampled: usize,
    /// Not resident. Large pages are nonpaged, so any of these is wrong.
    pub not_resident: usize,
    /// Resident without the LargePage bit: the silent 4 KiB downgrade.
    pub not_large: usize,
    /// Resident on a different node from the one the thread asked for.
    pub wrong_node: usize,
    pub first_problem: Option<String>,
}

impl PageAudit {
    pub fn is_clean(&self) -> bool {
        self.not_resident == 0 && self.not_large == 0 && self.wrong_node == 0
    }
}

/// Ask the working set what backs `runs`, one sample per 2 MiB. `MEM_LARGE_PAGES` should deliver
/// or fail outright, but a silent 4 KiB downgrade is the trap the large-page alignment rule warns
/// about, so this checks behaviour rather than trusting the flags.
pub(crate) fn audit_pages(runs: &[AuditRun]) -> Result<PageAudit, String> {
    let samples: Vec<(usize, Option<u32>, usize)> = runs
        .iter()
        .flat_map(|run| {
            (run.addr..run.addr + run.len)
                .step_by(LARGE_PAGE_SIZE_USIZE)
                .map(|addr| (addr, run.numa_node, run.thread_id))
        })
        .collect();

    let mut audit = PageAudit::default();
    for batch in samples.chunks(1 << 16) {
        let mut info: Vec<PSAPI_WORKING_SET_EX_INFORMATION> = batch
            .iter()
            .map(|&(addr, _, _)| PSAPI_WORKING_SET_EX_INFORMATION {
                VirtualAddress: addr as *mut c_void,
                ..Default::default()
            })
            .collect();
        let bytes = u32::try_from(std::mem::size_of_val(info.as_slice()))
            .map_err(|_| "page audit: batch too large".to_string())?;
        // SAFETY: `info` is an initialised buffer of exactly `bytes` bytes that lives across the
        // call; the kernel fills the attribute half of each entry in place and keeps no reference
        // after returning. `GetCurrentProcess` is a pseudo-handle, never closed.
        unsafe { QueryWorkingSetEx(GetCurrentProcess(), info.as_mut_ptr().cast(), bytes) }
            .map_err(|e| format!("page audit: QueryWorkingSetEx failed: {e}"))?;

        for (entry, &(addr, want_node, thread_id)) in info.iter().zip(batch) {
            // SAFETY: `Flags` is the whole-word view of the attribute bitfield; every bit pattern
            // is a valid `usize`.
            let flags = unsafe { entry.VirtualAttributes.Flags };
            // PSAPI_WORKING_SET_EX_BLOCK: Valid bit 0, Node bits 16..22, LargePage bit 23.
            let resident = flags & 1 != 0;
            let large = (flags >> 23) & 1 != 0;
            let node = ((flags >> 16) & 0x3F) as u32;
            audit.sampled += 1;
            let problem = if !resident {
                audit.not_resident += 1;
                Some("not resident".to_string())
            } else if !large {
                audit.not_large += 1;
                Some("no LargePage bit".to_string())
            } else if want_node.is_some_and(|n| n != node) {
                audit.wrong_node += 1;
                Some(format!("on node {node}, asked for {want_node:?}"))
            } else {
                None
            };
            if let Some(problem) = problem
                && audit.first_problem.is_none()
            {
                audit.first_problem =
                    Some(format!("thread {thread_id}: {addr:#x} {problem} (flags {flags:#x})"));
            }
        }
    }
    Ok(audit)
}

/// Audit `runs` and log the outcome: one line when clean, a warning naming the first problem
/// otherwise, nothing when no memory is on large pages.
pub(crate) fn log_audit(who: &str, runs: &[AuditRun]) {
    if runs.is_empty() {
        return;
    }
    match audit_pages(runs) {
        Ok(audit) if audit.is_clean() => {
            log::info!("{who}: page audit clean ({} samples)", audit.sampled)
        }
        Ok(audit) => log::warn!(
            "{who}: page audit of {} samples: {} not resident, {} without LargePage, {} on \
             another node; first: {}",
            audit.sampled,
            audit.not_resident,
            audit.not_large,
            audit.wrong_node,
            audit.first_problem.unwrap_or_default()
        ),
        Err(e) => log::warn!("{who}: page audit unavailable: {e}"),
    }
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
        fn request(&mut self, i: usize, len: usize, _page: PageSizeLevel) -> Result<Grant, String> {
            let left = self.pool.get_mut(&self.nodes[i]).expect("every node has a pool");
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
        }
    }

    /// Two nodes, two threads each, one shared order: each node splits its own pool, one page at
    /// a time, and running dry on one node does not stop the other.
    #[test]
    fn nodes_split_their_own_pools_one_page_apart() {
        let nodes = [Some(0), Some(1), Some(0), Some(1)];
        let mut fill = pools(&nodes, 8 * GIB, &[(Some(0), 3), (Some(1), 9)]);
        fill_ladder(&mut fill, PageSizeLevel::Huge, GIB, GIB, "test").unwrap();
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
        fill_ladder(&mut fill, PageSizeLevel::Huge, 4 * GIB, GIB, "test").unwrap();
        let asked: Vec<(usize, usize, bool)> =
            fill.requests.iter().map(|&(i, len, ok)| (i, len / GIB, ok)).collect();
        assert_eq!(
            asked,
            vec![(0, 4, true), (1, 4, false), (1, 2, true), (1, 2, false), (1, 1, false)]
        );
        assert_eq!(fill.filled, vec![4 * GIB, 2 * GIB]);
    }

    #[test]
    fn prev_power_of_two_rounds_down() {
        assert_eq!(prev_power_of_two(1), 1);
        assert_eq!(prev_power_of_two(GIB), GIB);
        assert_eq!(prev_power_of_two(GIB + 1), GIB);
        assert_eq!(prev_power_of_two(3 * GIB), 2 * GIB);
    }
}
