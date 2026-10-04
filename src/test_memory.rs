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
/// `1136 MiB 1G 1024 MiB/2M 112 MiB (3 x 440 MiB, 184 MiB overlap)`; legacy blocks
/// `1024 MiB 1G (1 x 1024 MiB) + 512 MiB 2M (1 x 512 MiB)`. A piece on more than one NUMA node
/// ends in `nodes 0+1`.
pub fn describe_pieces(blocks: &[AllocationBlock], pieces: &[TestBlock<'_>], chunks_of: impl Fn(usize) -> ChunkSpread) -> String {
    const NAMES: [&str; 3] = ["1G", "2M", "4K"];
    pieces.iter().map(|piece| {
        let (mix, nodes) = page_mix(blocks, piece.ptr, piece.test_size);
        let kinds: Vec<usize> = (0..3).filter(|&k| mix[k] > 0).collect();
        let pages = if kinds.len() == 1 {
            NAMES[kinds[0]].to_string()
        } else {
            kinds.iter().map(|&k| format!("{} {}", NAMES[k], size_str(mix[k]))).collect::<Vec<_>>().join("/")
        };
        let spread = chunks_of(piece.test_size);
        let overlap = if spread.overlap() > 0 { format!(", {} overlap", size_str(spread.overlap())) } else { String::new() };
        let nodes = if nodes.len() > 1 {
            format!(" nodes {}", nodes.iter().map(|n| n.to_string()).collect::<Vec<_>>().join("+"))
        } else {
            String::new()
        };
        format!("{} {} ({} x {}{overlap}){nodes}", size_str(piece.test_size), pages, spread.count(), size_str(spread.chunk()))
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

    /// Every guarantee `ChunkSpread` documents, for one extent and chunk.
    fn check_spread(extent: usize, chunk: usize, g: usize) {
        let s = ChunkSpread::with_granule(extent, chunk, g);
        let c = s.chunk();
        let n = s.count();
        let case = format!("extent {extent} chunk {chunk} granule {g}");
        assert_eq!(c, chunk.min(extent), "{case}");
        assert_eq!(n, extent.div_ceil(c), "{case}");
        assert_eq!(s.start(0), 0, "{case}");
        assert_eq!(s.start(n - 1) + c, extent, "{case}: the last chunk ends at the extent");
        let starts: Vec<usize> = (0..n).map(|k| s.start(k)).collect();
        assert!(starts.iter().all(|&x| x % g == 0), "{case}: starts on the granule");
        let overlaps: Vec<usize> = starts.windows(2).map(|w| {
            assert!(w[1] > w[0] && w[1] - w[0] <= c, "{case}: a hole between {} and {}", w[0], w[1]);
            w[0] + c - w[1]
        }).collect();
        if let (Some(lo), Some(hi)) = (overlaps.iter().min(), overlaps.iter().max()) {
            assert!(hi - lo <= g, "{case}: overlaps {lo}..{hi} differ by more than a granule");
        }
        assert_eq!(s.overlap(), overlaps.iter().sum::<usize>(), "{case}");
        assert!(s.overlap() < c, "{case}");
        if extent.is_multiple_of(c) {
            assert!(starts.iter().enumerate().all(|(k, &x)| x == k * c), "{case}: exact tiles");
        }
        if n >= 3 {
            assert!(starts.windows(3).all(|w| w[2] - w[0] >= c), "{case}: k and k+2 overlap");
        }
    }

    #[test]
    fn chunks_spread_evenly_with_no_holes() {
        const KIB: usize = 1024;
        // The canary: 640 KiB in 68 KiB chunks is 10 chunks, 60 or 64 KiB apart
        let s = ChunkSpread::new(640 * KIB, 68 * KIB);
        assert_eq!(s.count(), 10);
        let starts: Vec<usize> = (0..10).map(|k| s.start(k) / KIB).collect();
        assert_eq!(starts, [0, 60, 124, 188, 252, 316, 380, 444, 508, 572]);
        // One step rounded once left a hole before the last chunk in each of these
        for (e, c) in [(640 * KIB, 68 * KIB), (977 * MIB, 8 * MIB), (880 * MIB, 4100 * KIB), (7 * GIB + 12 * KIB, 64 * KIB)] {
            check_spread(e, c, GRANULE);
        }
        // One chunk, exact tiles, two chunks (the worst case: walks almost twice the extent)
        assert_eq!(ChunkSpread::new(GIB, 2 * GIB).count(), 1);
        assert_eq!(ChunkSpread::new(5 * GIB, GIB).overlap(), 0);
        let two = ChunkSpread::new(GIB, 880 * MIB);
        assert_eq!((two.count(), two.start(1), two.overlap()), (2, 144 * MIB, 736 * MIB));
        // Halves of a 4 KiB-multiple extent
        check_spread(320 * KIB, 34 * KIB, GRANULE / 2);
        // Random extents and chunks, in granules
        let mut x = 0x9E37_79B9_7F4A_7C15_u64;
        for _ in 0..20_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let e = 1 + (x % 3000) as usize;
            let c = 1 + ((x >> 32) % (e as u64 + 50)) as usize;
            check_spread(e * GRANULE, c * GRANULE, GRANULE);
        }
        assert_eq!(ChunkSpread::new(0, GRANULE).count(), 0);
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
        let line = describe_pieces(&span, &pieces, |piece| ChunkSpread::new(piece, 440 * MIB));
        assert_eq!(line, "1136 MiB 1G 1024 MiB/2M 112 MiB (3 x 440 MiB, 184 MiB overlap)");
        assert_eq!(size_str(293 * MIB + MIB / 3), "293.33 MiB");
        assert_eq!(size_str(64 * 1024), "64 KiB");
    }
}
