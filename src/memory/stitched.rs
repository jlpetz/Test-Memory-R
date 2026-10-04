//! Stitched placeholder allocator, TMR's only one since TODO 76: one reservation, one contiguous
//! VA span per thread (TODO 70, brought into TMR by TODO 75 C).
//!
//! A chunk allocator (plan-pagesize-pref, removed in TODO 76) hands each thread a list of
//! independent `VirtualAlloc2` blocks, wherever the OS puts them. This one reserves a single
//! placeholder region up front, carves it into one
//! 1 GiB-aligned slice per thread, and fills each slice from its base upward: 1 GiB HUGE pages
//! first, 2 MiB LARGE pages once those run out, then 4 KiB REGULAR pages. Each thread's memory is
//! one contiguous VA range, 1 GiB pages at its base, with the unused tail of its slice left as
//! placeholder.
//!
//! # Committing: the replace bug and the workaround
//!
//! The natural way to commit into a placeholder is one `MEM_REPLACE_PLACEHOLDER` call
//! ([`CommitMethod::Replace`]). With large pages that is not safe on Windows Server 2025
//! (10.0.26100). Once the kernel refuses such a replace for lack of contiguous memory (1450), the
//! next `VirtualFree` of that placeholder, whole or split, bugchecks 0x139 / 0xE
//! (`INVALID_REFERENCE_COUNT`) in `nt!MiUnlockAndDereferenceVad`. It has happened with 1 GiB pages
//! (the E5 run, on release) and with 2 MiB pages (2026-09-24, `threads=4 size=16GiB`, on the split
//! that halves the retry: `MiDeletePartialVad`). 2 MiB pages just need more memory pressure before
//! they are refused.
//!
//! So by default ([`CommitMethod::FreeThenAllocate`]) every slot, at every page size, is committed
//! like this instead:
//!
//! 1. split the slot off as its own placeholder (`MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER`),
//! 2. release that placeholder outright, leaving a momentary hole in the VA,
//! 3. allocate at exactly that address as an ordinary allocation.
//!
//! If step 3 is refused, the hole is re-reserved as a placeholder so a smaller commit can fill it.
//! A refusal on this path never touches a VAD we hold, and it was shown live to be safe with 1 GiB
//! pages (refused, re-reserved, later split and freed). 4 KiB pages never showed the bug. But a
//! refused 4 KiB replace would be followed by teardown freeing that same placeholder, which is the
//! crashing sequence, so they take this path too. Between steps 2 and 3 another allocation in the
//! process could land in the hole; that shows up as the re-reserve failing and is reported as an
//! error, not retried.
//!
//! `Replace` is kept whole for when the kernel is fixed: one call per slot and no VA hole. Do not
//! select it on a build that still has the bug; any refusal takes the machine down.
//!
//! # Fairness: commit-size ladders, least-filled first
//!
//! The order is [`fill::fill_ladder`]; this file supplies the commits. HUGE and LARGE pages are each handed out one commit at a time, and the next commit always goes
//! to the thread holding the fewest committed bytes; equally filled threads take turns. Commit
//! sizes come down a power-of-two ladder. They start at `huge_chunk` / `large_chunk` (in TMR,
//! 1 GiB and 128 MiB by default). When a commit is refused, that node steps down to the largest rung below it and
//! the same thread retries at the same address. The node is out of that page size once the bottom
//! rung is refused: 1 GiB for HUGE, `large_floor` (16 MiB by default) for LARGE.
//!
//! At the default 1 GiB `huge_chunk`, HUGE is plain round-robin, one page per thread per round:
//! every thread gets its `r`-th page before any thread gets its `r+1`-th. When a node runs out, its
//! threads end at most one page apart. Committing each thread's whole HUGE share in one call
//! instead lets the first threads drain the pool and leaves the rest with none. A bigger
//! `huge_chunk` cuts the calls on machines with hundreds of GiB, at the price of a coarser split:
//! threads can end about one chunk apart.
//!
//! Nothing committed is released afterwards to level the split. A released 1 GiB page goes back to
//! the whole system and may not come back, and TMR never frees memory to break a block into
//! smaller ones (TMR-APP `CLAUDE.md`).
//!
//! LARGE then tops up the threads HUGE left short before anyone else gets more. Each thread's
//! locked total ends within about one `large_chunk` of the others on its node, and so does its
//! 4 KiB remainder, instead of piling up on the highest thread indices.
//!
//! # Handing memory out
//!
//! Committed runs become power-of-two [`AllocationBlock`]s whose `MemoryBuffer`s all
//! point back at the arena as their `Backend`. Dropping a buffer releases nothing; the arena
//! releases every piece (each commit and each placeholder is its own VAD and needs its own
//! `VirtualFree`) once the last buffer drops its `Arc`.
//!
//! All OS calls go through [`VmOps`], so the fill logic runs against a fake in the tests below: the
//! exhaustion paths are exactly the ones that are dangerous to exercise for real.
#![warn(clippy::undocumented_unsafe_blocks)]

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::Arc;

use crate::constants::{
    BYTES_PER_MIB_USIZE, HUGE_PAGE_SIZE_USIZE, LARGE_PAGE_SIZE_USIZE, bytes_to_gib_f64,
};
use crate::memory::allocator::{AllocationConfig, BlockSizing, PageSizeLevel};
use crate::memory::backend::{
    AllocError, Backend, BackendAllocation, ERROR_INVALID_ADDRESS, NUMA_NODE_MANDATORY,
    describe_error, is_exhaustion,
};
use crate::memory::buffer::{BufferInfo, MemoryBuffer, PageType};
use crate::memory::fill::{
    self, Fill, Grant, PageSizes, node_label, page_label, prev_power_of_two, size_label,
};
use crate::{AllocationBlock, BlockInfo};

use windows::Win32::Foundation::GetLastError;
use windows::Win32::System::Memory::{
    MEM_ADDRESS_REQUIREMENTS, MEM_COMMIT, MEM_EXTENDED_PARAMETER, MEM_EXTENDED_PARAMETER_TYPE,
    MEM_LARGE_PAGES, MEM_PRESERVE_PLACEHOLDER, MEM_RELEASE, MEM_REPLACE_PLACEHOLDER, MEM_RESERVE,
    MEM_RESERVE_PLACEHOLDER, MemExtendedParameterAddressRequirements,
    MemExtendedParameterAttributeFlags, MemExtendedParameterNumaNode, PAGE_NOACCESS,
    PAGE_READWRITE, VIRTUAL_ALLOCATION_TYPE, VIRTUAL_FREE_TYPE, VirtualAlloc2, VirtualFree,
};
use windows::Win32::System::SystemServices::{
    MEM_COALESCE_PLACEHOLDERS, MEM_EXTENDED_PARAMETER_NONPAGED_HUGE,
    MEM_EXTENDED_PARAMETER_NONPAGED_LARGE, MEM_EXTENDED_PARAMETER_TYPE_BITS,
};

const HUGE: usize = HUGE_PAGE_SIZE_USIZE;
const LARGE: usize = LARGE_PAGE_SIZE_USIZE;

/// Log prefix.
const WHO: &str = "stitched";

// ---------------------------------------------------------------------------------------------
// Request
// ---------------------------------------------------------------------------------------------

/// How a slot is committed, at every page size. See the module docs for why the default is not
/// the obvious one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitMethod {
    /// `MEM_REPLACE_PLACEHOLDER` straight into the split slot: one call, and the VA never leaves
    /// our hands. **Bugchecks 10.0.26100** (0x139) as soon as a 1 GiB or 2 MiB replace is
    /// refused and that placeholder is later freed or split. For kernels with the fix.
    #[cfg_attr(not(test), expect(dead_code, reason = "dormant until the kernel's refused-replace bug is fixed (module docs, TMR-APP CLAUDE.md)"))]
    Replace,
    /// Split the slot off, release it, allocate at the address: two more calls per slot and a
    /// momentary VA hole, but a refusal leaves no damaged placeholder behind.
    FreeThenAllocate,
}

/// One thread's share of the arena.
#[derive(Debug, Clone)]
pub struct StitchThread {
    pub thread_id: usize,
    pub bytes: usize,
    /// Preferred node for this thread's pages (`MemExtendedParameterNumaNode`); `None` lets the OS
    /// choose. Threads on the same node draw from one pool, and fairness is judged within it.
    pub numa_node: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct StitchRequest {
    /// Slices are laid out in this order, from the bottom of the reservation up.
    pub threads: Vec<StitchThread>,
    pub min_page: PageSizeLevel,
    pub max_page: PageSizeLevel,
    /// `LowestStartingAddress` for the reservation; `None` places it anywhere.
    pub lowest_address: Option<usize>,
    pub commit_method: CommitMethod,
    /// Top of the HUGE ladder: each 1 GiB-page commit is this big until one is refused, then
    /// halved on every refusal down to 1 GiB. A power of two, at least 1 GiB. The default 1 GiB is
    /// the fairest split. Bigger means fewer calls on very large machines, and threads can end
    /// about one chunk apart.
    pub huge_chunk: usize,
    /// Top of the LARGE ladder: each 2 MiB-page commit is this big until one is refused, then
    /// halved on every refusal down to `large_floor`. A power of two, at least `large_floor`.
    /// Roughly how far apart the threads on a node can end in locked bytes.
    pub large_chunk: usize,
    /// Bottom of the LARGE ladder: once a commit this small is refused, the node is out of 2 MiB
    /// pages, and whatever each thread still needs goes to 4 KiB. A power of two, at least
    /// `run_quantum`.
    pub large_floor: usize,
    /// NUMA nodes on the machine (`0..numa_nodes`). Threads short after their own node's 1 GiB and
    /// 2 MiB pages ask the others in turn, by name (`fill::fill_all`).
    pub numa_nodes: u32,
    /// Every thread size is rounded down to a multiple of this and every LARGE commit is one, so
    /// no run (and no `PowerOfTwo` block) is smaller. A power of two, 2 MiB ..= 1 GiB; the default
    /// 16 MiB is TMR's smallest planned block.
    pub run_quantum: usize,
}

impl StitchRequest {
    pub fn new(threads: Vec<StitchThread>) -> Self {
        Self {
            threads,
            min_page: PageSizeLevel::Regular,
            max_page: PageSizeLevel::Huge,
            lowest_address: None,
            // Back to `Replace` once the kernel's refused-replace bug is fixed (module docs).
            commit_method: CommitMethod::FreeThenAllocate,
            huge_chunk: HUGE,
            large_chunk: HUGE,
            large_floor: 16 * BYTES_PER_MIB_USIZE,
            numa_nodes: 1,
            run_quantum: 16 * BYTES_PER_MIB_USIZE,
        }
    }
}

/// The address-space layout a request resolves to, before any OS call.
#[derive(Debug, Clone)]
pub struct StitchPlan {
    /// Distance between thread bases: the largest target rounded up to 1 GiB, which keeps every
    /// base 1 GiB-aligned for HUGE pages.
    pub stride: usize,
    /// Size of the single reservation: `stride` per thread.
    pub reservation: usize,
    /// Per-thread bytes after rounding down to `run_quantum`, in `threads` order.
    pub targets: Vec<usize>,
}

pub fn plan(req: &StitchRequest) -> Result<StitchPlan, String> {
    if req.threads.is_empty() {
        return Err("stitched: no threads to allocate for".to_string());
    }
    if req.min_page > req.max_page {
        return Err(format!(
            "stitched: min page size {:?} is above max page size {:?}",
            req.min_page, req.max_page
        ));
    }
    let quantum = req.run_quantum;
    if !quantum.is_power_of_two() || !(LARGE..=HUGE).contains(&quantum) {
        return Err(format!(
            "stitched: run quantum {quantum:#x} must be a power of two between 2 MiB and 1 GiB"
        ));
    }
    if !req.huge_chunk.is_power_of_two() || req.huge_chunk < HUGE {
        return Err(format!(
            "stitched: huge chunk {:#x} must be a power of two of at least 1 GiB",
            req.huge_chunk
        ));
    }
    if !req.large_floor.is_power_of_two() || req.large_floor < quantum {
        return Err(format!(
            "stitched: large floor {:#x} must be a power of two of at least the run quantum \
             {quantum:#x}",
            req.large_floor
        ));
    }
    if !req.large_chunk.is_power_of_two() || req.large_chunk < req.large_floor {
        return Err(format!(
            "stitched: large chunk {:#x} must be a power of two of at least the large floor {:#x}",
            req.large_chunk, req.large_floor
        ));
    }

    let mut targets = Vec::with_capacity(req.threads.len());
    for (i, thread) in req.threads.iter().enumerate() {
        if req.threads[..i]
            .iter()
            .any(|t| t.thread_id == thread.thread_id)
        {
            return Err(format!(
                "stitched: thread {} is listed twice",
                thread.thread_id
            ));
        }
        let target = thread.bytes / quantum * quantum;
        if target == 0 {
            return Err(format!(
                "stitched: thread {} asked for {} bytes, below the {} MiB run quantum",
                thread.thread_id,
                thread.bytes,
                quantum / BYTES_PER_MIB_USIZE
            ));
        }
        if target != thread.bytes {
            log::debug!(
                "stitched: thread {} rounded down {} -> {} bytes ({} MiB quantum)",
                thread.thread_id,
                thread.bytes,
                target,
                quantum / BYTES_PER_MIB_USIZE
            );
        }
        targets.push(target);
    }

    let largest = targets.iter().copied().max().unwrap_or(0);
    let stride = largest
        .checked_next_multiple_of(HUGE)
        .ok_or("stitched: thread size overflows the address space")?;
    let reservation = stride
        .checked_mul(req.threads.len())
        .ok_or("stitched: reservation size overflows the address space")?;
    Ok(StitchPlan {
        stride,
        reservation,
        targets,
    })
}

// ---------------------------------------------------------------------------------------------
// Report types
// ---------------------------------------------------------------------------------------------

/// A stretch of one thread's span committed at a single page size from a single node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Run {
    /// From the thread's base.
    pub offset: usize,
    pub len: usize,
    pub page: PageSizeLevel,
    /// The node the commits named: required for HUGE and LARGE, preferred for REGULAR.
    pub numa_node: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct ThreadSpan {
    pub thread_id: usize,
    pub numa_node: Option<u32>,
    /// The thread's memory is `[base, base + len)`, contiguous.
    pub base: usize,
    pub len: usize,
    /// What it asked for; `len` falls short only when the allowed page sizes ran out.
    pub target: usize,
    /// Committed runs from `base` upward. Each starts on its own page size's boundary: 1 GiB runs
    /// can follow 2 MiB ones from another node when those end on a whole GiB.
    pub runs: Vec<Run>,
}

impl ThreadSpan {
    /// Bytes on 1 GiB or 2 MiB pages from a node other than the thread's own, which the commits
    /// named strictly. 4 KiB runs only prefer the home node.
    pub fn remote_bytes(&self) -> usize {
        self.runs
            .iter()
            .filter(|r| r.page != PageSizeLevel::Regular && r.numa_node != self.numa_node)
            .map(|r| r.len)
            .sum()
    }

    pub fn bytes_at(&self, page: PageSizeLevel) -> usize {
        self.runs
            .iter()
            .filter(|r| r.page == page)
            .map(|r| r.len)
            .sum()
    }
}

// ---------------------------------------------------------------------------------------------
// OS operations
// ---------------------------------------------------------------------------------------------

/// The six OS operations the arena is built from. [`Win32Vm`] is the real thing; the tests swap in
/// a fake that models the placeholder rules, finite page pools and the refused-replace bug, so
/// refusals can be driven without exhausting anything. Errors are raw Win32 codes.
pub trait VmOps {
    /// Reserve `len` bytes of placeholder VA aligned to `align`, at or above `lowest`.
    fn reserve(&mut self, len: usize, align: usize, lowest: Option<usize>) -> Result<usize, u32>;
    /// Reserve a placeholder at exactly `[addr, addr + len)`, which must be free VA.
    fn reserve_at(&mut self, addr: usize, len: usize) -> Result<(), u32>;
    /// Split `[addr, addr + len)` off the front of the larger placeholder that starts at `addr`.
    fn split(&mut self, addr: usize, len: usize) -> Result<(), u32>;
    /// Commit over the placeholder that is exactly `[addr, addr + len)`. A refusal leaves the
    /// placeholder in place, and on 10.0.26100 a large-page refusal leaves it poisoned: see
    /// [`CommitMethod::Replace`].
    fn replace(
        &mut self,
        addr: usize,
        len: usize,
        page: PageSizeLevel,
        numa: Option<u32>,
    ) -> Result<(), u32>;
    /// Merge the adjacent placeholders that exactly cover `[addr, addr + len)` into one, the VA
    /// staying reserved throughout (`MEM_RELEASE | MEM_COALESCE_PLACEHOLDERS`). The range must
    /// start and end on placeholder edges, else 487 (`../numa-test/FINDINGS.md`, part 5).
    fn coalesce(&mut self, addr: usize, len: usize) -> Result<(), u32>;
    /// Commit an ordinary allocation at exactly `[addr, addr + len)`, which must be free VA.
    fn allocate_at(
        &mut self,
        addr: usize,
        len: usize,
        page: PageSizeLevel,
        numa: Option<u32>,
    ) -> Result<(), u32>;
    /// Release the region (placeholder or allocation) based at `addr`; `len` is for the log.
    fn release(&mut self, addr: usize, len: usize) -> Result<(), u32>;
}

/// `VirtualAlloc2` / `VirtualFree` on the current process. Every call is logged at debug level
/// *before* it is made, so a log that is written through survives a bugcheck on that call.
#[derive(Debug, Default)]
pub struct Win32Vm;

impl Win32Vm {
    /// Same packing as `WindowsBackend::create_extended_param`: the type goes in the low
    /// `MEM_EXTENDED_PARAMETER_TYPE_BITS` of the bitfield.
    fn ext_param(kind: MEM_EXTENDED_PARAMETER_TYPE, value: u64) -> MEM_EXTENDED_PARAMETER {
        let mut param = MEM_EXTENDED_PARAMETER::default();
        param.Anonymous1._bitfield =
            (kind.0 as u64) & ((1_u64 << MEM_EXTENDED_PARAMETER_TYPE_BITS) - 1);
        param.Anonymous2.ULong64 = value;
        param
    }

    /// Page-size attribute plus node: required for HUGE and LARGE (`NUMA_NODE_MANDATORY`),
    /// preferred for REGULAR, which refuses the strict form. Plain values only, no pointers, so
    /// these carry no lifetime hazard.
    fn page_params(page: PageSizeLevel, numa: Option<u32>) -> Vec<MEM_EXTENDED_PARAMETER> {
        let mut params = Vec::with_capacity(2);
        let attribute = match page {
            PageSizeLevel::Huge => Some(MEM_EXTENDED_PARAMETER_NONPAGED_HUGE),
            PageSizeLevel::Large => Some(MEM_EXTENDED_PARAMETER_NONPAGED_LARGE),
            PageSizeLevel::Regular => None,
        };
        if let Some(attribute) = attribute {
            params.push(Self::ext_param(
                MemExtendedParameterAttributeFlags,
                attribute as u64,
            ));
        }
        if let Some(node) = numa {
            let strict = if attribute.is_some() { NUMA_NODE_MANDATORY } else { 0 };
            params.push(Self::ext_param(
                MemExtendedParameterNumaNode,
                u64::from(node) | strict,
            ));
        }
        params
    }

    fn commit_type(page: PageSizeLevel) -> VIRTUAL_ALLOCATION_TYPE {
        match page {
            PageSizeLevel::Regular => MEM_RESERVE | MEM_COMMIT,
            PageSizeLevel::Large | PageSizeLevel::Huge => {
                MEM_RESERVE | MEM_COMMIT | MEM_LARGE_PAGES
            }
        }
    }

    fn last_error() -> u32 {
        // SAFETY: reads this thread's error slot; no arguments. Called straight after the failed
        // call, before anything else can overwrite the slot.
        unsafe { GetLastError() }.0
    }

    /// A `VirtualAlloc2` at a fixed address returns that address or NULL; anything else is
    /// released and treated as a refusal rather than trusted.
    fn at_address(ptr: *mut c_void, addr: usize) -> Result<(), u32> {
        if ptr.is_null() {
            return Err(Self::last_error());
        }
        if ptr as usize != addr {
            // SAFETY: `ptr` was just returned by a successful `VirtualAlloc2`; we own it and
            // release it once, by its base.
            let _ = unsafe { VirtualFree(ptr, 0, MEM_RELEASE) };
            return Err(ERROR_INVALID_ADDRESS);
        }
        Ok(())
    }

    fn free(addr: usize, len: usize, kind: VIRTUAL_FREE_TYPE) -> Result<(), u32> {
        // SAFETY: `addr` is the base of a region the arena owns (tracked piece by piece), and
        // `len` is either 0 for `MEM_RELEASE` or the exact front slice of that placeholder for a
        // split. The call dereferences nothing of ours.
        unsafe { VirtualFree(addr as *mut c_void, len, kind) }.map_err(|e| win32_code(&e))
    }
}

/// Log a call before it is made and its result after.
fn os_call<T>(call: String, f: impl FnOnce() -> Result<T, u32>) -> Result<T, u32> {
    log::debug!("{call}");
    let result = f();
    match &result {
        Ok(_) => log::debug!("  -> ok"),
        Err(code) => log::debug!("  -> {}", describe_error(*code)),
    }
    result
}

impl VmOps for Win32Vm {
    fn reserve(&mut self, len: usize, align: usize, lowest: Option<usize>) -> Result<usize, u32> {
        // LIFETIME: `addr_req` is reached through a raw pointer in `params`, which the kernel
        // dereferences inside the `VirtualAlloc2` call. It must stay in this scope until after
        // the call (the full note is on `WindowsBackend::allocate_with_virtualalloc2`).
        let mut addr_req = MEM_ADDRESS_REQUIREMENTS {
            LowestStartingAddress: lowest.unwrap_or(0) as *mut c_void,
            HighestEndingAddress: std::ptr::null_mut(),
            Alignment: align,
        };
        let mut params = [Self::ext_param(MemExtendedParameterAddressRequirements, 0)];
        params[0].Anonymous2.Pointer = &mut addr_req as *mut _ as *mut c_void;

        let result = os_call(
            format!(
                "VirtualAlloc2(NULL, {len:#x}, MEM_RESERVE|MEM_RESERVE_PLACEHOLDER, PAGE_NOACCESS, \
                 align {align:#x}, lowest {:#x})",
                lowest.unwrap_or(0)
            ),
            || {
                // SAFETY: `params` holds a pointer to `addr_req`, which is live for this whole
                // function (see LIFETIME above). Placeholder reservation takes no base address,
                // which is the only form that accepts address requirements.
                let ptr = unsafe {
                    VirtualAlloc2(
                        None,
                        None,
                        len,
                        MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
                        PAGE_NOACCESS.0,
                        Some(&mut params),
                    )
                };
                if ptr.is_null() {
                    Err(Self::last_error())
                } else {
                    Ok(ptr as usize)
                }
            },
        );
        // Keep `addr_req` provably alive across the call above.
        let _ = &addr_req;
        result
    }

    fn reserve_at(&mut self, addr: usize, len: usize) -> Result<(), u32> {
        os_call(
            format!(
                "VirtualAlloc2({addr:#x}, {len:#x}, MEM_RESERVE|MEM_RESERVE_PLACEHOLDER, PAGE_NOACCESS)"
            ),
            || {
                // SAFETY: no extended parameters; the kernel either reserves exactly this free
                // range or fails. Nothing of ours is dereferenced.
                let ptr = unsafe {
                    VirtualAlloc2(
                        None,
                        Some(addr as *const c_void),
                        len,
                        MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
                        PAGE_NOACCESS.0,
                        None,
                    )
                };
                Self::at_address(ptr, addr)
            },
        )
    }

    fn split(&mut self, addr: usize, len: usize) -> Result<(), u32> {
        os_call(
            format!("VirtualFree({addr:#x}, {len:#x}, MEM_RELEASE|MEM_PRESERVE_PLACEHOLDER)"),
            || {
                Self::free(
                    addr,
                    len,
                    VIRTUAL_FREE_TYPE(MEM_RELEASE.0 | MEM_PRESERVE_PLACEHOLDER.0),
                )
            },
        )
    }

    fn replace(
        &mut self,
        addr: usize,
        len: usize,
        page: PageSizeLevel,
        numa: Option<u32>,
    ) -> Result<(), u32> {
        let mut params = Self::page_params(page, numa);
        os_call(
            format!(
                "VirtualAlloc2({addr:#x}, {len:#x}, MEM_REPLACE_PLACEHOLDER|commit, {}, node {numa:?})",
                page_label(page)
            ),
            || {
                // SAFETY: `params` holds plain values (no pointers) and lives across the call.
                // `[addr, addr + len)` is exactly one placeholder we own, as a replace requires.
                let ptr = unsafe {
                    VirtualAlloc2(
                        None,
                        Some(addr as *const c_void),
                        len,
                        Self::commit_type(page) | MEM_REPLACE_PLACEHOLDER,
                        PAGE_READWRITE.0,
                        (!params.is_empty()).then_some(params.as_mut_slice()),
                    )
                };
                Self::at_address(ptr, addr)
            },
        )
    }

    fn coalesce(&mut self, addr: usize, len: usize) -> Result<(), u32> {
        os_call(
            format!("VirtualFree({addr:#x}, {len:#x}, MEM_RELEASE|MEM_COALESCE_PLACEHOLDERS)"),
            || Self::free(addr, len, VIRTUAL_FREE_TYPE(MEM_RELEASE.0 | MEM_COALESCE_PLACEHOLDERS)),
        )
    }

    fn allocate_at(
        &mut self,
        addr: usize,
        len: usize,
        page: PageSizeLevel,
        numa: Option<u32>,
    ) -> Result<(), u32> {
        let mut params = Self::page_params(page, numa);
        os_call(
            format!(
                "VirtualAlloc2({addr:#x}, {len:#x}, commit, {}, node {numa:?})",
                page_label(page)
            ),
            || {
                // SAFETY: `params` holds plain values (no pointers) and lives across the call.
                // The range is free VA we released a moment ago; the kernel either maps it or
                // fails.
                let ptr = unsafe {
                    VirtualAlloc2(
                        None,
                        Some(addr as *const c_void),
                        len,
                        Self::commit_type(page),
                        PAGE_READWRITE.0,
                        (!params.is_empty()).then_some(params.as_mut_slice()),
                    )
                };
                Self::at_address(ptr, addr)
            },
        )
    }

    fn release(&mut self, addr: usize, len: usize) -> Result<(), u32> {
        os_call(
            format!("VirtualFree({addr:#x}, 0, MEM_RELEASE)  [{len:#x}]"),
            || Self::free(addr, 0, MEM_RELEASE),
        )
    }
}

fn win32_code(e: &windows::core::Error) -> u32 {
    // HRESULT_FROM_WIN32 puts the Win32 code in the low word under facility 7.
    let hr = e.code().0 as u32;
    if hr & 0xFFFF_0000 == 0x8007_0000 {
        hr & 0xFFFF
    } else {
        hr
    }
}

fn gib(bytes: usize) -> f64 {
    bytes_to_gib_f64(bytes as u64)
}

// ---------------------------------------------------------------------------------------------
// Arena
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Holding {
    /// Reserved placeholder VA we own, not backed.
    Placeholder,
    Committed(PageSizeLevel, Option<u32>),
    /// VA we released and have not (yet) taken back. Never freed by us: it may be someone else's.
    Lost,
}

/// One VAD's worth of a slice: each is created, and released, by its own call.
#[derive(Debug, Clone, Copy)]
struct Piece {
    addr: usize,
    len: usize,
    holding: Holding,
}

/// One thread's carved stretch of the reservation.
#[derive(Debug)]
struct Slice {
    thread_id: usize,
    numa_node: Option<u32>,
    base: usize,
    target: usize,
    /// Bytes committed from `base`. Everything below is committed; the piece starting here and
    /// everything above it is placeholder.
    cursor: usize,
    /// Covers the slice, in address order.
    pieces: Vec<Piece>,
}

impl Slice {
    fn remaining(&self) -> usize {
        self.target - self.cursor
    }

    fn next_addr(&self) -> usize {
        self.base + self.cursor
    }

    fn runs(&self) -> Vec<Run> {
        let mut runs: Vec<Run> = Vec::new();
        for piece in &self.pieces {
            let Holding::Committed(page, numa_node) = piece.holding else {
                continue;
            };
            let offset = piece.addr - self.base;
            match runs.last_mut() {
                Some(run)
                    if run.page == page
                        && run.numa_node == numa_node
                        && run.offset + run.len == offset =>
                {
                    run.len += piece.len;
                }
                _ => runs.push(Run {
                    offset,
                    len: piece.len,
                    page,
                    numa_node,
                }),
            }
        }
        runs
    }
}

/// Sort a refused commit: exhaustion steps down the ladder (`backend::is_exhaustion`), anything
/// else ends the build.
fn refusal(code: u32, page: PageSizeLevel, addr: usize, len: usize) -> Result<Grant, String> {
    if is_exhaustion(code) {
        return Ok(Grant::Refused(code));
    }
    Err(format!(
        "stitched: committing {len:#x} bytes of {} pages at {addr:#x} failed: {}{}",
        page_label(page),
        describe_error(code),
        fill::hard_error_hint(page)
    ))
}

/// One placeholder reservation, carved into per-thread slices and filled HUGE → LARGE → REGULAR.
/// Owns every piece of it; `Drop` releases them.
#[derive(Debug)]
pub struct StitchedArena<V: VmOps = Win32Vm> {
    vm: V,
    base: usize,
    reservation: usize,
    stride: usize,
    commit_method: CommitMethod,
    slices: Vec<Slice>,
    /// The part of the reservation not yet carved into slices: set only while `build_with` carves,
    /// or if carving failed partway.
    uncarved: Option<(usize, usize)>,
}

impl StitchedArena<Win32Vm> {
    /// Reserve, carve and fill for real.
    pub fn build(req: &StitchRequest) -> Result<Arc<Self>, String> {
        Self::build_with(Win32Vm, req).map(Arc::new)
    }

    /// Large-page bytes each thread got from another node, by (home, source), for
    /// `fill::warn_remote`.
    fn remote_memory(&self) -> fill::RemoteMemory {
        let mut remote = fill::RemoteMemory::new();
        for span in self.spans() {
            let Some(home) = span.numa_node else {
                continue;
            };
            let mut from: HashMap<u32, usize> = HashMap::new();
            for run in span.runs.iter().filter(|r| r.page != PageSizeLevel::Regular) {
                if let Some(source) = run.numa_node.filter(|&n| n != home) {
                    *from.entry(source).or_default() += run.len;
                }
            }
            for (source, bytes) in from {
                let entry = remote.entry((home, source)).or_default();
                entry.0 += bytes;
                entry.1 += 1;
            }
        }
        remote
    }
}

impl<V: VmOps> StitchedArena<V> {
    /// Reserve, carve and fill through `vm`. On error everything taken so far is released (the
    /// half-built arena drops) before the error returns.
    pub fn build_with(vm: V, req: &StitchRequest) -> Result<Self, String> {
        let plan = plan(req)?;
        let mut vm = vm;
        let base = vm
            .reserve(plan.reservation, HUGE, req.lowest_address)
            .map_err(|code| {
                format!(
                    "stitched: reserving {:.2} GiB of placeholder VA failed: {}",
                    gib(plan.reservation),
                    describe_error(code)
                )
            })?;
        let mut arena = Self {
            vm,
            base,
            reservation: plan.reservation,
            stride: plan.stride,
            commit_method: req.commit_method,
            slices: Vec::with_capacity(req.threads.len()),
            uncarved: Some((base, plan.reservation)),
        };
        arena.carve(req, &plan)?;

        let pages = PageSizes { min: req.min_page, max: req.max_page };
        let sizing = BlockSizing {
            huge_chunk: req.huge_chunk,
            large_chunk: req.large_chunk,
            large_floor: req.large_floor,
        };
        // No fairness question for 4 KiB pages: a refusal there is the commit limit, and that
        // fails the build.
        if let Some((i, code)) = fill::fill_all(&mut arena, pages, &sizing, req.numa_nodes, WHO)? {
            let slice = &arena.slices[i];
            return Err(format!(
                "stitched: committing {:.2} GiB of 4 KiB pages for thread {} was refused: {}",
                gib(slice.remaining()),
                slice.thread_id,
                describe_error(code)
            ));
        }

        if let Some(empty) = arena.slices.iter().find(|s| s.cursor == 0) {
            return Err(format!(
                "stitched: thread {} got no memory with page sizes {:?}..={:?}",
                empty.thread_id, req.min_page, req.max_page
            ));
        }
        for short in arena.slices.iter().filter(|s| s.remaining() > 0) {
            log::warn!(
                "stitched: thread {} got {:.2} of {:.2} GiB: page sizes {:?}..={:?} ran out",
                short.thread_id,
                gib(short.cursor),
                gib(short.target),
                req.min_page,
                req.max_page
            );
        }
        Ok(arena)
    }

    /// Cut the reservation into one placeholder per thread, bottom up. Each cut is a front split
    /// of what is left.
    fn carve(&mut self, req: &StitchRequest, plan: &StitchPlan) -> Result<(), String> {
        let end = self.base + plan.reservation;
        for (t, (thread, &target)) in req.threads.iter().zip(&plan.targets).enumerate() {
            let addr = self.base + t * plan.stride;
            if t + 1 < req.threads.len() {
                self.vm.split(addr, plan.stride).map_err(|code| {
                    format!(
                        "stitched: carving thread {}'s slice at {addr:#x} failed: {}",
                        thread.thread_id,
                        describe_error(code)
                    )
                })?;
                self.uncarved = Some((addr + plan.stride, end - addr - plan.stride));
            } else {
                self.uncarved = None;
            }
            self.slices.push(Slice {
                thread_id: thread.thread_id,
                numa_node: thread.numa_node,
                base: addr,
                target,
                cursor: 0,
                pieces: vec![Piece {
                    addr,
                    len: plan.stride,
                    holding: Holding::Placeholder,
                }],
            });
        }
        Ok(())
    }

    /// The most one commit at slice `i`'s cursor can cover. A refused commit leaves its hole
    /// re-reserved at the size it asked for, so the free space there can be several placeholders
    /// in a row. A commit spans them (`cover`): free-then-allocate releases them all and
    /// allocates once across them, a replace merges them into one placeholder first.
    fn slot_len(&self, i: usize) -> Result<usize, String> {
        let slice = &self.slices[i];
        let addr = slice.next_addr();
        let start = slice
            .pieces
            .iter()
            .position(|p| p.addr == addr && p.holding == Holding::Placeholder)
            .ok_or_else(|| {
                format!(
                    "stitched: bug: no placeholder at thread {}'s cursor {addr:#x}",
                    slice.thread_id
                )
            })?;
        Ok(slice.pieces[start..]
            .iter()
            .take_while(|p| p.holding == Holding::Placeholder)
            .map(|p| p.len)
            .sum())
    }

    /// Make `[cursor, cursor + len)` of slice `i` a run of whole placeholder pieces, splitting the
    /// last one where the range ends. Returns their indices.
    fn cover(&mut self, i: usize, len: usize) -> Result<std::ops::Range<usize>, String> {
        let slice = &mut self.slices[i];
        let addr = slice.next_addr();
        let start = slice
            .pieces
            .iter()
            .position(|p| p.addr == addr)
            .ok_or_else(|| format!("stitched: bug: no piece starts at {addr:#x}"))?;
        let (mut end, mut covered) = (start, 0);
        while covered < len {
            let piece = *slice.pieces.get(end).ok_or_else(|| {
                format!("stitched: bug: {len:#x} at {addr:#x} runs past thread {}'s slice", slice.thread_id)
            })?;
            if piece.holding != Holding::Placeholder {
                return Err(format!(
                    "stitched: bug: piece at {:#x} is {:?}, cannot be committed into",
                    piece.addr, piece.holding
                ));
            }
            if covered + piece.len > len {
                let keep = len - covered;
                self.vm.split(piece.addr, keep).map_err(|code| {
                    format!(
                        "stitched: splitting {keep:#x} off the placeholder at {:#x} failed: {}",
                        piece.addr,
                        describe_error(code)
                    )
                })?;
                slice.pieces[end].len = keep;
                slice.pieces.insert(
                    end + 1,
                    Piece {
                        addr: piece.addr + keep,
                        len: piece.len - keep,
                        holding: Holding::Placeholder,
                    },
                );
            }
            covered += slice.pieces[end].len;
            end += 1;
        }
        Ok(start..end)
    }

    /// Commit `len` bytes of `node`'s pages at slice `i`'s cursor, by the request's
    /// [`CommitMethod`].
    fn commit(
        &mut self,
        i: usize,
        len: usize,
        page: PageSizeLevel,
        node: Option<u32>,
    ) -> Result<Grant, String> {
        match self.commit_method {
            CommitMethod::Replace => self.commit_replace(i, len, page, node),
            CommitMethod::FreeThenAllocate => self.commit_free_then_allocate(i, len, page, node),
        }
    }

    /// Commit `len` bytes at slice `i`'s cursor by replacing the placeholder in one call.
    fn commit_replace(
        &mut self,
        i: usize,
        len: usize,
        page: PageSizeLevel,
        node: Option<u32>,
    ) -> Result<Grant, String> {
        let range = self.cover(i, len)?;
        let slice = &mut self.slices[i];
        let addr = slice.next_addr();
        let idx = range.start;
        if range.len() > 1 {
            // Several placeholders a refusal left behind: merge them, the VA never leaving our
            // hands. (On 10.0.26100 a placeholder a refused large-page replace poisoned would
            // bugcheck here as it would on any split or free; Replace waits for the fix.)
            self.vm.coalesce(addr, len).map_err(|code| {
                format!(
                    "stitched: merging the placeholders at {addr:#x}+{len:#x} failed: {}",
                    describe_error(code)
                )
            })?;
            slice.pieces.splice(
                range,
                [Piece {
                    addr,
                    len,
                    holding: Holding::Placeholder,
                }],
            );
        }
        match self.vm.replace(addr, len, page, node) {
            Ok(()) => {
                slice.pieces[idx].holding = Holding::Committed(page, node);
                slice.cursor += len;
                Ok(Grant::Done)
            }
            // A refused replace leaves the placeholder where it was, ready for a smaller retry.
            // On 10.0.26100 a refused large-page one is also poisoned: the next `VirtualFree` of
            // it bugchecks (module docs).
            Err(code) => refusal(code, page, addr, len),
        }
    }

    /// Commit `len` bytes at slice `i`'s cursor without a replace: split the slot off in its own
    /// call, release it, allocate into the hole.
    fn commit_free_then_allocate(
        &mut self,
        i: usize,
        len: usize,
        page: PageSizeLevel,
        node: Option<u32>,
    ) -> Result<Grant, String> {
        // 1. The slot becomes whole placeholders, in calls separate from the commit.
        let range = self.cover(i, len)?;
        let slice = &mut self.slices[i];
        let addr = slice.next_addr();

        // 2. Free them outright. Until step 3 or the re-reserve lands, this VA is not ours.
        for k in range.clone() {
            let piece = slice.pieces[k];
            self.vm.release(piece.addr, piece.len).map_err(|code| {
                format!(
                    "stitched: releasing the {} slot at {:#x} failed: {}",
                    page_label(page),
                    piece.addr,
                    describe_error(code)
                )
            })?;
            slice.pieces[k].holding = Holding::Lost;
        }
        // From here one piece stands for the whole hole.
        let idx = range.start;
        slice.pieces.splice(
            range,
            [Piece {
                addr,
                len,
                holding: Holding::Lost,
            }],
        );

        // 3. Allocate into the hole as an ordinary allocation at a fixed address.
        match self.vm.allocate_at(addr, len, page, node) {
            Ok(()) => {
                slice.pieces[idx].holding = Holding::Committed(page, node);
                slice.cursor += len;
                Ok(Grant::Done)
            }
            Err(code) => {
                // Take the hole back so a smaller page size can fill it. If that fails, another
                // allocation in the process got there first.
                self.vm.reserve_at(addr, len).map_err(|again| {
                    format!(
                        "stitched: lost VA {addr:#x}+{len:#x}: the {} commit was refused ({}) \
                         and re-reserving the hole failed ({}), so another allocation took it",
                        page_label(page),
                        describe_error(code),
                        describe_error(again)
                    )
                })?;
                slice.pieces[idx].holding = Holding::Placeholder;
                refusal(code, page, addr, len)
            }
        }
    }

    /// Per-thread spans, in slice (address) order.
    pub fn spans(&self) -> Vec<ThreadSpan> {
        self.slices
            .iter()
            .map(|s| ThreadSpan {
                thread_id: s.thread_id,
                numa_node: s.numa_node,
                base: s.base,
                len: s.cursor,
                target: s.target,
                runs: s.runs(),
            })
            .collect()
    }

    pub fn log_summary(&self) {
        log::info!(
            "Stitched arena: {:#x} + {:.2} GiB placeholder VA, {} thread slices x {:.2} GiB",
            self.base,
            gib(self.reservation),
            self.slices.len(),
            gib(self.stride)
        );
        for span in self.spans() {
            let remote = span.remote_bytes();
            log::info!(
                "  thread {:>3} {:<8} {:#014x}  1GB {:>7.2} | 2MB {:>7.2} | 4KB {:>7.2} GiB = {:.2}/{:.2} GiB{}",
                span.thread_id,
                node_label(span.numa_node),
                span.base,
                gib(span.bytes_at(PageSizeLevel::Huge)),
                gib(span.bytes_at(PageSizeLevel::Large)),
                gib(span.bytes_at(PageSizeLevel::Regular)),
                gib(span.len),
                gib(span.target),
                if remote > 0 { format!(" ({:.2} GiB remote)", gib(remote)) } else { String::new() }
            );
        }
    }
}

/// stitched's side of `fill::Fill`: a request is a commit at the slice's cursor. Neighbouring
/// commits of one page size merge into a single run, so a commit can be any multiple of the floor;
/// the run is cut into power-of-two blocks only when it is handed out.
impl<V: VmOps> Fill for StitchedArena<V> {
    fn thread_count(&self) -> usize {
        self.slices.len()
    }

    fn thread_id(&self, i: usize) -> usize {
        self.slices[i].thread_id
    }

    fn numa_node(&self, i: usize) -> Option<u32> {
        self.slices[i].numa_node
    }

    fn filled(&self, i: usize) -> usize {
        self.slices[i].cursor
    }

    fn bytes_at(&self, i: usize, page: PageSizeLevel) -> usize {
        self.slices[i]
            .pieces
            .iter()
            .filter(|p| matches!(p.holding, Holding::Committed(pg, _) if pg == page))
            .map(|p| p.len)
            .sum()
    }

    fn remaining(&self, i: usize) -> usize {
        self.slices[i].remaining()
    }

    fn request_len(&self, i: usize, rung: usize, floor: usize) -> Result<usize, String> {
        let len = rung.min(self.slices[i].remaining()).min(self.slot_len(i)?) / floor * floor;
        if len < floor {
            return Err(format!(
                "stitched: bug: thread {} has no {} slot at its cursor",
                self.slices[i].thread_id,
                size_label(floor)
            ));
        }
        Ok(len)
    }

    fn request(
        &mut self,
        i: usize,
        len: usize,
        page: PageSizeLevel,
        node: Option<u32>,
    ) -> Result<Grant, String> {
        self.commit(i, len, page, node)
    }

    /// 1 GiB pages need a 1 GiB-aligned cursor. The base is one, and every 1 GiB commit and every
    /// whole-GiB 2 MiB run keeps it, so a remote pass can still put 1 GiB pages above local 2 MiB
    /// ones. A 2 MiB tail that is not whole GiB (the node's last 256, 64, 32 MiB pieces) moves the
    /// cursor off the boundary, and from then on the thread takes 2 MiB pages only.
    fn can_take(&self, i: usize, page: PageSizeLevel) -> bool {
        page != PageSizeLevel::Huge || self.slices[i].cursor.is_multiple_of(HUGE)
    }

    fn gap_to_boundary(&self, i: usize, page: PageSizeLevel) -> usize {
        let cursor = self.slices[i].cursor;
        match page {
            PageSizeLevel::Huge => cursor.next_multiple_of(HUGE) - cursor,
            _ => 0,
        }
    }
}

impl<V: VmOps> Drop for StitchedArena<V> {
    fn drop(&mut self) {
        let vm = &mut self.vm;
        let (mut released, mut failed) = (0usize, 0usize);
        // Commits before the placeholders around them. Each piece is its own VAD and is released
        // on its own; `Lost` VA may belong to someone else and is left alone.
        for commits in [true, false] {
            for piece in self.slices.iter().flat_map(|s| &s.pieces) {
                let wanted = match piece.holding {
                    Holding::Committed(..) => commits,
                    Holding::Placeholder => !commits,
                    Holding::Lost => false,
                };
                if !wanted {
                    continue;
                }
                match vm.release(piece.addr, piece.len) {
                    Ok(()) => released += 1,
                    Err(code) => {
                        failed += 1;
                        log::error!(
                            "stitched: teardown: releasing {:#x}+{:#x} failed: {}",
                            piece.addr,
                            piece.len,
                            describe_error(code)
                        );
                    }
                }
            }
        }
        if let Some((addr, len)) = self.uncarved.take() {
            match vm.release(addr, len) {
                Ok(()) => released += 1,
                Err(code) => {
                    failed += 1;
                    log::error!(
                        "stitched: teardown: releasing the uncarved {addr:#x}+{len:#x} failed: {}",
                        describe_error(code)
                    );
                }
            }
        }
        if failed > 0 {
            log::error!("stitched: teardown released {released} regions, {failed} failed");
        } else {
            log::debug!("stitched: teardown released {released} regions");
        }
    }
}

impl<V: VmOps + Send + Sync + std::fmt::Debug + 'static> Backend for StitchedArena<V> {
    fn allocate(&self, _config: &AllocationConfig) -> Result<BackendAllocation, AllocError> {
        Err(AllocError::other(
            "stitched arena: memory is committed up front by StitchedArena::build; \
             there is no per-block allocate",
        ))
    }

    fn free(&self, allocation: BackendAllocation) -> Result<(), String> {
        // A view going away frees nothing: the arena releases every piece together when the last
        // `MemoryBuffer` drops its `Arc`.
        log::trace!(
            "stitched: view {:?} (+{:#x}) returned",
            allocation.ptr,
            allocation.size
        );
        Ok(())
    }

    fn name(&self) -> &'static str {
        "Windows Stitched Placeholder"
    }
}

// ---------------------------------------------------------------------------------------------
// Handing out blocks
// ---------------------------------------------------------------------------------------------

/// `len` as powers of two, largest first (its binary digits).
fn power_of_two_split(mut len: usize) -> Vec<usize> {
    let mut sizes = Vec::new();
    while len > 0 {
        let size = prev_power_of_two(len);
        sizes.push(size);
        len -= size;
    }
    sizes
}

fn single_page_type(page: PageSizeLevel, size: usize) -> PageType {
    match page {
        PageSizeLevel::Huge => PageType::Huge(size),
        PageSizeLevel::Large => PageType::Large(size),
        PageSizeLevel::Regular => PageType::Regular(size),
    }
}

/// Cut each thread's span into `AllocationBlock`s, in address order: every same-page-size run split
/// into power-of-two blocks, largest first, each but the last marked `joins_next`. The correctness
/// tests see the span as one region (`test_memory::regions`, TODO 76); the bandwidth and latency
/// tests still take the power-of-two blocks (`prepare_blocks_for_extent`). The buffers are views:
/// they share `arena` as their backend, and the memory is released when the last of them drops.
pub fn into_allocation_blocks<V>(arena: Arc<StitchedArena<V>>) -> HashMap<usize, Vec<AllocationBlock>>
where
    V: VmOps + Send + Sync + std::fmt::Debug + 'static,
{
    let backend: Arc<dyn Backend> = arena.clone();
    let mut out = HashMap::new();
    for span in arena.spans() {
        let mut blocks = Vec::new();
        for run in &span.runs {
            let numa_node = run.numa_node.or(span.numa_node).unwrap_or(0);
            let mut addr = span.base + run.offset;
            for size in power_of_two_split(run.len) {
                blocks.push(AllocationBlock {
                    buffer: MemoryBuffer::new(
                        BackendAllocation {
                            ptr: addr as *mut u8,
                            size,
                            info: BufferInfo {
                                numa_node,
                                page_type: single_page_type(run.page, size),
                            },
                        },
                        backend.clone(),
                    ),
                    block_info: BlockInfo {
                        size_bytes: size,
                        thread_id: span.thread_id,
                    },
                    joins_next: false, // set below, once the next block is known
                });
                addr += size;
            }
        }
        // The runs tile the span, so each block but the last runs into the next
        for i in 1..blocks.len() {
            let end = blocks[i - 1].buffer.as_mut_ptr() as usize + blocks[i - 1].buffer.size();
            blocks[i - 1].joins_next = end == blocks[i].buffer.as_mut_ptr() as usize;
        }
        out.insert(span.thread_id, blocks);
    }
    out
}

// ---------------------------------------------------------------------------------------------
// TMR entry point
// ---------------------------------------------------------------------------------------------

/// Every thread's share as one span: blocks keyed by thread, power-of-two shaped, each joined to
/// the next (`AllocationBlock::joins_next`).
pub fn chunk_allocate_stitched(
    thread_blocks: &HashMap<usize, Vec<BlockInfo>>,
    runtime_config: &crate::RuntimeConfig,
) -> Result<HashMap<usize, Vec<AllocationBlock>>, String> {
    let threads: Vec<StitchThread> = fill::thread_shares(thread_blocks, runtime_config)
        .into_iter()
        .map(|share| StitchThread {
            thread_id: share.thread_id,
            bytes: share.bytes,
            numa_node: Some(share.numa_node),
        })
        .collect();

    let mut request = StitchRequest::new(threads);
    let pages = fill::allowed_page_sizes(runtime_config)?;
    request.min_page = pages.min;
    request.max_page = pages.max;
    let alloc = &runtime_config.memory_allocation;
    request.numa_nodes = fill::numa_node_count();
    let sizing = alloc.block_sizing()?;
    request.huge_chunk = sizing.huge_chunk;
    request.large_chunk = sizing.large_chunk;
    request.large_floor = sizing.large_floor;
    log::debug!(
        "Stitched: commits start at {} of 1 GiB pages and {} of 2 MiB pages, down to {}",
        size_label(request.huge_chunk),
        size_label(request.large_chunk),
        size_label(request.large_floor)
    );

    let arena = StitchedArena::build(&request)?;
    arena.log_summary();
    fill::warn_remote(WHO, &arena.remote_memory());
    Ok(into_allocation_blocks(arena))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::backend::{
        ERROR_COMMITMENT_LIMIT, ERROR_INVALID_PARAMETER, ERROR_NO_SYSTEM_RESOURCES,
        ERROR_PRIVILEGE_NOT_HELD,
    };
    use std::collections::{BTreeMap, HashSet};
    use std::sync::{Mutex, PoisonError};

    const MIB: usize = BYTES_PER_MIB_USIZE;
    const GIB: usize = HUGE;

    fn page_bytes(page: PageSizeLevel) -> usize {
        match page {
            PageSizeLevel::Huge => HUGE,
            PageSizeLevel::Large => LARGE,
            PageSizeLevel::Regular => crate::constants::PAGE_SIZE_4KB,
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Region {
        Placeholder,
        Committed(PageSizeLevel, Option<u32>),
        /// Somebody else's allocation that landed in a hole we opened.
        Foreign,
    }

    /// Finite page pools per node, plus the placeholder rules applied as strictly as the kernel
    /// does (stricter where the docs are vague). A pool is unlimited unless set.
    #[derive(Debug, Default)]
    struct State {
        regions: BTreeMap<usize, (usize, Region)>,
        huge_pages: HashMap<Option<u32>, usize>,
        large_bytes: HashMap<Option<u32>, usize>,
        regular_bytes: Option<usize>,
        no_privilege: bool,
        /// Large-page commits refused for want of the privilege, in order: (page size, bytes).
        privilege_refusals: Vec<(PageSizeLevel, usize)>,
        /// A foreign allocation grabs the range as soon as the next release happens.
        steal_next_release: bool,
        /// Placeholders whose large-page replace was refused with 1450: the 10.0.26100 bug.
        poisoned: HashSet<usize>,
        /// A kernel with Microsoft's fix: a refused replace poisons nothing.
        replace_fixed: bool,
        /// `MEM_COALESCE_PLACEHOLDERS` calls made.
        coalesces: usize,
        /// `MEM_REPLACE_PLACEHOLDER` calls made, granted or not.
        replaces: usize,
        /// Every 1 GiB commit is refused with this code instead (e.g. 87, a bad request).
        huge_error: Option<u32>,
        /// Every commit asked for, in order: (page size, node, granted).
        commits: Vec<(PageSizeLevel, Option<u32>, bool)>,
    }

    impl State {
        /// Split or release a poisoned placeholder and the real kernel bugchecks (seen at 1 GiB and
        /// 2 MiB). Replacing it again is untested live, so that is assumed to bugcheck too. Quiet
        /// while already unwinding, so a test's teardown does not turn the panic into an abort.
        fn touch(&self, addr: usize, op: &str) {
            if self.poisoned.contains(&addr) && !std::thread::panicking() {
                panic!("bugcheck 0x139/0xE: {op} of {addr:#x} after a refused large-page replace");
            }
        }

        fn overlaps(&self, addr: usize, len: usize) -> bool {
            self.regions
                .range(..addr + len)
                .next_back()
                .is_some_and(|(&a, &(l, _))| a + l > addr)
        }

        fn pool(&mut self, page: PageSizeLevel, node: Option<u32>) -> &mut usize {
            match page {
                PageSizeLevel::Huge => self.huge_pages.entry(node).or_insert(usize::MAX),
                PageSizeLevel::Large => self.large_bytes.entry(node).or_insert(usize::MAX),
                PageSizeLevel::Regular => self.regular_bytes.get_or_insert(usize::MAX),
            }
        }

        fn units(page: PageSizeLevel, len: usize) -> usize {
            if page == PageSizeLevel::Huge {
                len / GIB
            } else {
                len
            }
        }

        /// Take `len` bytes of `page` from the pool. On failure, return the code the kernel would.
        fn take(
            &mut self,
            addr: usize,
            len: usize,
            page: PageSizeLevel,
            node: Option<u32>,
        ) -> Result<(), u32> {
            let unit = page_bytes(page);
            if len == 0 || !addr.is_multiple_of(unit) || !len.is_multiple_of(unit) {
                return Err(ERROR_INVALID_PARAMETER);
            }
            if self.no_privilege && page != PageSizeLevel::Regular {
                self.privilege_refusals.push((page, len));
                return Err(ERROR_PRIVILEGE_NOT_HELD);
            }
            if page == PageSizeLevel::Huge && let Some(code) = self.huge_error {
                return Err(code);
            }
            let want = Self::units(page, len);
            let pool = self.pool(page, node);
            if *pool < want {
                return Err(if page == PageSizeLevel::Regular {
                    ERROR_COMMITMENT_LIMIT
                } else {
                    ERROR_NO_SYSTEM_RESOURCES
                });
            }
            *pool -= want;
            Ok(())
        }
    }

    #[derive(Debug, Clone, Default)]
    struct FakeVm(Arc<Mutex<State>>);

    impl FakeVm {
        fn state(&self) -> std::sync::MutexGuard<'_, State> {
            // A modelled bugcheck panics with the lock held; teardown still needs the state.
            self.0.lock().unwrap_or_else(PoisonError::into_inner)
        }
    }

    impl VmOps for FakeVm {
        fn reserve(
            &mut self,
            len: usize,
            align: usize,
            lowest: Option<usize>,
        ) -> Result<usize, u32> {
            let mut s = self.state();
            let top = s
                .regions
                .iter()
                .next_back()
                .map_or(0, |(&a, &(l, _))| a + l);
            let addr = top
                .max(lowest.unwrap_or(0x100_0000_0000))
                .next_multiple_of(align);
            s.regions.insert(addr, (len, Region::Placeholder));
            Ok(addr)
        }

        fn reserve_at(&mut self, addr: usize, len: usize) -> Result<(), u32> {
            let mut s = self.state();
            if s.overlaps(addr, len) {
                return Err(ERROR_INVALID_ADDRESS);
            }
            s.regions.insert(addr, (len, Region::Placeholder));
            Ok(())
        }

        fn split(&mut self, addr: usize, len: usize) -> Result<(), u32> {
            let mut s = self.state();
            s.touch(addr, "split");
            match s.regions.get(&addr).copied() {
                Some((whole, Region::Placeholder)) if len > 0 && len < whole => {
                    s.regions.insert(addr, (len, Region::Placeholder));
                    s.regions
                        .insert(addr + len, (whole - len, Region::Placeholder));
                    Ok(())
                }
                _ => Err(ERROR_INVALID_PARAMETER),
            }
        }

        fn replace(
            &mut self,
            addr: usize,
            len: usize,
            page: PageSizeLevel,
            numa: Option<u32>,
        ) -> Result<(), u32> {
            let mut s = self.state();
            s.replaces += 1;
            s.touch(addr, "replace");
            if s.regions.get(&addr) != Some(&(len, Region::Placeholder)) {
                return Err(ERROR_INVALID_PARAMETER);
            }
            let taken = s.take(addr, len, page, numa);
            s.commits.push((page, numa, taken.is_ok()));
            if let Err(code) = taken {
                if code == ERROR_NO_SYSTEM_RESOURCES && !s.replace_fixed {
                    s.poisoned.insert(addr);
                }
                return Err(code);
            }
            s.regions.insert(addr, (len, Region::Committed(page, numa)));
            Ok(())
        }

        fn allocate_at(
            &mut self,
            addr: usize,
            len: usize,
            page: PageSizeLevel,
            numa: Option<u32>,
        ) -> Result<(), u32> {
            let mut s = self.state();
            if s.overlaps(addr, len) {
                return Err(ERROR_INVALID_ADDRESS);
            }
            let taken = s.take(addr, len, page, numa);
            s.commits.push((page, numa, taken.is_ok()));
            taken?;
            s.regions.insert(addr, (len, Region::Committed(page, numa)));
            Ok(())
        }

        fn coalesce(&mut self, addr: usize, len: usize) -> Result<(), u32> {
            let mut s = self.state();
            s.coalesces += 1;
            let mut covered = Vec::new();
            let mut at = addr;
            while at < addr + len {
                s.touch(at, "coalesce");
                match s.regions.get(&at) {
                    Some(&(piece, Region::Placeholder)) if at + piece <= addr + len => {
                        covered.push(at);
                        at += piece;
                    }
                    _ => return Err(ERROR_INVALID_ADDRESS),
                }
            }
            for piece in covered {
                s.regions.remove(&piece);
            }
            s.regions.insert(addr, (len, Region::Placeholder));
            Ok(())
        }

        fn release(&mut self, addr: usize, _len: usize) -> Result<(), u32> {
            let mut s = self.state();
            s.touch(addr, "release");
            let (len, region) = match s.regions.get(&addr).copied() {
                Some((_, Region::Foreign)) | None => return Err(ERROR_INVALID_ADDRESS),
                Some(found) => found,
            };
            s.regions.remove(&addr);
            if let Region::Committed(page, node) = region {
                *s.pool(page, node) += State::units(page, len);
            }
            if std::mem::take(&mut s.steal_next_release) {
                s.regions.insert(addr, (len, Region::Foreign));
            }
            Ok(())
        }
    }

    fn threads(sizes: &[usize], nodes: &[Option<u32>]) -> Vec<StitchThread> {
        sizes
            .iter()
            .zip(nodes)
            .enumerate()
            .map(|(thread_id, (&bytes, &numa_node))| StitchThread {
                thread_id,
                bytes,
                numa_node,
            })
            .collect()
    }

    fn huge_counts(arena: &StitchedArena<FakeVm>) -> Vec<usize> {
        arena
            .spans()
            .iter()
            .map(|s| s.bytes_at(PageSizeLevel::Huge) / GIB)
            .collect()
    }

    /// Every span is contiguous from its 1 GiB-aligned base, each run on its page size's boundary.
    fn assert_well_formed(arena: &StitchedArena<FakeVm>) {
        for span in arena.spans() {
            let mut offset = 0;
            for run in &span.runs {
                assert_eq!(run.offset, offset, "thread {} has a gap", span.thread_id);
                assert_eq!(
                    run.offset % page_bytes(run.page),
                    0,
                    "thread {}: a {:?} run off its page boundary",
                    span.thread_id,
                    run.page
                );
                offset += run.len;
            }
            assert_eq!(offset, span.len);
            assert_eq!(span.base % GIB, 0);
        }
    }

    #[test]
    fn plan_rounds_down_and_strides_by_the_largest() {
        let req = StitchRequest::new(threads(&[GIB + 20 * MIB, 3 * GIB], &[None, None]));
        let plan = plan(&req).unwrap();
        assert_eq!(plan.targets, vec![GIB + 16 * MIB, 3 * GIB]);
        assert_eq!(plan.stride, 3 * GIB);
        assert_eq!(plan.reservation, 6 * GIB);

        let tiny = StitchRequest::new(threads(&[10 * MIB], &[None]));
        assert!(super::plan(&tiny).is_err());
        let mut odd = StitchRequest::new(threads(&[GIB], &[None]));
        odd.run_quantum = 24 * MIB;
        assert!(super::plan(&odd).is_err());
    }

    #[test]
    fn huge_round_robin_ends_at_most_one_page_apart() {
        let vm = FakeVm::default();
        vm.state().huge_pages.insert(Some(0), 6);
        let req = StitchRequest::new(threads(&[4 * GIB; 4], &[Some(0); 4]));
        let arena = StitchedArena::build_with(vm.clone(), &req).unwrap();
        assert_eq!(huge_counts(&arena), vec![2, 2, 1, 1]);
        assert_well_formed(&arena);
        for span in arena.spans() {
            assert_eq!(span.len, 4 * GIB);
            assert_eq!(span.bytes_at(PageSizeLevel::Regular), 0);
        }
        assert_eq!(vm.state().huge_pages[&Some(0)], 0);
    }

    fn huge_commits(vm: &FakeVm) -> Vec<usize> {
        vm.state()
            .regions
            .values()
            .filter(|(_, r)| matches!(r, Region::Committed(PageSizeLevel::Huge, _)))
            .map(|&(len, _)| len / GIB)
            .collect()
    }

    /// 11 pages for four 8 GiB threads. From 1 GiB that is 11 commits ending 3, 3, 3, 2; from
    /// 4 GiB it is 4 commits (4, 4, then 4 refused -> 2, then 2 refused -> 1, then 1 refused ->
    /// dry) ending 4, 4, 2, 1.
    #[test]
    fn huge_ladder_trades_evenness_for_fewer_commits() {
        let run = |huge_chunk| {
            let vm = FakeVm::default();
            vm.state().huge_pages.insert(Some(0), 11);
            let mut req = StitchRequest::new(threads(&[8 * GIB; 4], &[Some(0); 4]));
            req.huge_chunk = huge_chunk;
            let arena = StitchedArena::build_with(vm.clone(), &req).unwrap();
            assert_well_formed(&arena);
            assert!(arena.spans().iter().all(|s| s.len == 8 * GIB));
            assert_eq!(vm.state().huge_pages[&Some(0)], 0);
            (huge_counts(&arena), huge_commits(&vm).len())
        };
        assert_eq!(run(GIB), (vec![3, 3, 3, 2], 11));
        assert_eq!(run(4 * GIB), (vec![4, 4, 2, 1], 4));

        // A thread's whole-GiB remainder caps the commit: one call, not a refusal.
        let vm = FakeVm::default();
        let mut req = StitchRequest::new(threads(&[3 * GIB + 512 * MIB], &[None]));
        req.huge_chunk = 8 * GIB;
        let arena = StitchedArena::build_with(vm.clone(), &req).unwrap();
        assert_eq!(huge_commits(&vm), vec![3]);
        assert_eq!(arena.spans()[0].bytes_at(PageSizeLevel::Large), 512 * MIB);
    }

    #[test]
    fn chunks_must_be_powers_of_two_on_their_ladder() {
        let with = |huge_chunk, large_chunk, large_floor| {
            let mut req = StitchRequest::new(threads(&[GIB], &[None]));
            req.huge_chunk = huge_chunk;
            req.large_chunk = large_chunk;
            req.large_floor = large_floor;
            plan(&req).map(|_| ())
        };
        assert!(with(8 * GIB, 32 * MIB, 16 * MIB).is_ok());
        assert!(with(GIB, 16 * MIB, 16 * MIB).is_ok());
        assert!(with(GIB, 64 * MIB, 64 * MIB).is_ok());
        assert!(with(3 * GIB, GIB, 16 * MIB).is_err());
        assert!(with(512 * MIB, GIB, 16 * MIB).is_err());
        assert!(with(GIB, 24 * MIB, 16 * MIB).is_err());
        assert!(with(GIB, 8 * MIB, 16 * MIB).is_err());
        // The floor: a power of two, no smaller than the run quantum, no bigger than the chunk.
        assert!(with(GIB, GIB, 48 * MIB).is_err());
        assert!(with(GIB, GIB, 8 * MIB).is_err());
        assert!(with(GIB, 32 * MIB, 64 * MIB).is_err());
    }

    /// 1 GiB + 48 MiB of 2 MiB pages for a 2 GiB thread. The ladder takes 1 GiB, is refused from
    /// 1 GiB down to 64 MiB, then takes 32 MiB. From there the default 16 MiB floor also takes the
    /// last 16 MiB; a 32 MiB floor stops and leaves it to 4 KiB.
    #[test]
    fn large_floor_is_where_the_ladder_gives_up() {
        let run = |large_floor| {
            let vm = FakeVm::default();
            vm.state().huge_pages.insert(None, 0);
            vm.state().large_bytes.insert(None, GIB + 48 * MIB);
            let mut req = StitchRequest::new(threads(&[2 * GIB], &[None]));
            req.large_floor = large_floor;
            let arena = StitchedArena::build_with(vm.clone(), &req).unwrap();
            assert_well_formed(&arena);
            let span = &arena.spans()[0];
            assert_eq!(span.len, 2 * GIB);
            (
                span.bytes_at(PageSizeLevel::Large),
                vm.state().large_bytes[&None],
            )
        };
        assert_eq!(run(16 * MIB), (GIB + 48 * MIB, 0));
        assert_eq!(run(32 * MIB), (GIB + 32 * MIB, 16 * MIB));
    }

    #[test]
    fn huge_stops_at_whole_gib_and_large_takes_the_tail() {
        let vm = FakeVm::default();
        let req = StitchRequest::new(threads(&[2 * GIB + 48 * MIB], &[None]));
        let arena = StitchedArena::build_with(vm, &req).unwrap();
        let span = &arena.spans()[0];
        assert_eq!(
            span.runs,
            vec![
                Run {
                    offset: 0,
                    len: 2 * GIB,
                    page: PageSizeLevel::Huge,
                    numa_node: None,
                },
                Run {
                    offset: 2 * GIB,
                    len: 48 * MIB,
                    page: PageSizeLevel::Large,
                    numa_node: None,
                },
            ]
        );
    }

    #[test]
    fn nodes_run_dry_independently() {
        let vm = FakeVm::default();
        vm.state().huge_pages.insert(Some(0), 1);
        vm.state().huge_pages.insert(Some(1), 8);
        let nodes = [Some(0), Some(0), Some(1), Some(1)];
        let req = StitchRequest::new(threads(&[2 * GIB; 4], &nodes));
        let arena = StitchedArena::build_with(vm, &req).unwrap();
        assert_eq!(huge_counts(&arena), vec![1, 0, 2, 2]);
        assert_well_formed(&arena);
    }

    /// Node 0 has one 1 GiB page and 512 MiB of 2 MiB pages for two 2 GiB threads; node 1 has
    /// plenty. Thread 0 tops up with a remote 1 GiB page. Thread 1's 512 MiB of local 2 MiB pages
    /// leave it off a 1 GiB boundary, so it pads with node 1's 2 MiB pages up to the boundary and
    /// then takes a 1 GiB page from node 1 too. Each run records the node it came from.
    #[test]
    fn short_node_threads_go_remote_and_runs_record_the_node() {
        let vm = FakeVm::default();
        vm.state().huge_pages.insert(Some(0), 1);
        vm.state().large_bytes.insert(Some(0), 512 * MIB);
        vm.state().huge_pages.insert(Some(1), 8);
        let nodes = [Some(0), Some(0), Some(1), Some(1)];
        let mut req = StitchRequest::new(threads(&[2 * GIB; 4], &nodes));
        req.numa_nodes = 2;
        let arena = StitchedArena::build_with(vm, &req).unwrap();
        assert_well_formed(&arena);
        let spans = arena.spans();
        assert!(spans.iter().all(|s| s.len == s.target));
        let run = |offset, len, page, node| Run { offset, len, page, numa_node: Some(node) };
        assert_eq!(spans[0].runs, vec![run(0, GIB, PageSizeLevel::Huge, 0), run(GIB, GIB, PageSizeLevel::Huge, 1)]);
        assert_eq!(
            spans[1].runs,
            vec![
                run(0, 512 * MIB, PageSizeLevel::Large, 0),
                run(512 * MIB, 512 * MIB, PageSizeLevel::Large, 1),
                run(GIB, GIB, PageSizeLevel::Huge, 1),
            ]
        );
        let remote: Vec<usize> = spans.iter().map(ThreadSpan::remote_bytes).collect();
        assert_eq!(remote, vec![GIB, GIB + 512 * MIB, 0, 0]);
    }

    /// The user's 2026-10-02 case, small: node 0's 1 GiB pages split 2/2/1/1 and its 2 MiB pages
    /// end in a 512 MiB and a 256 MiB piece, which leave threads 2 and 3 off a 1 GiB boundary.
    /// Unpadded they took no 1 GiB pages from node 1 (4/4/1/1). Padded with node 1's 2 MiB pages up
    /// to the boundary first, they take one each: 4/4/2/2, every thread exact.
    #[test]
    fn misaligned_threads_are_padded_before_remote_huge_pages() {
        let vm = FakeVm::default();
        vm.state().huge_pages.insert(Some(0), 6);
        vm.state().large_bytes.insert(Some(0), 2 * GIB + 768 * MIB);
        vm.state().huge_pages.insert(Some(1), 20);
        let mut req = StitchRequest::new(threads(&[4 * GIB; 4], &[Some(0); 4]));
        req.numa_nodes = 2;
        let arena = StitchedArena::build_with(vm, &req).unwrap();
        assert_well_formed(&arena);
        let spans = arena.spans();
        assert!(spans.iter().all(|s| s.len == s.target));
        assert_eq!(huge_counts(&arena), vec![4, 4, 2, 2]);
        let remote: Vec<usize> = spans.iter().map(ThreadSpan::remote_bytes).collect();
        assert_eq!(remote, vec![2 * GIB, 2 * GIB, GIB + 512 * MIB, GIB + 768 * MIB]);
    }

    /// Thread 0's 2 MiB requests are refused down to the floor, which leaves the free space at its
    /// cursor as 16, 16, 32, 64, 128 MiB placeholders and the rest. Its next 1 GiB page, from node
    /// 1, spans those pieces in one commit instead of failing on the 16 MiB one.
    #[test]
    fn a_commit_spans_the_pieces_a_refusal_left() {
        let vm = FakeVm::default();
        vm.state().huge_pages.insert(Some(0), 2);
        vm.state().large_bytes.insert(Some(0), 0);
        vm.state().huge_pages.insert(Some(1), 4);
        let mut req = StitchRequest::new(threads(&[2 * GIB; 2], &[Some(0); 2]));
        req.numa_nodes = 2;
        let arena = StitchedArena::build_with(vm, &req).unwrap();
        assert_well_formed(&arena);
        assert_eq!(huge_counts(&arena), vec![2, 2]);
        let remote: Vec<usize> = arena.spans().iter().map(ThreadSpan::remote_bytes).collect();
        assert_eq!(remote, vec![GIB, GIB]);
    }

    /// The same as `a_commit_spans_the_pieces_a_refusal_left`, by `Replace` on a kernel with
    /// Microsoft's fix: the pieces are merged into one placeholder, never freed, and the result
    /// is the same layout.
    #[test]
    fn replace_merges_the_pieces_a_refusal_left() {
        let layout = |method| {
            let vm = FakeVm::default();
            vm.state().replace_fixed = true;
            vm.state().huge_pages.insert(Some(0), 2);
            vm.state().large_bytes.insert(Some(0), 0);
            vm.state().huge_pages.insert(Some(1), 4);
            let mut req = StitchRequest::new(threads(&[2 * GIB; 2], &[Some(0); 2]));
            req.numa_nodes = 2;
            req.commit_method = method;
            let arena = StitchedArena::build_with(vm.clone(), &req).unwrap();
            assert_well_formed(&arena);
            let runs: Vec<Vec<Run>> = arena.spans().into_iter().map(|s| s.runs).collect();
            (runs, vm.state().coalesces)
        };
        let (replaced, merges) = layout(CommitMethod::Replace);
        let (freed, none) = layout(CommitMethod::FreeThenAllocate);
        assert_eq!(replaced, freed);
        assert!(merges > 0, "the pieces were merged");
        assert_eq!(none, 0, "free-then-allocate releases them instead");
    }

    #[test]
    fn large_exhaustion_halves_then_falls_to_regular() {
        let vm = FakeVm::default();
        vm.state().huge_pages.insert(None, 0);
        vm.state().large_bytes.insert(None, GIB + 256 * MIB);
        let req = StitchRequest::new(threads(&[GIB + 512 * MIB; 2], &[None; 2]));
        let arena = StitchedArena::build_with(vm.clone(), &req).unwrap();
        assert_well_formed(&arena);
        let spans = arena.spans();
        assert_eq!(spans[0].bytes_at(PageSizeLevel::Large), GIB);
        assert_eq!(spans[1].bytes_at(PageSizeLevel::Large), 256 * MIB);
        assert!(spans.iter().all(|s| s.len == s.target));
        assert_eq!(vm.state().large_bytes[&None], 0);
    }

    /// Plain round-robin would give 1, 1, 0.5, 0 GiB of 2 MiB pages here: 3, 3, 1.5, 1 GiB locked,
    /// with the 4 KiB remainder piling up on the threads that were already short of 1 GiB pages.
    /// Least-filled first tops threads 2 and 3 up to level, and they keep the lead after: the last
    /// 512 MiB goes to thread 2, not to thread 0, which already has two 1 GiB pages.
    #[test]
    fn large_tops_up_the_threads_short_of_huge_first() {
        let vm = FakeVm::default();
        vm.state().huge_pages.insert(Some(0), 6);
        vm.state().large_bytes.insert(Some(0), 2 * GIB + 512 * MIB);
        let req = StitchRequest::new(threads(&[4 * GIB; 4], &[Some(0); 4]));
        let arena = StitchedArena::build_with(vm, &req).unwrap();
        assert_well_formed(&arena);
        assert_eq!(huge_counts(&arena), vec![2, 2, 1, 1]);
        let spans = arena.spans();
        let large: Vec<_> = spans
            .iter()
            .map(|s| s.bytes_at(PageSizeLevel::Large))
            .collect();
        assert_eq!(large, vec![0, 0, GIB + 512 * MIB, GIB]);
        let locked: Vec<_> = spans
            .iter()
            .map(|s| s.len - s.bytes_at(PageSizeLevel::Regular))
            .collect();
        assert_eq!(locked, vec![2 * GIB, 2 * GIB, 2 * GIB + 512 * MIB, 2 * GIB]);
        assert!(spans.iter().all(|s| s.len == s.target));
    }

    #[test]
    fn commit_methods_lay_out_the_same() {
        let layout = |method| {
            let vm = FakeVm::default();
            let mut req = StitchRequest::new(threads(&[2 * GIB + 48 * MIB; 2], &[None; 2]));
            req.commit_method = method;
            let arena = StitchedArena::build_with(vm.clone(), &req).unwrap();
            assert_well_formed(&arena);
            let runs: Vec<_> = arena.spans().into_iter().map(|s| s.runs).collect();
            drop(arena);
            let s = vm.state();
            assert!(s.regions.is_empty(), "{method:?}: {:?}", s.regions);
            (runs, s.replaces)
        };
        let (replaced, replaces) = layout(CommitMethod::Replace);
        let (moved, none) = layout(CommitMethod::FreeThenAllocate);
        assert_eq!(replaced, moved);
        assert!(replaces > 0);
        assert_eq!(
            none, 0,
            "the default must never issue MEM_REPLACE_PLACEHOLDER"
        );
    }

    /// The 2026-09-24 crash in miniature: a 1 GiB commit of 2 MiB pages is refused, then the
    /// halving retry splits that same placeholder.
    fn refused_large_request(method: CommitMethod) -> (FakeVm, StitchRequest) {
        let vm = FakeVm::default();
        vm.state().large_bytes.insert(None, GIB + 256 * MIB);
        let mut req = StitchRequest::new(threads(&[GIB + 512 * MIB; 2], &[None; 2]));
        req.max_page = PageSizeLevel::Large;
        req.commit_method = method;
        (vm, req)
    }

    #[test]
    #[should_panic(expected = "bugcheck 0x139/0xE: split")]
    fn replace_reproduces_the_refused_large_bugcheck() {
        let (vm, req) = refused_large_request(CommitMethod::Replace);
        let _ = StitchedArena::build_with(vm, &req);
    }

    #[test]
    fn free_then_allocate_survives_the_same_refusal() {
        let (vm, req) = refused_large_request(CommitMethod::FreeThenAllocate);
        let arena = StitchedArena::build_with(vm.clone(), &req).unwrap();
        assert_well_formed(&arena);
        let spans = arena.spans();
        let large: usize = spans.iter().map(|s| s.bytes_at(PageSizeLevel::Large)).sum();
        assert_eq!(large, GIB + 256 * MIB);
        assert!(spans.iter().all(|s| s.len == s.target));
        drop(arena);
        let s = vm.state();
        assert!(s.regions.is_empty(), "{:?}", s.regions);
        assert_eq!(s.large_bytes[&None], GIB + 256 * MIB);
        assert_eq!(s.replaces, 0);
    }

    #[test]
    fn min_large_leaves_threads_short_but_not_empty() {
        let vm = FakeVm::default();
        vm.state().huge_pages.insert(None, 0);
        vm.state().large_bytes.insert(None, 3 * GIB);
        let mut req = StitchRequest::new(threads(&[2 * GIB; 2], &[None; 2]));
        req.min_page = PageSizeLevel::Large;
        let arena = StitchedArena::build_with(vm, &req).unwrap();
        let lens: Vec<_> = arena.spans().iter().map(|s| s.len).collect();
        assert_eq!(lens, vec![2 * GIB, GIB]);
    }

    #[test]
    fn a_thread_with_nothing_fails_the_build_and_cleans_up() {
        let vm = FakeVm::default();
        vm.state().huge_pages.insert(None, 1);
        vm.state().large_bytes.insert(None, 0);
        let mut req = StitchRequest::new(threads(&[GIB; 2], &[None; 2]));
        req.min_page = PageSizeLevel::Large;
        let err = StitchedArena::build_with(vm.clone(), &req).unwrap_err();
        assert!(err.contains("thread 1 got no memory"), "{err}");
        let s = vm.state();
        assert!(s.regions.is_empty(), "{:?}", s.regions);
        assert_eq!(s.huge_pages[&None], 1);
    }

    /// A missing privilege walks both ladders like a dry pool before falling to 4 KiB.
    #[test]
    fn no_privilege_drops_to_regular() {
        let vm = FakeVm::default();
        vm.state().no_privilege = true;
        let req = StitchRequest::new(threads(&[GIB + 64 * MIB; 2], &[None; 2]));
        let arena = StitchedArena::build_with(vm.clone(), &req).unwrap();
        for span in arena.spans() {
            let whole = Run {
                offset: 0,
                len: span.target,
                page: PageSizeLevel::Regular,
                numa_node: None,
            };
            assert_eq!(span.runs, vec![whole]);
        }
        let large = [1024, 512, 256, 128, 64, 32, 16].map(|mib| (PageSizeLevel::Large, mib * MIB));
        let mut ladder = vec![(PageSizeLevel::Huge, GIB)];
        ladder.extend(large);
        assert_eq!(vm.state().privilege_refusals, ladder);
        drop(arena);
        assert!(vm.state().regions.is_empty());
    }

    #[test]
    fn no_privilege_with_min_large_fails_and_cleans_up() {
        let vm = FakeVm::default();
        vm.state().no_privilege = true;
        let mut req = StitchRequest::new(threads(&[GIB; 2], &[None; 2]));
        req.min_page = PageSizeLevel::Large;
        let err = StitchedArena::build_with(vm.clone(), &req).unwrap_err();
        assert!(err.contains("thread 0 got no memory"), "{err}");
        assert!(vm.state().regions.is_empty());
    }

    /// A refusal that is not exhaustion (87: a bad request) ends the build with the hint instead
    /// of stepping down to 2 MiB pages, and nothing granted is kept.
    #[test]
    fn a_bad_request_is_an_error_not_a_step_down() {
        let vm = FakeVm::default();
        vm.state().huge_error = Some(ERROR_INVALID_PARAMETER);
        let req = StitchRequest::new(threads(&[2 * GIB; 2], &[None; 2]));
        let err = StitchedArena::build_with(vm.clone(), &req).unwrap_err();
        assert!(err.contains("error 87") && err.contains("maxpage=large"), "{err}");
        let s = vm.state();
        assert!(s.regions.is_empty(), "{:?}", s.regions);
        assert!(s.commits.iter().all(|&(page, _, _)| page == PageSizeLevel::Huge), "{:?}", s.commits);
    }

    /// 1 GiB and 2 MiB requests name their node strictly, so a block's recorded node is true;
    /// 4 KiB requests name it as a preference, since the strict form is refused for them.
    #[test]
    fn large_pages_name_their_node_strictly() {
        let decode = |params: Vec<MEM_EXTENDED_PARAMETER>| -> Vec<(i32, u64)> {
            params
                .iter()
                .map(|p| {
                    let kind = (p.Anonymous1._bitfield & ((1 << MEM_EXTENDED_PARAMETER_TYPE_BITS) - 1)) as i32;
                    // SAFETY: `ext_param` writes `ULong64` for both kinds it builds.
                    (kind, unsafe { p.Anonymous2.ULong64 })
                })
                .collect()
        };
        let attribute = MemExtendedParameterAttributeFlags.0;
        let node = MemExtendedParameterNumaNode.0;
        assert_eq!(
            decode(Win32Vm::page_params(PageSizeLevel::Huge, Some(1))),
            vec![(attribute, MEM_EXTENDED_PARAMETER_NONPAGED_HUGE as u64), (node, 1 | NUMA_NODE_MANDATORY)]
        );
        assert_eq!(
            decode(Win32Vm::page_params(PageSizeLevel::Large, Some(0))),
            vec![(attribute, MEM_EXTENDED_PARAMETER_NONPAGED_LARGE as u64), (node, NUMA_NODE_MANDATORY)]
        );
        assert_eq!(decode(Win32Vm::page_params(PageSizeLevel::Regular, Some(1))), vec![(node, 1)]);
        assert_eq!(
            decode(Win32Vm::page_params(PageSizeLevel::Large, None)),
            vec![(attribute, MEM_EXTENDED_PARAMETER_NONPAGED_LARGE as u64)]
        );
        assert!(decode(Win32Vm::page_params(PageSizeLevel::Regular, None)).is_empty());
    }

    /// Both threads live on node 0, which runs out of 1 GiB pages. They take node 0's 2 MiB pages
    /// before anything from node 1, so every node 0 request comes before the first node 1 one.
    #[test]
    fn local_2mib_pages_come_before_remote_1gib_pages() {
        let vm = FakeVm::default();
        vm.state().huge_pages.insert(Some(0), 1);
        vm.state().large_bytes.insert(Some(0), 512 * MIB);
        vm.state().huge_pages.insert(Some(1), 8);
        let mut req = StitchRequest::new(threads(&[2 * GIB; 2], &[Some(0); 2]));
        req.numa_nodes = 2;
        let arena = StitchedArena::build_with(vm.clone(), &req).unwrap();
        assert_well_formed(&arena);
        let commits = vm.state().commits.clone();
        let last_local = commits.iter().rposition(|&(_, node, _)| node == Some(0)).unwrap();
        let first_remote = commits.iter().position(|&(_, node, _)| node == Some(1)).unwrap();
        assert!(last_local < first_remote, "{commits:?}");
        assert!(commits[..first_remote].iter().any(|&(page, _, granted)| page == PageSizeLevel::Large && granted), "{commits:?}");
        assert!(commits[first_remote..].iter().any(|&(page, _, granted)| page == PageSizeLevel::Huge && granted), "{commits:?}");
    }

    #[test]
    fn commit_limit_is_fatal() {
        let vm = FakeVm::default();
        vm.state().no_privilege = true;
        vm.state().regular_bytes = Some(GIB);
        let req = StitchRequest::new(threads(&[GIB; 2], &[None; 2]));
        let err = StitchedArena::build_with(vm.clone(), &req).unwrap_err();
        assert!(err.contains("4 KiB pages for thread 1"), "{err}");
        assert!(vm.state().regions.is_empty());
    }

    #[test]
    fn teardown_releases_every_piece() {
        let vm = FakeVm::default();
        vm.state().huge_pages.insert(Some(0), 3);
        vm.state().large_bytes.insert(Some(0), GIB);
        let sizes = [3 * GIB + 16 * MIB, GIB, 2 * GIB];
        let req = StitchRequest::new(threads(&sizes, &[Some(0); 3]));
        let arena = StitchedArena::build_with(vm.clone(), &req).unwrap();
        assert_well_formed(&arena);
        assert!(vm.state().regions.len() > 3);
        drop(arena);
        let s = vm.state();
        assert!(s.regions.is_empty(), "{:?}", s.regions);
        assert_eq!(s.huge_pages[&Some(0)], 3);
        assert_eq!(s.large_bytes[&Some(0)], GIB);
    }

    #[test]
    fn losing_the_hole_is_fatal_and_leaves_the_intruder_alone() {
        let vm = FakeVm::default();
        vm.state().steal_next_release = true;
        let req = StitchRequest::new(threads(&[2 * GIB; 2], &[None; 2]));
        let err = StitchedArena::build_with(vm.clone(), &req).unwrap_err();
        assert!(err.contains("lost VA"), "{err}");
        let s = vm.state();
        assert_eq!(s.regions.len(), 1, "{:?}", s.regions);
        assert!(
            s.regions
                .values()
                .all(|&(len, r)| r == Region::Foreign && len == GIB)
        );
    }

    #[test]
    fn power_of_two_split_is_the_binary_digits() {
        assert_eq!(
            power_of_two_split(3 * GIB + 48 * MIB),
            vec![2 * GIB, GIB, 32 * MIB, 16 * MIB]
        );
        assert_eq!(power_of_two_split(GIB), vec![GIB]);
        assert!(power_of_two_split(0).is_empty());
    }

    #[test]
    fn blocks_tile_each_span_and_free_the_arena_last() {
        let vm = FakeVm::default();
        vm.state().huge_pages.insert(None, 3);
        vm.state().large_bytes.insert(None, 512 * MIB);
        let req = StitchRequest::new(threads(&[2 * GIB + 48 * MIB; 2], &[None; 2]));
        let arena = Arc::new(StitchedArena::build_with(vm.clone(), &req).unwrap());
        let spans = arena.spans();

        let blocks = into_allocation_blocks(arena.clone());
        for span in &spans {
            let mut addr = span.base;
            for block in &blocks[&span.thread_id] {
                assert_eq!(block.buffer.as_mut_ptr() as usize, addr);
                assert!(block.buffer.size().is_power_of_two());
                assert!(block.buffer.size() >= 16 * MIB);
                assert_eq!(block.block_info.size_bytes, block.buffer.size());
                assert_eq!(block.block_info.thread_id, span.thread_id);
                addr += block.buffer.size();
            }
            assert_eq!(addr, span.base + span.len);
            // Every block but the last joins the next, so the span is one region (TODO 76)
            let thread_blocks = &blocks[&span.thread_id];
            assert!(thread_blocks[..thread_blocks.len() - 1].iter().all(|b| b.joins_next));
            assert!(!thread_blocks.last().unwrap().joins_next);
            let regions = crate::test_memory::regions(thread_blocks);
            assert_eq!((regions.len(), regions[0].len), (1, span.len));
        }

        drop(arena);
        assert!(
            !vm.state().regions.is_empty(),
            "views must keep the arena alive"
        );
        drop(blocks);
        assert!(vm.state().regions.is_empty());
    }
}
