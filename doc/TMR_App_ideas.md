# TM5 Rust Rewrite — Design Discussion

A discussion about designing a modern memory testing tool inspired by TM5, written in Rust, targeting x86-64 on Windows 11.

---

## User: Exploring new high-performance memory tests

I want to explore new types of high speed/performance and high memory intensity (both modules and memory controller) tests. The tests should help detect memory subsystem errors from overclocking too far.

For context I am working to remake a classic memory testing app called TM5 in Rust (instead of assembly ASM) and targeting modern x86-64 CPUs on modern Windows 11 type OSes.

Help me contemplate what test I should add to this application beyond the original TM5 test (primarily MirrorMove, SimpleTest). The memory allocation model and striding etc. It seems that's a key factor in how test exercise memory spatially to stress a memory controller and CPU cachelines/prefetching etc.

## Assistant

There's a lot of fertile ground here. The core insight you've already landed on is right: for a memory controller / DRAM stress tool, *what* bytes you write matters far less than *where* and *when* you touch them. TM5's original tests were clever about patterns but conservative about access geometry — modern DDR4/DDR5 controllers, multi-channel topologies, and large LLCs open up failure modes the old tests barely scratch. Let me walk through the dimensions worth considering, then sketch concrete test ideas.

### The design axes

Think of every test as a point in a multi-dimensional space:

**Working set vs. cache hierarchy.** Sub-L1 (32-48 KB) exercises core/AGU paths, never touches DRAM. L2-sized (~1 MB) hits the L2 controller. LLC-sized (tens of MB) stresses ring/mesh interconnect. Larger than LLC is the only regime that actually loads the IMC. A serious tool runs the same kernel at multiple working-set sizes and reports where it fails — overclocked SoC/uncore often fails *only* at LLC-resident working sets, while DRAM clock issues only show up beyond it.

**Stride / DRAM geometry.** This is where modern testing diverges most from TM5. DDR4/5 addresses decompose into channel → rank → bank-group → bank → row → column. Strides chosen to land repeatedly in the *same bank, different row* force constant ACTIVATE/PRECHARGE cycles and stress tRC, tRRD, tFAW timings — the timings overclockers most often relax. Strides that stay on one row exercise burst length and column timing. Random within a row is the row-buffer-hit best case (often where data integrity slips because the controller is most aggressive). You generally don't know the address mapping on Windows without a kernel driver, but you can probe it empirically: time strided reads at varying offsets and the latency curve reveals bank/channel boundaries. Once you have an estimate, you can craft adversarial strides.

**Read/write ratio and direction.** Pure reads exercise different buffers than pure writes. RMW (read-modify-write) at 50/50 is the worst case for write queue scheduling. Asymmetric — 7:1 read:write — is what real workloads look like and often where marginal trains fail. Don't forget *read-after-write to the same address* — bypass buffer territory.

**Cache bypass and prefetch.** Use the full toolkit: temporal stores (default), `_mm_stream_si*` non-temporal stores (bypass cache, go direct to write-combining buffers and out to memory — beautiful for raw BW), `clflushopt` / `clwb` for forced writeback, software `prefetchnta` / `prefetcht0`. A "no prefetch" test (random walk with dependency chain) and a "full prefetch" test (linear AVX-512 streaming) catch different controller paths. Hardware prefetchers will hide many marginal errors on linear workloads — defeating them is essential.

**Threading and topology.** Per-core threads with NUMA-local allocation is the floor. Beyond that: producer/consumer where two threads on different cores share a cache line (MESI ping-pong stresses coherence and uncore), all-threads-same-line RMW (extreme contention), and cross-CCD/cross-socket variants on Ryzen / multi-socket. SMT-paired threads sharing L1/L2 stress shared structures.

**Page size.** 4 KB pages give you basically random physical placement. 2 MB large pages give 512-cacheline contiguous regions — much more predictable for crafting stride attacks. 1 GB pages let you reason about hundreds of MB at a time. On Windows you need `SeLockMemoryPrivilege` and `MEM_LARGE_PAGES` with `VirtualAlloc`; worth supporting because it's the only way to do controlled spatial testing without a driver.

### Concrete test ideas to consider

Beyond MirrorMove and SimpleTest:

*BankThrash.* Compute strides (empirically calibrated or assumed, e.g. 8 KB/16 KB/32 KB depending on channels) that maximize same-bank/different-row hits. Walk through N rows in one bank, then N in the next. The DRAM is forced into worst-case ACT/PRE cycling. Use AVX-512 loads with NT hints to keep cache out of the way. Catches relaxed tRC/tRCD/tRP/tRRD.

*Hammer (non-malicious rowhammer-style).* Pick two row-aligned addresses likely to be aggressor/victim pairs and read-flush-read in tight loops. You're not trying to flip bits via Rowhammer — you're trying to detect cells that are already marginal because of overclocked refresh intervals (tREFI) or undervolted VDD. Pair with a verify pass that reads everything else in the bank. Especially useful for catching bad tREFI extension on DDR5.

*Subchannel divergence (DDR5).* DDR5 has two independent 32-bit subchannels per DIMM. Craft a test where two threads issue traffic addressed to keep both subchannels fully pipelined — and another where they collide on one. Differences in pass/fail tell you about controller scheduling and per-subchannel training.

*Streaming bandwidth with NT stores.* AVX-512 (or AVX2 fallback) `vmovntdq` writes at line rate to a working set 4-8× LLC. Round-robin across all channels via stride. Verify with a separate read pass. This is the "is your IMC actually stable at advertised BW?" test — surprisingly many overclocks fail here when SimpleTest passes.

*Pointer chase (dependency chain).* Build a randomized linked list where each load's address depends on the previous one's data. No prefetcher can help. The CPU stalls on real DRAM latency. Marginal command bus or CA training shows up here as random data corruption that the prefetched tests hide. Verify by chasing twice and comparing.

*Coherence storm.* N threads, each pinned to a different core, all RMW the same cache line with atomic ops or fenced stores. Then expand to N lines with N+1 threads. Stresses the ring/mesh, snoop filter, and on Ryzen the Infinity Fabric — all common overclock casualties (FCLK/UCLK instability often manifests here, not in raw DRAM tests).

*Modulo-N coupling patterns.* The classic March-family idea but with stride: write pattern P at address i, write ¬P at address i + stride, verify P. Sweep stride from 64B (line) up to multi-MB. Exposes coupling faults and sense-amp issues that solid-bit patterns miss.

*Address-in-address (variant).* Each cache line stores its own address (as 64-bit words) plus a per-line salt. After write phase, verify each line knows where it lives. Catches addressing faults — the controller delivering data from the wrong row. Good complement to data-pattern tests.

*Refresh-window probe.* Time a sustained workload's micro-latency and look for periodic spikes corresponding to refresh. Then deliberately schedule write bursts overlapping those windows. Detects tRFC violations and refresh-related corruption that's hard to catch with constant-rate tests.

*Frequency / intensity ramp.* Many overclock failures are resonant — a specific transition rate of Idd current causes voltage droop. Modulate workload intensity (sleep duty cycle) at varying frequencies (10 Hz to 10 kHz). Failures clustered at certain frequencies point at VRM/PSU rather than DRAM.

*Pseudo-random with verifiable seed.* PCG or xoshiro to generate both addresses and data from a single 64-bit seed. Run for hours; on suspected error, re-derive the expected value from the seed. Lets you do effectively unbounded coverage with fixed memory for "expected" data — important when your working set is 90% of physical RAM.

### A few cross-cutting suggestions

Make the test harness, not just the tests, first-class. A scheduler that sweeps working-set size, thread count, and stride for each kernel and reports a heatmap is far more diagnostic than ten fixed configurations. Overclockers want to know "fails at 3800 only when WS > LLC and threads ≥ 12" — that's actionable.

Verification has to be cheap or it dominates. CRC-per-block (CRC32C is hardware-accelerated on every modern x86-64) is usually the right tool. For mirror tests, AVX-512 `vpcmpd` is fast enough to verify at near-memory-BW.

Decide early whether you want physical-address awareness. Without it (no driver), large pages are your best lever; calibrate stride empirically by latency-curving a known buffer. With it (a tiny signed driver to read PFNs), bank-conflict tests become deterministic. The former is shippable to anyone, the latter is the difference between "approximate" and "definitive" testing.

Finally: error reporting. Capture address, expected, actual, time-since-start, thread, core, and ideally the last few cache events (PMU). When TM5 catches an error after 8 hours, the user wants to know if it was a stuck bit, a flipped bit, a flipped *adjacent* bit, or wholesale wrong-row delivery.

---

## User: vpcmpd, no driver, SIMD accumulator tradeoffs

What's `vpcmpd` is that an accumulator?

> Decide early whether you want physical-address awareness. Without it (no driver) <---- decided no driver. We either assume or have standard stride sizes/patterns. Or as you said calibrate it. I do have however cache size detections to best gauge and size worksets to target different memory tiers.

> Finally: error reporting... <---- While we can provide expected vs what we saw in some cases. If we go with a SIMD accumulator for validation, then we don't typically have it

## Assistant

Good questions — let me clear up `vpcmpd` and then dig into the verification-vs-diagnostics tradeoff, which is actually one of the more interesting design decisions for this kind of tool.

### `vpcmpd`

It's not an accumulator — it's an AVX-512 packed compare. Specifically: compare 16 pairs of 32-bit integers in a pair of ZMM registers, with a 3-bit immediate selecting the predicate (EQ, LT, LE, NEQ, NLT, NLE, FALSE, TRUE), and write the 16 boolean results into a *mask register* (k1-k7), one bit per lane. So `vpcmpeqd k1, zmm0, zmm1` gives you a 16-bit mask where bit *i* is 1 iff lane *i* matched.

The reason it's relevant to memory testing is that you can chain it with `kortestw` / `ktestw` to branch only when *any* lane mismatched, which is essentially free in the common (no-error) case. So the verify loop looks like:

```
load zmm0 from buffer A          ; 64 bytes
load zmm1 from buffer B          ; 64 bytes (or expected pattern)
vpcmpeqd k1, zmm0, zmm1
kortestw k1, k1                  ; sets flags from mask
jnz error_handler                ; only taken on mismatch
```

Two loads, one compare, one branch, per 64 bytes. That runs at L1/L2 BW limit. AVX2 has the equivalent with `vpcmpeqd` ymm + `vpmovmskb` + `test` — slightly clunkier but same idea, 32 bytes per iteration.

### Now your real question: SIMD verify gives up diagnostic info — is that OK?

You're exactly right that the fast-path SIMD compare loses information. Once `kortestw` says "some lane in this 64-byte chunk mismatched," all you know is *which 64 bytes* — you don't have expected vs. actual values cheaply. That's actually a feature, not a bug, if you design around it. The trick is a **two-tier verifier**:

**Tier 1 (hot path):** SIMD compare, branch only on any-lane-mismatch. Throughput-bound, runs at memory speed, no per-element bookkeeping. The `kortestw` branch is a near-perfect predictor in the no-error case. Cost on error: zero diagnostic data — but you don't care, because errors are rare.

**Tier 2 (cold path, only on mismatch):** Once you know *some* line in this region failed, walk the failing region scalar-style and capture full diagnostics: address, expected, actual, XOR (which gives you the bit pattern of the flip), and ideally re-read N times to see if it's persistent or transient. Since this only fires on actual errors, it can be as expensive as you want — even re-run the whole pattern generation to recover the expected value if you didn't keep it in memory.

This is much better than a checksum/CRC accumulator because:

- A CRC tells you "the block is wrong" but not *which line* — so you still have to do a second pass to localize. The SIMD `kortestw` already localizes to the chunk boundary, which on AVX-512 is one cache line. On AVX2 it's half a line.
- A CRC has aliasing — two errors in one block can cancel. `vpcmpeqd` cannot alias; if any 32-bit lane differs, the mask reflects it.

The mask itself is also genuinely useful diagnostic data even in the hot path. When you take the error branch, the value of `k1` tells you *which 32-bit lanes* in that line failed. One lane = single-word error, likely a bit flip. All 16 lanes = whole-line wrong, likely an addressing fault (wrong row delivered). Adjacent-lane patterns suggest burst errors. You can log the mask + line address very cheaply (it's already in a register) and decide whether to escalate to the scalar tier based on the bit-population of the mask.

So the recipe I'd suggest:

1. SIMD compare in the hot loop, branch on any mismatch.
2. On mismatch: log `(line_addr, mask, timestamp, thread, core)` to a thread-local ring buffer — that's already enough to classify single-bit vs. multi-bit vs. whole-line.
3. Then do a scalar re-read of the failing line, capturing expected/actual/XOR per 32-bit lane. Re-read it 3-5 times to flag transient vs. persistent.
4. Optionally, re-read the *neighboring* lines too — adjacent corruption is a strong signal for row/bank addressing faults vs. random bit flips.

The "expected" value in step 3 is the only real cost. Three options, in order of preference for your use case:

- **For pattern tests** (solid bits, walking ones, address-in-address): just re-derive the expected value from address + test parameters. Free.
- **For PRNG tests with verifiable seed**: re-run the PRNG from the seed up to that address. Costs CPU but no extra memory.
- **For mirror tests**: the mirror buffer *is* the expected value. Free, already in memory.

This means you essentially never need to keep a separate "expected" buffer at all, which matters when working sets are 90% of RAM. The cost is purely on the cold error path.

One more thing worth noting: the AVX-512 mask approach also gives you a free pattern-of-corruption histogram. Over a long run, accumulate `popcnt(mask)` distributions per test. If you see "always exactly 1 lane" the failure mode is bit flips (DRAM cell or transmission). "Always 16 lanes" means addressing/wrong-row. "Always 2-3 adjacent lanes" suggests burst-length or DQ lane issues. That classification falls out of the SIMD verifier essentially for free, which is something the original TM5 couldn't do.

---

## User: VirtAlloc2 large blocks vs. shared VA vs. worker model

Interesting, I think most of my tests verify with an accumulator for SIMD ops, but again that loses granularity in both error stats/frequency and transparency (bit flip vs entire payload wrong). I think I'll explorer your recommendation and see what the performance trade off is, if any.

I'm less interested in CPU, but currently my tests operate by using VirtAlloc2 in Rust to get large/huge page blocks up to 4Gb in size. while nice large blocks I typically run a test on the blocks sequentially. Would there be better access patterns if I sticked blocks into a single virtual address space OR even made a worker thread to govern blocks and then route test request (load/store) through the workers?

## Assistant

Good question, and the answer depends on what failure modes you're trying to surface. Let me unpack both options because they have very different implications.

### Sequential per-block testing (what you have now)

Strengths: simple, predictable, each block is contiguous in VA and (for large/huge pages) contiguous-ish in PA. Easy to reason about strides.

Weakness: at any given moment, you're only exercising the physical addresses that block happens to land on. If your motherboard has 4 DIMMs across 2 channels and one block lands entirely on channel 0, that test phase puts zero pressure on channel 1's controller path. You'll eventually cover everything as you rotate through blocks, but you never get *simultaneous* multi-channel pressure — which is exactly the regime where IMC scheduling, FCLK/UCLK, and inter-channel timing margins fail.

This is a real gap. Many overclock instabilities only manifest under sustained multi-channel load because the controller's arbitration logic is what's marginal, not any individual DRAM die.

### Stitching blocks into one VA region

You can do this with `VirtualAlloc2` + `MEM_RESERVE_PLACEHOLDER` and then map your large-page allocations into adjacent slots, or just allocate one giant reservation and commit it in chunks. The benefit is one logical address space your test kernels stride across without caring about block boundaries.

But — and this is the important part — **stitching VA does not stitch PA.** Two adjacent 1 GB virtual regions backed by separate 1 GB huge-page allocations will land at arbitrary, unrelated physical addresses. Your stride-2MB walk that crosses the boundary will jump to a totally unrelated row/bank/channel at the seam. So you don't gain deterministic spatial coverage from VA stitching alone; you mostly gain ergonomic simplicity in the test kernel (one base pointer, one length).

That ergonomic win is still worth something. It also lets you size the working set arbitrarily without per-block awareness in the kernel. I'd say: do it, but understand it's a code-cleanliness win, not a coverage win.

### Workers governing blocks (the more interesting option)

This is where the real coverage gain is. The model:

- N worker threads, one per channel-affinity-group (or just one per core, oversubscribed across channels).
- Each worker owns its blocks and runs test kernels against them concurrently with all other workers.
- A coordinator decides *which* test runs on *which* worker at any given moment, possibly different tests on different workers simultaneously.

What this buys you that sequential testing can't:

**Simultaneous multi-channel pressure.** N workers hammering N different blocks (likely on different channels and ranks) means the IMC sees aggregate bandwidth and request queue depth approaching its real maximum, not just one channel's worth. This is the regime that actually exposes IMC/FCLK/SoC voltage marginality. It's also where realistic workloads live — nothing real-world uses one channel at a time.

**Read/write asymmetry across blocks.** Worker A streaming reads on block 1 while worker B does NT writes on block 2 forces the controller to schedule mixed traffic across banks, which is harder than uniform traffic. Many marginal trains pass pure-read and pure-write but fail mixed.

**Cross-thread coherence pressure.** If two workers occasionally touch the same line (deliberately, in a "coherence storm" test), you get MESI/snoop-filter pressure on top of DRAM pressure. This is the only way to catch CCX/CCD interconnect issues on Ryzen, which are common FCLK casualties.

**Refresh-window aliasing.** With N independent workers, the chance that *all* of them are between refresh stalls at the same moment is low, so aggregate observed BW is smoother, but the *per-channel* refresh windows still create periodic dips. A worker doing latency-bound pointer chase will see those dips while another worker doing streaming BW won't. You can correlate.

### The "route load/store through workers" variant

You also asked about routing test requests through workers as a kind of RPC. I'd push back on that one — adding a queueing/routing layer between the test kernel and the actual memory access destroys the very thing you're trying to measure. You want the load/store to issue from the core in the tightest possible loop with the fewest possible intervening branches and dependencies. Any queue, channel, or atomic-handoff between "test logic" and "memory access" introduces synchronization traffic that dominates the signal.

The right division of labor is:

- **Coordinator thread:** chooses test parameters, assigns work, collects results, sequences test phases, manages the schedule. Cold path. Touches little memory.
- **Worker threads:** receive a *description* of work to do (test ID, address range, stride, pattern, duration), then run a tight self-contained kernel against their own block(s). Hot path. The worker is the one issuing every load and store.

So workers don't *route* memory operations — they *own* them and execute them locally. The coordinator only routes *jobs*.

A useful pattern: each worker holds a small lock-free queue of jobs (a job being a struct describing test + block + parameters). The coordinator pushes jobs; workers pop and execute. Workers report results via another lock-free queue or a thread-local ring buffer the coordinator drains periodically. This keeps all hot-path memory traffic per-worker and per-block, with synchronization only at job boundaries (seconds apart, not nanoseconds).

### Concrete recommendation

For your tool, I'd structure it as:

1. **Allocate large/huge page blocks as you do now**, but track them as a pool of "memory regions" with metadata (size, page size, NUMA node, time-since-last-touched).

2. **Optionally stitch them into one VA range** for kernels that benefit (large strided walks, pointer chase) using placeholder reservation. Keep the per-block view for kernels where the boundary semantics matter (BankThrash with calibrated stride wants to stay within a contiguous block).

3. **Add worker threads pinned to cores**, one per core or one per channel-affinity-group. Each worker holds a job queue.

4. **Define test phases** at the coordinator level: "for the next 30 seconds, every worker runs BankThrash on its assigned block," then "for 30 seconds, half run streaming-write while half run pointer-chase," etc. The interesting phases are the *heterogeneous* ones — that's where you find errors sequential testing misses.

5. **Measure aggregate behavior** at the coordinator: total BW achieved, error rate per phase, error rate per worker. Errors clustering on one worker point at a specific core/channel/rank; errors uniform across workers point at uncore/IMC/voltage.

The one extra thing this enables that's hard otherwise: **inter-block reference patterns**. Worker A's pointer chase can have its list nodes pointing into worker B's block (with appropriate sync at setup). Now you have one core issuing dependent loads that hit a different channel than its "home" channel — stresses the cross-channel routing in the IMC. Nasty test, finds things nothing else does.

Performance-wise, the worker model is essentially free at steady state — workers aren't talking to each other on the hot path, just doing their own thing on their own memory. The only cost is the job-dispatch overhead, which at second-scale phases is invisible. You should see *higher* aggregate memory pressure than sequential testing, not lower, because you're actually using all the channels at once.

---

## User: Current design — co-ordinator with per-thread blocks; rotation idea

Interesting, at the moment I have an allocator that just dishes out multiple blocks to fill as much memory as possible, equally among the platforms CPUs/threads. I then have a co-ordinator/runner that issues each CPU/thread it's own set of memory and the test op. The test runs then control goes back to the co-ordinator to issue the next test set. Part of the reason for this is synchronisation. As all threads should finish that test before moving to the next.

What I could probably do is improve upon this design with the push/pop queue per thread to allow cross thread/cpu working sets. ie. Group cpus (core 1+2, 3+4, 5+6 etc or loop them 1->2, 2->3, 3->4, and 4->1). Then I could still have each core work on it's allocated memory blocks, but when it's finished hand it over (push) to the next core for further work and grab (pop) blocks from other CPUs? Is this kind of what you mean. That would allow logical core 1 to pass on data to another CPU then hand it back or onward again

## Assistant

Yes, you've got the right shape — and the rotation pattern you're describing (1→2→3→4→1) is actually a really clean way to structure it. Let me push on the design a bit because there are two distinct things worth separating, and your sketch is mixing them in a way that's worth being explicit about.

### What the rotation actually exercises

When core 1 finishes a test on block A and hands block A to core 2, what happens physically?

Core 1's L1 and L2 are now full of block A's lines, mostly in Modified state (assuming the test did writes). When core 2 starts touching block A, every load misses core 2's L1/L2 and snoops out to core 1's caches via the ring/mesh/Infinity Fabric. Core 1 has to evict (writeback to LLC or DRAM, depending on inclusivity) and forward. This is **massive coherence/uncore traffic** on top of whatever DRAM traffic the test itself generates.

That's actually fantastic for stress testing — you're now exercising:

- The cache-to-cache transfer path (which on Ryzen crosses CCX/CCD boundaries if you pair cores carefully — exactly where FCLK marginality lives)
- The snoop filter / directory
- Writeback queues and victim caches
- Coherence ordering under contention

And you're doing it *while still hammering DRAM*, because once the handoff completes core 2's working set still has to be fetched from somewhere. So the rotation pattern gives you coherence stress *and* DRAM stress simultaneously, which sequential per-thread testing can't.

The CCX/CCD-crossing rotation is the spicy variant. On a Ryzen 7950X (two CCDs of 8 cores each), pairing core 0↔core 8 forces every handoff across Infinity Fabric. Pairing core 0↔core 1 keeps it within one CCX. The same test, two different rotation topologies, will fail at very different overclocks — and the difference tells you whether the problem is FCLK or core-local.

### The synchronization concern

You mentioned "all threads should finish before moving to the next" as the reason for the current barrier-style coordinator. That's the right instinct for *test phase boundaries* but too coarse for *work within a phase*.

Two-level synchronization solves this cleanly:

**Phase barrier (coarse, every few seconds):** All workers complete the current test phase. Coordinator collects results, decides next phase, releases workers. This is what you have now and you should keep it.

**Intra-phase handoff (fine, every few hundred ms):** Within a phase, workers pass blocks around according to a topology defined by the coordinator at phase start. No global barrier — just per-pair handoffs via SPSC queues.

The handoff itself doesn't need to be synchronous. Worker N pushes "block A done" to worker N+1's inbox and immediately starts on whatever's already in *its own* inbox. If nothing's there yet, it waits or works on its originally-assigned block. As long as the topology is a cycle (or any strongly-connected graph), work flows smoothly and no worker starves.

### A design that I think fits your tool

Let me sketch the structure:

**At phase start**, the coordinator decides:
- Which test kernel runs (BankThrash, pointer chase, streaming, etc.)
- The handoff topology (none / pairs / ring / cross-CCD ring)
- Phase duration or iteration count
- Per-block parameters (stride, pattern seed, etc.)

**During the phase**, each worker runs a loop:
```
loop {
    block = my_inbox.pop_or_take_own()
    run_test_kernel(block, params)
    if topology.has_handoff() {
        next_worker.inbox.push(block)
    } else {
        my_own_queue.push(block)  // keep it local
    }
    if phase_deadline_reached() { break }
}
```

**At phase end**, a barrier; coordinator drains result queues, computes statistics, possibly reassigns block ownership for the next phase.

The push/pop queues are SPSC (single-producer single-consumer) lock-free ring buffers — `crossbeam::queue::ArrayQueue` works fine, or roll your own with atomics. The traffic on those queues is tiny (one pointer + a few bytes of metadata per handoff, every few hundred ms) so contention is negligible.

### Topologies worth supporting

A small enum the coordinator picks from at phase start:

- **None / local:** each worker keeps its own blocks. Your current behavior. Useful baseline.
- **Pairs:** workers paired (0↔1, 2↔3, ...). Each handoff alternates ownership. Tightest coherence pressure between two specific cores.
- **Ring:** 0→1→2→...→N→0. Block visits every core once per cycle. Broad coverage, smoother load.
- **Cross-CCX/CCD ring:** ring ordering deliberately picks cores from alternating CCXs. Every single handoff crosses Infinity Fabric. Brutal on FCLK.
- **Random shuffle:** at each handoff, push to a randomly-chosen other worker. Defeats any prefetching of "the next handoff target's working set" by the cache hierarchy.
- **Producer/consumer:** half the workers only write, half only read, blocks flow from writers to readers. Realistic workload shape, stresses the IMC's read/write turnaround scheduling.

The user (or an automated sweep) selects which topology runs in which phase. Errors that appear under "cross-CCD ring" but not "local" point at uncore; errors in "local" but not the rings point at a specific core or its directly-attached DRAM region.

### Two things to be careful of

**Block ownership lifetime.** When you hand a block off, is the receiving worker free to reuse it however it wants, or does it need to preserve some invariant (like "this block holds expected pattern X")? For mirror tests with paired blocks, the pair must travel together or the receiver needs to know where the partner went. Easiest: the "block" abstraction the queue carries is actually a *job descriptor* that includes pointers to all related blocks plus the test parameters, not a raw block pointer. Keeps the unit of work self-describing.

**NUMA.** On a 2-socket system or even a Threadripper with multiple memory controllers, handing a block from a worker on socket 0 to a worker on socket 1 means every subsequent access crosses the socket interconnect. That's interesting stress (UPI/Infinity Fabric again) but it's a different failure mode than intra-socket coherence. Make NUMA awareness a parameter of the topology — "ring within NUMA node" vs. "ring across NUMA nodes" are different tests.

**Don't let handoffs starve the actual test.** The point is to stress memory, not to benchmark queue throughput. Handoffs should happen at a granularity where the test kernel runs for 100ms-1s between them, not microseconds. Otherwise the worker spends its time in queue ops instead of memory ops.

### So to directly answer your question

Yes, this is exactly what I meant, and your instinct to use it as an enhancement to the existing coordinator (rather than replacing the phase-barrier model) is the right call. The phase barrier handles "which test, what parameters" — the per-thread queues handle "where does the data live right now" within a phase. Two layers, two timescales, no conflict.

The cleanest mental model: the coordinator decides the *choreography* at phase boundaries; the workers and their queues execute the *dance* within a phase. Your current design is the choreography layer; you're adding the dance.

---

## User: Worker IPC / signalling — currently only a global atomic for shutdown

I think my workers currently don't have a good IPC/signalling mechanism to communicate work queues etc. The only thing they currently have is a global atomic shared from the runner/co-ordinator that is checked periodically to see if a shutdown/halt has been signalled. Apart from that there is no signal handling implemented

## Assistant

Right, that's a gap worth filling thoughtfully because the wrong choice here will either bottleneck your tests or make the code a nightmare. The good news is the requirements for a memory tester are unusually friendly — your signaling is *low frequency* (phase boundaries, handoffs every 100ms+) so you don't need anything exotic. The bad news is Rust's concurrency primitive landscape has lots of options and they're not equally good fits.

Let me lay out what you actually need, then map primitives to needs.

### The signaling needs you have (or will have)

Looking at the design we've been discussing, there are really four distinct communication patterns:

1. **Coordinator → all workers: "here's your next phase."** Broadcast, low frequency (seconds), workers are blocked waiting for it. Payload is a job descriptor (test ID, parameters, topology, deadline).

2. **Coordinator → all workers: "stop now."** Broadcast, asynchronous, must be observable mid-test-kernel without adding overhead to the hot loop. This is what your current global atomic does, and it's the right tool for this case — keep it.

3. **Worker → worker: "here's a block, your turn."** Point-to-point, medium frequency (per-handoff, ~100ms-1s), small payload (job descriptor pointer). SPSC.

4. **Workers → coordinator: "phase done" + results.** Many-to-one, low frequency, can be batched. Either a barrier + shared result slots, or an MPSC queue.

Each has different properties and the right primitive differs. Trying to use one mechanism for all four is where designs go wrong.

### Primitive-by-primitive

**`AtomicBool` / `AtomicU64` — keep using these for the shutdown flag.** They're free in the hot loop (one relaxed load per iteration of an outer loop, predicted-not-taken branch). Don't replace this with anything fancier. The pattern is:

```rust
// In worker hot loop, between test iterations (not inside SIMD loop):
if shutdown.load(Ordering::Relaxed) { return; }
```

You can extend this to a `AtomicU32` "phase epoch" — coordinator increments it when a new phase starts, workers compare against their last-seen epoch to notice. Cheap, lock-free, no allocation. Useful when combined with phase descriptors stored elsewhere.

**`std::sync::Barrier` — perfect for phase boundaries.** This is exactly what `Barrier::new(N+1)` is for: N workers + 1 coordinator. Everyone calls `.wait()`, no one proceeds until all arrive. Built into std, no dependencies, semantically obvious. Use it for "all workers finish phase, coordinator gathers results, coordinator releases workers into next phase."

The slight downside: barriers don't carry data. So you pair the barrier with a shared `Mutex<PhaseDescriptor>` or `ArcSwap<PhaseDescriptor>` that the coordinator writes between the two barrier waits. Workers read it after passing the barrier. Clean and obvious.

**`crossbeam::channel` — your workhorse for job queues.** This is what I'd reach for first. It's the de-facto standard, well-tested, supports both bounded and unbounded, MPSC and MPMC, and has a `select!` macro for waiting on multiple channels simultaneously.

For worker→worker handoffs, create N bounded SPSC channels (one inbox per worker, capacity 4-16). The coordinator wires them up at phase start according to the topology. Workers do `inbox.recv()` to wait or `inbox.try_recv()` to poll.

For worker→coordinator results, one MPSC channel with all workers as senders, coordinator as receiver. Workers push result records, coordinator drains at phase end.

The reason crossbeam is better than `std::sync::mpsc` here: it supports MPMC, has bounded channels (important — you don't want unbounded growth if a worker stalls), and `select!` lets a worker wait on `(inbox, shutdown)` without busy-polling.

**`tokio` / async — don't.** Async Rust is for I/O-bound concurrency. Memory testing is the pure opposite — you want OS threads pinned to cores running tight loops. Async would add scheduler overhead and obscure the threading model. Skip it entirely.

**`parking_lot::Mutex` / `RwLock` — for shared state at phase boundaries only.** If you have a shared "results so far" structure or a shared block ownership map, lock it. parking_lot is faster than std's mutex and gives you `try_lock_for(timeout)`. Don't put any locks in the hot test loop.

**Condition variables — usually unnecessary if you have channels.** A channel `recv()` already blocks. A barrier already blocks. The only time I'd reach for condvar is if I had a "wait until this complex predicate becomes true" situation, which you probably don't.

**Lock-free queues directly (`crossbeam::queue::ArrayQueue`) — only if profiling shows channels are too slow.** They will not be too slow at your handoff frequencies. Channels add maybe a few hundred nanoseconds per send/recv; you're handing off every 100ms+, so the overhead is invisible. Use channels until proven otherwise.

### Suggested structure

Here's what I'd build:

```
WorkerHandle (one per worker, owned by coordinator):
    inbox_tx: Sender<Job>          // coordinator (or peer) sends jobs
    inbox_rx: lives in worker thread
    result_tx: cloned MPSC sender to coordinator
    shutdown: Arc<AtomicBool>      // shared
    phase_epoch: Arc<AtomicU32>    // shared, lets workers detect phase change
    phase_barrier: Arc<Barrier>    // N+1 wide
    phase_desc: Arc<ArcSwap<PhaseDescriptor>>  // coordinator writes, workers read
```

Worker main loop:

```rust
loop {
    // Wait at phase barrier until coordinator releases us
    phase_barrier.wait();
    if shutdown.load(Relaxed) { break; }

    // Read current phase descriptor (set by coordinator before barrier release)
    let phase = phase_desc.load();

    // Run jobs until phase deadline or shutdown
    let deadline = Instant::now() + phase.duration;
    while Instant::now() < deadline && !shutdown.load(Relaxed) {
        // Get next job: from inbox if available, else our own assigned block
        let job = match inbox_rx.try_recv() {
            Ok(j) => j,
            Err(TryRecvError::Empty) => self.own_block.take_or_clone(),
            Err(TryRecvError::Disconnected) => return,
        };

        let result = run_test_kernel(&job, &phase);
        result_tx.send(result).ok();

        // Hand off if topology dictates
        if let Some(next) = phase.handoff_target_for(self.id) {
            workers[next].inbox_tx.send(job).ok();
        } else {
            self.own_block.put_back(job);
        }
    }

    // Phase done — sync up with coordinator
    phase_barrier.wait();
}
```

Coordinator main loop:

```rust
loop {
    // Decide next phase
    let next_phase = scheduler.next();
    phase_desc.store(Arc::new(next_phase));

    // Release workers into the phase
    phase_barrier.wait();

    // Drain results during the phase (or wait until end)
    let phase_end = Instant::now() + next_phase.duration;
    while Instant::now() < phase_end {
        if let Ok(result) = result_rx.recv_timeout(Duration::from_millis(100)) {
            stats.record(result);
        }
    }

    // Wait for workers to finish phase
    phase_barrier.wait();

    // Drain any remaining results
    while let Ok(result) = result_rx.try_recv() {
        stats.record(result);
    }

    // Report, decide whether to continue
    if should_stop() {
        shutdown.store(true, Relaxed);
        phase_barrier.wait();  // release workers so they can exit
        break;
    }
}
```

This gives you:

- Clean phase boundaries via the barrier.
- Loose coupling within a phase via channels.
- Cheap shutdown via the existing atomic, checked at job granularity (not inside SIMD loops).
- Per-worker job inboxes that the coordinator wires up however it wants — local-only, paired, ring, cross-CCD, whatever.
- Results streaming back continuously so a long phase doesn't lose data on crash.

### A few tactical notes

**Channel capacity matters.** Make worker inboxes bounded (4-16 jobs). If full, the sender blocks — that's actually correct backpressure. You don't want one slow worker accumulating an infinite queue.

**Be explicit about Job ownership.** A `Job` carrying a raw pointer to a block needs `unsafe Send`. Wrap blocks in a type that's `Send + Sync` and carries the pointer + length + metadata. Probably `Arc<MemoryBlock>` where `MemoryBlock` has the raw pointer in a `NonNull<u8>` field with appropriate unsafe impls.

**Don't `select!` in the hot path.** The worker only does channel ops between test runs (every 100ms+), so even an expensive `select!` is free. But don't be tempted to put channel ops *inside* test kernels.

**Memory ordering for the shutdown flag: Relaxed is fine.** You don't need Acquire/Release because shutdown isn't synchronizing data — it's just a "stop soon" flag. Workers will notice within one outer-loop iteration regardless. The phase barrier handles all the actual data synchronization.

**Crossbeam adds one dependency.** It's worth it. The std `mpsc` is being deprecated/replaced anyway and lacks features you'll want.

### The migration path

You don't have to do this all at once. A reasonable order:

1. Add `Arc<Barrier>` for phase synchronization. Replace whatever you have for "wait for all workers to finish a phase" with this. Smallest change, biggest correctness win.

2. Add the MPSC result channel. Workers send `TestResult` records; coordinator drains. Lets you stream results out instead of collecting them all at the end. Useful even before you add handoffs.

3. Add the `ArcSwap<PhaseDescriptor>` for passing phase parameters. Now your coordinator can change tests/strides/sizes per phase without restructuring worker code.

4. *Then* add per-worker job inboxes and the topology-driven handoff logic. This is the new capability; do it last when the rest of the plumbing is solid.

Steps 1-3 are pure infrastructure improvements over your current design — they make the existing model cleaner without changing its semantics. Step 4 is where the new test capabilities (cross-CCD rotation etc.) come in.

---

## User: Block size — few large vs. many small for handoffs

Plus my current allocation strategy is few large blocks to fill memory. I imagine with a worker sharing system we would alternatively want few small blocks, to make handover of work easier/faster. Waiting to process a 4GB block before handover likely won't do, and handing over a half finished block means more signalling/synchronisation

## Assistant

Exactly right, and this is one of those design decisions where the allocation strategy and the work-distribution strategy are deeply coupled — you can't really pick one without the other. Let me think through the tradeoffs because there's a sweet spot that's neither "few huge" nor "many tiny."

### Why 4 GB blocks are wrong for handoffs

A test kernel running over 4 GB at, say, 30 GB/s read+write bandwidth takes ~130ms minimum. In practice with verification, mixed patterns, and stride-induced bank conflicts, it's often seconds per block. That's way too coarse for handoff:

- A worker that finishes early has to wait for the slowest peer.
- A handoff that arrives mid-run can't be acted on until the current block completes.
- Phase boundaries with deadlines either truncate work (waste) or run over (sloppy).
- Cross-thread coherence stress only happens at handoff moments, so few handoffs = little coherence pressure.

You correctly identified the alternative — interrupting a 4 GB block mid-run — is worse because now you're carrying state ("I was at offset 1.7 GB with pattern P and seed S") across the handoff, which means more sync, more bookkeeping, and the receiving worker has to resume in a context it didn't set up.

### Why "many tiny" is also wrong

If you go to the other extreme — say, 2 MB blocks per job — you get different problems:

- **Allocation overhead.** `VirtualAlloc2` with large pages has setup cost. Thousands of separate allocations means thousands of large-page reservations, which on Windows can fragment the large-page pool and eventually fail.
- **TLB pressure.** Many small mapped regions = more TLB entries to cover the working set. Defeats one of the main reasons to use huge pages.
- **Per-job overhead dominates.** If the test kernel itself runs for 5ms on a 2 MB block, then queue ops, result reporting, and statistics gathering start eating significant fractions of CPU.
- **Spatial coverage shrinks.** A 2 MB block on Windows is one huge page = one contiguous physical region. Stride tests that need to hit many rows/banks need a working set bigger than the bank-group span (often 64 MB+ on DDR5). Below that, you can't even *construct* a meaningful BankThrash test.

### The two-level model

The right answer is to decouple **allocation granularity** from **job granularity**:

- **Allocate** a small number of large blocks (your current strategy — few 1-4 GB huge-page allocations to fill memory). This keeps allocation simple, TLB-efficient, and gives you contiguous physical regions big enough for stride tests.

- **Subdivide** each block logically into "work units" or "tiles" — say, 16-64 MB each — that are the unit of *job dispatch*. A job descriptor points to a block + offset + length. The block itself never moves; only the *right to operate on a region of it* moves between workers.

So a 2 GB block becomes 64 jobs of 32 MB each (or 128 jobs of 16 MB, or 32 jobs of 64 MB — tunable). Each job is small enough to complete in 5-50ms, which is the right granularity for handoffs.

This is essentially the same pattern as a parallel for-loop with chunk size — except the chunks are routed through a topology rather than a work-stealing pool.

### Picking the work-unit size

The right size depends on what the kernel does. Some constraints:

- **Big enough for the test to be meaningful.** A BankThrash test needs to span multiple rows and banks — say, 16-64 MB minimum. A streaming-bandwidth test wants enough data to amortize loop setup — 1 MB+ is fine. A pointer-chase wants enough nodes to defeat the prefetcher and exceed LLC — depends on LLC size.
- **Small enough for fast handoff.** Target 10-100ms per job. Below 10ms, dispatch overhead matters. Above 100ms, handoff latency hurts coherence stress and phase deadlines.
- **Aligned to architectural boundaries.** Make work units multiples of 2 MB (huge page granularity) at minimum, and probably multiples of 64 MB or 128 MB so that they're guaranteed to span enough banks for spatial tests.

A reasonable default: **32 MB work units**, giving you ~64 units per 2 GB block. At 30 GB/s a streaming test eats one in ~1ms (too fast — you'd batch multiple per job); at typical mixed-test rates a stride test takes 50-200ms per unit (about right). The kernel itself can decide whether to process the whole work unit in one shot or do a few internal iterations.

For tests that genuinely need huge working sets (cross-block strides, very long pointer chase), you can have *coarser* work units for those specific kernels — say, "this kernel takes 256 MB units, others take 32 MB." The work-unit size becomes a parameter of the test kernel, not a global constant.

### Block-locality vs. handoff freedom

There's a subtle question: when worker A hands a job to worker B, is the job pointing into a block that worker A owns, or are blocks shared?

Two models:

**Shared-block model.** All blocks are visible to all workers. A job is just `(block_id, offset, length, params)`. Workers read/write into any block. Simplest. Handoffs are pure metadata.

**Owned-block model.** Each block has a home worker. Cross-worker jobs are explicit handoffs that change ownership. More complex but gives you a notion of "home" — useful if you want to reason about NUMA placement.

For your tool, I'd go with the **shared-block model**. Reasons:

- The blocks are huge-page allocations in the same process VA space anyway. There's no real "ownership" at the OS level.
- Cross-block strides and inter-block reference patterns become trivial (a job can address any region of any block).
- The interesting locality property is *which core's caches the data is currently in*, and that's naturally tracked by the test execution itself, not by a block-ownership abstraction.

You can still have a notion of "preferred home" for NUMA — meaning "if no handoff topology forces otherwise, this work unit prefers worker X" — without making it an exclusive ownership property.

### Job descriptor shape

So a `Job` ends up looking something like:

```rust
struct Job {
    block_idx: u32,        // index into the global block table
    offset: u64,           // byte offset within the block
    length: u64,           // size of the work unit
    test_id: TestId,       // which kernel runs
    params: TestParams,    // stride, pattern seed, iterations, etc.
    generation: u32,       // for tracking which phase this belongs to
}
```

Tiny — fits in a cache line easily. Cheap to copy through channels. No pointers requiring `unsafe Send`. The block table itself (a `Vec<MemoryBlock>` shared via `Arc`) holds the actual raw pointers, and workers index into it.

This is much cleaner than passing block pointers around: jobs are pure data, cheap to copy, easy to log for diagnostics, easy to replay if you want to re-run a failing job for triage.

### Generating jobs

The coordinator at phase start does something like:

```rust
fn generate_jobs(blocks: &[MemoryBlock], phase: &PhaseDescriptor) -> Vec<Job> {
    let mut jobs = Vec::new();
    let unit_size = phase.test.work_unit_size();
    for (block_idx, block) in blocks.iter().enumerate() {
        for offset in (0..block.len).step_by(unit_size) {
            let length = unit_size.min(block.len - offset);
            jobs.push(Job {
                block_idx: block_idx as u32,
                offset,
                length,
                test_id: phase.test_id,
                params: phase.params.clone(),
                generation: phase.generation,
            });
        }
    }
    // Optionally shuffle to avoid sequential block access patterns
    jobs.shuffle(&mut rng);
    jobs
}
```

Then distribute initial jobs across workers (round-robin, or NUMA-aware) and let the topology-driven handoffs flow them around during the phase.

### What this gives you

- **Allocation stays simple** — your existing few-large-blocks strategy is unchanged.
- **TLB and physical contiguity benefits preserved** — huge pages, big regions, predictable spatial layout for stride tests.
- **Handoffs become cheap** — passing a 32-byte struct through a channel, no data movement.
- **Phase deadlines are clean** — workers check the deadline between work units, not mid-kernel. No partial-job carry-over.
- **Coverage is uniform** — small work units mean every worker touches many regions of many blocks during a phase, not just the one block it was assigned.
- **Tests can pick their own granularity** — the test kernel declares its preferred unit size, the coordinator generates jobs accordingly.

### One thing to watch

If your tests rely on **prior state in memory** (e.g., a verify pass that expects "block X was written with pattern P in the previous phase"), then sharded jobs need to carry enough information to verify their slice independently. This usually means:

- Patterns that are address-derivable: `pattern[addr] = f(addr, seed)`. Each work unit can verify its own range without coordination.
- Or paired write/verify phases where the write phase populates and the verify phase checks, with a phase barrier between them.

What you want to avoid is "worker A wrote half the block with pattern P, worker B has to verify it but doesn't know which half got pattern P." Make patterns location-deterministic and this evaporates.

### Summary

Keep the few-large-blocks allocation. Add a layer between blocks and jobs: blocks are physical resources, **work units are job-sized slices** (~32 MB), jobs are tiny descriptors that route through your handoff channels. Workers see "here's a region to test" without caring which block it came from or where it's going next.

This is also a structure that scales naturally: more memory = more work units in the pool, but the same 32 MB job size, the same handoff topology, the same channel infrastructure. You don't have to redesign anything when you go from a 16 GB system to a 256 GB system.
