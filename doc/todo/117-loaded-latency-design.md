# TODO 117: the loaded-latency tests, the design

From a design workflow on 2026-11-05: three independent designs (minimal, measurement-first, overclocker-first) and a judge that took
the third as its base and grafted parts of the others in. Owner decisions since: both tests, on main, in large commits. The
measurements behind it: `117-write-latency.md`.

## Approach

I took design 3 as the base and grafted in parts of designs 2 and 1. I also added three changes that the daddy-petz overclock captures call for. Those captures are already in baselines\2026-11-05-write-latency\oc-3600\probe-v3_{1,2}.txt.

**The base (design 3):**
- One test function with two registrations: A is flat out and idle; B is A plus matched levels.
- The whole schedule runs inside one step, as alternated write/read pairs in ABBA order.
- The measurer drives a phase word. Loaders that have nothing to do spin with PAUSE, so the idle rows run at the same clocks as the loaded ones.
- Levels can be pinned in MiB/s, so two settings can be compared at the same load.
- Every window's raw figures go into the results JSON.
- A is built first.

**From design 2:**
- The chain has a fixed size: max(1 GiB, 32 x the CPUID L3 instance), capped at the span. Design 3's DRAMFull grows and shrinks with memory= and the thread count, which changes how much of the chain the TLB covers, and takes seconds to build.
- One loader per physical core, so cputype= doesn't change the figure.
- Pacing in 4 KiB blocks, with staggered deadlines, a debt cap and a fell-behind count.
- The names Lat-Loaded-DRAM and Lat-Turnaround-DRAM.

**From design 1:**
- The L3-hit analysis behind 128 B chain cells.
- The loader kernels are stamped from Spd's validated hot-loop macros.
- Each thread picks its role once.
- Its minimalism: no ThreadPool state and no StepReport field. Roles come from cpu_assignments.

**Added from the overclock data:**
- At 50% load the turnaround read -3.7 ns in one run and +8.4 ns in the other (under writes 69.0 against 80.5 ns), while each run's 7 windows sat within 1 ns of each other.
- So the noise in measure B lies between instances of the load, not between windows. Each window therefore restarts every loader's stream at a fresh random block offset, and the summary rule is one pure function over the raw windows. It can be re-chosen after the data, for runs already taken.

**Claims checked against the code:**
- The shared-step pattern exists. ThreadPool::execute_test builds exchange::Step and storm::Step from cpu_assignments (src/thread_pool.rs:234-246). The storm's Step carries a Barrier (src/storm.rs:42-65), and the pool's barrier aligns each step's start (thread_pool.rs:466).
- Thread 0 is the top-ranked core's first CPU: thread_id is the index into cpu_list (thread_pool.rs:141-145), and cpu_list follows the rank order (src/cpu_selection.rs:294-303). Design 1's "probe on CPU 0" is wrong.
- The step's MiB/s is all bytes over the slowest thread's timed clock (src/runner.rs:1584-1585), so a measurer that counts no bytes leaves it to the loaders.
- The step's P50 merges every thread's histogram (runner.rs:1601-1630, 4659-4665). A P50 merged over several phases means nothing, so a loaded step leaves latency_* as None.
- The Lat- prefix already gives a 448 B minimum chunk (src/tests.rs:1111). seal_use gives Never to anything that isn't Mem- or Seal-Check (runner.rs:607-617), which is right because the loaders overwrite their extents.
- **Design 3's names are a bug.** --ram-latency filters on Lat-DRAM-*,Lat-DRAMFull-* and --cache-latency on ...,Lat-DRAM-* (src/session.rs:26,28). Lat-DRAM-Loaded would run in both, and under --cache-latency (cpus=1) it would have no loaders.
- Lat-DRAM-Read's chain links 8-byte slots (src/latency_tests.rs:110-121) over Dram{scale 4} of one L3 instance (tests.rs:126, 234-237). Each line is visited 8 times a cycle at random, so one thread hits L3 about a quarter of the time. That bias is the size of the turnaround: read loaders flush L3, NT writes don't. Hence 128 B cells over 32 x L3 or more.
- Layout A's 128 B blocks exist (src/latency_tests_v2.rs:44, 72-96). I don't reuse that builder, because it also shuffles data targets; a strided Sattolo is enough.
- spd_write_hot!/spd_read_hot! (src/bandwidth_tests.rs:51-118) are the loader bodies to reuse.
- StepReport already carries TestSummary (src/frontend.rs:354-375), so a field on TestSummary reaches both frontends. Design 2's StepReport.loaded isn't needed.
- The run compare compares only duration and throughput (StepChanges, src/results.rs:422-429), not latency. Design 1's "compare per step" would only cover MiB/s.
- latency_tests_run_in_place_on_the_span (runner.rs:5969-5988) requires one sample and bytes > 0 from every Latency test, so it needs a branch for these two.
- Design 3's config sites are right: storm_lines at src/config.rs:1144/1201/1228; mirror= at session.rs:670 and runner.rs:805.

**Rejected:**
- Design 1's step per condition: a fixed order so drift doesn't cancel, a peak cached across steps in ThreadPool, and no raw windows.
- Design 3's DRAMFull extent and Lat-DRAM-* names.
- Design 2's span_facts plumbing, per-window clock probe, block-random pattern, loaders=other-l3 and retarget rule. Each is deferred as a knob for later, if the data asks for it.

**The owner's point 3 ("wait for the overclock result"):**
- What it decides stays open: whether B ships beyond plan=all and in which plan, B's default levels, its headline row and its statistic, and which write measure TODO 117 keeps.
- The build order puts first what no result changes: the roles, the measurer, flat-out loaders, raw windows and A's report. B's pacing and levels are the first step that decision gates.
- What the captures show so far:
  - A moves a lot: idle 102.8 to 65.3 ns, flat-out writes 451.5 to 214.0 ns, flat-out reads 264.5 to about 151-157 ns.
  - B sits in its own noise: at 25% +1.4/-2.6 slow against +0.4/-1.7 overclocked; at 50% +6/+2 against -3.7/+8.4.
  - The overclock changed MT/s 2133 to 3600, FCLK 1067 to 1800 and the write timings together (WTRS 3 to 4, CWL 11 to 14, RDWR 10 to 8, WRRD 1 to 4 clocks; ZenTimings snapshots).
  - The write peak follows 16 B a clock x FCLK (17.1 against 16.9 GB/s slow, 28.8 against 28.3 overclocked). Flat-out writes there are fabric-bound.
  - So the comparison can't yet attribute B to timings. A one-timing A/B at fixed MT/s and FCLK can.

## Load control

Roles and kernels are fixed once at the start.

**A loader's loop**, per 4 KiB block:
- one call of its kernel (a function pointer at the run's width; the per-access loop is the Spd macro body, with no branches);
- one Relaxed store of its running byte count to its own padded slot;
- one Relaxed load of the phase word;
- under a level, one rdtsc and a PAUSE spin until the block's deadline.

Every 256 blocks (1 MiB) it also calls `add_bytes`, `update_progress_in_cycle` and the shutdown check. Kind::Done ends it within a block.

**Flat out:** interval 0, so there is no rdtsc and no spin.

**Park (idle windows):** the loader spins with PAUSE on the phase word, staying in C0. The idle rows then see the same core and fabric clocks as the loaded rows.

**Held to a rate:**
- share = target / loaders, and interval = 4096 x TSC Hz / share, from config.tsc_frequency_ghz.
- At each phase change, loader i takes its first deadline i/L of an interval late, so the loaders' blocks interleave. The aggregate is steady at microsecond scale, where the probe's 1 MiB steps burst.
- Debt is capped: a loader more than one interval late resets its deadline to now and counts a fell-behind block. It never bursts to catch up, and any shortfall shows in the reached MiB/s.
- Each phase change also moves every loader to a fresh random 4 KiB-aligned offset in its extent, so a window is a new instance of the load (see stability).

**The measurer drives the schedule:**
1. It writes the interval (Release), then bumps the phase word.
2. It chases untimed for SETTLE (200 ms).
3. It sums the slots and reads rdtscp.
4. It times 1000-read samples into the row's histogram until the window's TSC end, one compare a sample.
5. It sums the slots and reads rdtscp again.

A window's MiB/s is the loaders' bytes over exactly the timed window. Its ns is the window's timed ticks over its reads. The measurer counts no bytes, so the step's MiB/s is the loaders'.

**Matched levels:**
- A percent is of min(median flat-out writes MiB/s, median flat-out reads MiB/s) from this pass's flat-out pairs. That is normally the write peak: 16,117 MiB/s slow and about 27,000 overclocked on daddy-petz.
- A number is MiB/s, pinned. Pinned levels are how two settings are compared: the overclock moved 50% from 8.4 to 14.1 GB/s, so '50%' is a different load in each run.
- The header prints the resolved MiB/s and the load-levels= that pins them.
- A row is matched when both kinds' median reached MiB/s is within 3% of the target.
- Each cycle's step measures its own peaks; there is no cross-step state.
- Only the measurer's node loads. Loaders run one a core, so cputype=threads and cputype=cores give the same loader set.

## Stability

**Probe evidence:**
- Flat out repeats well: under writes 451.7/451.3 ns slow and 214.0/213.8 ns overclocked, and 90.3/91.1 ns on petz007.
- Flat-out reads move with the bandwidth reached: 150.8 against 156.7 ns at 48.5 against 45.9 GB/s. So each cell shows its MiB/s beside its ns.
- Matched levels were tight within a run (each condition's 7 windows within 0.5-1 ns) and far apart between runs: at 50% overclocked, under writes 69.0 against 80.5 ns, a turnaround of -3.7 against +8.4.
- So the controller settles into a state that lasts a whole condition. More window length alone won't fix that; sampling more instances will.

The design does that:
1. **Every window is a new instance.** The phase changes, the loaders restart at random offsets and staggered deadlines, and a 200 ms settle keeps the switch and the queues filling outside the timed window.
2. **ABBA pairs at every level and flat out.** Writes and reads alternate W R R W, so linear drift cancels within a pair: DIMM heating, DDR5's doubled refresh above 85 C, boost decay. Turnaround is the median of the pair differences, shown with [lowest-highest], so a timing change reads as real only when it clears that range.
3. **Robust figures.** A cell is the median of its window means, so one window hit by a DPC or SMI doesn't move it. P50/P99 of the 1000-read samples go to the JSON for the tail.
4. **No cache artifact.** One chain cell per 128 B pair over 32 x L3 or more means no L3 hits and no adjacent-line-prefetch hits in any state. Writes and reads then differ only in their DRAM traffic, not in what they evict. The 8 B-slot chain would have about 25% hits at 4 x L3 single-thread, and only the read load would take them away.
5. **Fixed geometry.** The measurer is always the top-ranked core with no busy sibling; loaders are one a physical core on its node; the chain has a fixed size and seed. All of it is recorded in the header and the JSON, so a comparison can confirm it.
6. **Same clocks.** Parked loaders spin in C0, so idle and loaded rows run at the same all-core boost and fabric state.
7. **Bandwidth counted over the timed window only**, from padded per-loader slots. This avoids the probe's early bugs: counting the warm-up, and counting in 256 MiB passes.
8. **Raw windows kept.** If the data shows the state is bimodal, the statistic (median, mean or trimmed mean of the windows or pairs) can be changed in summarize() and recomputed from results already written.
9. **Repeats inside a run.** The default 3 suite cycles give three schedules a run, with fresh peaks each.

**What it can't fix, so it is measured first:** if the state is set by where each loader's memory sits physically, it is fixed for a run, because TMR allocates once and never releases committed memory. Then a run's spread across cycles would understate the spread between runs. The first host runs compare the two.

## Reporting

**Console step report.** Illustrated with the probe's daddy-petz figures at 2133. The window draws the same table through Reporter::report_loaded.

```
📊 Test report - Cycle 1 - Step 2 Lat-Turnaround-DRAM: ✅ 41.3s, 0 errors, WHEA 0, Cycles 🟢 1/1, Time Limit 🟢
   Test: 38.4s, 301.22 GiB @ 8031 MiB/s, 0.31B ops @ 8.07M ops/s, Turnaround +6.0 ns at 8058 MiB/s
   Intermediate: ...
   Config: parameter=load_levels=25%,50% load_pairs=5 load_window_ms=1000, mode=-, extent=1.00 GiB, chunk=whole
🔍 Under load: thread 0 (CPU 2, core 1, node 0) measures; 7 threads load, one a core; 8 sit out
   1.00 GiB chain in 128 B cells; 256-bit loaders in 4 KiB blocks; 1.0 s windows after 0.2 s settles; median of window means [lowest-highest]
   Levels 25%, 50% of 16117 MiB/s (the lower flat-out peak): 4029, 8058 MiB/s; pin them with load-levels=4029,8058
   Load              Under NT writes               Under reads                   Turnaround
   idle              102.8 ns [102.5-103.4]
   25%  4029 MiB/s   113.0 ns [111.9-114.2] 4027  111.6 ns [111.0-112.3] 4028  +1.4 ns [-0.3 - +3.0]
   50%  8058 MiB/s   127.0 ns [125.1-129.9] 8051  121.0 ns [120.2-121.9] 8055  +6.0 ns [+3.9 - +8.1]
   full              451.7 ns [449.0-453.2] 16117 264.8 ns [263.9-265.6] 27180
```

The thread timing table that follows lists the loaders only, which shows how evenly they held their shares. There is no per-thread latency table: one P50 over every phase would mean nothing.

Lat-Loaded-DRAM shows only the idle and full rows, with no Turnaround column. Its headline is 'Under load: writes 451.7 ns @ 16117 MiB/s, reads 264.8 ns @ 27180 MiB/s'. With no loaders (cpus=1) the header says so and only idle is shown.

Units are TMR's one form: MiB/s for bandwidth, x.x ns, and [lowest-highest] for every spread. 'Turnaround' is the one name in the table, the headline and the JSON (turnaround_ns).

**Results JSON:**
- TestResult.loaded holds:
  - the measurer {thread, cpu, core, node}, loaders [threads], sat_out [threads];
  - chain_bytes, cell_bytes, width, block_bytes, window_ms, settle_ms;
  - levels {given, base_mib_s, mib_s, pinned};
  - rows [{load, target_mib_s, writes and reads {ns, lowest, highest, p50_ns, p99_ns, mib_s, windows}, turnaround {ns, lowest, highest, pairs[]} or null, matched}];
  - every raw window {index, kind, level, target_mib_s, mean_ns, mib_s, reads, fell_behind};
  - partial.
- Each threads[] record carries its role (measures / loads / sits out).
- latency_p* are absent for these steps.
- The step's throughput_mib_s is defined as before (the loaders' bytes over the step's timed time), so it is a mix of the phases. The rows carry the per-phase figures. The run compare and the final summary's per-test MiB/s mean little for these steps until the compare learns the rows, a follow-on after the owner picks the headline.

**The turnaround figure:**
- turnaround(L) = median over the ABBA pairs at level L of (window mean under NT writes - window mean under reads), at reached MiB/s matched within 3%.
- It is signed: negative means reads queue worse behind reads than behind writes (petz007: -3.7/-11.3 at 25/50%).
- The headline level, and the statistic, are the owner's to decide after the overclock result. Keeping the raw windows lets either be recomputed for old runs, by summarize() or by hw-pass.

## Framework changes

Paths are relative to C:\Users\Administrator\Downloads\TMR\TMR-APP.

1. **New src/loaded.rs** (pub mod in src/lib.rs), modelled on storm.rs and exchange.rs.
   - Constants: LOADED_NAME, TURNAROUND_NAME, NAMES, is_loaded(); BLOCK = 4 KiB, CELL = 128 B, SETTLE = 200 ms, MATCH = 3%.
   - Pure, unit-tested parts:
     - `enum Role { Measures, Loads{index}, SitsOut }` and `assign_roles(&[CpuAssignment], core_of: impl Fn(usize) -> usize) -> Vec<Role>`.
     - `enum Kind { Park, Writes, Reads, Done }`.
     - `Schedule::new(preset, &params)`: ABBA windows, `expected_secs()`.
     - `enum Level { Percent, MiBs }` with parse and `resolve(levels, peak_w, peak_r)`.
     - `pace(due, now, interval) -> (wait_until, fell_behind)`, with the stagger and the debt cap.
     - `summarize(&LoadedPart) -> LoadedSummary` (Serialize/Deserialize): rows, ranges, pair differences, matched flags, `headline()`.
     - `build_cycle_strided(base, cells, stride_words, seed)`: Sattolo as build_cycle_u64, using latency_tests::below.
   - `struct Step` (with a Debug impl):
     - roles;
     - a Barrier for the participants only (measurer and loaders);
     - the phase word, an AtomicU64 holding epoch and Kind;
     - an interval AtomicU64, ticks a block, written with Release before the phase;
     - one 128 B-padded AtomicU64 byte slot per loader;
     - the parsed params.
     `Step::new(&[CpuAssignment], &TestMemoryConfig)`.
   - `LoadedPart` (boxed): roles, the schedule, a WindowRecord per window, one Histogram per row (about 229 KB each, allocated before the clock), chain bytes, width, partial.
   - Thin wrappers `lat_loaded_dram_multi` and `lat_turnaround_dram_multi`, with the TestFunction::Latency signature, as the exchange's three names do (src/exchange.rs:755-775). Both call `run(preset)`, which branches on the role once into `measure()`, `load()` or `sit_out()`. The measurer holds a guard that stores Kind::Done on drop.
2. **src/bandwidth_tests.rs:** a `load_block_impl!` macro_rules stamps nt_write_block_{128,256,512} and read_block_{128,256,512}.
   - Each is #[target_feature] and #[inline(never)], taking (ptr, len).
   - Bodies: spd_write_hot! with stream stores and a lane-varied 0xA55AA55AA55AA55A pattern, then one SFENCE; spd_read_hot!, which has 4 accumulators.
   - `pub(crate) fn load_kernels(Width)` returns the pair of function pointers.
3. **src/tests.rs:**
   - `TestMemoryConfig::loaded: Option<Arc<loaded::Step>>`, None in new(), beside exchange and storm (tests.rs:836-839).
   - `picked_width()` (tests.rs:8017) gives both names Some(simd::test_width()).
   - Chunk sizing is unchanged (Lat- gives 448).
4. **src/thread_pool.rs `ThreadPool::execute_test`:** a third arm, `else if loaded::is_loaded(test_name)`, building the Step from self.cpu_assignments. No new pool state.
5. **src/latency_tests.rs `LatencyTestStats`:** `loaded: Option<Box<loaded::LoadedPart>>`, None in from_histogram. The `..from_histogram` sites are unchanged.
6. **src/runner.rs:**
   - registered_tests (after Lat-DRAMFull, about line 2868): the two entries, with ExtentMode::Absolute{max(1 GiB, 32 x cache_info.l3_cache)}, ChunkMode::Whole, cycles_only(1), with_tsc, memory_type None, and the preset's parameter_context.
   - execute_test_cycle:
     - progress.start_test gets Schedule::expected_secs for a loaded step (about line 1353), so the ticker has an ETA.
     - When the measurer's result carries `loaded`: summary.loaded = Some(Box::new(summarize(..))); latency_* stay None; no LatencyTestSummaryReport is built; create_thread_timing_report takes the loaders' rows only; the step is left out of thread_totals (lines 1694-1702), because paced bytes are no thread's speed.
   - thread_step_record (line 1787): sets role from LoadedPart.roles, and latency only when sample_count > 0.
   - Test latency_tests_run_in_place_on_the_span (line 5969): the two names run with load_window_ms=1 and assert the idle rows instead of one sample and bytes > 0.
7. **src/progress.rs `TestSummary`:** `loaded: Option<Box<loaded::LoadedSummary>>`. Constructors that get None: src/runner.rs:1417 and 1632, src/results.rs:1173, 1421 and 1721, src/test_support.rs:219, 272, 279, 345 and 353, crates/tmr-cli/src/render/tests.rs:280, crates/tmr-gui/src/model.rs:1600.
8. **src/results.rs:** `TestResult::loaded` (serde default, skipped when None, so old results still load), filled at the summary conversion (about line 592); `ThreadStepRecord::role: Option<String>` (skipped when None).
9. **src/reporting:**
   - `Reporter::report_loaded(&LoadedSummary)` beside report_latency_summary (mod.rs:159).
   - The formatter gets prepare_loaded_table and its header lines.
   - converters::format_parameter_context learns load_*.
10. **Frontends:**
    - crates/tmr-cli/src/render/run.rs `step_report` (lines 268-328): with s.loaded set, the headline replaces ', P50: .., Spread: ..' and report_loaded replaces the latency report.
    - crates/tmr-gui/src/report.rs `Report::of_step` (lines 36-84): the same two changes.
11. **src/config.rs:** load_levels (strings, 'N%' or MiB/s), load_pairs (1-50) and load_window_ms (10-60000) in TestConfig and TestParameterContext.
    - Range checks beside storm_lines (config.rs:1144).
    - Refused on any other test (line 1201), merged at line 1228.
    - A `load-levels=` override in src/session.rs (about line 670) and src/runner.rs (lines 110 and 805), beside mirror=.
12. **Docs:** doc/test_harness_tiers.md §6 (a row) and §8 (shared-step tests exist: exchange, storm, loaded; roles); doc/test_parameters.md (load_*); a new doc/todo/117-latency-under-load.md; the TODO 117 entry.

Unchanged: plans.rs, StepReport, ThreadPool fields, seal_use, chunk sizing, and every existing Lat-* kernel and its asm.

## Risks

- **B may not track timings, or not repeat between runs.**
  - The overclock captures show 50% turnaround -3.7 against +8.4 ns between two runs, with each run tight within itself.
  - If that state is set by physical placement, it is fixed for a TMR run, because memory is allocated once and never released. Only several runs per setting would resolve it; step 8(a) finds out.
  - If B doesn't clear its pair range in the single-timing A/B, it stays a diagnostic (by name or plan=all) and only A earns a plan place. A is built to stand alone.
- **Attribution.**
  - On daddy-petz the write peak is the Infinity Fabric write link (16 B a clock x FCLK: 17.1/28.8 GB/s against 16.9/28.3 measured), so flat-out-under-writes there tracks FCLK more than DRAM timings.
  - Under reads, loaders on the measurer's CCD share its link and L3. Part of any write/read difference can be fabric, not DRAM.
  - The report states what it measures; it can't separate the two.
- **Platform-shaped.**
  - petz007's turnaround is negative and grows with load.
  - m8i's is +7/+142/+520 ns at 25/50/75%, with a knee near 50%.
  - One headline level won't suit every CPU; the table shows them all.
- **Near the write peak the controller flips states** (75%: 123/176 ns slow, 95.5/86.1 overclocked). A pinned level near the peak will read unstable; its range shows it.
- **4 KiB pages.** Page walks read page tables that the read load evicts and NT writes don't, so the turnaround is biased. TMR already warns at allocation when large pages fail. Such a run isn't comparable with a large-page one; the detail file says so.
- **Topology.**
  - Only the measurer's node loads: one node's figure on SNC or multi-socket hosts.
  - A 4 KiB span that spilled to another node isn't detected (design 2's span_facts is deferred).
  - Hybrid or E-core loaders may miss their share and show 'not matched'.
  - Power limits can lower the measurer's clock under load; that isn't recorded (the clock probe is deferred).
- **Scaffolding.**
  - A participant that panics before the Step's barrier hangs the step, as the storm's and the exchange's would. After the barrier the measurer's guard ends it.
  - The ABBA schedule sets the step's length, not TestRunner's time limit.
  - Sit-outs report 0 cycles, so the results' cycles_completed_min reads 0.
  - In a sealed run, every thread's 1 GiB extent is seal-checked before the step, sit-outs' too: between-step time only.
- **Memory.** 1 GiB or more a thread. Smaller spans cap the chain, and the header warns below 16 x L3.
- **Size and contention.** The change touches runner.rs, the reporting code, results.rs and both frontends' goldens while other agents commit to main, so expect rebases. The new loader kernels and chase loop need the asm and spill check.
- **Meaning of the step's MiB/s.** The Test: line's MiB/s and the final summary's per-test MiB/s for these steps mix the phases. The run compare reads only throughput (src/results.rs:422-429) until it learns the rows.
- **Adjacent, named, not fixed.** Lat-DRAM-Read/Write/Copy chain 8-byte slots over 4 x L3 (src/latency_tests.rs:110-121). Run single-thread (--cache-latency), about a quarter of their reads should hit L3, so the DRAM figure reads low. That is analysis, not yet measured: a single-thread Lat-DRAM-Read against Lat-V2-DRAM-Read would show it.

## The tests

### Lat-Loaded-DRAM

**Measures:** Measure A, latency under load: the latency of one chained read (1000 dependent reads a sample, timed by TSC between rdtscp fences, as Lat-* do) in three states:
- idle, with the loaders spinning on PAUSE and moving no memory;
- every other core streaming NT writes flat out;
- every other core streaming reads flat out.
Each state reports the median of its 1 s window means with [lowest-highest], P50/P99 of its samples (JSON) and the loaders' MiB/s over exactly those windows. No correctness check. With no loaders (one thread, or a call with no Step) it measures the idle rows and says so.

**Roles:** Chosen once from loaded::assign_roles(cpu_assignments), a pure function:
- **Measures:** thread 0, the run's top-ranked core.
- **Loads:** the first thread of each other physical core on thread 0's NUMA node.
- **Sits out:** thread 0's SMT siblings, the loaders' siblings and other nodes' threads. They return at once and wait in the worker's recv, so the measurer's core has no busy sibling and other controllers stay idle.
- Each loader picks its block kernels at the run's width (simd::test_width) once.

**Extent:** ExtentMode::Absolute{max(1 GiB, 32 x CPUID's L3 instance)}, set at registration from cache_info. That is 1 GiB on the 5700X and the 8600G, 3 GiB on an X3D. It is capped at the span; under 16 x L3 the header warns.
- The measurer chains one cell per 128 B line pair (build_cycle_strided, Sattolo, stride 16 words). Each cell comes round once a cycle and the adjacent-line prefetcher never fetches a chain cell. At 1 GiB on 2 MiB pages that is 512 TLB entries; on 1 GiB pages, one.
- Loaders stream over the same extent of their own span, filled once with an NT pass (setup_done(PatternFill), which also faults 4 KiB pages in).
- ChunkMode::Whole, TestTiming::cycles_only(1): one schedule pass a step.
- Schedule: idle, then 3 ABBA pairs flat out (W R R W W R), then idle. 8 windows of 0.2 s settle + 1 s, about 10 s, plus about 1-2 s of untimed chain build and fill.

**Reported:** Under load table rows idle and full, with Under NT writes and Under reads cells (ns [lowest-highest] plus MiB/s), and no Turnaround column.

The Test: line headline replaces P50/Spread: 'Under load: writes 451.7 ns @ 16117 MiB/s, reads 264.8 ns @ 27180 MiB/s'.

Results JSON: TestResult.loaded with rows and raw windows, ThreadStepRecord.role, and latency_p* absent.

### Lat-Turnaround-DRAM

**Measures:** Measure B, write turnaround: read latency under NT writes minus read latency under reads at the same MiB/s, at each matched level. It is what writes cost a read beyond the bus time they take: write-to-read bubbles, write-drain switches, scheduling. It is signed, because petz007 reads negative.

The step also runs all of A; its flat-out rows set the levels. A level is a percent of the lower flat-out median (normally the write peak), or MiB/s pinned with load_levels=4029,8058 or the load-levels= command-line override.

**Roles:** As Lat-Loaded-DRAM. Under a level each loader is held to its share (level / loaders) by TSC deadlines, one per 4 KiB block.

**Extent:** As Lat-Loaded-DRAM.

Schedule: idle, then load_pairs ABBA pairs flat out (the peaks and A), then load_pairs ABBA pairs at each level, then idle.

Provisional defaults, to be settled after the overclock result:
- load_levels=25%,50%. 75% flipped between states on daddy-petz at both settings: 123/176 ns slow, 95.5/86.1 overclocked.
- load_pairs=5, load_window_ms=1000.
- About 38 s a pass at 2 levels.

**Reported:** The Under load table with a row per level between idle and full. A level row has its writes and reads cells (each with its reached MiB/s) and a Turnaround cell: the median of its ABBA pair differences, with [lowest pair - highest pair].
- The cell reads 'not matched' when either kind's median reached MiB/s is more than 3% under the target.
- Test: line headline: 'Turnaround +6.0 ns at 8058 MiB/s'. The provisional rule is the highest level both kinds matched; the owner settles it after the overclock result.
- The header prints the resolved levels and the load-levels= to pin them.
- JSON: as A, plus the levels and each row's pairs.

## Build steps

1. 0. Before starting, tell the owner the size: about 1,300 lines over the engine, CLI, GUI and results schema. CLAUDE.md asks for a branch for a large change or one that may be thrown away. Ask whether it goes to main in the small commits below or on a branch. Show the overclock captures already in baselines\2026-11-05-write-latency\oc-3600 for the point-3 decision (A moves a lot; B sits in its noise; the capture changed MT/s, FCLK and timings together). That decision gates step 6 onwards (B's pacing, levels and defaults), not steps 1-5. Write doc/todo/117-latency-under-load.md (the probe data at 2133, OC 3600, petz007 and m8i; this design; the open decisions) and update TODO 117's Next; run python todo_check.py.
2. 1. src/loaded.rs pure parts and their unit tests:
- assign_roles over synthetic topologies: 5700X 8c/16t under cputype=threads and =cores; a 2-node box; one thread; cpus=50%; the measurer's sibling and loaders' siblings sitting out; one loader a core.
- Schedule: ABBA order and expected_secs for both presets.
- build_cycle_strided at stride 16: one cycle through every cell, touching only the cells' words (a canary elsewhere).
- summarize on synthetic windows: medians, ranges, pair differences, the 3% matched flag, headline().
Then cargo fmt and python tools/ci_local.py --quick; commit.
3. 2. src/bandwidth_tests.rs: load_block_impl! stamps nt_write_block_{128,256,512} and read_block_{128,256,512} from spd_write_hot!/spd_read_hot!, plus load_kernels(width). Unit-test that a write block holds the pattern and a read block returns the XOR, at each width the CPU has. Read the post-LTO asm (cargo rustc --release -p test-memory-r --bin tmr -- --emit asm): vmovntdq at xmm/ymm/zmm, 4 independent vpxor accumulators, no memset/memcpy. Run hot_loop_spills.py. Quick CI; commit.
4. 3. The A half of the bodies: Step (roles, participant Barrier, phase word, padded slots), measure() (chain, settle, windows, a histogram per row, the Done guard), load() flat out and parked (fill, per-block phase check, per-window random restart, progress and shutdown every 256 blocks), sit_out() (return at once), and the wrappers. In-process tests on test_support Regions with scoped threads, as storm.rs does: a 4-thread step with 5 ms windows and a small Absolute extent gets the right roles and every A row; loaders stop on Done; sit-outs return at once; SHUTDOWN_REQUESTED mid-schedule ends every role with a partial result (nextest runs each test in its own process); with no Step it gives the idle rows only. Asm and spill check of the chase loop (a bare mov chain). Quick CI; commit.
5. 4. Plumbing: TestMemoryConfig::loaded; the execute_test arm; register Lat-Loaded-DRAM; picked_width; the ETA in execute_test_cycle; the branch in latency_tests_run_in_place_on_the_span. Quick CI; commit.
6. 5. Results and report: LatencyTestStats.loaded; TestSummary.loaded and every constructor listed in framework_changes; TestResult.loaded and ThreadStepRecord.role (serde round trip, and an old file without them still loads); the runner's result path (latency_* None, loaders-only timing table, no thread_totals); Reporter::report_loaded and the formatter table; the CLI step_report and GUI of_step headline and table. Golden tests: a console step report in crates/tmr-cli/src/render/tests.rs, the table in crates/tmr-cli/src/renderer/tests.rs, the GUI lines. Docs: test_harness_tiers.md §6/§8. Quick CI; commit. A is complete here and needs nothing B decides.
7. 6. (Gated by the owner's point-3 decision.) B: pace() wired into load() with stagger, debt cap and fell_behind; levels resolved from the flat-out pairs; the Turnaround preset and registration of Lat-Turnaround-DRAM; load_levels / load_pairs / load_window_ms in src/config.rs with range checks and refusals beside storm_*; the load-levels= override beside mirror= (session.rs, runner.rs). Unit tests: pace arithmetic; level parsing; percent against pinned; config refusals. A pacing test: one loader at a low target holds within 5% over 200 ms. A golden for the Turnaround row and headline. Docs: test_parameters.md. Quick CI; commit.
8. 7. Once for the batch: the full python tools/ci_local.py (release build, nextest, audit); keep the exe in ..\builds\<commit>\.
9. 8. Measure first. The owner runs it, asked first, with the release exe from builds\<commit>\ and memory=50% cputype=cores cpus=100% skip-cores=0; about 3 minutes a run:
(a) daddy-petz at its current overclock: test=Lat-Loaded-DRAM,Lat-Turnaround-DRAM load_levels=25%,50%,75% at the default 3 cycles, two runs. Set against the probe (idle 65.3; full W 214.0, R 150.8-156.7; 50% W 69.0/80.5, R 72.2-72.7). Then the key comparison: the spread across one run's 3 cycles against the spread between the two runs, which says whether the state is per window (the restarts sample it) or per allocation (fixed for a run).
(b) The same at load_window_ms=250 with load_pairs=20, and at 3000 with 2 pairs: keep the window whose pair median repeats best between runs.
(c) A single-timing A/B at fixed MT/s and FCLK, the attribution test: one BIOS value moved (tWRRD 4 to 1 or 8, or tRDWR 8 to 12, or tWTR_L 8 to 16), with load-levels= pinned from run (a). Does B clear its pair range? Does flat-out-under-writes move, or only follow FCLK?
(d) Sanity: the flat-out loaders' MiB/s against Spd-DRAMFull-Write-Auto and -Read-Auto on the same host; and cputype=threads once, which must give the same loader set and figures.
(e) petz007 once (negative turnaround, sign handling) and desktop-v6v7goc once (hybrid loaders falling behind).
10. 9. With the owner, from that data: B's default levels and headline row; the statistic in summarize() (median or mean of pairs); whether B ships beyond plan=all; Lat-Loaded-DRAM into plan perf once it repeats on more than one host; whether the run compare learns the loaded rows. Then close or narrow TODO 117 in TODO_ARCHIVE.md.
