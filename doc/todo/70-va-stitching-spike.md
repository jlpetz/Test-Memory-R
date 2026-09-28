# TODO 70. [TMR-APP] Spike: VA stitching into one placeholder, with large pages intact

> Full record, moved verbatim out of `TODO.md` on 2026-09-28 (open item).
> The short entry in `TODO.md` holds the current status; this file holds the reasoning.

**Priority**: High (blocks the memory-system redesign — the answer decides how much multi-row
machinery `LogicalSpace` needs in its first version)
**Status**: Spike done in `../stitch-test2/` (2026-09-24/25). Large pages do go into a
placeholder, but by free-then-allocate, because a refused large-page replace bugchecks the machine.
The result is one span per thread, not the two rows expected below. Integration is #75 C; close
this when it lands. (Raised 2026-09-18.)

**Design doc: `TMR-APP/doc/memory_system_design.md` §8** (plan + the ordering correction),
with §4.5–4.7 for the chunk model it unblocks and `doc/tile_abstraction_spec.md` §14–15 for L1.

**Goal**: give each thread ONE contiguous virtual address range covering all of its memory, without
losing 2 MiB/1 GiB pages. Today each thread holds a list of separately-allocated blocks, so a
configured chunk size larger than a block silently becomes `min(C, block remainder)` — the chunk
contract is not honoured (bug B5), and relational kernels (Mirror, BlockMove) cannot span a seam.

**The ordering correction — read this before starting.** The obvious plan ("allocate as today, let the
balancer distribute, *then* stitch each thread's blocks") **cannot be implemented**:
`MEM_REPLACE_PLACEHOLDER` is a commit-time argument, and there is no operation that relocates
already-committed memory into a placeholder. Large pages make this doubly true — a 1 GiB page is a
PDPTE covering one specific aligned VA range. "Allocate first, place second" would require section
objects (`CreateFileMapping2` + `MapViewOfFile3` + `SEC_LARGE_PAGES`), a different acquisition path.

**So invert only the reservation, which is free (128 TiB of VA):**

1. Reserve ONE placeholder for the whole run:
   `VirtualAlloc2(NULL, total, MEM_RESERVE | MEM_RESERVE_PLACEHOLDER, PAGE_NOACCESS, ...)`
   with `MEM_ADDRESS_REQUIREMENTS { Alignment = 1 GiB }`.
2. Acquire blocks exactly as today (page-size-first, greedy, **descending size**), splitting the next
   slot off the placeholder head and committing into it with `MEM_REPLACE_PLACEHOLDER`.
3. Distribute to threads afterwards (balancing logic unchanged) — each thread gets a contiguous slice.
4. Return one base pointer plus per-thread `(offset, len)`.

**Why this is better than per-thread stitching**: distribution moves *after* acquisition, which is
exactly what the stranded-huge-page bug needs (opportunistic 1 GiB blocks rejected against an
already-divided per-thread plan, `allocator.rs:1141/1180`) — the two fixes become one change. It also
makes cross-thread handover (#19) trivial, since a tile descriptor is then just an offset into one space.

**The base-address floor is preserved and in fact strengthened.** Today all ~24 allocations each pass
`MEM_ADDRESS_REQUIREMENTS { LowestStartingAddress = 0x1C0000000 }` and independently hope to land above
it. With a placeholder that policy is stated **once**, on the reservation, and then holds structurally
for the whole span. The pre-buffer / post-reserve fragmentation strategy is untouched — that governs how
much *physical* memory is requested, and reserving VA consumes none.

**Sub-task, do this FIRST (independently useful): report page-type fairness.** The 2026-09-18 run shows
the current distributor is byte-fair but **page-type-blind** — 7 huge 4 GiB blocks across 8 threads
means one thread (thread 6) got **0 % huge pages** while the other seven got 57 %, and the fairness
table still printed `CV=0.000 ✅ Fair` because it only measures bytes. Two threads running the same test
on differently-paged memory are not comparable. Add huge/large share to the fairness analysis so the
imbalance is visible now and the after-state has a baseline to beat. Under the placeholder design the
cut is an offset rather than a block, so the same run becomes `huge = 4,4,4,4,3,3,3,3 GiB` — worst
deviation 0.5 GiB instead of 3.5 GiB, with cuts still aligned to their own page size.

**Expected outcome is TWO rows per thread, not one.** A 1 GiB page can only be committed at a
1 GiB-aligned offset, so huge and large blocks cannot be interleaved — page types stay segregated in
the space. One contiguous slice per thread would therefore require making some threads all-huge and
others all-large, which is *worse* than today. So the target is one row per page type per thread (24
blocks → 16 rows in that run). Every chunk up to ~3 GiB resolves to a single segment; a chunk cannot
span the huge/large divide, so `C = 4 GiB` on that machine must be **rejected with the reason** at plan
time rather than silently truncated. `LogicalSpace`'s multi-row path stays load-bearing either way.

**Alignment is the crux, and it has an answer.** The suspicion that "the way we call it today to FORCE
huge pages stops stitching" is correct as stated: alignment currently comes from
`MEM_ADDRESS_REQUIREMENTS` with `BaseAddress = NULL` (`allocator.rs:548/553/699/758`), and you cannot
pass address requirements while naming an explicit placeholder address. **Move the alignment to the
placeholder reservation**, where `BaseAddress` *is* NULL and the request is legal. Every 1 GiB-multiple
offset inside it is then 1 GiB-aligned by construction, and descending-size packing guarantees each
block's offset is a multiple of every page size still to come. Alignment is preserved structurally.
This does not weaken the settled rule that large-page requests must pass a matching `alignment` — it
relocates where the alignment is obtained.

**The open question the spike exists to answer**: is `MEM_LARGE_PAGES` accepted together with
`MEM_COMMIT | MEM_REPLACE_PLACEHOLDER` at all? Undocumented; may just fail with `87`.

**If it appears to succeed, DO NOT BELIEVE IT.** Real page size is not observable after the fact. The
plausible failure mode is not an error return but a **silent downgrade to 4 KB while reporting
success** — worse than not stitching, because it converts every test into a TLB-thrash benchmark.
Validation is behavioural and mandatory:

- Build a TLB-sensitive random-access proxy (pointer-chase / random 64-byte strides over the whole
  region, working set ≫ L3) over the stitched region.
- Run the identical proxy over a plain `MEM_LARGE_PAGES` allocation of the same size, same machine,
  same thread pinning, as the control.
- Comparable latency ⇒ real large pages. A large regression ⇒ 4 KB pages, and the path is dead.
- Classify any failure code rather than guessing: `1450` = no contiguous physical / fragmentation,
  `1314` = missing `SeLockMemoryPrivilege`, `87` = malformed request (our bug).

**Do it as a standalone probe first** (a few hours, like `../shuffle-test/`), not inside the allocator.
Try 2 MiB before 1 GiB; a 2 MiB-only result is still a useful partial answer.

**This reopens a settled decision on purpose.** TMR-APP `CLAUDE.md` lists "No VA stitching" as settled
with the revival trigger *"a kernel needing one contiguous logical span larger than the biggest single
allocatable block"* — 4 GiB chunks over a 5–6 GiB space is that case. If the spike succeeds, rewrite
that entry rather than silently contradicting it. Keep the part of it that is still true: **stitching
VA does not stitch PA**, so a seam remains a physical discontinuity and nothing here improves physical
locality. The gain is honest chunk sizes and span-crossing kernels, not better DRAM adjacency.

**Not blocking**: if the spike fails, the redesign proceeds unchanged — `LogicalSpace` just carries
more rows. Nothing in the design depends on stitching succeeding.
