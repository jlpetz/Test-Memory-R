//! Where a test's extent lies in a thread's memory (TODO 76).
//!
//! Each thread gets one contiguous VA span from the stitched allocator, as a list of
//! `AllocationBlock`s that each end where the next begins. A test's extent is the first `extent`
//! bytes of that span, and its chunks spread evenly over the extent (`ChunkSpread`).

use crate::runner::AllocationBlock;
use crate::tests::TestBlock;

/// Extent pieces and chunks are multiples of this, and chunks start on it. It makes halves,
/// quarters and subdivisions exact for every vector width, and keeps chunk starts page-aligned.
pub const GRANULE: usize = 4096;

/// How a test's chunks cover its extent (TODO 76): `count()` chunks of exactly `chunk()` bytes,
/// spread evenly, the first at 0 and the last ending at the extent. Start k is k(E-c)/(n-1)
/// floored to the granule, each start on its own, so consecutive starts are at most c apart (no
/// holes) and the overlaps differ by at most one granule; the total overlap is under c. When c
/// divides the extent the starts are k*c, TM5's tiling with no overlap; when c is at least the
/// extent there is one chunk, the extent. With 3 or more chunks, chunks k and k+2 never overlap.
/// Every word must be position-pure for this: a chunk rewrites what its neighbour wrote.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChunkSpread {
    extent: usize,
    chunk: usize,
    count: usize,
    /// (E-c)/G = q*(n-1) + r, so start(k) = (k*q + k*r/(n-1)) * G without overflow.
    q: usize,
    r: usize,
    d: usize,
    granule: usize,
}

impl ChunkSpread {
    pub fn new(extent: usize, chunk: usize) -> Self {
        Self::with_granule(extent, chunk, GRANULE)
    }

    /// For halves (BlockMove, Spd-Copy): `with_granule(E / 2, c / 2, GRANULE / 2)`.
    pub fn with_granule(extent: usize, chunk: usize, granule: usize) -> Self {
        debug_assert!(extent.is_multiple_of(granule) && chunk.is_multiple_of(granule) && chunk > 0,
                      "extent {extent} and chunk {chunk} must be non-zero multiples of {granule}");
        if extent == 0 {
            return Self { extent, chunk: 0, count: 0, q: 0, r: 0, d: 1, granule };
        }
        let chunk = chunk.clamp(1, extent);
        let count = extent.div_ceil(chunk);
        let span = (extent - chunk) / granule;
        let d = (count - 1).max(1);
        Self { extent, chunk, count, q: span / d, r: span % d, d, granule }
    }

    #[inline(always)]
    pub fn count(&self) -> usize {
        self.count
    }

    #[inline(always)]
    pub fn chunk(&self) -> usize {
        self.chunk
    }

    /// Byte offset of chunk `k` (`k < count()`), a multiple of the granule.
    #[inline(always)]
    pub fn start(&self, k: usize) -> usize {
        (k * self.q + k * self.r / self.d) * self.granule
    }

    /// Bytes walked per pass, overlaps counted each time: `count() * chunk()`.
    pub fn walked(&self) -> usize {
        self.count * self.chunk
    }

    /// Bytes walked twice per pass: `walked() - extent`, under one chunk.
    pub fn overlap(&self) -> usize {
        self.walked() - self.extent
    }
}

/// The extent: the first `extent` bytes of the thread's span, floored to `GRANULE`, at most the
/// span. Empty below one granule, or when the thread has no memory.
pub fn extent(blocks: &[AllocationBlock], extent: usize) -> TestBlock<'_> {
    debug_assert!(blocks.windows(2).all(|w| w[0].buffer.as_mut_ptr() as usize + w[0].buffer.size() == w[1].buffer.as_mut_ptr() as usize),
                  "a thread's blocks must be one contiguous span");
    let Some(first) = blocks.first() else {
        return TestBlock::empty();
    };
    let span: usize = blocks.iter().map(|b| b.buffer.size()).sum();
    TestBlock::in_blocks(blocks, first.buffer.as_mut_ptr(), extent.min(span) / GRANULE * GRANULE)
}

/// Bytes of `[ptr, ptr + len)` on 1 GiB, 2 MiB and 4 KiB pages, and the NUMA nodes it lies on.
fn page_mix(blocks: &[AllocationBlock], ptr: *mut u8, len: usize) -> ([usize; 3], Vec<u32>) {
    let (start, end) = (ptr as usize, ptr as usize + len);
    let mut mix = [0usize; 3];
    let mut nodes = Vec::new();
    for block in blocks {
        let b_start = block.buffer.as_mut_ptr() as usize;
        let b_end = b_start + block.buffer.size();
        let overlap = end.min(b_end).saturating_sub(start.max(b_start));
        if overlap > 0 {
            let kind = if block.buffer.uses_huge_pages() { 0 } else if block.buffer.uses_large_pages() { 1 } else { 2 };
            mix[kind] += overlap;
            let node = block.buffer.info().numa_node;
            if !nodes.contains(&node) {
                nodes.push(node);
            }
        }
    }
    (mix, nodes)
}

/// A size for log lines: "880 MiB", "293.33 MiB", "64 KiB".
pub fn size_str(bytes: usize) -> String {
    const MIB: usize = 1024 * 1024;
    if bytes >= MIB {
        let mib = bytes as f64 / MIB as f64;
        if bytes.is_multiple_of(MIB) { format!("{} MiB", bytes / MIB) } else { format!("{mib:.2} MiB") }
    } else {
        format!("{} KiB", bytes.div_ceil(1024))
    }
}

/// The extent for the per-thread log line, with its page sizes and chunks: `880 MiB 1G (2 x 440
/// MiB)`, or across a page-size seam `1136 MiB 1G 1024 MiB/2M 112 MiB (3 x 440 MiB, 184 MiB
/// overlap)`. An extent on more than one NUMA node ends in `nodes 0+1`.
pub fn describe_extent(blocks: &[AllocationBlock], extent: &TestBlock<'_>, spread: ChunkSpread) -> String {
    const NAMES: [&str; 3] = ["1G", "2M", "4K"];
    let (mix, nodes) = page_mix(blocks, extent.ptr, extent.test_size);
    let kinds: Vec<usize> = (0..3).filter(|&k| mix[k] > 0).collect();
    let pages = if kinds.len() == 1 {
        NAMES[kinds[0]].to_string()
    } else {
        kinds.iter().map(|&k| format!("{} {}", NAMES[k], size_str(mix[k]))).collect::<Vec<_>>().join("/")
    };
    let overlap = if spread.overlap() > 0 { format!(", {} overlap", size_str(spread.overlap())) } else { String::new() };
    let nodes = if nodes.len() > 1 {
        format!(" nodes {}", nodes.iter().map(|n| n.to_string()).collect::<Vec<_>>().join("+"))
    } else {
        String::new()
    };
    format!("{} {} ({} x {}{overlap}){nodes}", size_str(extent.test_size), pages, spread.count(), size_str(spread.chunk()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use crate::BlockInfo;
    use crate::memory::MemoryBuffer;
    use crate::memory::allocator::AllocationConfig;
    use crate::memory::backend::{AllocError, Backend, BackendAllocation};
    use crate::memory::buffer::{BufferInfo, PageType};

    const MIB: usize = 1024 * 1024;
    const GIB: usize = 1024 * MIB;

    /// A backend for blocks at made-up addresses: nothing to allocate or free.
    #[derive(Debug)]
    struct NoMemory;
    impl Backend for NoMemory {
        fn allocate(&self, _: &AllocationConfig) -> Result<BackendAllocation, AllocError> {
            unreachable!("test blocks are built directly")
        }
        fn free(&self, _: BackendAllocation) -> Result<(), String> { Ok(()) }
        fn name(&self) -> &'static str { "none" }
    }

    /// One block: `(size, page_type)`.
    type Spec = (usize, fn(usize) -> PageType);

    /// Blocks at `addr`, in order.
    fn blocks(addr: usize, spec: &[Spec]) -> Vec<AllocationBlock> {
        let backend: Arc<dyn Backend> = Arc::new(NoMemory);
        let mut at = addr;
        spec.iter().map(|&(size, page)| {
            let block = AllocationBlock {
                buffer: MemoryBuffer::new(
                    BackendAllocation { ptr: at as *mut u8, size, info: BufferInfo { numa_node: 0, page_type: page(size) } },
                    backend.clone(),
                ),
                block_info: BlockInfo { size_bytes: size, thread_id: 0 },
            };
            at += size;
            block
        }).collect()
    }

    #[test]
    fn the_extent_is_the_first_bytes_of_the_span() {
        // A span: 8 GiB + 1 GiB of 1 GiB pages, then 256 MiB of 2 MiB pages
        let span = blocks(1 << 40, &[(8 * GIB, PageType::Huge), (GIB, PageType::Huge), (256 * MIB, PageType::Large)]);
        let size = |e: usize| extent(&span, e).test_size;
        assert_eq!(size(880 * MIB), 880 * MIB);
        assert_eq!(size(9 * GIB + 100 * MIB), 9 * GIB + 100 * MIB);
        // More than there is: all of it
        assert_eq!(size(usize::MAX), 9 * GIB + 256 * MIB);
        // Floored to the granule, and empty below it
        assert_eq!(size(880 * MIB + 100), 880 * MIB);
        assert_eq!(size(GRANULE - 1), 0);
        assert_eq!(extent(&[], 880 * MIB).test_size, 0);
    }

    #[test]
    fn the_extent_reports_its_page_sizes_across_a_seam() {
        let span = blocks(1 << 40, &[(GIB, PageType::Huge), (256 * MIB, PageType::Large)]);
        let e = extent(&span, GIB + 112 * MIB);
        let line = describe_extent(&span, &e, ChunkSpread::new(e.test_size, 440 * MIB));
        assert_eq!(line, "1136 MiB 1G 1024 MiB/2M 112 MiB (3 x 440 MiB, 184 MiB overlap)");
        assert_eq!(size_str(293 * MIB + MIB / 3), "293.33 MiB");
        assert_eq!(size_str(64 * 1024), "64 KiB");
    }
}
