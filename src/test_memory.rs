//! Where a test's extent lies in a thread's memory (TODO 76).
//!
//! Each thread gets a list of `AllocationBlock`s. Under `allocator=stitched` they are one
//! contiguous VA span, and every block but the last carries `joins_next`; under
//! `plan-pagesize-pref` each block is its own range. A *region* is a longest run of joined blocks,
//! so a stitched thread has one region and a legacy thread one per block. The extent is the first
//! `extent` bytes of the regions in order: whole regions, then the remainder.

use std::ops::Range;

use crate::runner::AllocationBlock;
use crate::tests::TestBlock;

/// Extent pieces and chunks are multiples of this, and chunks start on it. It makes halves,
/// quarters and subdivisions exact for every vector width, and keeps chunk starts page-aligned.
pub const GRANULE: usize = 4096;

/// A longest run of address-adjacent blocks the allocator joined.
#[derive(Debug)]
pub struct Region {
    pub base: *mut u8,
    pub len: usize,
    /// Which of the thread's blocks it covers.
    pub blocks: Range<usize>,
}

/// The thread's regions, in block order.
pub fn regions(blocks: &[AllocationBlock]) -> Vec<Region> {
    let mut out: Vec<Region> = Vec::new();
    for (i, block) in blocks.iter().enumerate() {
        let base = block.buffer.as_mut_ptr();
        let len = block.buffer.size();
        if i > 0
            && blocks[i - 1].joins_next
            && let Some(last) = out.last_mut()
        {
            let adjacent = last.base as usize + last.len == base as usize;
            debug_assert!(adjacent, "joins_next set on a block that doesn't end where the next starts");
            if adjacent {
                last.len += len;
                last.blocks.end = i + 1;
                continue;
            }
        }
        out.push(Region { base, len, blocks: i..i + 1 });
    }
    out
}

/// The extent's pieces: the first `extent` bytes of the regions, whole regions first, each piece
/// a multiple of `GRANULE` (the remainder is floored to it). Empty when `extent < GRANULE`.
pub fn extent_pieces(blocks: &[AllocationBlock], extent: usize) -> Vec<TestBlock<'_>> {
    let mut left = extent - extent % GRANULE;
    let mut pieces = Vec::new();
    for region in regions(blocks) {
        if left == 0 {
            break;
        }
        let take = region.len.min(left) / GRANULE * GRANULE;
        if take == 0 {
            break;
        }
        pieces.push(TestBlock::in_blocks(blocks, region.base, take));
        left -= take;
    }
    pieces
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

/// The pieces for the per-thread log line, each with its page sizes and chunks: one piece of a
/// stitched span `880 MiB 1G (2 x 440 MiB)`; one across a page-size seam
/// `1136 MiB 1G 1024 MiB/2M 112 MiB (3 x 440 MiB)`; legacy blocks `1024 MiB 1G (1 x 1024 MiB) +
/// 512 MiB 2M (1 x 512 MiB)`. A piece on more than one NUMA node ends in `nodes 0+1`.
pub fn describe_pieces(blocks: &[AllocationBlock], pieces: &[TestBlock<'_>], chunk_of: impl Fn(usize) -> usize) -> String {
    const NAMES: [&str; 3] = ["1G", "2M", "4K"];
    pieces.iter().map(|piece| {
        let (mix, nodes) = page_mix(blocks, piece.ptr, piece.test_size);
        let kinds: Vec<usize> = (0..3).filter(|&k| mix[k] > 0).collect();
        let pages = if kinds.len() == 1 {
            NAMES[kinds[0]].to_string()
        } else {
            kinds.iter().map(|&k| format!("{} {}", NAMES[k], size_str(mix[k]))).collect::<Vec<_>>().join("/")
        };
        let chunk = chunk_of(piece.test_size).max(1);
        let nodes = if nodes.len() > 1 {
            format!(" nodes {}", nodes.iter().map(|n| n.to_string()).collect::<Vec<_>>().join("+"))
        } else {
            String::new()
        };
        format!("{} {} ({} x {}){nodes}", size_str(piece.test_size), pages, piece.test_size.div_ceil(chunk), size_str(chunk))
    }).collect::<Vec<_>>().join(" + ")
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

    /// One block: `(size, page_type, joins_next)`.
    type Spec = (usize, fn(usize) -> PageType, bool);

    /// Blocks at `addr`, in order.
    fn blocks(addr: usize, spec: &[Spec]) -> Vec<AllocationBlock> {
        let backend: Arc<dyn Backend> = Arc::new(NoMemory);
        let mut at = addr;
        spec.iter().map(|&(size, page, joins_next)| {
            let block = AllocationBlock {
                buffer: MemoryBuffer::new(
                    BackendAllocation { ptr: at as *mut u8, size, info: BufferInfo { numa_node: 0, page_type: page(size) } },
                    backend.clone(),
                ),
                block_info: BlockInfo { size_bytes: size, thread_id: 0 },
                joins_next,
            };
            at += size;
            block
        }).collect()
    }

    fn sizes(pieces: &[TestBlock<'_>]) -> Vec<usize> {
        pieces.iter().map(|p| p.test_size).collect()
    }

    #[test]
    fn joined_blocks_are_one_region_and_the_rest_one_each() {
        // A stitched span: 8 GiB + 1 GiB of 1 GiB pages, then 256 MiB of 2 MiB pages
        let span = blocks(1 << 40, &[(8 * GIB, PageType::Huge, true), (GIB, PageType::Huge, true), (256 * MIB, PageType::Large, false)]);
        let r = regions(&span);
        assert_eq!(r.len(), 1);
        assert_eq!((r[0].len, r[0].blocks.clone()), (9 * GIB + 256 * MIB, 0..3));

        // Legacy: adjacent or not, unjoined blocks stay separate
        let legacy = blocks(1 << 40, &[(8 * GIB, PageType::Huge, false), (GIB, PageType::Huge, false)]);
        assert_eq!(regions(&legacy).len(), 2);
    }

    #[test]
    fn the_extent_is_the_first_bytes_of_the_regions() {
        let span = blocks(1 << 40, &[(8 * GIB, PageType::Huge, true), (GIB, PageType::Huge, true), (256 * MIB, PageType::Large, false)]);
        // One piece per region: no power-of-two slivers
        assert_eq!(sizes(&extent_pieces(&span, 880 * MIB)), [880 * MIB]);
        assert_eq!(sizes(&extent_pieces(&span, 9 * GIB + 100 * MIB)), [9 * GIB + 100 * MIB]);
        // More than there is: all of it
        assert_eq!(sizes(&extent_pieces(&span, usize::MAX)), [9 * GIB + 256 * MIB]);
        // Floored to the granule, and empty below it
        assert_eq!(sizes(&extent_pieces(&span, 880 * MIB + 100)), [880 * MIB]);
        assert!(extent_pieces(&span, GRANULE - 1).is_empty());

        // Legacy: whole blocks in order, then the remainder
        let legacy = blocks(1 << 40, &[(GIB, PageType::Huge, false), (GIB, PageType::Huge, false), (32 * MIB, PageType::Large, false)]);
        assert_eq!(sizes(&extent_pieces(&legacy, 1536 * MIB)), [GIB, 512 * MIB]);
        assert_eq!(sizes(&extent_pieces(&legacy, 880 * MIB)), [880 * MIB]);
    }

    #[test]
    fn pieces_report_their_page_sizes_across_a_seam() {
        let span = blocks(1 << 40, &[(GIB, PageType::Huge, true), (256 * MIB, PageType::Large, false)]);
        let pieces = extent_pieces(&span, GIB + 112 * MIB);
        let line = describe_pieces(&span, &pieces, |piece| piece.clamp(GRANULE, 440 * MIB));
        assert_eq!(line, "1136 MiB 1G 1024 MiB/2M 112 MiB (3 x 440 MiB)");
        assert_eq!(size_str(293 * MIB + MIB / 3), "293.33 MiB");
        assert_eq!(size_str(64 * 1024), "64 KiB");
    }
}
