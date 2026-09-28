# Memory Allocation Models: TM5, TMR, and the wider field

Reference doc for how memory testers acquire, address, and subdivide memory — and which
config knob controls which concept. Written to answer "is TMR's allocation strategy ideal?",
so it is deliberately mechanism-first.

Sources: `../TM5/` (authoritative x86 asm), `TMR-APP/src/memory/`, `TMR-APP/src/tests.rs`,
`TMR-APP/src/test_scaffolding.rs`. TM5 line references are to the files in `../TM5/`.

- Part 1 — [TM5](#part-1--tm5-32-bit--awe)
- Part 2 — [TMR](#part-2--tmr-64-bit-no-aperture)
- Part 3 — [Other testers](#part-3--how-other-testers-do-it)
- Part 4 — [Side-by-side](#part-4--side-by-side)
- Part 5 — [Loop nesting: which knob repeats what](#part-5--loop-nesting-which-knob-repeats-what)
- Part 6 — [Why TM5 could reshape memory and TMR cannot](#part-6--why-tm5-could-reshape-memory-and-tmr-cannot)

Design analysis and recommendations live in a companion doc:
[`allocation_strategy_analysis.md`](allocation_strategy_analysis.md).

---

## Part 1 — TM5: 32-bit + AWE

### 1.1 The constraint that forces the whole design

TM5 is a **32-bit process**. Every pointer in the source is a `dd` (dword). A 32-bit process
gets 2 GB of user virtual address space, and `bin/awe.asm:10` records the measured practical
ceiling:

```
; VirtualAlloc в XP и XP x64 более 1.5G      -> can't reserve more than ~1.5 GB
```

That is why `Testing Window Size (Mb)` has `max = 1536` (`mt_ini.inc:207-209`). TM5 physically
cannot address more than ~1.5 GB at once. To test 64 GB of DDR5 it needs a way to *rotate*
physical memory through a small virtual aperture — that is AWE (Address Windowing Extensions).

Two multipliers get TM5 past its own address space:

1. **One OS process per tested core.** `bin/ThreadManager.asm:136` `CreateProcess`,
   `TM5.asm:85` `SetProcessAffinityMask`. Each process has its own 2 GB VA, its own window,
   and its own locked physical pool. They coordinate through a shared memory-mapped file
   (`MT_IC_SM`, `bin/mmf.inc:244`) holding a `My_MMF_MT` struct with per-CPU records
   (`MaxMemCPU = 256`).
2. **AWE inside each process** — physical pages are locked once, then map/unmapped through
   the window in slices.

### 1.2 Per-process address space

```
  One OS process per tested core (CreateProcess + SetProcessAffinityMask)

  ┌──────────────────────── Process for CPU 0 (32-bit, 2 GB VA) ─────────────────────────┐
  │                                                                                       │
  │   ┌───────────────┐                                                                   │
  │   │ code + data   │                                                                   │
  │   ├───────────────┤                                                                   │
  │   │ PFN array     │  VirtualAlloc(MEM_RESERVE|MEM_COMMIT|MEM_LARGE_PAGES)             │
  │   │               │  4 bytes per 4 KB page  ==  1 KB of array per 1 MB locked         │
  │   ├───────────────┤                                                                   │
  │   │               │                                                                   │
  │   │  AWE  WINDOW  │  VirtualAlloc(NULL, size, MEM_RESERVE|MEM_PHYSICAL, PAGE_RW)      │
  │   │   (aperture)  │  RESERVED ONLY — never committed, no pages of its own             │
  │   │  1024 MB def. │  size  = "Testing Window Size (Mb)"     <── config                │
  │   │               │  base  = pMemTestWindow, FIXED for the whole run                  │
  │   └───────────────┘                                                                   │
  └───────────────────────────────────────────────────────────────────────────────────────┘
```

`MemMan_Init` (`bin/function.asm:69-229`) reserves the window. Note the non-AWE fallback path
at `:215` uses `MEM_RESERVE|MEM_LARGE_PAGES` instead, and `MemMan_Get`'s no-AWE branch (`:387`)
commits it with `MEM_COMMIT|MEM_LARGE_PAGES` — so **TM5 already used 2 MB large pages** when
AWE was unavailable, optionally `VirtualLock`ed (gated on the `Capable` bit
`Capable_UseVirtualLock`).

### 1.3 Acquiring physical memory

Per-core budget (`MainThread.asm:158-264`):

```
  availableRAM  (Mb, from GlobalMemoryStatus)
        │
        ├── minus reserve: max("Reserved Memory for Windows (Mb)", min(computed, 1024))
        │                  MaxMemRes_NoSwap = 1024   (MainThread.asm:16)
        │                  floor: if what's left < availableRAM/8, use availableRAM/8,
        │                         and never less than 16 MB (DefBlkGranularityMB)
        │
        ├── if AWE: minus 1/512 of itself       <- headroom for the PFN array
        │                                          (true cost is 1/1024; 2x safety margin)
        │
        ├── divide by core count  ("Cores", 0 = auto)
        │
        ├── if NOT AWE: clamp to "Testing Window Size (Mb)"
        │              (without AWE the window *is* the memory)
        │
        └── floor to a multiple of "Lock Memory Granularity (Mb)", min 1 granule
                                                     │
                                                     └──> dMemForCore
```

**Two-phase acquisition** (`MainThread.asm:277-293`) — a detail worth stealing:

```
  every core:  Cmd_Get(ExtCmd_GetMem,   dHalfMemForCore)   // 5/8 of target, ~2/3
                          │
                    ══ barrier: WaitToStateCritical(CoreState_GetMemoryHalf) ══
                          │
  every core:  Cmd_Get(ExtCmd_ResizeMem, dMemForCore)      // grow to full target
                          │
                    ══ barrier: WaitToStateCritical(CoreState_GetMemoryFull) ══
```

Grabbing ~2/3 first, syncing, *then* growing means no core can starve its peers by winning the
race to `AllocateUserPhysicalPages`. Fairness is enforced by ordering, not by a planner.

**Backoff on failure** (`bin/function.asm:336-355`):

```
  request N pages
      │
      ├─ AllocateUserPhysicalPages OK ────────────────> done
      │
      └─ failed
            N = N - N/8                     (×7/8, i.e. -12.5%)
            N = floor(N / granularityPages) * granularityPages
                                            granularityPages = LockGranularityMb << 8
            if N < 1 granule ──> give up with 0
            else retry
```

So TM5 is *also* a "take what you can get" allocator. The difference is what it ends up with:
a **flat pool of 4 KB physical pages**, with no block structure at all. Every page is
interchangeable. The block/chunk structure is imposed later, in virtual space, by the window.

### 1.4 The sliding window

The pool can be far larger than the window. `MemMan_Set` (`bin/function.asm:412-550`) rotates
it through the aperture in **window pages** (a "window page" = one window-sized slice of the
pool, *not* a hardware page):

```
  Locked pool: AWE_locked 4 KB pages, physically scattered, initially unmapped
  PFN array (one dword per page):

  ┌────┬────┬────┬────┬────┬────┬────┬────┬────┬────┬────┬────┬────┬────┐
  │ p0 │ p1 │ p2 │ .. │    │    │    │    │    │    │    │    │    │ pN │
  └────┴────┴────┴────┴────┴────┴────┴────┴────┴────┴────┴────┴────┴────┘
   \_________________/ \_________________/ \_________________/ \________/
     window page 0       window page 1       window page 2      page 3
     262144 pg = 1 GB    262144 pg           262144 pg          PARTIAL tail
                                                                (clamped, :503-506)

           wCurrentPage selects one slice:
             UnMap: MapUserPhysicalPages(base, curSize, NULL)
             Map:   MapUserPhysicalPages(base, curSize, &PFN[page * winPages])
                                     │
                                     ▼
                     ┌───────────────────────────────┐
                     │      AWE WINDOW  (fixed VA)   │   exactly ONE slice is
                     │      1 GB aperture            │   addressable at a time
                     └───────────────────────────────┘

  Return value of Cmd_Set = AWE_mapped << 12 = valid bytes in the window right now.
  The tail slice returns LESS than the window size — the test must honour it.
```

Cost, measured by the author on Win7 x64 @3.2 GHz for 1 GB (`bin/awe.asm:4-14`):

| Op | Cost |
|---|---|
| `AllocateUserPhysicalPages` (Get) | 55 ms |
| `MapUserPhysicalPages` (Map) | 32 ms |
| `MapUserPhysicalPages(NULL)` (UnMap) | 30 ms |
| `FreeUserPhysicalPages` (Free) | 85 ms |
| Map *without* a preceding UnMap | 35 ms — "неожиданный эффект!" |

That last row is why the rotation is cheap: remapping implicitly unmaps, so a slice change
costs ~35 ms, not 62 ms. At 1 GB per 35 ms the aperture rotation adds ~3% overhead to a
DRAM-bandwidth-bound sweep. Acceptable in 2012; pure loss today.

**AWE can be switched off** by three independent paths, all landing on the non-AWE branch where
window == memory:
- `bLang` bit 3 (`d3=1 → AWE blocked`, `bin/function.asm:96`) — set when privileges fail
- `Capable` bit `Capable_NoAWEbelow1G` — skip AWE if per-core memory ≤ window (`mt_ini.asm:591-598`)
- `Capable` bit `Capable_AWEdisable_UseMoreCores` (`mt_ini.asm:604`)

AWE requires `SeLockMemoryPrivilege`; `bin/awe.asm` will even try to *grant* it to the current
user via `LsaAddAccountRights` and then asks for a restart.

### 1.5 Test-side subdivision

`RunTestSequency` (`MainThread.asm:575-770`):

```
  for each test in "Test Sequence"
    │
    ├─ Cmd_Set(ExtCmd_SetPageNumb, 0)          reset to window page 0
    │
    └─ ToNextPage:  eax = valid bytes in window
         │  if 0 -> test done
         │  dWindowMemorySize = dNeedTested = eax
         │  pBaseAddr = window base
         │
         ├─ TestNewAddrInWindow:
         │     compute dRealBlockSize          (see 1.6)
         │     ┌──────────────────── window (dWindowMemorySize) ─────────────────────┐
         │     │ chunk 0 │ chunk 1 │ chunk 2 │ ... │ chunk n │ tail — NEVER TESTED   │
         │     └─────────┴─────────┴─────────┴─────┴─────────┴───────────────────────┘
         │       each = dRealBlockSize            remainder < 1 chunk is dropped
         │                                        (MainThread.asm:741 jge)
         │     per chunk:
         │        Test0.Cmd_Check   retention check of the previous fill
         │        Test0.Cmd_Set     re-fill
         │        TestN.Cmd_Check   the actual test for this sequence step
         │        Test0.Cmd_Set     re-fill, ready for the next step
         │     pBaseAddr += dRealBlockSize;  dNeedTested -= dRealBlockSize
         │     if dNeedTested >= dRealBlockSize -> next chunk
         │
         └─ Cmd_Set(ExtCmd_SetNextPage); Sleep(0); goto ToNextPage
```

Two structural facts:

- **Test 0 owns the pattern.** The orchestrator drives fill/verify through test 0's
  `Cmd_Set`/`Cmd_Check`; the sequence test only ever gets `Cmd_Check`. Tests are stateless
  per-chunk callbacks; the orchestrator owns every loop. (`Capable_UseTst0ForGenAndCheck`
  selects the variant at `:673`.)
- **The window slides, so coverage is 100%.** Every test visits every locked page, chunk by
  chunk, slice by slice. There is no notion of "the part of memory we didn't get to".

### 1.6 `Test Block Size (Mb)` — the discontinuity trap

This is the single nastiest TM5 compatibility detail, because the value is interpreted in
**two different unit systems** depending on magnitude. Loader `mt_ini.asm:287-303`, runtime
`MainThread.asm:627-661`:

```
  config value V
      │
      ├─ V <= 3   ──> kept as-is  ──────────┐
      │                                     │  runtime: V <= 15, so treated as a
      │                                     │  FRACTION CODE -> size = window/(V+1)
      │                                     │  then floor to LockGranularity, min 1 granule
      │
      └─ V >= 4   ──> V << 20 (MB→bytes) ───┐
                      clamp to window size  │  runtime: value > 15, so ABSOLUTE bytes
                      if 0 -> 1 MB          │  then floor to 4 KB, min 4 KB
                                            │  (no granularity rounding on this path)
```

So:

| `Test Block Size (Mb)` | Actual meaning |
|---|---|
| `0` | whole window (1/1) |
| `1` | **half** the window — *not* 1 MB |
| `2` | **one third** of the window — *not* 2 MB |
| `3` | one quarter of the window |
| `4` .. | 4 MB, 8 MB, ... genuinely megabytes |

`1usmus_v3.cfg` uses `1` and `2` in Test6/Test7, so this is not a theoretical corner — a
compat layer that reads those as "1 MB" and "2 MB" gets a chunk **~450× too small** on an
880 MB window. Values 4–15 are unreachable from config (they become megabytes first).

### 1.7 How long a chunk is worked — `Time (%)`

`bin/mtests0.asm:298-308`:

```
  dLoopCounter = (test "Time (%)" × global "Time (%)") / 2000     min 1     [ST_Div_Down=2000]
  dWriteReadCycleCounter = 4                                                [ST_WriteReadCycles]

  per chunk:   4 × ( 1 fill pass  +  dLoopCounter verify passes )
```

At the 100 % / 100 % default that is `10000/2000 = 5` → 4 × (1+5) = **24 passes over each
chunk** before moving on. This is the knob that decides *dwell time per chunk*, and it is
co-tuned with `Test Block Size` — see the sweep in 1.9.

### 1.8 Config parameter → concept map

`[Global Memory Setup]`:

| Parameter | Range | Controls | Where |
|---|---|---|---|
| `Testing Window Size (Mb)` | 512–1536, def 1024 | AWE aperture size = max addressable at once; also the hard cap on per-core memory when AWE is off | `CalcTstWindowSize`, `function.asm:655` |
| `Lock Memory Granularity (Mb)` | 1–512, def 16 | Quantum for per-core budget rounding, for allocation backoff steps, and for fraction-mode chunk sizes | `function.asm:342`, `MainThread.asm:233,637` |
| `Reserved Memory for Windows (Mb)` | 0–1024, def 128 | Memory withheld from the OS so it doesn't swap | `MainThread.asm:158-189` |
| `Channels` | 1–3, def 2 | Stride multiplier: `JumpStep = BlkSize × (Channels×Parameter − 1)` | `mtests0.asm:260-278` |
| `Interleave Type` | 0/1, def 1 | Selects the stride formula (0 = legacy dual) | `mtests0.asm:267` |
| `Single DIMM width, bits` | 64 only | DIMM→DIMM step for SIMD register rotation | `GetDIMM2DIMMstep` |
| `Operation Block, byts` | 64 only | `BlkSize`, the unit of a strided access | `mtests0.asm:246-250` |
| `Capable` | bitmask | `0x1` = `Capable_NoAWEbelow1G`; other bits gate `VirtualLock`, AWE-off-more-cores | `mt_ini.asm:591-604` |

`[Main Section]`:

| Parameter | Controls |
|---|---|
| `Cores` | Process count (0 = auto). Divides the memory budget; each core is a separate process with its own window + pool |
| `Time (%)` | Global multiplier into `dLoopCounter` — dwell per chunk |
| `Cycles` | Repeats of the whole `Test Sequence` |
| `Test Sequence` | Order of test indices; the same test index can appear many times |

`[TestN]`:

| Parameter | Controls |
|---|---|
| `Test Block Size (Mb)` | Chunk size within the window — **fraction if ≤3, megabytes if ≥4** (1.6) |
| `Time (%)` | Per-test multiplier into `dLoopCounter` (1.7) |
| `Parameter` | Test-specific: stride multiplier for SimpleTest, subblock count for MirrorMove |
| `Pattern Mode` / `Param0` / `Param1` | Pattern generator selection and seeds |
| `Enable` | Whether the test participates |

### 1.9 Reading the two community configs

```
                              1usmus_v3.cfg      Check_absolutnew.cfg
  Config Author               1usmus_v3          anta777 (ABSOLUT 01102021)
  Testing Window Size (Mb)    880                1536      <- max allowed
  Lock Memory Granularity     16                 64        <- 4x coarser
  Reserved for Windows (Mb)   128                512       <- 4x more held back
  Capable                     0x0                0x0
  global Time (%)             100                1250      <- 12.5x
  Cycles                      3                  3
```

`880` is a hand-tuned VA compromise: big enough to be efficient, small enough that a 32-bit
process still has room for its heap and the PFN array. `1536` takes the documented maximum.
`Granularity 64` + `Reserved 512` is the conservative pairing — coarser quantum, more headroom,
fewer allocation retries on a fragmented system.

**`Check_absolutnew.cfg` is a deliberate cache-residency sweep.** Its `Test Block Size` and
`Time (%)` are inversely co-tuned:

```
  Test  BlockSize   Time(%)   loops = T×1250/2000   BlockSize × (1+loops)
  ────────────────────────────────────────────────────────────────────────
   1      4 MB        240           150               604 MB
   7      4 MB        240           150               604 MB
   8      8 MB        120            75               608 MB
   9     16 MB         60            37               608 MB
  10     32 MB         30            18               608 MB
  11     64 MB         16            10               704 MB
  12    128 MB          8             5               768 MB
  13    256 MB          8             5              1536 MB   <- loops floor at 5
  14    512 MB          8             5              3072 MB      so the invariant
  15    window          8             5              9216 MB      breaks down here
```

For 4–128 MB the product is pinned at ~600 MB: **each chunk gets the same amount of traffic
before the test moves on**, while the chunk's *size* sweeps from L2/L3-resident to
DRAM-resident. That is the design intent — hold dwell constant, vary cache residency. Above
128 MB `dLoopCounter` saturates at its minimum of 5 and the invariant can no longer hold.

Total work per window pass is `window × 4 × (1+loops)`, i.e. proportional to `loops` alone —
so Test1 (150 loops) does ~25× the total work of Test12 (5 loops). The sweep equalises
*dwell per chunk*, not test duration.

`1usmus_v3.cfg` instead holds `Time (%)=100` everywhere and sweeps block size across
`0, 16, 32, 1, 2, 4, 8, 64` — mixing absolute sizes with the fraction codes.

---

## Part 2 — TMR: 64-bit, no aperture

64-bit VA (128 TiB of user space) removes the constraint that created AWE. TMR maps everything
it owns, permanently. What TM5 achieved with *one* mechanism (a sliding aperture over a flat
page pool) TMR splits into **three independent stages**.

```
  Stage 1  ALLOCATION   once at startup, per NUMA node
           OS-level blocks owned by each thread            [memory/allocator.rs]

  Stage 2  WINDOW       per test: byte budget for coverage
           how much of the thread's memory this test touches    [tests.rs]

  Stage 3  CHUNK        per test: iteration unit inside the window
           controls shutdown responsiveness + cache residency   [tests.rs]
```

### 2.1 Stage 1 — allocation

Per-NUMA-node target is split evenly across that node's threads, then decomposed greedily
(`allocator.rs:480-512`):

```
  per_thread_target = numa_total / threads_on_this_numa

  block_sizes_mb = [4096, 2048, 1024, 512, 256, 128, 64, 32, 16]
  greedy descending: take as many of each size as fit, move to the next

  target 3.5 GiB  ─────>  3 × 1 GiB  +  1 × 512 MiB
                          └──────────────────────┘
                          ONE thread, TWO different block sizes
  remainder < 16 MiB is warned about and dropped
```

Then the plan is executed under one of three strategies (`allocator=`):

```
  plan-pagesize-pref   (DEFAULT)  — page size wins, block size yields
  ┌─────────────────────────────────────────────────────────────────────────┐
  │ Phase 1   every planned size ≥1 GiB, Require(Huge 1 GiB) + align 1 GiB  │
  │ Phase 1b  extra 1 GiB chunks not in the plan, still huge pages          │
  │ Phase 2a  planned sizes ≥16 MiB, Require(Large 2 MiB) + align 2 MiB     │
  │ Phase 2b  ANY 2 MiB-backed size, to close the remaining byte deficit    │
  │ Phase 3   Prefer(Regular 4 KiB) for whatever is still missing           │
  └─────────────────────────────────────────────────────────────────────────┘

  plan-blocksize-pref            — block size wins, page size yields
  ┌─────────────────────────────────────────────────────────────────────────┐
  │ Phase 1   for each planned size (desc): try Huge, then Large            │
  │ Phase 2   Regular pages as absolute last resort                         │
  └─────────────────────────────────────────────────────────────────────────┘

  greedy                         — legacy, bypasses planning entirely
                                   (returns early to chunk_allocate)
```

Phases 1b/2b/3 can hand back block sizes that were never in the plan, so the *actual* set of
block sizes a thread owns is not knowable from the plan alone. `distribute_planned_chunks`
(`allocator.rs:1045+`) hands out `total_blocks / thread_count` of each planned size and then
mops up the off-plan chunks separately.

`Require(Huge)`/`Require(Large)` are always paired with a matching `alignment` — this is
load-bearing, not redundant, because on x86-64 a 1 GiB page *is* a PDPTE with PS=1 and can
only exist at a 1 GiB-aligned VA. Drop the alignment and the kernel silently falls back to
smaller pages while still reporting success. (See CLAUDE.md, settled decisions.)

### 2.2 Stage 2 — the window, and where it differs from TM5

`TestRunner::new` (`test_scaffolding.rs:66-68`) → `calculate_window_size(test_name, total_allocated)`
→ `prepare_blocks_for_window` (`tests.rs:1433-1478`):

```
  window_size = f(WindowMode, sum of ALL this thread's blocks)      then floor to 64 B

  blocks, largest first:
  ┌─────────── 1 GiB ───────────┐ ┌─────── 1 GiB ───────┐ ┌── 512 MiB ──┐
  │████████████████████████████ │ │█████████            │ │             │
  └─────────────────────────────┘ └─────────────────────┘ └─────────────┘
   test_size = min(size, remaining)  partial: prev_power_of_two   budget spent:
                                     (keeps chunk division exact)  block never
                                                                   opened at all
  █ = covered by the window budget
```

The pointer each test uses is `tb.block.buffer.as_mut_ptr()` (`test_harness.rs:192`) — the
**block base, always**. There is no window offset and no rotation between cycles.

> **This is the one capability TMR lost with AWE.** TM5's window slides, so every test covers
> 100 % of locked memory. TMR's window is pinned to offset 0 of each block, so whenever
> `window_size < total_allocated` the tail of the allocation is *never visited* — not this
> cycle, not any cycle. For deliberately cache-resident tests that is the intent (you want a
> small hot working set). But it means those tests always exercise the *same* DRAM cells at the
> *same* addresses, and cells outside the window get no coverage from them at all. TM5 got both
> properties at once: small working set *and* full coverage, because the small window walked.

Window modes (`tests.rs:14-28`):

| `WindowMode` | Meaning |
|---|---|
| `FullAllocation` | Everything the thread owns (unless `requires_locality`, which redirects to a cache-derived size) |
| `Cache { target }` | Tier-aware: `"L3/2"`, `"L3*4"`, `"DRAM*8"`. Divides per-thread for L3, per-SMT-sibling for L1/L2. Uses calibration data when present |
| `CacheTotal { fraction }` | `(L1+L2+L3) × fraction`. Not tier- or thread-aware |
| `Absolute { size_bytes }` | Hard byte count, e.g. `"880MB"`. This is the TM5 window analogue |

### 2.3 Stage 3 — chunk

`calculate_chunk_size(test_name, window_size)` (`tests.rs:1120+`), capped at the window:

| `ChunkMode` | Meaning |
|---|---|
| `Auto` | Per-test heuristic (`calculate_optimal_block_for_test`) |
| `Cache { target }` | Tier-aware. `L3/N` keeps writes warm through verify; `DRAM*N` forces eviction (refresh stress) |
| `CacheTotal { fraction }` | `(L1+L2+L3) × fraction` |
| `Absolute { size_bytes }` | Hard byte count — the TM5 `Test Block Size ≥4` analogue |
| `Fraction { fraction }` | Fraction of the resolved window — the TM5 fraction-code analogue |

```
  ┌──────────────── window span within one block ────────────────┐
  │ chunk │ chunk │ chunk │ chunk │ chunk │ chunk │ chunk │ chunk │
  └───────┴───────┴───────┴───────┴───────┴───────┴───────┴───────┘
    ^ ErrorCheckInterval decides how often the accumulator is inspected
      (power-of-2 shift, so the hot-loop test is `i & mask == 0`)
```

### 2.4 Config parameter → concept map

CLI (`params.rs`), all overridable from JSON:

| Parameter | Stage | Controls |
|---|---|---|
| `memory=20%` / `2GiB` / `2048MB` | 1 | Total reservation. `-from-available` (default, TM5-like), `-from-total`, `-target` |
| `allocator=plan-pagesize-pref` | 1 | Phase ordering: page size first (default), block size first, or legacy greedy |
| `min_page_size` / `max_page_size` (JSON) | 1 | Gate `regular`/`large`/`huge`; feeds `is_page_size_allowed` |
| `cpus=50%`, `cputype=`, `skip-cores=`, `cpu-stride=` | 1 | Thread count → the divisor for `per_thread_target`. `cpu-stride=even` also spreads across CCDs/memory domains |
| `window_mode` (JSON per test) | 2 | `full` / `cache` / `cache_total` / `absolute` |
| `chunk_mode` (JSON per test) | 3 | `auto` / `cache` / `cache_total` / `absolute` / `fraction` |
| `channels=` (JSON `system.channels`) | test | Stride formula, same role as TM5 `Channels` |
| `parameter=stride:N` / `subblocks:N` | test | TM5 `Parameter` equivalent |
| `write-read-cycles=4`, `verify-reps=`, `test-reps=` | test | TM5 `ST_WriteReadCycles` / `dLoopCounter` equivalents |
| `cycles=`, `duration=` | run | Repeats and wall-clock cap |

### 2.5 TM5 → TMR concept mapping

| TM5 | TMR | Note |
|---|---|---|
| 1 process per core, own VA | 1 thread per core, shared VA | `std::thread` + affinity, no MMF needed |
| `Testing Window Size (Mb)` | *(no equivalent)* | 64-bit removes the aperture. `WindowMode::Absolute` covers the *sizing* role but not the *addressability* role |
| AWE locked page pool | Stage 1 blocks | Flat 4 KB pages → sized VA blocks |
| `MapUserPhysicalPages` rotation | *(nothing)* | Lost capability — see the callout in 2.2 |
| `Lock Memory Granularity (Mb)` | *(partial)* — plan's 16 MiB floor | TMR has no single user-facing quantum |
| `Reserved Memory for Windows (Mb)` | `memory=` reserve semantics + `split` | Reframed as "how much to take" not "how much to leave" |
| `Test Block Size (Mb)` | `ChunkMode::Absolute` / `::Fraction` | The two TM5 unit systems became two explicit modes — a genuine improvement |
| `Time (%)` → `dLoopCounter` | `write-read-cycles`, `verify-reps` | Explicit counts instead of a percentage-of-a-magic-constant |
| Test 0 owns the pattern | `pattern_gen.rs` + phased harness | Pattern is a first-class module, not test index 0 |
| Orchestrator owns all loops | Tier 1 `run_phased_test` / Tier 2 `TestRunner` | Same separation, without the trait-dispatch perf cost |

---

## Part 3 — How other testers do it

Positioned on the axis that matters here: **who owns the memory, at what granularity, and is
the granularity uniform**. Treated at the architectural level — for anything but TM5 and TMR
this is from published documentation and general knowledge, not source I read for this doc, so
verify before depending on a specific number.

### 3.1 MemTest86 / MemTest86+ — bare metal, owns everything

```
  ┌──────────────────── No OS. The tester IS the kernel. ────────────────────┐
  │  Builds its own page tables. Physical address space directly visible.    │
  │                                                                          │
  │  0                                                              TOP      │
  │  ├──[tester]──┬──────────────── all remaining RAM ─────────────────┤     │
  │               │                                                          │
  │  Memory is addressed by PHYSICAL address; test windows are physical      │
  │  ranges split across CPUs. No allocator, no fragmentation, no page size  │
  │  negotiation — it maps what it wants.                                    │
  └──────────────────────────────────────────────────────────────────────────┘
```

This is the reference point for *capability*: because physical addresses are known, MemTest86
can implement address-line tests, a true row-hammer test, and per-DIMM/per-rank error
attribution (with SPD/SMBUS decode). It can also disable caching to force DRAM traffic.

Cost: it is not an OS-level tool. It cannot test under the OS's actual memory pressure,
cannot use the OS scheduler, and requires a reboot. For an overclocker iterating on BIOS
settings, reboot-per-test is the whole cost model.

The `Lowest/Highest address` config options are the analogue of a window: a physical range
limit, not a rotating aperture.

### 3.2 HCI MemTest — many tiny processes

```
  Historically a 32-bit binary. Each instance allocates a modest fixed amount
  (users typically run "MB per instance" ≈ 2000 or less) from ordinary user VA.

  ┌─proc─┐ ┌─proc─┐ ┌─proc─┐ ┌─proc─┐  ... one per core, or many per core
  │ flat │ │ flat │ │ flat │ │ flat │
  │buffer│ │buffer│ │buffer│ │buffer│      uniform size, by construction:
  └──────┘ └──────┘ └──────┘ └──────┘      the user types one number
```

Relevant because it is the extreme of the "uniform blocks" design — every worker gets an
identical flat buffer and there is no allocation planner at all. Coverage is whatever the OS
hands out. The multi-instance launcher culture around it exists precisely because a single
32-bit process couldn't address enough.

### 3.3 Karhu RAMTest — one process, coverage as the headline metric

Windows userspace, single process, user picks the amount to allocate (typically most of free
RAM). Its notable design choice is making **coverage %** the primary displayed metric rather
than elapsed time — the tool reports how much memory has been swept, which only makes sense
if the working set is walked systematically. Well regarded among DDR4/DDR5 overclockers for
error detection per unit time.

### 3.4 stressapptest / GSAT (Google) — uniform pages + a work queue

The most directly relevant comparison for TMR's open question, because it made the opposite
choice deliberately:

```
  One big mmap'd region (hugepages / shmem when available), then carved into
  UNIFORM fixed-size pages (--blocksize, ~1 MB class default):

  ┌────┬────┬────┬────┬────┬────┬────┬────┬────┬────┬────┬────┬────┐
  │ p0 │ p1 │ p2 │ p3 │ p4 │ p5 │ p6 │ p7 │ p8 │ p9 │p10 │p11 │ .. │
  └────┴────┴────┴────┴────┴────┴────┴────┴────┴────┴────┴────┴────┘
      │                                                    │
      └──── empty queue ◄───────────────────────┐          │
                │                               │          │
         ┌──────▼──────┐   fill            ┌────┴──────┐   │
         │ fill thread │ ────────────────► │valid queue│   │
         └─────────────┘                   └────┬──────┘   │
                                                │           │
         ┌─────────────┐   check + invert  ┌────▼──────┐   │
         │check thread │ ◄──────────────── │           │───┘
         └─────────────┘                   └───────────┘
      plus copy threads, and optionally disk/net threads sharing the same pool
```

Uniform page size is what makes the queue model work: any page can go to any thread, pages are
interchangeable tokens. It buys cross-thread memory exchange (a page filled by thread A is
verified by thread B, so it must survive a cache-coherence round trip) essentially for free.
That capability is TMR's TODO #19 cross-thread page exchange, and stressapptest gets it as a
*consequence* of uniform sizing.

Note the tradeoff it accepts: a queue between the test logic and the access. TMR's settled
"no request-routing through workers" rule rejects that for the *inner* loop — but stressapptest
queues whole pages, not individual accesses, so the hot loop inside a page is still a tight
local sweep. The two rules are compatible if the quantum is large enough.

### 3.5 memtester (Linux) — the minimal case

Single `malloc` of the requested size, `mlock` to pin it, then run a fixed battery of patterns
over one flat buffer. No threads, no NUMA, no page-size negotiation. Useful as the baseline of
"how simple can this be" — and a reminder that the allocation machinery in TM5/TMR exists to
serve *specific* test capabilities, not for its own sake.

### 3.6 Prime95 / y-cruncher / OCCT — cache-tier working sets

Not memory testers, but they solved TMR's Stage 2 problem first. Prime95's FFT size selection
is exactly `WindowMode::Cache`: small FFTs sit in L1/L2 (a core/power test), large FFTs spill
to DRAM (a memory test). The overclocking community's use of "Large FFT" as a memory-controller
test is the same idea as `Cache { target: "DRAM*N" }`.

y-cruncher's stress tests let you pick both total memory and an allocation mode, and it is
explicit that different algorithms have different access patterns (some sequential, some
strided/recursive) — i.e. it treats the *access pattern*, not the allocation, as the test
variable. OCCT's memory test is a userspace allocate-and-sweep with SIMD variants.

---

## Part 4 — Side-by-side

| | TM5 | TMR | MemTest86 | stressapptest | HCI MemTest |
|---|---|---|---|---|---|
| Runs under OS | yes (32-bit) | yes (64-bit) | **no, bare metal** | yes | yes |
| Memory owner | 1 process/core | 1 process, N threads | the tester | 1 process, N threads | N processes |
| Acquisition | `AllocateUserPhysicalPages` (AWE) | `VirtualAlloc2` | direct physical | `mmap`/shmem | `malloc` |
| Addressable at once | **window only** (≤1.5 GB) | everything | everything | everything | everything |
| Page sizes | 4 KB (AWE) or 2 MB (non-AWE) | 1 GiB / 2 MiB / 4 KiB | its own tables | hugepages when available | OS default |
| Block granularity | flat 4 KB pool, no blocks | **variable** 16 MiB–4 GiB | physical ranges | **uniform** (`--blocksize`) | uniform (user's number) |
| Uniform blocks? | n/a (no blocks) | **no** | n/a | **yes, by design** | yes, trivially |
| Coverage of owned memory | **100 %, window slides** | window budget from offset 0; tail may never be visited | 100 % | 100 % via page queue | 100 % of its buffer |
| Physical addresses known | no | no (settled decision) | **yes** | no | no |
| Cross-thread page exchange | no | not yet (TODO #19) | n/a | **yes, core design** | no |
| NUMA aware | no | yes | yes | yes | no |

### What each design's allocation model *unlocks*

```
  MemTest86      physical addressing  ──> address-line tests, rowhammer, per-rank blame
  stressapptest  uniform quantum      ──> page queue ──> cross-thread coherence stress
  TM5            sliding aperture     ──> small working set AND 100% coverage, together
  TMR            large + huge pages   ──> big physically-contiguous spans, best TLB behaviour
                 variable block sizes ──> maximum bytes captured under fragmentation
```

The last line is the one worth questioning, because "maximum bytes captured" is the only thing
variable block sizing buys, and it is not obviously worth what it costs. That argument is in
[`allocation_strategy_analysis.md`](allocation_strategy_analysis.md).

---

## Part 5 — Loop nesting: which knob repeats what

The single most important thing to know about either tool is **at which level of the loop nest a
given config knob multiplies**, because that determines locality, not just total work. The same
number of memory accesses concentrated on one chunk versus spread over a whole window are
completely different tests.

### 5.1 The size hierarchy, both tools

```
  TM5                                            TMR
  ───────────────────────────────────────        ────────────────────────────────────────
  locked physical pool          per process      allocation blocks        per thread
    dMemForCore                                    greedy 16 MiB .. 4 GiB
        │                                              │
  window / AWE aperture         global           window                   per test
    Testing Window Size (Mb)                       window_mode  (SIZE ONLY — no position)
    512..1536, VA aperture                         a byte budget, pinned to block offset 0
        │                                              │
  test block  ("chunk")         PER TEST         chunk                    per test
    Test Block Size (Mb)                           chunk_mode
    <-- THIS is what the kernel receives           <-- THIS is what the kernel receives
        │                                              │
  operation block               global           SIMD lane width
    Operation Block, byts = 64                     u64x2 / u64x4 / u64x8
```

**TM5 has no separate "chunk" parameter — `Test Block Size (Mb)` *is* the chunk**, it is
per-test, and the kernel is handed exactly `(pBaseAddr, dRealBlockSize)`
(`MainThread.asm:627-741`). The names differ from TMR's but the three levels correspond 1:1.

### 5.2 TM5's loop nest

Verified from `bin/mtests0.asm:312` (`LoopCheckCycle:`) → `:411` (`LoopRead:`) → `:520-522`
(`jg LoopRead` / `dec dWriteReadCycleCounter` / `jg LoopCheckCycle`). Both repetition loops sit
*inside* one test invocation, and one invocation covers **one chunk**.

```
  Cycles                                     <- [Main Section] Cycles
   └─ Test Sequence  (list of test indices)  <- [Main Section] Test Sequence
       └─ Test
           └─ Window slice   wCurrentPage++  <- slice SIZE  = Testing Window Size (Mb)
              │                                 slice COUNT = ceil(pool / window)
              └─ Chunk    pBaseAddr += size  <- TestN Test Block Size (Mb)
                  │                              (tail < 1 chunk is DROPPED, every pass)
                  └─ WriteReadCycles = 4     <- ST_WriteReadCycles — HARDCODED, not in config
                      ├─ 1 write pass over the chunk
                      └─ LoopRead            <- dLoopCounter
                          = (TestN Time(%) x global Time(%)) / 2000, min 1
```

At TM5 defaults (`Time (%)` 100 / 100): `dLoopCounter = 5`, so each chunk gets
`4 × (1 write + 5 reads) = 24 passes` **before `pBaseAddr` moves at all.** Locality is maximal
and deliberate — it is what makes `Check_absolutnew.cfg`'s block-size sweep (§1.9) meaningful.

Note what this means for the two `Time (%)` keys: they do **not** control duration by repeating
coverage. They control **dwell on each chunk**. Total runtime rises as a side effect.

### 5.3 TMR's loop nest (Tier 1, `run_phased_test`)

`test_harness.rs:239-266`. Structurally the same shape, with explicit counts instead of a
percentage of a magic constant:

```
  cycles / duration                          <- cycles= , duration= , min_duration_secs
   └─ Test Sequence                          <- test_sequence[]
       └─ Test
           └─ (NO slice level — window is pinned to block offset 0, see §2.2)
              └─ Block   (interleaved across the thread's blocks)
                  └─ Chunk                   <- chunk_mode  (window_mode caps total coverage)
                      └─ write_read_cycles   <- write_read_cycles   (= TM5 ST_WriteReadCycles)
                          ├─ test_reps       <- test_reps           (test op, e.g. mirror trips)
                          ├─ [flush + mfence]<- flush_before_verify
                          └─ verify_reps     <- verify_reps         (= TM5 dLoopCounter)
```

### 5.4 Knob → level table

| Loop level | Locality | TM5 knob | TMR knob |
|---|---|---|---|
| Whole run | lowest — full re-sweep | `[Main Section] Cycles` | `cycles=` / `duration=` |
| Test sequence | — | `Test Sequence` | `test_sequence[]` |
| Window slice | medium — advances coverage | `Testing Window Size (Mb)` (size) + pool size (count) | **none** — no position concept |
| Chunk walk | medium | `Test Block Size (Mb)` | `chunk_mode` (+ `window_mode` as a cap) |
| Write/read cycle | **highest — same chunk** | `ST_WriteReadCycles` (hardcoded 4) | `write_read_cycles` |
| Verify rep | **highest — same chunk** | `Time (%)` → `dLoopCounter` | `verify_reps` |
| Test-op rep | **highest — same chunk** | — | `test_reps` |

Two things fall out of this table, both covered in
[`allocation_questions_answered.md`](allocation_questions_answered.md):

- TMR has no **window slice** row. That is the missing sliding window — the reason a test whose
  window is smaller than its allocation never visits the tail (§2.2).
- TM5's `Time (%)` belongs on the **bottom** rows. TMR's legacy loader puts it on the **top** row
  (`config.rs:1439-1447`), which preserves neither the amount of work nor the locality.

### 5.5 Coverage of Tier 1 vs Tier 2

The nest in §5.3 is the **Tier 1** phased harness, which is the dominant path: 28 `run_phased_test`
call sites in `tests.rs` versus 8 `TestRunner::new` (Tier 2, where the test owns its own loop).
So `write_read_cycles` / `test_reps` / `verify_reps` are already the majority convention, not a
one-test special case — but whether each Tier-2 test honours them has to be checked per test. An
ignored dwell knob is silently lossy in the same way the memset trap was.

---

## Part 6 — Why TM5 could reshape memory and TMR cannot

This is the structural difference that decides how TMR should handle uniform tiles, and it is not
obvious from either tool's config.

```
  TM5 — the aperture is VIRTUAL, the pages are LOOSE
  ┌──────────────────────────────────────────────────────────────────────────────┐
  │  Locked pool = a flat bag of interchangeable 4 KB physical pages.            │
  │  Any page can be mapped to any window offset, at any time, for ~35 ms/GB.    │
  │                                                                              │
  │  => TM5 can RESHAPE the addressable region at will between tests:            │
  │     different slice size, different page order, different pool subset.       │
  │     The cost is a syscall, not a reallocation.                               │
  └──────────────────────────────────────────────────────────────────────────────┘

  TMR — the block IS the allocation, fused VA+PA
  ┌──────────────────────────────────────────────────────────────────────────────┐
  │  VirtualAlloc2 hands back a block of a FIXED size at a FIXED VA, and its     │
  │  large/huge pages exist only because the VA is size-aligned.                 │
  │                                                                              │
  │  => Reshaping means FREE + REALLOCATE:  seconds, not milliseconds; and it     │
  │     risks failing to re-acquire the same page size on a fragmented system.    │
  │     So block geometry is effectively FROZEN for the whole run.                │
  └──────────────────────────────────────────────────────────────────────────────┘
```

So TM5's per-test flexibility came from a cheap remap that TMR structurally cannot perform
without giving up large pages (AWE is 4 KB-only) or a driver.

**The resolution is that TMR does not need to reshape memory — only its description of it.**
A tile is a *descriptor* (`ptr`, `len`, `page_type`, `index`) computed by pointer arithmetic over
an existing mapping. Varying tile size between tests costs nothing at all: no syscall, no
reallocation, no page-size risk.

```
  fixed for the whole run          re-derived freely per test, for free
  ┌──────── block (1 GiB, huge pages, fixed VA) ────────┐
  │                                                     │
  │  test A, Q = 256 MiB:   [ T0 ][ T1 ][ T2 ][ T3 ]    │
  │  test B, Q =  64 MiB:   [][][][][][][][][][][][]…   │
  │  test C, Q = 512 MiB:   [   T0   ][   T1   ]        │
  └─────────────────────────────────────────────────────┘
```

Which means TMR can end up **more** flexible than TM5 was, not less: TM5 paid ~35 ms per GB to
change its view of memory and could only change it in window-sized slices; TMR pays nanoseconds
of setup arithmetic and can change tile size, tile order, and starting offset independently, per
test. The constraint that remains is the one TM5 didn't have — a tile can never exceed its
containing block, because there is no remap to stitch two blocks together (and VA stitching is
ruled out: adjacent VA is unrelated PA).
