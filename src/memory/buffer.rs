// Every `unsafe` block in this file crosses a Win32 FFI boundary, where the borrow checker is
// switched off precisely where the invariants get subtle (TODO #66). The lint below makes a missing
// `// SAFETY:` a warning *here* rather than relying on a periodic audit — it is deliberately not
// crate-wide, because the SIMD test kernels' `unsafe` is a different, repetitive story already
// covered by `test_fn_safety.md`, and a blanket rule there would produce boilerplate that trains
// you to skip reading these.
#![warn(clippy::undocumented_unsafe_blocks)]

use crate::memory::backend::{Backend, BackendAllocation};
use std::sync::Arc;

/// Memory from a backend: where it is and how long. What pages and nodes it is on is the
/// allocator's to report, per thread span (`stitched::ThreadSpan`), not the buffer's.
#[derive(Debug)]
pub struct MemoryBuffer {
    ptr: *mut u8,
    size: usize,
    backend: Arc<dyn Backend>,
}

/// Caching behaviour requested for an allocation.
///
/// The single canonical memory-type vocabulary for TMR, formed by merging the old
/// `driver::MemoryType` into this enum when the driver client was purged (TODO #4/5,
/// 2026-09-14). The duplicate `WriteCombined` spelling was dropped in that merge;
/// `WriteCombining` is the one.
///
/// Only `WriteBack` is constructed today: `VirtualAlloc2` cannot request UC/WC/WP for
/// ordinary RAM, and WB + CLFLUSHOPT forces a DRAM round-trip faster than UC would.
/// The other variants are kept deliberately as the vocabulary a ring-0 backend would
/// map onto `MEMORY_CACHING_TYPE`, so one can slot back in as a `Backend` impl without
/// a redesign.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[expect(dead_code, reason = "revival seam (TODO #4/5): only `WriteBack` is constructed, see above")]
pub enum MemoryType {
    WriteBack,
    WriteThrough,
    WriteCombining,
    Uncached,
    WriteProtected,
}

impl MemoryBuffer {
    pub fn new(allocation: BackendAllocation, backend: Arc<dyn Backend>) -> Self {
        Self {
            ptr: allocation.ptr,
            size: allocation.size,
            backend,
        }
    }
    
    pub fn as_mut_ptr(&self) -> *mut u8 { 
        self.ptr 
    }
    
    pub fn size(&self) -> usize { 
        self.size 
    }
    
}

impl Drop for MemoryBuffer {
    fn drop(&mut self) {
        let allocation = BackendAllocation {
            ptr: self.ptr,
            size: self.size,
        };
        
        if let Err(e) = self.backend.free(allocation) {
            free_failed(&e);
        }
    }
}

/// Logs a buffer the backend couldn't free. Out of line and cold, as the `eprintln!` it replaced
/// was one call (TODO 29 S4): the worker pool's setup and shutdown inline the drop of their
/// buffers, and a log record built in place changed their code.
#[cold]
#[inline(never)]
fn free_failed(e: &str) {
    log::error!("Failed to free memory buffer: {}", e);
}

// SAFETY: `MemoryBuffer` owns its allocation — `ptr` is produced by the backend and
// freed exactly once in `Drop` (below), which runs on whichever thread holds the buffer. No
// thread-affine state (no TLS, no HANDLE tied to a thread), and `VirtualFree` may be called
// from any thread, so transferring ownership across threads is sound.
unsafe impl Send for MemoryBuffer {}

// SAFETY: this is the stronger claim and it rests on a CONVENTION, not on the type.
// `as_mut_ptr(&self)` hands out a `*mut u8` from a *shared* reference, so `&MemoryBuffer`
// shared across threads could alias mutable memory. It is sound only because the allocator
// gives each worker thread its OWN blocks and workers never write outside the block(s) they
// were assigned — the disjointness is enforced by the coordinator (see `AllocationBlock` /
// `BlockInfo` thread assignment), not by the borrow checker.
//
// WARNING: if a future change lets two threads write the same block (e.g. the shared-block
// worker model in TODO #28, or a cross-thread page-exchange test), this `Sync` impl is no
// longer justified by the above and the aliasing must be made explicit instead — split the
// buffer into disjoint `&mut [u8]` slices per worker, or move to atomics/`UnsafeCell` with a
// documented protocol. Do not rely on this comment staying true by accident.
unsafe impl Sync for MemoryBuffer {}
